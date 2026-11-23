"""HNSW 专项基准（0041 第 5 步）：同机对照 3.x HNSW、本实现 FLAT 与本实现 HNSW，2.5.1 FLAT 作 FLAT 对 FLAT 的参照。

用法：bench.py <环境标签> <索引> <数据集> <行数> <工作目录> <输出 jsonl> [阶段]
  环境标签：v2 / v3 / rs（由解释器里装的包决定实现，标签只写进结果）
  索引：FLAT / HNSW；数据集：uniform（基线口径的均匀随机）/ hard（0041 的门槛夹具，需 MLR_HARD_DIR）
  阶段：main（默认：写入、就绪、检索、过滤、大批量、并发、重开）/ sweep（只测过滤曲线，用于标定距离预算）

服务端在独立进程中运行（baseline/server_process.py），RSS 只含服务端。召回以 float64 精确结果为真值。
检索一律只取 id：3.x 与本实现默认返回向量字段、2.5.1 不返回，大批量时会把测量变成传输。
"""

import json
import math
import multiprocessing as mp
import os
import re
import subprocess
import sys
import threading
import time
from pathlib import Path

import numpy as np
from pymilvus import DataType, MilvusClient

TAG, IDX, DATASET, N, WORK, OUT = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), Path(sys.argv[5]), sys.argv[6]
PHASE = sys.argv[7] if len(sys.argv) > 7 else "main"
DIM, K, BATCH = 128, 10, 10_000
SECONDS = float(os.environ.get("MILVUS_BENCH_SECONDS", "8"))
SERVER_SCRIPT = Path(__file__).resolve().parent.parent / "baseline" / "server_process.py"
PASS_RATES = {"main": [0.1, 0.01, 0.001], "sweep": [1.0, 0.5, 0.2, 0.1, 0.07, 0.05, 0.03, 0.02, 0.01]}[PHASE]
SEARCH_PARAMS = {"metric_type": "L2", "params": {"ef": 64} if IDX == "HNSW" else {}}

for var in ("http_proxy", "https_proxy", "all_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
    os.environ.pop(var, None)


def load_data():
    if DATASET == "uniform":
        rng = np.random.default_rng(42)
        data = rng.random((N, DIM), dtype=np.float32)
        queries = rng.random((100, DIM), dtype=np.float32)
    else:
        hard = Path(os.environ["MLR_HARD_DIR"])
        data = np.fromfile(hard / "data.f32", dtype=np.float32).reshape(-1, DIM)[:N]
        queries = np.fromfile(hard / "queries.f32", dtype=np.float32).reshape(-1, DIM)
    return np.ascontiguousarray(data), np.ascontiguousarray(queries)


def exact(data, queries, mask=None):
    """float64 的 L2 真值：每条查询的前 K 个行号（并列按行号）。"""
    ids = np.arange(len(data)) if mask is None else np.flatnonzero(mask)
    sub = data[ids].astype(np.float64)
    norms = (sub * sub).sum(1)
    out = []
    for q in queries.astype(np.float64):
        d = norms - 2 * sub @ q + q @ q
        top = np.lexsort((ids, d))[:K]
        out.append(set(ids[top].tolist()))
    return out


def recall(hits, truth):
    return round(sum(len({h["id"] for h in row} & t) for row, t in zip(hits, truth)) / (K * len(truth)), 4)


def pctl(xs, p):
    xs = sorted(xs)
    return round(xs[min(len(xs) - 1, math.ceil(p * len(xs)) - 1)], 3)


def lat(fn, n):
    fn()
    xs = []
    for i in range(n):
        s = time.perf_counter()
        fn(i)
        xs.append((time.perf_counter() - s) * 1000)
    return {"p50_ms": pctl(xs, 0.5), "p99_ms": pctl(xs, 0.99), "n": n}


class Server:
    def __init__(self, db):
        self.proc = subprocess.Popen([sys.executable, str(SERVER_SCRIPT), str(db)],
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        self.uri = json.loads(self.proc.stdout.readline())["uri"]

    def pids(self):
        pids, pending = [self.proc.pid], [self.proc.pid]
        while pending:
            r = subprocess.run(["pgrep", "-P", str(pending.pop())], capture_output=True, text=True)
            kids = [int(k) for k in r.stdout.split()]
            pids += kids
            pending += kids
        # 2.5.1 的服务端是 C++ 子进程，Python 辅助进程不计入（与 baseline 相同）。
        return pids[1:] if TAG == "v2" else pids

    def rss_mb(self):
        pids = ",".join(map(str, self.pids()))
        r = subprocess.run(["ps", "-o", "rss=", "-p", pids], capture_output=True, text=True, check=True)
        return round(sum(int(x) for x in r.stdout.split()) / 1024, 1)

    def stop(self):
        self.proc.stdin.close()
        if self.proc.wait(timeout=120) != 0:
            raise RuntimeError(f"服务端退出码 {self.proc.returncode}")


def peak_rss(server, fn):
    """fn 执行期间每 10 ms 采样一次服务端 RSS，返回（结果，耗时 ms，峰值 MiB）。"""
    peak, done = [server.rss_mb()], threading.Event()

    def sample():
        while not done.wait(0.01):
            peak.append(server.rss_mb())

    t = threading.Thread(target=sample)
    t.start()
    s = time.perf_counter()
    try:
        result = fn()
    finally:
        elapsed = (time.perf_counter() - s) * 1000
        done.set()
        t.join()
    return result, round(elapsed, 1), max(peak)


def graphs_ready(db):
    """本实现：manifest 中每个不少于 1 万行的段都有图文件。只写不删，段的行数等于文件名中的 seq 范围长度。"""
    root = db / "collections" / "c"
    manifest = json.loads((root / "manifest.json").read_text())
    for name, part in manifest["partitions"].items():
        for rel in part["data_files"]:
            lo, hi = map(int, re.search(r"data_(\d+)_(\d+)\.parquet$", rel).groups())
            stem = Path(rel).stem
            if hi - lo + 1 >= 10_000 and not (root / "partitions" / name / "indexes" / f"{stem}.v.hnsw.mlrs").exists():
                return False
    return True


def segments(db):
    manifest = json.loads((db / "collections" / "c" / "manifest.json").read_text())
    rows = []
    for part in manifest["partitions"].values():
        for rel in part["data_files"]:
            lo, hi = map(int, re.search(r"data_(\d+)_(\d+)\.parquet$", rel).groups())
            rows.append(hi - lo + 1)
    return sorted(rows)


def worker(uri, queries, barrier, deadline_box, result_q):
    client = MilvusClient(uri=uri)
    barrier.wait()
    start = time.perf_counter()
    deadline = start + deadline_box
    xs, i = [], 0
    while time.perf_counter() < deadline:
        s = time.perf_counter()
        client.search("c", data=[queries[i % len(queries)].tolist()], limit=K, output_fields=["id"], search_params=SEARCH_PARAMS)
        xs.append((time.perf_counter() - s) * 1000)
        i += 1
    result_q.put((len(xs), time.perf_counter() - start, xs))
    client.close()


def concurrency(uri, queries, clients):
    ctx = mp.get_context("spawn")
    barrier, q = ctx.Barrier(clients), ctx.Queue()
    procs = [ctx.Process(target=worker, args=(uri, queries, barrier, SECONDS, q)) for _ in range(clients)]
    for p in procs:
        p.start()
    results = [q.get(timeout=SECONDS + 300) for _ in procs]
    for p in procs:
        p.join()
        if p.exitcode != 0:
            raise RuntimeError("并发客户端失败")
    count = sum(r[0] for r in results)
    wall = max(r[1] for r in results)
    xs = [x for r in results for x in r[2]]
    return {"qps": round(count / wall, 1), "p50_ms": pctl(xs, 0.5), "p99_ms": pctl(xs, 0.99)}


def main():
    data, queries = load_data()
    tags = np.arange(N) % 1000
    WORK.mkdir(parents=True, exist_ok=True)
    db = WORK / f"h_{TAG}_{IDX}_{DATASET}_{N}_{PHASE}.db"
    subprocess.run(["rm", "-rf", str(db)], check=True)
    out = {"tag": TAG, "index": IDX, "dataset": DATASET, "N": N, "phase": PHASE,
           "build": (Path(sys.executable).parent.parent / "BUILD_COMMIT").read_text().strip()
           if (Path(sys.executable).parent.parent / "BUILD_COMMIT").exists() else None,
           "budget_env": os.environ.get("MLR_GRAPH_BUDGET"), "load": os.getloadavg()[0]}

    server = Server(db)
    client = MilvusClient(uri=server.uri)
    schema = client.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("v", DataType.FLOAT_VECTOR, dim=DIM)
    schema.add_field("tag", DataType.INT64)
    params = client.prepare_index_params()
    params.add_index("v", index_type=IDX, metric_type="L2", params={"M": 16, "efConstruction": 200} if IDX == "HNSW" else {})
    client.create_collection("c", schema=schema, index_params=params)

    s = time.perf_counter()
    for start in range(0, N, BATCH):
        end = min(N, start + BATCH)
        client.insert("c", [{"id": i, "v": data[i].tolist(), "tag": int(tags[i])} for i in range(start, end)])
    client.flush("c")
    out["write_flush_s"] = round(time.perf_counter() - s, 2)

    # 就绪：3.x 在 load 时同步为全部段建索引；本实现 load 不等，等后台把每段的图建完。
    s = time.perf_counter()
    if TAG == "v3" and IDX == "HNSW":
        client.release_collection("c")
        client.load_collection("c")
    elif TAG == "rs" and IDX == "HNSW":
        while not graphs_ready(db):
            if time.perf_counter() - s > 3600:
                raise RuntimeError("图在一小时内没有建完")
            time.sleep(0.5)
    out["ready_s"] = round(time.perf_counter() - s, 2)
    if TAG == "rs":
        out["segments"] = segments(db)
    out["rss_ready_mb"] = server.rss_mb()

    qlist = queries.tolist()

    def searcher(client):
        return lambda data_, limit=K, expr="": client.search(
            "c", data=data_, limit=limit, filter=expr, output_fields=["id"], search_params=SEARCH_PARAMS)

    def filters(search):
        result = {}
        for rate in PASS_RATES:
            cut = int(round(rate * 1000))
            expr = f"tag < {cut}" if cut < 1000 else ""
            truth = exact(data, queries, tags < cut if cut < 1000 else None)
            result[str(rate)] = {**lat(lambda i=0: search([qlist[i % len(qlist)]], expr=expr), 100),
                                 "recall": recall(search(qlist, expr=expr), truth)}
        return result

    search = searcher(client)
    if PHASE == "main":
        out["nq1"] = lat(lambda i=0: search([qlist[i % len(qlist)]]), 200)
        out["nq100"] = lat(lambda i=0: search(qlist), 20)
        out["recall"] = recall(search(qlist), exact(data, queries))
        out["filter"] = filters(search)
        big = (qlist * 10)[:1000]
        out["big"] = {}
        for limit in (100, 1000):
            _, ms, peak = peak_rss(server, lambda: search(big, limit=limit))
            out["big"][str(limit)] = {"ms": ms, "peak_rss_mb": peak}
        out["concurrency"] = {str(c): concurrency(server.uri, queries, c) for c in (1, 4, 8)}
    client.close()
    server.stop()

    if PHASE == "sweep":
        # 每个预算倍数重启一次服务（插桩 wheel 在进程内只读一次 MLR_GRAPH_BUDGET），load 时读入已有的图。
        # FLAT 也在重启后测一次：重开后段按行组分成多块，与 HNSW 臂的条件相同。
        out["sweep"] = {}
        budgets = os.environ.get("MLR_SWEEP_BUDGETS", "1") if IDX == "HNSW" else "-"
        for budget in budgets.split(","):
            os.environ["MLR_GRAPH_BUDGET"] = budget
            server = Server(db)
            client = MilvusClient(uri=server.uri)
            client.load_collection("c")
            out["sweep"][budget] = filters(searcher(client))
            client.close()
            server.stop()

    if PHASE == "main":
        server = Server(db)
        client = MilvusClient(uri=server.uri)
        s = time.perf_counter()
        client.load_collection("c")
        out["reopen_load_s"] = round(time.perf_counter() - s, 3)
        s = time.perf_counter()
        client.search("c", data=[qlist[0]], limit=K, output_fields=["id"], search_params=SEARCH_PARAMS)
        out["reopen_first_ms"] = round((time.perf_counter() - s) * 1000, 2)
        out["reopen_nq1"] = lat(lambda i=0: client.search("c", data=[qlist[i % len(qlist)]], limit=K, output_fields=["id"], search_params=SEARCH_PARAMS), 100)
        out["reopen_rss_mb"] = server.rss_mb()
        client.close()
        server.stop()

    with open(OUT, "a") as f:
        f.write(json.dumps(out) + "\n")
    print(json.dumps(out))
    subprocess.run(["rm", "-rf", str(db)], check=True)


if __name__ == "__main__":
    main()
