"""稀疏向量 SPARSE_INVERTED_INDEX + IP 的检索专项（0030）：写入、flush、首次检索（含建倒排）、热查询与 RSS。

同一脚本由 2.5.1、3.x 与本实现各自的解释器运行；数据由种子生成，三方相同。每次检索都与 numpy 独立计算的
精确内积 top-k 核对（分数相对误差 1e-5；截断处同分可取不同行，但不能漏掉更高分的行）。只写入不存在的新目录。
"""

import argparse
import hashlib
import json
import os
import platform
import statistics
import subprocess
import sys
import time

import numpy as np
from pymilvus import DataType, MilvusClient

DIMS = 30_000
NNZ = 32
TOPK = 10


def dataset(rows, seed):
    rng = np.random.default_rng(seed)
    p = 1.0 / np.arange(1, DIMS + 1) ** 1.1
    p /= p.sum()
    vectors = []
    for _ in range(rows):
        dims = np.unique(rng.choice(DIMS, size=NNZ, replace=False, p=p))
        weights = rng.uniform(0.01, 1.0, len(dims)).astype(np.float32)
        vectors.append(dict(zip(map(int, dims), map(float, weights))))
    return vectors


def postings(vectors):
    lists = {}
    for row, vector in enumerate(vectors):
        for dim, weight in vector.items():
            lists.setdefault(dim, ([], []))
            lists[dim][0].append(row)
            lists[dim][1].append(weight)
    return {dim: (np.array(r, dtype=np.int64), np.array(w, dtype=np.float64)) for dim, (r, w) in lists.items()}


def reference(lists, rows, query, allowed=None):
    scores = np.zeros(rows, dtype=np.float64)
    for dim, weight in query.items():
        if dim in lists:
            r, w = lists[dim]
            np.add.at(scores, r, float(np.float32(weight)) * w)
    if allowed is not None:
        scores[~allowed] = 0.0
    return scores


def check(hits, scores, label):
    got = [(h["id"], h["distance"]) for h in hits]
    assert len({pk for pk, _ in got}) == len(got), (label, "duplicate ids")
    assert all(h["entity"]["n"] == h["id"] for h in hits), (label, "output field n")
    positive = np.flatnonzero(scores > 0)
    expected = np.sort(scores[positive])[::-1][:TOPK].astype(np.float32)
    assert len(got) == len(expected), (label, len(got), len(expected))
    for (pk, distance), want in zip(got, expected):
        assert abs(distance - float(want)) <= 1e-5 * abs(float(want)) + 1e-7, (label, pk, distance, want)
        assert abs(distance - float(np.float32(scores[pk]))) <= 1e-5 * abs(scores[pk]) + 1e-7, (label, pk)
    if len(expected):
        cutoff = float(expected[-1])
        better = {int(pk) for pk in positive if np.float32(scores[pk]) > cutoff * (1 + 1e-5)}
        assert better <= {pk for pk, _ in got}, (label, better - {pk for pk, _ in got})


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
    parser.add_argument("--seed", type=int, default=30)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    os.makedirs(args.work_dir)  # 必须是新目录

    vectors = dataset(args.rows, args.seed)
    queries = dataset(args.queries + 32, args.seed + 1)
    lists = postings(vectors)
    half = np.arange(args.rows) < args.rows // 2

    client = MilvusClient(os.path.join(args.work_dir, "sparse.db"))
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("n", DataType.INT64)
    schema.add_field("sp", DataType.SPARSE_FLOAT_VECTOR)
    index = client.prepare_index_params()
    index.add_index("sp", index_type="SPARSE_INVERTED_INDEX", metric_type="IP",
                    params={"drop_ratio_build": 0, "inverted_index_algo": "TAAT_NAIVE"})
    client.create_collection("s", schema=schema, index_params=index)
    result = {"impl": args.impl, "rows": args.rows, "nnz": NNZ, "dims": DIMS, "queries": args.queries,
              "python": platform.python_version(), "machine": platform.machine(), "cpu_count": os.cpu_count(),
              "script_sha256": hashlib.sha256(open(__file__, "rb").read()).hexdigest()}
    for module in ["pymilvus", "milvus_lite"]:
        try:
            result[module] = __import__(module).__version__
        except AttributeError:
            result[module] = "unknown"

    def insert_all():
        for start in range(0, args.rows, 10_000):
            batch = [{"id": i, "n": i, "sp": vectors[i]} for i in range(start, min(start + 10_000, args.rows))]
            assert client.insert("s", batch)["insert_count"] == len(batch)
    _, result["insert_ms"] = timed(insert_all)
    _, result["flush_ms"] = timed(lambda: client.flush("s"))
    client.load_collection("s")
    params = {"metric_type": "IP", "params": {"drop_ratio_search": 0}}

    def search(qs, **kw):
        return client.search("s", qs, anns_field="sp", limit=TOPK, search_params=params, output_fields=["n"], **kw)

    first, result["first_search_ms"] = timed(lambda: search([queries[0]]))
    check(first[0], reference(lists, args.rows, queries[0]), "first")
    for q in queries[1:3]:
        search([q])  # 预热
    single = []
    for i, q in enumerate(queries[:args.queries]):
        hits, ms = timed(lambda q=q: search([q]))
        check(hits[0], reference(lists, args.rows, q), f"single{i}")
        single.append(ms)
    batch = queries[args.queries:args.queries + 32]
    batched = []
    for _ in range(9):
        hits, ms = timed(lambda: search(batch))
        assert len(hits) == len(batch)
        for q, h in zip(batch, hits):
            check(h, reference(lists, args.rows, q), "batch")
        batched.append(ms)
    filtered = []
    for i, q in enumerate(queries[:20]):
        hits, ms = timed(lambda q=q: search([q], filter=f"n < {args.rows // 2}"))
        check(hits[0], reference(lists, args.rows, q, half), f"filter{i}")
        filtered.append(ms)
    result.update({
        "single_median_ms": statistics.median(single),
        "single_p95_ms": sorted(single)[int(len(single) * 0.95) - 1],
        "batch32_median_ms": statistics.median(batched),
        "filtered_median_ms": statistics.median(filtered),
        "rss_kib": tree_rss_kib(),
    })
    client.close()
    with open(args.out, "a") as f:
        f.write(json.dumps(result, sort_keys=True) + "\n")
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    sys.exit(main())
