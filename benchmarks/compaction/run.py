"""后台合并的单轮耗时、退出等待与物理行回收验收；原始结果放仓库外。"""

import argparse
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time


def prepare(path, rows, dim, pk_kind="int64"):
    if path.exists():
        raise ValueError("fixture already exists")
    if pk_kind not in ("int64", "varchar"):
        raise ValueError("unknown primary key type")
    if rows < 200_000 or rows % 50_000 or dim <= 0:
        raise ValueError("rows must be a multiple of 50000 and at least 200000")
    import numpy as np
    import pyarrow as pa
    from milvus_lite.schema.types import CollectionSchema, FieldSchema, DataType
    from milvus_lite.schema.persistence import save_schema
    from milvus_lite.schema.arrow_builder import build_data_schema
    from milvus_lite.storage.data_file import write_data_file
    from milvus_lite.storage.manifest import Manifest

    collection = path / "collections/c"
    collection.mkdir(parents=True)
    primary = (FieldSchema("id", DataType.INT64, is_primary=True) if pk_kind == "int64"
               else FieldSchema("id", DataType.VARCHAR, is_primary=True, max_length=64))
    schema = CollectionSchema(fields=[primary, FieldSchema("vector", DataType.FLOAT_VECTOR, dim=dim)])
    save_schema(schema, "c", str(collection / "schema.json"))
    manifest = Manifest(str(collection))
    rng = np.random.default_rng(42)
    for start in range(0, rows, 50_000):
        end = start + 50_000
        vectors = rng.random((50_000, dim), dtype=np.float32)
        values = {
            "_seq": pa.array(np.arange(start+1, end+1, dtype=np.uint64)),
            "id": (pa.array(np.arange(start, end, dtype=np.int64)) if pk_kind == "int64"
                   else pa.array([f"{i:016}" for i in range(start, end)])),
            "vector": pa.FixedSizeListArray.from_arrays(pa.array(vectors.reshape(-1)), dim),
        }
        table = pa.Table.from_pydict(values, schema=build_data_schema(schema))
        relative = write_data_file(table, str(collection / "partitions/_default"), start+1, end)
        manifest.add_data_file("_default", relative)
        with (collection / "partitions/_default" / relative).open("rb") as source:
            os.fsync(source.fileno())
    manifest._current_seq = rows
    manifest.save()
    return {"rows": rows, "dim": dim, "files": rows // 50_000}


def sample(path, rows, implementation, pk_kind="int64"):
    if implementation == "rust":
        from milvus_lite._native import NativeServer
        from milvus_lite import __version__
        from pymilvus import MilvusClient
        server = NativeServer.start(str(path), "127.0.0.1", 0, __version__)
        client = MilvusClient(uri=f"http://127.0.0.1:{server.port}", dedicated=True)
        try:
            client.load_collection("c")
            assert client.query("c", filter="", output_fields=["count(*)"])[0]["count(*)"] == rows
            expected = [0, rows-1] if pk_kind == "int64" else [f"{i:016}" for i in (0, rows-1)]
            assert [r["id"] for r in client.get("c", expected)] == expected
        finally:
            client.close()
            start = time.perf_counter()
            server.stop(2.0)
            stop_seconds = time.perf_counter() - start
    else:
        from milvus_lite.db import MilvusLite
        db = MilvusLite(str(path))
        collection = db.get_collection("c")
        assert collection.num_entities == rows
        start = time.perf_counter()
        db.close()
        stop_seconds = time.perf_counter() - start
    return {"stop_seconds": stop_seconds}


def physical(path):
    import pyarrow.parquet as pq
    collection = path / "collections/c"
    manifest = json.loads((collection / "manifest.json").read_text())
    latest = {}
    deletes = {}
    total = 0
    byte_count = 0
    sources = []
    for relative in manifest["partitions"]["_default"]["data_files"]:
        source = collection / "partitions/_default" / relative
        byte_count += source.stat().st_size
        table = pq.read_table(source, columns=["id", "_seq"])
        rows = list(zip(table["id"].to_pylist(), table["_seq"].to_pylist()))
        total += len(rows)
        sources.append(rows)
        for pk, seq in rows:
            latest[pk] = max(seq, latest.get(pk, -1))
    for relative in manifest["partitions"]["_default"]["delta_files"]:
        table = pq.read_table(collection / "partitions/_default" / relative, columns=["id", "_seq"])
        for pk, seq in zip(table["id"].to_pylist(), table["_seq"].to_pylist()):
            deletes[pk] = max(seq, deletes.get(pk, -1))
    seen = set()
    live = 0
    for source in sources:
        for pk, seq in source:
            if seq == latest[pk] and deletes.get(pk, -1) <= seq and pk not in seen:
                seen.add(pk)
                live += 1
    return {"physical_rows": total, "live_rows": live, "dead_rows": total-live, "dead_ratio": (total-live)/total if total else 0.0, "data_bytes": byte_count, "data_files": len(sources)}


def checked_rounds(stderr):
    rounds = [(int(n), float(t)) for n, t in re.findall(r"compaction completed: input_rows=(\d+) elapsed_seconds=([\d.]+)", stderr)]
    if len(rounds) != 1 or rounds[0][0] != 200_000:
        raise ValueError(f"expected exactly one 200000-row compaction, got {rounds}")
    return rounds[0][1]


def validate_fixture(path, rows, dim, pk_kind="int64"):
    schema = json.loads((path / "collections/c/schema.json").read_text())
    keys = [field for field in schema["fields"] if field.get("is_primary")]
    if len(keys) != 1 or keys[0]["name"] != "id" or keys[0]["dtype"] != pk_kind:
        raise ValueError("fixture primary type does not match measurement")
    vectors = [field for field in schema["fields"] if field["dtype"] == "float_vector"]
    if len(vectors) != 1 or vectors[0]["name"] != "vector" or vectors[0]["dim"] != dim:
        raise ValueError("fixture vector dimension does not match requested measurement")
    layout = physical(path)
    if layout["physical_rows"] != rows or layout["live_rows"] != rows:
        raise ValueError("fixture must contain exactly the requested live rows")
    return layout


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--database", type=Path, required=True)
    parser.add_argument("--rows", type=int, default=1_000_000)
    parser.add_argument("--dim", type=int, default=128)
    parser.add_argument("--prepare", action="store_true")
    parser.add_argument("--pk-kind", choices=["int64", "varchar"], default="int64")
    parser.add_argument("--sample", choices=["rust", "reference"])
    parser.add_argument("--work-dir", type=Path)
    parser.add_argument("--python")
    parser.add_argument("--implementation", choices=["rust", "reference"], default="rust")
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--build", default="unspecified")
    args = parser.parse_args()
    if args.prepare:
        print(json.dumps(prepare(args.database, args.rows, args.dim, args.pk_kind)))
        return
    if not args.database.is_dir():
        raise ValueError("existing database required")
    if args.sample:
        print(json.dumps(sample(args.database, args.rows, args.sample, args.pk_kind)))
        return
    if not args.python or args.work_dir is None or args.rounds <= 0:
        raise ValueError("python/work-dir/positive rounds required")
    validate_fixture(args.database, args.rows, args.dim, args.pk_kind)
    args.work_dir.mkdir(parents=True, exist_ok=True)
    records = []
    for round_no in range(1, args.rounds+1):
        target = args.work_dir / f"{args.implementation}-{round_no}.db"
        if target.exists():
            raise ValueError("round destination already exists")
        shutil.copytree(args.database, target)
        environment = os.environ.copy()
        if args.implementation == "rust":
            environment["MILVUS_LITE_COMPACTION_DIAGNOSTICS"] = "1"
        result = subprocess.run([args.python, str(Path(__file__).resolve()), "--database", str(target), "--rows", str(args.rows), "--sample", args.implementation, "--pk-kind", args.pk_kind], text=True, capture_output=True, check=True, timeout=180, env=environment)
        record = json.loads(result.stdout)
        if args.implementation == "rust":
            record["round_seconds"] = checked_rounds(result.stderr)
            if record["round_seconds"] > 10:
                raise RuntimeError("single round exceeded 10 seconds; design reassessment required")
        layout = physical(target)
        if layout["live_rows"] != args.rows or layout["dead_rows"] != 0:
            raise ValueError("compaction fixture result changed logical rows")
        record.update(layout)
        record["round"] = round_no
        records.append(record)
    print(json.dumps({"implementation": args.implementation, "pk_kind": args.pk_kind, "rows": args.rows, "dim": args.dim, "build": args.build, "script_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), "versions": {name: importlib.metadata.version(name) for name in ("numpy", "pyarrow")}, "records": records}, ensure_ascii=False))


if __name__ == "__main__":
    main()
