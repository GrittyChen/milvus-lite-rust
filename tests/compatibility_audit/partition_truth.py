"""0016 分区调查：固定参照的生命周期、RPC、路径与分桶中断真值；显式离线执行。"""

import argparse
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import subprocess
import sys

from run import REF, REPO, outside


def attempt(fn):
    try:
        return {"ok": True, "value": fn()}
    except Exception as error:
        code = getattr(error, "code", None)
        code = code() if callable(code) else code
        return {"ok": False, "type": type(error).__name__, "code": getattr(code, "name", code), "reason": str(error)}


def schema(*, key=False, auto=False):
    from milvus_lite.schema.types import CollectionSchema, DataType, FieldSchema
    fields = [FieldSchema("id", DataType.INT64, is_primary=True, auto_id=auto),
              FieldSchema("vector", DataType.FLOAT_VECTOR, dim=2),
              FieldSchema("tag", DataType.VARCHAR, max_length=64),
              FieldSchema("x", DataType.INT64, nullable=True, default_value=0),
              FieldSchema("a", DataType.ARRAY, element_type=DataType.INT64, nullable=True)]
    if key:
        fields.append(FieldSchema("tenant", DataType.INT64, is_partition_key=True, nullable=True))
    return CollectionSchema(fields, enable_dynamic_field=True)


def row(pk, tag, **extra):
    result = {"vector": [float(pk or 0), 0.], "tag": tag, **extra}
    if pk is not None:
        result["id"] = pk
    return result


def open_db(path):
    from milvus_lite.db import MilvusLite
    return MilvusLite(str(path))


def make(path, *, key=False, auto=False):
    db = open_db(path)
    c = db.create_collection("c", schema(key=key, auto=auto))
    if not key:
        c.create_partition("p1"); c.create_partition("p2")
    c._wait_for_bg(timeout=30)
    return db, c


def flush(c):
    c.flush(); c._wait_for_bg(timeout=30)


def view(c):
    result = {}
    for label, parts in [("all", None), ("empty", []), ("p1", ["p1"]), ("p2", ["p2"]),
                         ("union", ["p2", "p1"]), ("duplicate", ["p1", "p1"]),
                         ("unknown", ["missing"]), ("mixed_unknown", ["missing", "p1"])]:
        result[label] = {
            "query": attempt(lambda: c.query(partition_names=parts, output_fields=["id", "tag", "x", "a"])),
            "get": attempt(lambda: c.get([1, 2], partition_names=parts, output_fields=["tag"])),
            "search": attempt(lambda: c.search([[0., 0.]], top_k=20, metric_type="L2", partition_names=parts, output_fields=["tag"])),
        }
    result["stats"] = {p: attempt(lambda p=p: c.partition_num_entities(p)) for p in ["_default", "p1", "p2", "missing"]}
    result["total"] = c.num_entities
    return result


def sync_wal(c):
    # 退出点固定在完整 batch 已到内核之后，避免把 Python 缓冲残留混入分桶语义。
    for sink in (c._wal._data_sink, c._wal._delta_sink):
        if sink is not None:
            sink.flush(); os.fsync(sink.fileno())


def child(args):
    db, c = make(args.work_dir, key=args.child == "key-crash", auto=args.child == "key-crash")
    if args.child == "key-crash":
        original = c._apply
        def exit_after_first_bucket(op):
            original(op)
            sync_wal(c)
            os._exit(73)
        c._apply = exit_after_first_bucket
        c.insert([row(None, str(i), tenant=i % 2) for i in range(6)])
    else:
        c.insert([row(1, "old")], partition_name="p1")
        if args.child == "old-segment-wal": flush(c)
        c.insert([row(1, "new")], partition_name="p2")
        sync_wal(c)
        os._exit(72)
    raise AssertionError("controlled WAL exit did not fire")


def engine_cases(root):
    cases = {}
    for state in ["active", "old-segment", "both-segment", "wal", "old-segment-wal", "compacted"]:
        path = root / (state + ".db")
        if state in ("wal", "old-segment-wal"):
            result = subprocess.run([sys.executable, __file__, "--reference", str(REPO / "references/milvus-lite"), "--work-dir", str(path), "--child", state], env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"}, timeout=60)
            assert result.returncode == 72, result.returncode
            db = open_db(path); c = db.get_collection("c")
        else:
            db, c = make(path)
            c.insert([row(1, "old")], partition_name="p1")
            if state != "active": flush(c)
            c.insert([row(1, "new")], partition_name="p2")
            if state in ("both-segment", "compacted"): flush(c)
            if state == "compacted":
                for i in range(3): c.insert([row(10+i, "extra")], partition_name="p1"); flush(c)
                assert len(c._manifest.get_data_files("p1")) == 1
        cases["duplicate/"+state] = view(c)
        flush(c); cases["duplicate/"+state+"/after-flush"] = view(c)
        db.close(); db = open_db(path); c = db.get_collection("c"); cases["duplicate/"+state+"/reopen"] = view(c); db.close()

    for stage in ["active", "segment"]:
        path = root / ("delete-" + stage + ".db"); db, c = make(path)
        c.insert([row(2, "only-p2")], partition_name="p2")
        if stage == "segment": flush(c)
        deleted = c.delete([2], partition_name="p1")
        before = view(c); flush(c); after = view(c); c.drop_partition("p1"); dropped = view(c)
        db.close(); db = open_db(path); c = db.get_collection("c"); reopened = view(c); db.close()
        cases["wrong-partition-delete/"+stage] = {"delete_count": deleted, "before_flush": before, "after_flush": after, "after_drop": dropped, "reopen": reopened}

    path = root / "drop-latest.db"; db, c = make(path)
    c.insert([row(1, "old")], partition_name="p1"); flush(c); c.insert([row(1, "new")], partition_name="p2"); flush(c)
    cases["drop-latest/before"] = view(c); c.drop_partition("p2"); cases["drop-latest/after"] = view(c); db.close()

    for target in ["missing", "_default", "p1"]:
        path = root / ("drop-side-effect-"+target+".db"); db, c = make(path); c.insert([row(1, "pending")])
        files = lambda: sorted(str(p.relative_to(path)) for p in path.rglob("*") if p.is_file())
        before = files(); seq = c._next_seq; outcome = attempt(lambda: c.drop_partition(target))
        cases["drop-side-effect/"+target] = {"result": outcome, "files_changed": before != files(), "data_files": c._manifest.get_all_data_files(), "current_seq": c._manifest.current_seq, "next_seq_changed": c._next_seq != seq}
        db.close()

    path = root / "snapshot.db"; db, c = make(path); c.insert([row(1, "pinned", a=[None, 7])], partition_name="p1"); flush(c); db.create_snapshot("c", "saved")
    files = {p: hashlib.sha256(p.read_bytes()).hexdigest() for p in (path / "collections/c/partitions/p1").rglob("*") if p.is_file()}
    c.drop_partition("p1"); protected = all(p.exists() and hashlib.sha256(p.read_bytes()).hexdigest() == h for p, h in files.items())
    c.create_partition("p1"); c.insert([row(2, "new")], partition_name="p1"); flush(c); restored = db.restore_snapshot("c", "saved", "restored")
    cases["snapshot/drop-recreate"] = {"protected": protected, "restored": restored.query(output_fields=["id", "tag", "a"]), "current": c.query(output_fields=["id", "tag", "a"])}; db.close()
    return cases


def path_cases(root):
    cases = {}
    for label, name in [("empty", ""), ("dot", "."), ("dotdot", ".."), ("relative", "../escape"), ("absolute", str(root / "absolute-target")), ("slash", "nested/name"), ("backslash", "back\\slash"), ("null", "bad\0name"), ("unicode", "分区"), ("sentinel", "_all")]:
        path = root / (label+".db"); db, c = make(path)
        before = c.list_partitions(); outcome = attempt(lambda: c.create_partition(name))
        cases[label] = {"name": name, "result": outcome, "registered_after": name in c.list_partitions(), "partitions_before": before}
        if label == "absolute" and outcome["ok"]:
            c.insert([row(1, "outside-collection")], partition_name=name); flush(c)
            cases[label]["parquet_outside_collection"] = [str(p.relative_to(root)) for p in (root / "absolute-target").rglob("*.parquet")]
        cases[label]["close"] = attempt(db.close)
    path = root / "case-alias.db"; db, c = make(path); c.create_partition("P1")
    p = path / "collections/c/partitions/p1"; q = path / "collections/c/partitions/P1"
    same = os.path.samestat(p.stat(), q.stat())
    c.insert([row(1, "lower")], partition_name="p1"); flush(c); c.insert([row(2, "upper")], partition_name="P1"); flush(c)
    referenced = [p / f for f in c._manifest.get_data_files("p1")]; c.drop_partition("P1")
    cases["case-alias"] = {"same_directory": same, "remaining_referenced_files_exist": all(p.exists() for p in referenced), "live": c.query(output_fields=["id", "tag"])}
    db.close(); db = open_db(path); cases["case-alias"]["reopen"] = attempt(lambda: db.get_collection("c").query(output_fields=["id", "tag"])); attempt(db.close)
    return cases


def key_cases(root):
    import pyarrow as pa
    cases = {}
    db, c = make(root / "keys.db", key=True, auto=True)
    values = [0, 1, -1, 42, None, 0]
    result = c.insert([row(None, str(i), tenant=v) for i, v in enumerate(values)])
    cases["routing"] = {"ids": result, "partitions": c.list_partitions(), "rows": [r for b in c._memtable._insert_batches for r in b.to_pylist()], "manual_create": attempt(lambda: c.create_partition("manual")), "manual_drop": attempt(lambda: c.drop_partition("_default"))}
    db.close()
    path = root / "key-crash.db"
    run = subprocess.run([sys.executable, __file__, "--reference", str(REPO / "references/milvus-lite"), "--work-dir", str(path), "--child", "key-crash"], env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"}, timeout=60)
    assert run.returncode == 73, run.returncode
    wal_rows = [r for f in (path / "collections/c/wal").glob("wal_data_*.arrow") for b in pa.ipc.open_stream(f) for r in b.to_pylist()]
    db = open_db(path); c = db.get_collection("c"); before = c.query(output_fields=["id", "tag"]); inserted = c.insert([row(None, "after", tenant=0) for _ in range(3)]); after = c.query(output_fields=["id", "tag"])
    cases["auto-id-interrupted"] = {"wal_rows": wal_rows, "rows_before": before, "inserted": inserted, "rows_after": after, "expected_count_without_overwrite": len(before)+3, "actual_count": len(after)}; db.close()
    return cases


def rpc_cases(root):
    import grpc
    from pymilvus import DataType, MilvusClient
    from pymilvus.grpc_gen import common_pb2, milvus_pb2, milvus_pb2_grpc
    from milvus_lite.server_manager import ServerManager
    m = ServerManager(); uri = m.start_and_get_uri(str(root / "rpc.db")); client = MilvusClient(uri, dedicated=True); cases = {}
    try:
        s = client.create_schema(auto_id=False); s.add_field("id", DataType.INT64, is_primary=True); s.add_field("vector", DataType.FLOAT_VECTOR, dim=2); s.add_field("tag", DataType.VARCHAR, max_length=32)
        indexes = client.prepare_index_params(); indexes.add_index("vector", index_type="FLAT", metric_type="L2"); client.create_collection("c", schema=s, index_params=indexes)
        client.create_partition("c", "p1"); client.create_partition("c", "p2"); client.insert("c", [row(1, "old")], partition_name="p1"); client.flush("c"); client.insert("c", [row(1, "new"), row(2, "second")], partition_name="p2"); client.flush("c")
        with grpc.insecure_channel(uri.removeprefix("http://")) as ch:
            stub = milvus_pb2_grpc.MilvusServiceStub(ch)
            def call(method, request):
                try:
                    response = getattr(stub, method)(request); status = response if isinstance(response, common_pb2.Status) else response.status
                    result = {"code": status.code, "error_code": status.error_code, "reason": status.reason}
                    if hasattr(response, "value"): result["value"] = response.value
                    if hasattr(response, "partition_names"): result["partitions"] = list(response.partition_names); result["percentages"] = list(response.inMemory_percentages)
                    if hasattr(response, "stats"): result["stats"] = {kv.key: kv.value for kv in response.stats}
                    return result
                except grpc.RpcError as error: return {"grpc": error.code().name, "details": error.details()}
            for collection in ["c", "missing"]:
                for part in ["p1", "missing", "_default"]:
                    for method in ["CreatePartition", "HasPartition", "GetPartitionStatistics", "DropPartition"]:
                        if (method == "DropPartition" and part == "p1") or (method == "CreatePartition" and part == "missing" and collection == "c"): continue
                        req = getattr(milvus_pb2, method+"Request")(collection_name=collection, partition_name=part)
                        cases[f"{collection}/{method}/{part}"] = call(method, req)
            cases["create/new"] = call("CreatePartition", milvus_pb2.CreatePartitionRequest(collection_name="c", partition_name="created"))
            cases["drop/new"] = call("DropPartition", milvus_pb2.DropPartitionRequest(collection_name="c", partition_name="created"))
            for parts in [[], ["p2", "p1"], ["missing"], ["p1", "p1"]]:
                label = json.dumps(parts)
                cases["show/"+label] = call("ShowPartitions", milvus_pb2.ShowPartitionsRequest(collection_name="c", partition_names=parts))
                for expr in ["", "id >= 0"]:
                    cases["count/"+expr+"/"+label] = list(client.query("c", filter=expr, partition_names=parts, output_fields=["count(*)"]))
                cases["query/"+label] = list(client.query("c", filter="id >= 0", partition_names=parts, output_fields=["id", "tag"]))
                cases["get/"+label] = list(client.get("c", [1], partition_names=parts, output_fields=["tag"]))
            for method in ["LoadPartitions", "ReleasePartitions"]:
                cases[method] = call(method, getattr(milvus_pb2, method+"Request")(collection_name="c", partition_names=["p1"]))
            client.release_collection("c")
            cases["released/show"] = call("ShowPartitions", milvus_pb2.ShowPartitionsRequest(collection_name="c"))
            cases["released/count-empty"] = attempt(lambda: list(client.query("c", filter="", partition_names=["missing"], output_fields=["count(*)"])))
            cases["released/count-expr"] = attempt(lambda: list(client.query("c", filter="id >= 0", partition_names=["missing"], output_fields=["count(*)"])))
            cases["released/partition-stats"] = call("GetPartitionStatistics", milvus_pb2.GetPartitionStatisticsRequest(collection_name="c", partition_name="p1"))
    finally:
        client.close(); m.release_all()
    return cases


def main():
    p = argparse.ArgumentParser(description=__doc__); p.add_argument("--reference", type=Path, required=True); p.add_argument("--work-dir", type=Path, required=True); p.add_argument("--kind", choices=["engine", "paths", "key", "rpc"], default="engine"); p.add_argument("--child", choices=["wal", "old-segment-wal", "key-crash"])
    p.add_argument("--verify", type=Path)
    args = p.parse_args(); root = outside(args.work_dir); reference = args.reference.resolve()
    assert subprocess.check_output(["git", "-C", str(reference), "rev-parse", "HEAD"], text=True).strip() == REF
    import milvus_lite, pyarrow as pa
    assert Path(milvus_lite.__file__).resolve().is_relative_to(reference), milvus_lite.__file__
    assert pa.__version__ in ("15.0.0", "25.0.1"), pa.__version__
    root.mkdir(parents=True, exist_ok=False)
    if args.child: args.work_dir = root; child(args); return
    cases = {"engine": engine_cases, "paths": path_cases, "key": key_cases, "rpc": rpc_cases}[args.kind](root)
    serialized = json.dumps(cases, ensure_ascii=True, allow_nan=False).replace(str(root), "<work>")
    result = {"reference": REF, "script_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), "arrow": pa.__version__, "pymilvus": importlib.metadata.version("pymilvus"), "kind": args.kind, "cases": json.loads(serialized)}
    if args.verify:
        expected = json.loads(args.verify.read_text())
        assert expected["reference"] == REF and expected["cases"][args.kind] == result["cases"], "partition fixture differs"
    (root / "result.json").write_text(json.dumps(result,indent=2)+'\n'); print(json.dumps({"kind":args.kind,"count":len(cases),"arrow":pa.__version__}))


if __name__ == "__main__": main()
