"""恢复专项：已有健康库的新进程启动、load 与独立服务 RSS；不准备或覆盖数据。"""
import argparse
import hashlib
import importlib.metadata
import importlib.util
import json
import os
from pathlib import Path
import sys
import time


def load_baseline(path, rows, dim):
    source = Path(__file__).resolve().parents[1] / "baseline/bench.py"
    spec = importlib.util.spec_from_file_location("baseline_recovery", source)
    base = importlib.util.module_from_spec(spec)
    saved = sys.argv
    try:
        sys.argv = [str(source), str(path), str(rows), str(dim), "FLAT", "reopen", "server"]
        spec.loader.exec_module(base)
    finally:
        sys.argv = saved
    return base


def check_count(result, rows):
    if result != [{"count(*)": rows}]:
        raise RuntimeError(f"恢复行数不一致：{result!r}，预期 {rows}")


def read_index(client, collection):
    names = client.list_indexes(collection)
    if len(names) != 1:
        raise RuntimeError(f"恢复基准要求唯一的 FLAT L2 索引，实际索引为 {names!r}")
    description = client.describe_index(collection, names[0])
    actual = {key: description.get(key) for key in ("index_type", "metric_type")}
    if actual != {"index_type": "FLAT", "metric_type": "L2"}:
        raise RuntimeError(f"恢复基准要求 FLAT L2，实际索引为 {actual!r}")
    return actual


def check_search(result, ids, scores):
    import numpy as np
    if len(result) != 1 or [hit["id"] for hit in result[0]] != ids:
        raise RuntimeError("恢复后 Search 的主键或顺序与 float64 真值不一致")
    actual = np.asarray([hit["distance"] for hit in result[0]], dtype=np.float64)
    expected = np.asarray(scores, dtype=np.float64)
    if (actual.shape != expected.shape or not np.isfinite(actual).all()
            or not np.isfinite(expected).all()
            or not (np.abs(actual - expected) <= np.maximum(1e-5 * np.abs(expected), 1e-6)).all()):
        raise RuntimeError("恢复后 Search 的距离超出 0004 口径")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("database", type=Path)
    parser.add_argument("--rows", type=int, required=True)
    parser.add_argument("--dim", type=int, default=128)
    parser.add_argument("--round", type=int, required=True)
    parser.add_argument("--build", required=True)
    args = parser.parse_args()
    if not args.database.exists():
        parser.error("只接受已存在的健康基准库，不新建数据目录")
    if args.rows < 10 or args.dim < 1:
        parser.error("rows >= 10 且 dim >= 1")
    base = load_baseline(args.database, args.rows, args.dim)
    np = base.np
    # 真值与 baseline.prepare 的随机数消耗顺序相同，全部在启动计时之前生成。
    rng = np.random.default_rng(42)
    rng.random((200, args.dim), dtype=np.float32)
    vectors = rng.random((args.rows, args.dim), dtype=np.float32).astype(np.float64)
    scores = ((vectors - base.queries[0].astype(np.float64)) ** 2).sum(axis=1)
    order = np.lexsort((np.arange(args.rows), scores))[:10]
    expected_ids, expected_scores = order.tolist(), scores[order].tolist()
    del vectors, scores
    client = None
    server = None
    ok = False
    try:
        start = time.perf_counter()
        server, uri = base.start_server()
        base.SERVER_PID = server.pid
        client = base.MilvusClient(uri, dedicated=True)
        connect_s = time.perf_counter() - start
        before_load = base.rss_mb()
        start = time.perf_counter()
        client.load_collection(base.C)
        load_s = time.perf_counter() - start
        after_load = base.rss_mb()
        index_described = read_index(client, base.C)
        check_count(client.query(base.C, output_fields=["count(*)"]), args.rows)
        start = time.perf_counter()
        result = client.search(base.C, [base.queries[0].tolist()], limit=10,
                               search_params=base.SEARCH_PARAMS)
        first_search_ms = (time.perf_counter() - start) * 1000
        check_search(result, expected_ids, expected_scores)
        after_search = base.rss_mb()
        ok = True
    finally:
        try:
            if client is not None:
                client.close()
        finally:
            if server is not None:
                base.stop_server(server, graceful=ok)
    digest = hashlib.sha256()
    for directory in [Path(base.__file__).parent, Path(__file__).parent]:
        for path in sorted(directory.iterdir()):
            if path.suffix in (".py", ".sh"):
                digest.update(f"{directory.name}/{path.name}".encode())
                digest.update(path.read_bytes())
    native_sha = None
    if base.VER == "rs":
        from milvus_lite import _native
        native_sha = hashlib.sha256(Path(_native.__file__).read_bytes()).hexdigest()
    print(json.dumps({
        "implementation": base.VER, "build": args.build, "round": args.round,
        "rows": args.rows, "dim": args.dim, "confirmed_rows": args.rows,
        "index_described": index_described,
        "connect_s": connect_s, "load_s": load_s, "ready_s": connect_s + load_s,
        "first_search_ms": first_search_ms,
        "rss_before_load_mib": before_load, "rss_after_load_mib": after_load,
        "rss_after_search_mib": after_search, "native_sha256": native_sha,
        "benchmark_sha256": digest.hexdigest(), "load_average": os.getloadavg(),
        "versions": {"python": sys.version.split()[0], "implementation": base.PKG_VERSION,
                     "pymilvus": importlib.metadata.version("pymilvus"),
                     "numpy": importlib.metadata.version("numpy")},
    }))


if __name__ == "__main__":
    main()
