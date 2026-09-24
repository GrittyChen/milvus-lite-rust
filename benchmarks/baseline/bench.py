"""milvus-lite 同负载基准：经 pymilvus MilvusClient 驱动，对 2.5.1 与 3.x 输出同一组指标。

用法: python bench.py <db_path> <N> <dim> <index_type> <phase> [mode]
  phase = build  : 建表 + 写入 + 建索引 + load + 检索 / 过滤 / 查询 / 点查 / 删除 / upsert
  phase = reopen : 在 build 产生的 db 上新起进程 load + 检索
  mode  = inproc : MilvusClient("<db>.db")，即用户实际的本地用法（默认）
          server : 仅 3.x，独立进程 `milvus-lite server`，用于只统计服务端内存并排除客户端同进程 GIL 干扰
输出一行 JSON。
"""
import json
import math
import os
import signal
import socket
import subprocess
import sys
import time
import warnings

warnings.filterwarnings("ignore")
import numpy as np
from pymilvus import DataType, MilvusClient
import milvus_lite

db, N, DIM, IDX, PHASE = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4], sys.argv[5]
MODE = sys.argv[6] if len(sys.argv) > 6 else "inproc"
# 2.5.1 的包里没有嵌入式引擎入口 MilvusLite，据此区分版本
IS_V2 = not hasattr(milvus_lite, "MilvusLite")
C = "bench"
rng = np.random.default_rng(42)
queries = rng.random((200, DIM), dtype=np.float32)
# 本基准覆盖的索引类型：2.5.1 只有 FLAT / IVF_FLAT（HNSW 被注释掉）；AUTOINDEX 会被映射成其他类型，不纳入
SUPPORTED_INDEX = {True: ("FLAT", "IVF_FLAT"), False: ("FLAT", "HNSW", "IVF_FLAT")}
# 各索引的检索参数；reopen 阶段沿用，保证两阶段的检索延迟可比
SEARCH_PARAMS = {"metric_type": "L2", "params": {"HNSW": {"ef": 64}, "IVF_FLAT": {"nprobe": 16}}.get(IDX, {})}
out = {"ver": "2.5" if IS_V2 else "3.x", "N": N, "dim": DIM, "index": IDX, "phase": PHASE, "mode": MODE}


def rss_mb():
    # 只统计服务端进程：2.5.1 与 3.x server 模式的服务端是子进程；
    # 3.x inproc 与客户端同进程，只能记整进程 RSS（含客户端数据，数值偏大）。
    pids = [os.getpid()] if MODE == "inproc" and not IS_V2 else []
    kids = subprocess.run(["pgrep", "-P", str(os.getpid())], capture_output=True, text=True).stdout.split()
    pids += [int(k) for k in kids]
    total = 0
    for p in pids:
        r = subprocess.run(["ps", "-o", "rss=", "-p", str(p)], capture_output=True, text=True).stdout.strip()
        total += int(r or 0)
    return round(total / 1024, 1)


def timed(fn):
    s = time.perf_counter(); fn(); return time.perf_counter() - s


def lat(fn, n):
    xs = sorted(timed(fn) * 1000 for _ in range(n))
    p99 = xs[min(n - 1, math.ceil(0.99 * n) - 1)]
    return {"p50_ms": round(xs[n // 2], 2), "p99_ms": round(p99, 2), "mean_ms": round(sum(xs) / n, 2), "n": n}


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def start_server():
    port = free_port()
    proc = subprocess.Popen([os.path.join(os.path.dirname(sys.executable), "milvus-lite"), "server", "--data-dir", db, "--port", str(port)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    deadline = time.time() + 120
    while True:
        if proc.poll() is not None:
            raise RuntimeError("milvus-lite server 启动失败")
        try:
            socket.create_connection(("127.0.0.1", port), 0.2).close()
            return proc, port
        except OSError:
            if time.time() > deadline:
                raise RuntimeError("milvus-lite server 启动超时")
            time.sleep(0.05)


def stop_server(proc, graceful):
    # 3.x server 只在 KeyboardInterrupt 时执行 db.close()（落盘 memtable、写索引）；
    # SIGTERM 会直接结束进程，reopen 阶段就会测到 WAL 回放后的未落盘状态。
    if proc.poll() is not None:
        return
    if graceful:
        proc.send_signal(signal.SIGINT)
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


def build(cli):
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

    # 召回以 numpy 精确 L2 为真值；均匀随机数据对 ANN 是最坏情况，只用于同索引纵向对比
    q = queries[:50]
    d = (q * q).sum(1)[:, None] - 2 * q @ data.T + (data * data).sum(1)[None, :]
    gt = np.argsort(d, axis=1)[:, :10]; del d
    res = cli.search(C, q.tolist(), limit=10, search_params=sp)
    out["recall@10"] = round(float(np.mean([len(set(gt[i].tolist()) & {h["id"] for h in res[i]}) / 10 for i in range(50)])), 4)

    out["delete_1000_ids_s"] = round(timed(lambda: cli.delete(C, ids=list(range(1000)))), 4)
    up = [{"id": j, "vec": data[j].tolist(), "cat": 0, "tag": "u"} for j in range(1000, 2000)]
    out["upsert_1000_s"] = round(timed(lambda: cli.upsert(C, up)), 4)
    out["rss_mb_after_build"] = rss_mb()


def reopen(cli):
    # 新起进程、页缓存仍热，测的是进程级冷启动而非磁盘冷启动；3.x 会在此阶段为末尾未建索引的 segment 补建索引
    dt = timed(lambda: cli.load_collection(C))
    out["reopen_load_s"] = round(dt, 2)
    out["reopen_total_to_ready_s"] = round(out["connect_s"] + dt, 2)
    out["first_search_ms"] = round(timed(lambda: cli.search(C, [queries[0].tolist()], limit=10, search_params=SEARCH_PARAMS)) * 1000, 1)
    out["search_nq1_top10"] = lat(lambda: cli.search(C, [queries[1].tolist()], limit=10, search_params=SEARCH_PARAMS), 100)
    out["rss_mb_after_reopen"] = rss_mb()


def main():
    # 空或拼错的 mode 会让 rss_mb 漏记 3.x inproc 的自身进程，内存列静默变成 0
    if MODE not in ("inproc", "server"):
        sys.exit(f"未知 mode: {MODE!r}（应为 inproc 或 server）")
    if MODE == "server" and IS_V2:
        sys.exit("server 模式只适用于 3.x")
    # 3.x 的 create_index 会原样登记未知类型，要到某个 segment 需要建索引时才报错；
    # 数据若还在 memtable，build 阶段会按暴力检索跑完并被误标为该索引，所以事先限定
    if IDX not in SUPPORTED_INDEX[IS_V2]:
        sys.exit(f"{out['ver']} 不支持的索引类型: {IDX!r}（可选 {', '.join(SUPPORTED_INDEX[IS_V2])}）")
    srv = None
    ok = False
    try:
        s0 = time.perf_counter()
        if MODE == "server":
            srv, port = start_server()
            cli = MilvusClient(uri=f"http://127.0.0.1:{port}")
        else:
            cli = MilvusClient(db)
        out["connect_s"] = round(time.perf_counter() - s0, 3)
        (build if PHASE == "build" else reopen)(cli)
        cli.close()
        ok = True
    finally:
        # 出错时不必保证落盘，直接结束服务端，避免留下孤儿进程
        if srv:
            stop_server(srv, graceful=ok)
    print(json.dumps(out, ensure_ascii=False))


if __name__ == "__main__":
    main()
