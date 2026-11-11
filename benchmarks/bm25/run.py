"""BM25 全文检索专项（0031）：写入（含生成稀疏向量）、flush、首次检索（含建倒排）、热查询与 RSS。

同一脚本由 2.5.1、3.x 与本实现各自的解释器运行；文本由种子生成，三方相同。全部检索结果（id 与 float32
距离的位模式）汇成 `results_sha256`：本实现与 3.x（NumPy 2）必须相同；2.5.1 的 BM25 用全局统计、距离为正，
只参加计时。只写入不存在的新目录。
"""

import argparse
import hashlib
import json
import os
import platform
import statistics
import struct
import subprocess
import sys
import time

import numpy as np
from pymilvus import DataType, Function, FunctionType, MilvusClient

VOCAB = 20_000
TOPK = 10


def words(rng, count):
    p = 1.0 / np.arange(1, VOCAB + 1) ** 1.1
    p /= p.sum()
    return [f"w{i}" for i in rng.choice(VOCAB, size=count, p=p)]


def dataset(rows, seed):
    rng = np.random.default_rng(seed)
    return [" ".join(words(rng, int(rng.integers(10, 41)))) for _ in range(rows)]


def queries(count, seed):
    rng = np.random.default_rng(seed)
    return [" ".join(words(rng, int(rng.integers(2, 6)))) for _ in range(count)]


def tree_rss_kib():
    pids, frontier = [os.getpid()], [os.getpid()]
    while frontier:
        out = subprocess.run(["pgrep", "-P", str(frontier.pop())], capture_output=True, text=True).stdout.split()
        pids += map(int, out)
        frontier += map(int, out)
    out = subprocess.run(["ps", "-o", "rss=", "-p", ",".join(map(str, pids))], capture_output=True, text=True).stdout
    return sum(int(x) for x in out.split())


def timed(fn):
    started = time.perf_counter_ns()
    value = fn()
    return value, (time.perf_counter_ns() - started) / 1e6


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--work-dir", required=True)
    parser.add_argument("--impl", required=True, help="结果中的实现标签")
    parser.add_argument("--rows", type=int, default=100_000)
    parser.add_argument("--queries", type=int, default=50)
    parser.add_argument("--seed", type=int, default=31)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    os.makedirs(args.work_dir)  # 必须是新目录

    docs = dataset(args.rows, args.seed)
    qs = queries(args.queries + 32, args.seed + 1)
    digest = hashlib.sha256()

    client = MilvusClient(os.path.join(args.work_dir, "bm25.db"))
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("n", DataType.INT64)
    schema.add_field("text", DataType.VARCHAR, max_length=1000, enable_analyzer=True)
    schema.add_field("sp", DataType.SPARSE_FLOAT_VECTOR)
    schema.add_function(Function(name="bm", function_type=FunctionType.BM25, input_field_names=["text"],
                                 output_field_names=["sp"]))
    index = client.prepare_index_params()
    index.add_index("sp", index_type="SPARSE_INVERTED_INDEX", metric_type="BM25")
    client.create_collection("s", schema=schema, index_params=index)
    result = {"impl": args.impl, "rows": args.rows, "vocab": VOCAB, "queries": args.queries,
              "python": platform.python_version(), "machine": platform.machine(), "cpu_count": os.cpu_count(),
              "numpy": np.__version__, "script_sha256": hashlib.sha256(open(__file__, "rb").read()).hexdigest()}
    for module in ["pymilvus", "milvus_lite"]:
        try:
            result[module] = __import__(module).__version__
        except AttributeError:
            result[module] = "unknown"

    def insert_all():
        for start in range(0, args.rows, 10_000):
            batch = [{"id": i, "n": i, "text": docs[i]} for i in range(start, min(start + 10_000, args.rows))]
            assert client.insert("s", batch)["insert_count"] == len(batch)
    _, result["insert_ms"] = timed(insert_all)
    _, result["flush_ms"] = timed(lambda: client.flush("s"))
    client.load_collection("s")

    def search(batch, **kw):
        hits = client.search("s", batch, anns_field="sp", limit=TOPK, output_fields=["n"], **kw)
        assert len(hits) == len(batch)
        for q in hits:
            assert len(q) == TOPK, len(q)
            for h in q:
                assert h["entity"]["n"] == h["id"]
                digest.update(struct.pack("<qf", h["id"], h["distance"]))
        return hits

    _, result["first_search_ms"] = timed(lambda: search([qs[0]]))
    for q in qs[1:3]:
        search([q])  # 预热
    single = [timed(lambda q=q: search([q]))[1] for q in qs[:args.queries]]
    batch = qs[args.queries:args.queries + 32]
    batched = [timed(lambda: search(batch))[1] for _ in range(9)]
    filtered = [timed(lambda q=q: search([q], filter=f"n < {args.rows // 2}"))[1] for q in qs[:20]]
    result.update({
        "single_median_ms": statistics.median(single),
        "single_p95_ms": sorted(single)[int(len(single) * 0.95) - 1],
        "batch32_median_ms": statistics.median(batched),
        "filtered_median_ms": statistics.median(filtered),
        "rss_kib": tree_rss_kib(),
        "results_sha256": digest.hexdigest(),
    })
    client.close()
    with open(args.out, "a") as f:
        f.write(json.dumps(result, sort_keys=True) + "\n")
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    sys.exit(main())
