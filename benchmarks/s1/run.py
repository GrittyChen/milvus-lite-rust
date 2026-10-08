"""S1 专项：复用 baseline 的数据、预热与采样口径，只测已 flush 的只读数据。

在各实现的独立环境运行；--prepare 只接受不存在的库，结果与库均放仓库外。
"""
import argparse
import hashlib
import importlib.metadata
import importlib.util
import json
import os
from pathlib import Path
import platform
import sys


def load_baseline(path, n, dim):
    source = Path(__file__).resolve().parents[1] / "baseline" / "bench.py"
    spec = importlib.util.spec_from_file_location("baseline_s1", source)
    module = importlib.util.module_from_spec(spec)
    saved = sys.argv
    try:
        sys.argv = [str(source), str(path), str(n), str(dim), "FLAT", "m2", "inproc"]
        spec.loader.exec_module(module)
    finally:
        sys.argv = saved
    return module


def check_query(rows, expected):
    if rows != expected:
        raise RuntimeError(f"Query 结果不一致：返回 {len(rows)} 行，预期 {len(expected)} 行")


def check_search(result, ids, scores):
    import numpy as np
    if len(result) != 1 or [hit["id"] for hit in result[0]] != ids:
        raise RuntimeError("过滤 Search 的候选或排序与 float64 FLAT 真值不一致")
    actual = np.asarray([hit["distance"] for hit in result[0]], dtype=np.float64)
    expected = np.asarray(scores, dtype=np.float64)
    tolerance = np.maximum(1e-5 * np.abs(expected), 1e-6)
    if (actual.shape != expected.shape or not np.isfinite(actual).all()
            or not np.isfinite(expected).all() or not (np.abs(actual - expected) <= tolerance).all()):
        raise RuntimeError("过滤 Search 的距离超出 0004 的 L2 误差范围")


def dependency_versions(implementation):
    versions = {"python": sys.version.split()[0], "impl": implementation,
                "pymilvus": importlib.metadata.version("pymilvus"),
                "numpy": importlib.metadata.version("numpy")}
    try:
        versions["pyarrow"] = importlib.metadata.version("pyarrow")
    except importlib.metadata.PackageNotFoundError:
        pass
    return versions


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("database", type=Path)
    parser.add_argument("--rows", type=int, required=True)
    parser.add_argument("--dim", type=int, default=128)
    parser.add_argument("--prepare", action="store_true")
    parser.add_argument("--round", type=int, required=True)
    parser.add_argument("--build", required=True, help="被测产物的构建身份")
    args = parser.parse_args()
    if args.rows < 2000 or args.dim < 1:
        parser.error("rows >= 2000 且 dim >= 1")
    if args.prepare and args.database.exists():
        parser.error("--prepare 不覆盖已存在的数据目录")
    if not args.prepare and not args.database.exists():
        parser.error("数据目录不存在，先 --prepare")
    base = load_baseline(args.database, args.rows, args.dim)
    np = base.np
    # 与 prepare 相同的随机数消耗顺序；客户端期望值不从被测引擎推导。
    rng = np.random.default_rng(42)
    rng.random((200, args.dim), dtype=np.float32)
    data = rng.random((args.rows, args.dim), dtype=np.float32)
    cats = rng.integers(0, 10, args.rows)
    expected = [{"id": int(i), "tag": f"t{i % 100}"}
                for i in np.flatnonzero((cats == 5) & (np.arange(args.rows) > 1000))[:100]]
    search_truth = {}
    for label, mask in [("integer", cats == 3), ("string", np.arange(args.rows) % 100 == 7)]:
        ids = np.flatnonzero(mask)
        vectors = data[ids].astype(np.float64)
        scores = ((vectors - base.queries[0].astype(np.float64)) ** 2).sum(axis=1)
        order = np.lexsort((ids, scores))[:10]
        search_truth[label] = (ids[order].tolist(), scores[order].tolist())
    del data
    client = base.MilvusClient(str(args.database), dedicated=True)
    try:
        if args.prepare:
            base.prepare(client)
            client.flush(base.C)
        client.load_collection(base.C)
        count = lambda: client.query(base.C, filter="cat >= 0", output_fields=["count(*)"])
        query = lambda: client.query(base.C, filter="cat == 5 and id > 1000",
                                     output_fields=["id", "tag"], limit=100)
        operations = [("query_count", count, lambda r: check_query(r, [{"count(*)": args.rows}]), 10),
                      ("query_filter_limit100", query, lambda r: check_query(r, expected), 50)]
        for label, expr in [("integer", "cat == 3"), ("string", 'tag == "t7"')]:
            ids, scores = search_truth[label]
            def search(expr=expr):
                return client.search(base.C, [base.queries[0].tolist()], limit=10,
                                     filter=expr, search_params=base.SEARCH_PARAMS)
            def check(result, ids=ids, scores=scores):
                check_search(result, ids, scores)
            operations.append((f"search_filter_{label}", search, check, 100))
        metrics = {}
        for name, operation, check, samples in operations:
            check(operation())
            results = []
            def measured():
                results.append(operation())
            metrics[name] = base.lat(measured, samples)
            for result in results:
                check(result)
            check(operation())
        check_query(client.query(base.C, output_fields=["count(*)"]), [{"count(*)": args.rows}])
    finally:
        client.close()
    digest = hashlib.sha256()
    for directory in [Path(base.__file__).parent, Path(__file__).parent]:
        for path in sorted(directory.iterdir()):
            if path.suffix in (".py", ".sh"):
                digest.update(f"{directory.name}/{path.name}".encode())
                digest.update(path.read_bytes())
    print(json.dumps({"implementation": base.VER, "build": args.build, "round": args.round,
                      "rows": args.rows, "dim": args.dim, "metrics": metrics,
                      "benchmark_sha256": digest.hexdigest(), "load_avg": os.getloadavg(),
                      "native_sha256": (hashlib.sha256(Path(importlib.import_module("milvus_lite._native").__file__).read_bytes()).hexdigest()
                                        if base.VER == "rs" else None),
                      "machine": {"platform": platform.platform(), "cpu_count": os.cpu_count()},
                      "versions": dependency_versions(base.PKG_VERSION)}, ensure_ascii=False))


if __name__ == "__main__":
    main()
