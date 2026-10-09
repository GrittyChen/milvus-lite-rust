"""隔离测试产物测量发布锁与受控提交窗口的 Query；不代表正式产物吞吐。"""

import argparse
import hashlib
import importlib.metadata
import importlib.util
import json
import math
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import threading
import time


def statistics(samples):
    if not samples or any(not math.isfinite(value) or value < 0 for value in samples):
        raise ValueError("finite nonnegative query samples required")
    ordered = sorted(samples)
    rank = (len(ordered) - 1) * 0.99
    lower = int(rank)
    upper = min(lower + 1, len(ordered) - 1)
    return {"samples": len(samples), "p99_seconds": ordered[lower] + (ordered[upper] - ordered[lower]) * (rank - lower), "max_seconds": ordered[-1]}


def checked_publication(stderr):
    matches = re.findall(r"compaction publication: input_rows=(\d+) wait_seconds=([\d.]+) lock_seconds=([\d.]+)", stderr)
    if len(matches) != 1 or int(matches[0][0]) != 200_000:
        raise ValueError(f"expected one 200000-row publication, got {matches}")
    values = [float(value) for value in matches[0][1:]]
    if any(not math.isfinite(value) or value < 0 for value in values):
        raise ValueError("invalid publication diagnostic")
    return {"publication_wait_seconds": values[0], "publication_lock_seconds": values[1]}


def sample(database, rows, pk_kind):
    from milvus_lite import _native, __version__
    from milvus_lite._native import NativeServer
    from pymilvus import MilvusClient

    if not hasattr(NativeServer, "_pause_compaction"):
        raise RuntimeError("isolated fault-injection wheel required")
    native_hash = hashlib.sha256(Path(_native.__file__).read_bytes()).hexdigest()
    primary = 0 if pk_kind == "int64" else f"{0:016}"
    predicate = f"id in [{json.dumps(primary)}]"
    server = NativeServer.start(str(database), "127.0.0.1", 0, __version__)
    client = MilvusClient(uri=f"http://127.0.0.1:{server.port}", dedicated=True)
    stopped = threading.Event()
    ready = threading.Event()
    concurrent = []
    errors = []
    worker = None
    paused = False

    def query():
        start = time.perf_counter()
        result = client.query("c", filter=predicate, output_fields=["id"], limit=1, timeout=30)
        elapsed = time.perf_counter() - start
        if [row["id"] for row in result] != [primary]:
            raise ValueError("Query result changed during compaction")
        return elapsed

    def queries():
        try:
            while not stopped.is_set():
                concurrent.append(query())
                ready.set()
        except BaseException as error:
            errors.append(error)
            ready.set()

    try:
        server._pause_compaction("before_commit")
        paused = True
        client.load_collection("c")
        server._wait_compaction_pause(90)
        count = client.query("c", filter="", output_fields=["count(*)"])[0]["count(*)"]
        if count != rows:
            raise ValueError("fixture row count changed")
        baseline = [query() for _ in range(200)]
        worker = threading.Thread(target=queries)
        worker.start()
        if not ready.wait(30):
            raise TimeoutError("concurrent Query did not start")
        if errors:
            raise errors[0]
        server._resume_compaction()
        paused = False
        report = server._wait_for_maintenance("c", 90)
        if report["failed"] or report["cancelled"] or report["last_input_rows"] != 200_000:
            raise ValueError(f"maintenance did not complete the expected round: {report}")
        stopped.set()
        worker.join(timeout=35)
        if worker.is_alive():
            raise TimeoutError("concurrent Query did not finish")
        if errors:
            raise errors[0]
        if client.query("c", filter="", output_fields=["count(*)"])[0]["count(*)"] != rows:
            raise ValueError("compaction changed logical rows")
        result = {"baseline_query": statistics(baseline), "commit_window_query": statistics(concurrent),
                  "native_sha256": native_hash, "pk_kind": pk_kind,
                  "versions": {"python": sys.version.split()[0], "pymilvus": importlib.metadata.version("pymilvus")}}
    finally:
        stopped.set()
        if paused:
            server._resume_compaction()
        if worker is not None and worker.is_alive():
            worker.join(timeout=35)
        client.close()
        server.stop(2)
    return result


def load_base():
    spec = importlib.util.spec_from_file_location("compaction_run", Path(__file__).with_name("run.py"))
    base = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(base)
    return base


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--database", type=Path, required=True)
    parser.add_argument("--rows", type=int, default=1_000_000)
    parser.add_argument("--dim", type=int, default=128)
    parser.add_argument("--pk-kind", choices=["int64", "varchar"], required=True)
    parser.add_argument("--sample", action="store_true")
    parser.add_argument("--python")
    parser.add_argument("--work-dir", type=Path)
    parser.add_argument("--rounds", type=int, default=3)
    args = parser.parse_args()
    if not args.database.is_dir():
        raise ValueError("existing database required")
    if args.sample:
        print(json.dumps(sample(args.database, args.rows, args.pk_kind)))
        return
    if not args.python or args.work_dir is None or args.rounds <= 0:
        raise ValueError("python/work-dir/positive rounds required")
    schema = json.loads((args.database / "collections/c/schema.json").read_text())
    keys = [field for field in schema["fields"] if field.get("is_primary")]
    if len(keys) != 1 or keys[0]["name"] != "id" or keys[0]["dtype"] != args.pk_kind:
        raise ValueError("fixture primary type does not match measurement")
    base = load_base()
    base.validate_fixture(args.database, args.rows, args.dim, args.pk_kind)
    args.work_dir.mkdir(parents=True, exist_ok=True)
    records = []
    environment = os.environ.copy()
    environment["MILVUS_LITE_COMPACTION_DIAGNOSTICS"] = "1"
    for number in range(1, args.rounds + 1):
        target = args.work_dir / f"{args.pk_kind}-{number}.db"
        if target.exists():
            raise ValueError("round destination already exists")
        shutil.copytree(args.database, target)
        command = [args.python, str(Path(__file__).resolve()), "--sample", "--database", str(target),
                   "--rows", str(args.rows), "--pk-kind", args.pk_kind]
        process = subprocess.run(command, capture_output=True, text=True, check=True, env=environment, timeout=240)
        result = json.loads(process.stdout)
        result.update(checked_publication(process.stderr))
        result["round_seconds"] = base.checked_rounds(process.stderr)
        layout = base.physical(target)
        if layout["live_rows"] != args.rows or layout["dead_rows"]:
            raise ValueError("physical row model changed")
        result["round"] = number
        result["layout"] = layout
        records.append(result)
    print(json.dumps({"rows": args.rows, "dim": args.dim, "pk_kind": args.pk_kind, "records": records,
                      "script_sha256": hashlib.sha256(Path(__file__).read_bytes() + Path(__file__).with_name("run.py").read_bytes()).hexdigest(),
                      "scope": "隔离 FI 产物；准备完成后的受控提交/发布/清理窗口，单并发主键 Query；不是仅写锁区间的 p99"}, ensure_ascii=False))


if __name__ == "__main__":
    main()
