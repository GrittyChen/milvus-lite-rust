"""milvus-lite 同负载基准：经 pymilvus MilvusClient 驱动，对 2.5.1 与 3.x 输出同一组指标。

用法: python bench.py <db_path> <N> <dim> <index_type> <phase> [mode]
  phase = build  : 建表 + 写入 + 建索引 + load + 检索 / 过滤 / 查询 / 点查 / 删除 / upsert
  phase = reopen : 在 build 产生的 db 上新起进程 load + 检索
  phase = m1     : M1 口径（0004「验收」第 4 项）：与 build 相同的建表、写入、建索引与 load，之后只测
                   无过滤检索、空 expr 的 count(*)、按主键 get，以及同一进程内多线程、多进程的并发检索；
                   三个实现（2.5.1、3.x、本仓的 Rust 实现）都可运行
  phase = m2     : 持久化完整操作、线程/进程并发、持续 upsert 时的检索 p99、显式 Flush
  phase = memory : 写入并 Flush 后检索预热，只测服务端 RSS，不做删除或并发写入
  mode  = inproc : MilvusClient("<db>.db")，即用户实际的本地用法（默认）
          server : 独立辅助进程内启动服务，用于只统计服务端内存，不依赖 CLI
输出一行 JSON。
"""
import importlib.metadata
import hashlib
import json
import math
import multiprocessing as mp
import multiprocessing.connection
import os
import platform
import selectors
import subprocess
import sys
import threading
import time
import warnings
from pathlib import Path

warnings.filterwarnings("ignore")
# grpc 会让 127.0.0.1 的连接也走 http_proxy 等变量指定的代理（no_proxy 未设时），3.x 与 Rust 实现经 TCP
# 连接就会多一跳，而 2.5.1 走 Unix socket 不受影响；在建任何连接之前清掉，spawn 出的客户端进程继承清过的环境
for _k in ("http_proxy", "https_proxy", "all_proxy", "grpc_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "GRPC_PROXY"):
    os.environ.pop(_k, None)
import numpy as np
from pymilvus import DataType, MilvusClient
import milvus_lite

db, N, DIM, IDX, PHASE = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4], sys.argv[5]
MODE = sys.argv[6] if len(sys.argv) > 6 else "inproc"


def detect():
    """(实现, 发行包版本)。本仓的 Rust 实现同样提供 milvus_lite 模块，按发行包区分。"""
    for dist, ver in (("milvus-lite-rust", "rs"), ("milvus-lite", None)):
        try:
            version = importlib.metadata.version(dist)
        except importlib.metadata.PackageNotFoundError:
            continue
        # 2.5.1 的包里没有嵌入式引擎入口 MilvusLite，据此区分 2.5.1 与 3.x
        return ver or ("3.x" if hasattr(milvus_lite, "MilvusLite") else "2.5"), version
    sys.exit("未安装 milvus-lite 或 milvus-lite-rust")


VER, PKG_VERSION = detect()
IS_V2 = VER == "2.5"
C = "bench"
rng = np.random.default_rng(42)
queries = rng.random((200, DIM), dtype=np.float32)
# 本基准覆盖的索引类型：2.5.1 只有 FLAT / IVF_FLAT（HNSW 被注释掉）；AUTOINDEX 会被映射成其他类型，不纳入；
# Rust 当前只实现 FLAT（0002「结论」）
SUPPORTED_INDEX = {"2.5": ("FLAT", "IVF_FLAT"), "3.x": ("FLAT", "HNSW", "IVF_FLAT"), "rs": ("FLAT",)}
# 各索引的检索参数；reopen 阶段沿用，保证两阶段的检索延迟可比
SEARCH_PARAMS = {"metric_type": "L2", "params": {"HNSW": {"ef": 64}, "IVF_FLAT": {"nprobe": 16}}.get(IDX, {})}
out = {"ver": VER, "N": N, "dim": DIM, "index": IDX, "phase": PHASE, "mode": MODE}
SERVER_PID = None
SERVER_URI = None


def rss_mb():
    # 只统计服务端进程：2.5.1 与 3.x server 模式的服务端是子进程；
    # 3.x 与 Rust 的 inproc 与客户端同进程，只能记整进程 RSS（含客户端数据，数值偏大）。
    root = SERVER_PID if MODE == "server" else os.getpid()
    pids = [] if IS_V2 else [root]
    pending = [root]
    while pending:
        parent = pending.pop()
        result = subprocess.run(["pgrep", "-P", str(parent)], capture_output=True, text=True)
        if result.returncode not in (0, 1):
            raise RuntimeError(f"pgrep 失败: {result.stderr}")
        kids = [int(k) for k in result.stdout.split()]
        pids.extend(kids)
        pending.extend(kids)
    total = 0
    for p in pids:
        r = subprocess.run(["ps", "-o", "rss=", "-p", str(p)], capture_output=True, text=True, check=True).stdout.strip()
        if not r:
            raise RuntimeError(f"服务端进程 {p} 没有 RSS 样本")
        total += int(r)
    if not pids:
        raise RuntimeError("没有找到服务端进程")
    return round(total / 1024, 1)


def timed(fn):
    s = time.perf_counter(); fn(); return time.perf_counter() - s


def lat(fn, n):
    if PHASE == "m2":
        fn()
    xs = sorted(timed(fn) * 1000 for _ in range(n))
    p99 = xs[min(n - 1, math.ceil(0.99 * n) - 1)]
    return {"p50_ms": round(xs[n // 2], 2), "p99_ms": round(p99, 2), "mean_ms": round(sum(xs) / n, 2), "n": n}


def start_server():
    proc = subprocess.Popen([sys.executable, str(Path(__file__).with_name("server_process.py")), db],
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(proc.stdout, selectors.EVENT_READ)
            if not selector.select(120):
                raise RuntimeError("服务端握手超时")
            line = proc.stdout.readline()
        if not line:
            raise RuntimeError(f"服务端握手失败，退出码 {proc.wait()}")
        return proc, json.loads(line)["uri"]
    except BaseException:
        proc.kill()
        proc.wait()
        raise


def stop_server(proc, graceful):
    # EOF 让辅助进程执行正常停止与持久化；直接 kill 会测到未完成关闭的状态。
    if proc.poll() is not None:
        if graceful:
            raise RuntimeError(f"服务端在停止握手前退出: {proc.returncode}")
        return
    if graceful:
        proc.stdin.close()
        try:
            rc = proc.wait(timeout=300)
        except subprocess.TimeoutExpired:
            rc = None
        if rc == 0:
            return
        if rc is not None:
            raise RuntimeError(f"milvus-lite server 退出码 {rc}，数据可能未正常落盘")
    proc.kill(); proc.wait()
    if graceful:
        raise RuntimeError("milvus-lite server 未能在 300 秒内正常退出")


def prepare(cli):
    """build 与 m1 共用：建表、写入、建索引、load。随机数的消耗顺序与此前的 build 相同。"""
    schema = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=DIM)
    schema.add_field("cat", DataType.INT64)
    schema.add_field("tag", DataType.VARCHAR, max_length=32)
    cli.create_collection(C, schema=schema)
    data = rng.random((N, DIM), dtype=np.float32)
    cats = rng.integers(0, 10, N)

    def insert_all():
        for i in range(0, N, 1000):
            cli.insert(C, [{"id": j, "vec": data[j].tolist(), "cat": int(cats[j]), "tag": f"t{j % 100}"} for j in range(i, min(i + 1000, N))])
    dt = timed(insert_all)
    out["insert_s"] = round(dt, 2); out["insert_rows_per_s"] = int(N / dt)

    ip = cli.prepare_index_params()
    params = {"HNSW": {"M": 16, "efConstruction": 200}, "IVF_FLAT": {"nlist": 1024}}.get(IDX, {})
    ip.add_index("vec", index_type=IDX, metric_type="L2", params=params)
    # pymilvus 以 0.5 秒为间隔轮询建索引进度，此值的分辨率约 0.5 秒
    out["create_index_s"] = round(timed(lambda: cli.create_index(C, ip)), 2)
    # 核对服务端实际登记的索引与请求一致
    out["index_described"] = {k: v for k, v in cli.describe_index(C, cli.list_indexes(C)[0]).items() if k in ("index_type", "metric_type")}
    if out["index_described"].get("index_type") != IDX:
        raise RuntimeError(f"服务端登记的索引为 {out['index_described']}，与请求的 {IDX} 不一致")
    out["load_s"] = round(timed(lambda: cli.load_collection(C)), 2)
    return data


def build(cli):
    data = prepare(cli)
    sp = SEARCH_PARAMS
    it = iter(range(10**9))
    nextq = lambda: [queries[next(it) % 200].tolist()]
    out["search_nq1_top10"] = lat(lambda: cli.search(C, nextq(), limit=10, search_params=sp), 200)
    out["search_nq100_top10"] = lat(lambda: cli.search(C, queries[:100].tolist(), limit=10, search_params=sp), 10)
    out["search_nq1_filter10pct"] = lat(lambda: cli.search(C, nextq(), limit=10, filter="cat == 3", search_params=sp), 100)
    out["search_nq1_filter1pct_str"] = lat(lambda: cli.search(C, nextq(), limit=10, filter='tag == "t7"', search_params=sp), 100)
    out["query_filter_limit100"] = lat(lambda: cli.query(C, filter="cat == 5 and id > 1000", output_fields=["id", "tag"], limit=100), 50)
    out["query_count"] = lat(lambda: cli.query(C, filter="cat >= 0", output_fields=["count(*)"]), 10)
    out["rss_mb_after_search"] = rss_mb()
    ids = rng.integers(0, N, 100).tolist()
    out["get_100_ids"] = lat(lambda: cli.get(C, ids=ids), 50)

    # 均匀随机数据对 ANN 是最坏情况，召回只用于同索引纵向对比
    out["recall@10"] = recall_at_10(cli, data, sp, np.float64 if PHASE == "m2" else np.float32)

    out["delete_1000_ids_s"] = round(timed(lambda: cli.delete(C, ids=list(range(1000)))), 4)
    up = [{"id": j, "vec": data[j].tolist(), "cat": 0, "tag": "u"} for j in range(1000, 2000)]
    out["upsert_1000_s"] = round(timed(lambda: cli.upsert(C, up)), 4)
    out["rss_mb_after_build"] = rss_mb()
    return data


def recall_at_10(cli, data, sp, dtype=np.float32):
    # m1/m2 用 float64 真值，避免第 10 与第 11 名接近并列时真值本身有误。
    q = queries[:50].astype(dtype)
    data = data.astype(dtype, copy=False)
    d = (q * q).sum(1)[:, None] - 2 * q @ data.T + (data * data).sum(1)[None, :]
    gt = np.argsort(d, axis=1)[:, :10]; del d
    res = cli.search(C, q.tolist(), limit=10, search_params=sp)
    return round(float(np.mean([len(set(gt[i].tolist()) & {h["id"] for h in res[i]}) / 10 for i in range(50)])), 4)


CONCURRENCY = (1, 2, 4, 8)
CONCURRENCY_S = float(os.environ.get("MILVUS_BENCH_SECONDS", "8"))
if not math.isfinite(CONCURRENCY_S) or CONCURRENCY_S <= 0:
    raise ValueError("MILVUS_BENCH_SECONDS 必须是有限正数")


def search_loop(uri, seed, start):
    """一个客户端循环单条检索 CONCURRENCY_S 秒，返回 (各次延迟毫秒, 从栅栏放行到最后一次返回的秒数)。
    dedicated 让每个客户端有自己的连接。连接建好后在 start（栅栏）上等其他客户端，一起开始计时：
    进程启动与连接的耗时不计入。出错时先打破栅栏，其他客户端不会永远等在栅栏上。"""
    try:
        c = MilvusClient(uri=uri, dedicated=True)
        qs = queries.tolist()
        lat_ms, n = [], seed
        start.wait()
        t0 = time.perf_counter()
        end = t0 + CONCURRENCY_S
        while time.perf_counter() < end:
            s = time.perf_counter()
            c.search(C, [qs[n % len(qs)]], limit=10, search_params=SEARCH_PARAMS)
            lat_ms.append((time.perf_counter() - s) * 1000)
            n += 1
        elapsed = time.perf_counter() - t0
        c.close()
        return lat_ms, elapsed
    except BaseException:
        start.abort()
        raise


def _process_worker(uri, seed, start, conn):
    try:
        conn.send(("ok", search_loop(uri, seed, start)))
    except BaseException as e:
        conn.send(("error", f"{type(e).__name__}: {e}"))
        raise
    finally:
        conn.close()


def concurrent(uri, k, processes):
    """k 个客户端（线程或进程）同时检索 CONCURRENCY_S 秒：吞吐与各次延迟的 p50 / p99。
    吞吐 = 完成的请求数 ÷ 从放行到最后一次返回的实际时长：截止前发出、截止后返回的请求也在分子里，
    分母不能固定用 CONCURRENCY_S，否则请求越慢吞吐越被高估。任何客户端失败都让本次测量失败，不输出指标。"""
    if processes:
        ctx = mp.get_context("spawn")
        start = ctx.Barrier(k)
        pipes = [ctx.Pipe(duplex=False) for _ in range(k)]
        ps = [ctx.Process(target=_process_worker, args=(uri, i * 7, start, w)) for i, (_, w) in enumerate(pipes)]
        for p in ps:
            p.start()
        # 父进程不写；留着写端的话，子进程没送结果就死掉时读端收不到 EOF
        for _, w in pipes:
            w.close()
        results, error = [], None
        pending = [r for r, _ in pipes]
        # 子进程被外力杀掉时打不破栅栏，其余子进程会一直等；父进程按上限等待后统一收拾
        deadline = time.monotonic() + CONCURRENCY_S + 120
        while pending and error is None:
            ready = mp.connection.wait(pending, timeout=max(0.0, deadline - time.monotonic()))
            if not ready:
                error = f"{CONCURRENCY_S + 120:.0f} 秒内没有全部返回"
                break
            for r in ready:
                pending.remove(r)
                try:
                    kind, payload = r.recv()
                except EOFError:
                    kind, payload = "error", "客户端进程没有返回结果就退出了"
                if kind == "ok":
                    results.append(payload)
                else:
                    error = payload
        if error is not None:
            for p in ps:
                if p.is_alive():
                    p.terminate()
        for p in ps:
            p.join()
        if error is None and any(p.exitcode != 0 for p in ps):
            error = "客户端进程退出码非零"
        if error is not None:
            raise RuntimeError(f"{k} 个客户端进程并发检索失败：{error}")
    else:
        start = threading.Barrier(k)
        results, errors = [], []

        def run(i):
            try:
                results.append(search_loop(uri, i * 7, start))
            except BaseException as e:
                errors.append(e)

        ts = [threading.Thread(target=run, args=(i,)) for i in range(k)]
        for t in ts:
            t.start()
        for t in ts:
            t.join()
        if errors:
            raise RuntimeError(f"{k} 个客户端线程并发检索失败") from errors[0]
    elapsed = max(e for _, e in results)
    xs = sorted(x for l, _ in results for x in l)
    return {"qps": round(len(xs) / elapsed, 1), "p50_ms": round(xs[len(xs) // 2], 2),
            "p99_ms": round(xs[min(len(xs) - 1, math.ceil(0.99 * len(xs)) - 1)], 2),
            "completed_requests": len(xs), "elapsed_s": elapsed}


def m1(cli):
    data = prepare(cli)
    sp = SEARCH_PARAMS
    it = iter(range(10**9))
    nextq = lambda: [queries[next(it) % 200].tolist()]
    # 每项先调用一次预热，首次调用的建连、缓存等开销不计入延迟
    measures = [
        ("search_nq1_top10", lambda: cli.search(C, nextq(), limit=10, search_params=sp), 200),
        ("search_nq100_top10", lambda: cli.search(C, queries[:100].tolist(), limit=10, search_params=sp), 10),
        # 空 expr 的 count(*)：3.x 走 num_entities，与 build 阶段带过滤的 query_count（0001 的口径）不是同一操作
        ("query_count_empty_expr", lambda: cli.query(C, filter="", output_fields=["count(*)"]), 10),
    ]
    for key, fn, n in measures:
        fn()
        out[key] = lat(fn, n)
    out["rss_mb_after_search"] = rss_mb()
    ids = rng.integers(0, N, 100).tolist()
    cli.get(C, ids=ids)
    out["get_100_ids"] = lat(lambda: cli.get(C, ids=ids), 50)
    # 三个实现都是暴力检索，召回必须是 1；否则说明配置或实现有误，结果不可用
    recall = recall_at_10(cli, data, sp, np.float64)
    if recall != 1.0:
        raise RuntimeError(f"FLAT 的 recall@10 为 {recall}，应为 1.0")
    out["recall@10"] = recall
    # 进程内服务的 URI：pymilvus 打开 .db 时同样经 server_manager 取得，同一路径返回已启动的服务
    uri = service_uri()
    if uri is None:
        raise RuntimeError("取不到进程内服务的 URI")
    # 同一进程内多线程：0004「验收」第 4 项的口径；多进程：每个客户端一个进程，与 0001 并发表的客户端形态相同，作为补充
    for kind, processes in (("threads", False), ("processes", True)):
        out[f"concurrency_{kind}"] = {f"k{k}": concurrent(uri, k, processes) for k in CONCURRENCY}


def service_uri():
    if MODE == "server":
        return SERVER_URI
    from milvus_lite.server_manager import server_manager_instance

    return server_manager_instance.start_and_get_uri(db)


def _mixed_worker(uri, start, conn, rows):
    client = None
    try:
        client = MilvusClient(uri=uri, dedicated=True)
        if rows is None:
            client.search(C, [queries[0].tolist()], limit=10, search_params=SEARCH_PARAMS)
        start.wait(timeout=120)
        begin = time.perf_counter()
        end = begin + CONCURRENCY_S
        latencies, writes, i = [], 0, 0
        while time.perf_counter() < end:
            t = time.perf_counter()
            if rows is None:
                client.search(C, [queries[i % len(queries)].tolist()], limit=10, search_params=SEARCH_PARAMS)
                latencies.append((time.perf_counter() - t) * 1000)
            else:
                result = client.upsert(C, rows)
                if result["upsert_count"] != len(rows):
                    raise RuntimeError(f"部分 upsert: {result}")
                writes += len(rows)
                # 固定目标写入速率，慢实现不补发积压；实际吞吐另记，不能把目标当结果。
                time.sleep(max(0.0, min(0.1 - (time.perf_counter() - t), end - time.perf_counter())))
            i += 1
        conn.send(("ok", {"latencies": latencies, "writes": writes,
                          "begin": begin, "end": time.perf_counter()}))
    except BaseException as error:
        start.abort()
        conn.send(("error", f"{type(error).__name__}: {error}"))
        raise
    finally:
        if client is not None:
            client.close()
        conn.close()


def mixed(uri, rows):
    """不同客户端进程并发：一个检索者与一个每批 100 行、目标 1000 行/秒的写者。"""
    ctx = mp.get_context("spawn")
    start = ctx.Barrier(2)
    pipes = [ctx.Pipe(duplex=False) for _ in range(2)]
    workers = [ctx.Process(target=_mixed_worker, args=(uri, start, w, batch))
               for (_, w), batch in zip(pipes, (None, rows))]
    pending, results = [r for r, _ in pipes], []
    try:
        for worker in workers:
            worker.start()
        for _, w in pipes:
            w.close()
        deadline = time.monotonic() + CONCURRENCY_S + 120
        while pending:
            ready = mp.connection.wait(pending, timeout=max(0.0, deadline - time.monotonic()))
            if not ready:
                raise RuntimeError("持续写入与检索客户端超时")
            for reader in ready:
                pending.remove(reader)
                kind, result = reader.recv()
                if kind != "ok":
                    raise RuntimeError(f"持续写入与检索失败: {result}")
                results.append(result)
        for worker in workers:
            worker.join(timeout=10)
            if worker.exitcode != 0:
                raise RuntimeError(f"持续写入与检索客户端退出码 {worker.exitcode}")
        search = next(r for r in results if r["latencies"])
        writer = next(r for r in results if r["writes"])
        overlap = min(search["end"], writer["end"]) - max(search["begin"], writer["begin"])
        if overlap <= 0:
            raise RuntimeError("写入与检索没有时间重叠")
        xs = sorted(search["latencies"])
        elapsed = max(r["end"] for r in results) - min(r["begin"] for r in results)
        return {"p50_ms": round(xs[len(xs) // 2], 2),
                "p99_ms": round(xs[math.ceil(0.99 * len(xs)) - 1], 2),
                "n": len(xs), "search_qps": round(len(xs) / elapsed, 1),
                "written_rows": writer["writes"], "write_rows_per_s": round(writer["writes"] / elapsed, 1),
                "target_write_rows_per_s": 1000, "overlap_s": round(overlap, 3), "elapsed_s": round(elapsed, 3)}
    finally:
        start.abort()
        for worker in workers:
            if worker.pid is not None and worker.is_alive():
                worker.kill()
            if worker.pid is not None:
                worker.join(timeout=10)
        for r, w in pipes:
            r.close()
            w.close()


def m2(cli):
    data = build(cli)
    if out["recall@10"] != 1.0:
        raise RuntimeError("M2 的 FLAT recall@10 必须为 1")
    expected = N - 1000
    if cli.query(C, filter="", output_fields=["count(*)"])[0]["count(*)"] != expected:
        raise RuntimeError("删除/upsert 后行数不符")
    out["query_count_empty_expr"] = lat(lambda: cli.query(C, filter="", output_fields=["count(*)"]), 10)
    uri = service_uri()
    if uri is None:
        raise RuntimeError("无法取得服务 URI")
    for kind, processes in (("threads", False), ("processes", True)):
        out[f"concurrency_{kind}"] = {f"k{k}": concurrent(uri, k, processes) for k in CONCURRENCY}
    rows = [{"id": j, "vec": data[j].tolist(), "cat": 0, "tag": "u"} for j in range(1000, 1100)]
    out["search_during_writes"] = mixed(uri, rows)
    out["flush_s"] = round(timed(lambda: cli.flush(C)), 4)
    actual = cli.query(C, filter="", output_fields=["count(*)"])[0]["count(*)"]
    if actual != expected:
        raise RuntimeError(f"持续写入改变行数: {actual} != {expected}")
    out["confirmed_rows"] = actual
    out["rss_mb_after_flush"] = rss_mb()


def memory(cli):
    prepare(cli)
    cli.flush(C)
    cli.search(C, [queries[0].tolist()], limit=10, search_params=SEARCH_PARAMS)
    out["rss_mb_after_flush"] = rss_mb()
    out["confirmed_rows"] = cli.query(C, filter="", output_fields=["count(*)"])[0]["count(*)"]
    if out["confirmed_rows"] != N:
        raise RuntimeError("内存基准行数不符")


def reopen(cli):
    # 新起进程、页缓存仍热，测的是进程级冷启动而非磁盘冷启动；3.x 会在此阶段为末尾未建索引的 segment 补建索引
    dt = timed(lambda: cli.load_collection(C))
    out["reopen_load_s"] = round(dt, 2)
    out["reopen_total_to_ready_s"] = round(out["connect_s"] + dt, 2)
    out["first_search_ms"] = round(timed(lambda: cli.search(C, [queries[0].tolist()], limit=10, search_params=SEARCH_PARAMS)) * 1000, 1)
    out["search_nq1_top10"] = lat(lambda: cli.search(C, [queries[1].tolist()], limit=10, search_params=SEARCH_PARAMS), 100)
    out["rss_mb_after_reopen"] = rss_mb()
    out["confirmed_rows"] = cli.query(C, filter="", output_fields=["count(*)"])[0]["count(*)"]
    if out["confirmed_rows"] != N - 1000:
        raise RuntimeError("关闭重开后的行数不符")


def main():
    global SERVER_PID, SERVER_URI
    # 空或拼错的 mode 会让 rss_mb 漏记 3.x inproc 的自身进程，内存列静默变成 0
    if MODE not in ("inproc", "server"):
        sys.exit(f"未知 mode: {MODE!r}（应为 inproc 或 server）")
    if PHASE not in ("build", "reopen", "m1", "m2", "memory"):
        sys.exit(f"未知 phase: {PHASE!r}（应为 build、reopen、m1、m2 或 memory）")
    minimum = 2000 if PHASE in ("build", "m2", "reopen") else 10 if PHASE == "m1" else 1
    if N < minimum or DIM < 1:
        sys.exit(f"{PHASE} 要求 N >= {minimum}、dim >= 1")
    if PHASE == "m2" and IDX != "FLAT":
        sys.exit("M2 三方对照只接受 FLAT")
    # 3.x 的 create_index 会原样登记未知类型，要到某个 segment 需要建索引时才报错；
    # 数据若还在 memtable，build 阶段会按暴力检索跑完并被误标为该索引，所以事先限定
    if IDX not in SUPPORTED_INDEX[VER]:
        sys.exit(f"{out['ver']} 不支持的索引类型: {IDX!r}（可选 {', '.join(SUPPORTED_INDEX[VER])}）")
    srv = None
    ok = False
    try:
        s0 = time.perf_counter()
        if MODE == "server":
            srv, uri = start_server()
            SERVER_PID = srv.pid
            SERVER_URI = uri
            cli = MilvusClient(uri=uri)
        else:
            cli = MilvusClient(db)
        out["connect_s"] = round(time.perf_counter() - s0, 3)
        {"build": build, "reopen": reopen, "m1": m1, "m2": m2, "memory": memory}[PHASE](cli)
        cli.close()
        ok = True
    finally:
        # 出错时不必保证落盘，直接结束服务端，避免留下孤儿进程
        if srv:
            stop_server(srv, graceful=ok)
    out["versions"] = {"impl": PKG_VERSION, "pymilvus": importlib.metadata.version("pymilvus"),
                       "python": sys.version.split()[0]}
    # setup_rs.sh 记下的构建 commit：Rust 实现的包版本号区分不了构建
    commit_file = os.path.join(sys.prefix, "BUILD_COMMIT")
    if os.path.exists(commit_file):
        with open(commit_file) as f:
            out["versions"]["build_commit"] = f.read().strip()
    out["load_avg"] = [round(x, 2) for x in os.getloadavg()]
    out["concurrency_duration_s"] = CONCURRENCY_S
    digest = hashlib.sha256()
    for source in sorted(p for p in Path(__file__).parent.iterdir() if p.suffix in (".py", ".sh")):
        digest.update(source.name.encode())
        digest.update(source.read_bytes())
    out["benchmark_sha256"] = digest.hexdigest()
    out["rss_scope"] = "server" if MODE == "server" or IS_V2 else "client_and_server"
    out["machine"] = {"platform": platform.platform(), "cpu_count": os.cpu_count()}
    out["round"] = os.environ.get("MILVUS_BENCH_ROUND")
    print(json.dumps(out, ensure_ascii=False))


if __name__ == "__main__":
    main()
