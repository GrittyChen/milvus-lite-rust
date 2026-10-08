"""批量 FLAT 专项：复用 baseline 的数据和采样口径，真值准备与计时分开。"""
import argparse
import hashlib
import importlib.metadata
import importlib.util
import json
import os
from pathlib import Path
import sys


def load_baseline(path, rows, dim):
    source = Path(__file__).resolve().parents[1] / "baseline/bench.py"
    spec = importlib.util.spec_from_file_location("baseline_batch_search", source)
    base = importlib.util.module_from_spec(spec)
    saved = sys.argv
    try:
        sys.argv = [str(source), str(path), str(rows), str(dim), "FLAT", "m2", "inproc"]
        spec.loader.exec_module(base)
    finally:
        sys.argv = saved
    return base


def prepare_truth(destination, rows, dim):
    import numpy as np
    if destination.exists():
        raise RuntimeError("真值目录已存在，不覆盖")
    destination.mkdir(parents=True)
    rng = np.random.default_rng(42)
    queries = rng.random((200, dim), dtype=np.float32)[:100]
    vectors = rng.random((rows, dim), dtype=np.float32)
    np.save(destination / "queries.npy", queries)
    np.save(destination / "vectors.npy", vectors)
    best_ids = [np.empty(0, dtype=np.int64) for _ in queries]
    best_scores = [np.empty(0, dtype=np.float64) for _ in queries]
    for start in range(0, rows, 8192):
        block = vectors[start:start + 8192].astype(np.float64)
        for qi, query in enumerate(queries):
            delta = block - query.astype(np.float64)
            scores = np.einsum("ij,ij->i", delta, delta)
            cutoff = np.partition(scores, min(9, len(scores) - 1))[min(9, len(scores) - 1)]
            keep = np.flatnonzero(scores <= cutoff)
            keep = keep[np.lexsort((keep, scores[keep]))[:10]]
            ids = np.concatenate((best_ids[qi], start + keep))
            distances = np.concatenate((best_scores[qi], scores[keep]))
            order = np.lexsort((ids, distances))[:10]
            best_ids[qi], best_scores[qi] = ids[order], distances[order]
    metadata = {"rows": rows, "dim": dim, "seed": 42,
                "ids": [ids.tolist() for ids in best_ids],
                "scores": [scores.tolist() for scores in best_scores]}
    for name in ("queries.npy", "vectors.npy"):
        metadata[name + "_sha256"] = hashlib.sha256((destination / name).read_bytes()).hexdigest()
    (destination / "truth.json").write_text(json.dumps(metadata))


def check_search(result, queries, vectors, truth):
    import numpy as np
    if len(result) != len(queries):
        raise RuntimeError("查询条数不符")
    for qi, (hits, query) in enumerate(zip(result, queries)):
        ids = [hit["id"] for hit in hits]
        if (len(ids) != 10 or len(set(ids)) != 10
                or any(type(pk) is not int or pk < 0 or pk >= len(vectors) for pk in ids)):
            raise RuntimeError("候选数量、类型或主键范围不符")
        actual = np.asarray([hit["distance"] for hit in hits], dtype=np.float64)
        expected = ((vectors[ids].astype(np.float64) - query.astype(np.float64)) ** 2).sum(axis=1)
        tolerance = np.maximum(1e-5 * np.abs(expected), 1e-6)
        if (not np.isfinite(actual).all() or not np.isfinite(expected).all()
                or not (np.abs(actual - expected) <= tolerance).all()):
            raise RuntimeError("距离不符合 0004 的 L2 口径")
        if not (actual[:-1] <= actual[1:]).all():
            raise RuntimeError("结果未按距离排序")
        wanted = set(truth["ids"][qi])
        missing = wanted - set(ids)
        if missing:
            reference = dict(zip(truth["ids"][qi], truth["scores"][qi]))
            added = sorted(expected[i] for i, pk in enumerate(ids) if pk not in wanted)
            removed = sorted(reference[pk] for pk in missing)
            cutoff = truth["scores"][qi][-1]
            if any(abs(d - cutoff) > max(1e-5 * abs(d), 1e-6) + max(1e-5 * abs(cutoff), 1e-6)
                   for d in removed):
                raise RuntimeError("缺失候选不在截断处的容差内")
            for a, b in zip(added, removed):
                if abs(a - b) > max(1e-5 * abs(a), 1e-6) + max(1e-5 * abs(b), 1e-6):
                    raise RuntimeError("截断处候选不同且非容差内并列")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--truth", type=Path, required=True)
    parser.add_argument("--rows", type=int, required=True)
    parser.add_argument("--dim", type=int, default=128)
    parser.add_argument("--prepare-truth", action="store_true")
    parser.add_argument("--database", type=Path)
    parser.add_argument("--round", type=int)
    parser.add_argument("--build")
    args = parser.parse_args()
    if args.rows < 10 or args.dim < 1:
        parser.error("rows >= 10 且 dim >= 1")
    if args.prepare_truth:
        prepare_truth(args.truth, args.rows, args.dim)
        return
    if args.database is None or not args.database.exists() or args.round is None or args.build is None:
        parser.error("测量要求已存在的数据库、round 和 build")
    base = load_baseline(args.database, args.rows, args.dim)
    np = base.np
    truth = json.loads((args.truth / "truth.json").read_text())
    if truth["rows"] != args.rows or truth["dim"] != args.dim or truth["seed"] != 42:
        raise RuntimeError("真值配置不符")
    arrays = {}
    for name in ("queries.npy", "vectors.npy"):
        path = args.truth / name
        if hashlib.sha256(path.read_bytes()).hexdigest() != truth[name + "_sha256"]:
            raise RuntimeError("真值数组指纹不符")
        arrays[name] = np.load(path, mmap_mode="r", allow_pickle=False)
    queries, vectors = arrays["queries.npy"], arrays["vectors.npy"]
    if (queries.shape != (100, args.dim) or vectors.shape != (args.rows, args.dim)
            or queries.dtype != np.float32 or vectors.dtype != np.float32):
        raise RuntimeError("真值数组形状或类型不符")
    client = base.MilvusClient(str(args.database), dedicated=True)
    try:
        client.load_collection(base.C)
        names = client.list_indexes(base.C)
        if len(names) != 1:
            raise RuntimeError("要求唯一 FLAT L2 索引")
        spec = client.describe_index(base.C, names[0])
        if spec.get("index_type") != "FLAT" or spec.get("metric_type") != "L2":
            raise RuntimeError("实际索引不是 FLAT L2")
        if client.query(base.C, output_fields=["count(*)"]) != [{"count(*)": args.rows}]:
            raise RuntimeError("行数不符")
        metrics = {}
        for nq, samples in ((1, 100), (100, 10)):
            fn = lambda: client.search(base.C, queries[:nq].tolist(), limit=10,
                                       search_params=base.SEARCH_PARAMS)
            check_search(fn(), queries[:nq], vectors, truth)
            results = []
            metrics[f"search_nq{nq}_top10"] = base.lat(lambda: results.append(fn()), samples)
            for result in results:
                check_search(result, queries[:nq], vectors, truth)
    finally:
        client.close()
    digest = hashlib.sha256()
    for directory in (Path(base.__file__).parent, Path(__file__).parent):
        for path in sorted(directory.iterdir()):
            if path.suffix in (".py", ".sh"):
                digest.update(f"{directory.name}/{path.name}".encode())
                digest.update(path.read_bytes())
    native = None
    if base.VER == "rs":
        from milvus_lite import _native
        native = hashlib.sha256(Path(_native.__file__).read_bytes()).hexdigest()
    print(json.dumps({"implementation": base.VER, "build": args.build, "round": args.round,
                      "rows": args.rows, "dim": args.dim, "metrics": metrics,
                      "native_sha256": native, "benchmark_sha256": digest.hexdigest(),
                      "truth_sha256": hashlib.sha256((args.truth / "truth.json").read_bytes()).hexdigest(),
                      "load_average": os.getloadavg(), "versions": {
                          "python": sys.version.split()[0], "implementation": base.PKG_VERSION,
                          "pymilvus": importlib.metadata.version("pymilvus"),
                          "numpy": importlib.metadata.version("numpy")}}))


if __name__ == "__main__":
    main()
