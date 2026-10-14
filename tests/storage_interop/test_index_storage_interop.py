"""0014：真实 3.x 索引侧车、snapshot 钉住与新建定义的双向互通。"""

import json
import shutil
import sys
from pathlib import Path
import pytest
from test_cross_implementation import interpreters, run, PYTHONS

SETUP = """
import os, sys
from pymilvus import MilvusClient, DataType
from milvus_lite.server_manager import server_manager_instance as manager

path, kind = sys.argv[1:]
c = MilvusClient(path, dedicated=True)
s = MilvusClient.create_schema(auto_id=False)
s.add_field("id", DataType.INT64, is_primary=True)
s.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
p = c.prepare_index_params()
p.add_index("vector", index_type=kind, metric_type="COSINE")
c.create_collection("c", schema=s, index_params=p)
c.insert(
    "c",
    [
        {"id": 1, "vector": [1.0, 0.0]},
        {"id": 2, "vector": [2.0, 0.0]},
        {"id": 3, "vector": [0.0, 1.0]},
        {"id": 4, "vector": [-1.0, 0.0]},
    ],
)
c.flush("c")
c.load_collection("c")
c.close()
manager.release_server(path)
"""
READ = """
import sys, math
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as manager

path, metric, *mode = sys.argv[1:]
c = MilvusClient(path, dedicated=True)
try:
    c.load_collection("c")
    assert sorted(
        row["id"] for row in c.query("c", output_fields=["id"], limit=10)
    ) == [1, 2, 3, 4]
    hits = c.search("c", [[1.5, 0.0]], limit=1)[0]
    assert hits[0]["id"] in [1, 2] and math.isfinite(hits[0]["distance"]), hits
    if mode and mode[0] == "ann":
        pass
    elif metric == "L2":
        assert hits[0]["id"] in [1, 2] and math.isclose(
            hits[0]["distance"], 0.25, abs_tol=1e-6
        ), hits
    else:
        assert hits[0]["id"] in [1, 2] and math.isclose(
            hits[0]["distance"], 1.0, abs_tol=1e-6
        ), hits
finally:
    c.close()
    manager.release_server(path)
"""
CHANGE = """
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as manager

path, drop, metric = sys.argv[1:]
c = MilvusClient(path, dedicated=True)
try:
    if drop == "yes":
        c.release_collection("c")
        c.drop_index("c", "vector_idx")
    p = c.prepare_index_params()
    p.add_index("vector", index_type="HNSW", metric_type=metric)
    c.create_index("c", p)
finally:
    c.close()
    manager.release_server(path)
"""


@pytest.mark.parametrize("kind", ["HNSW", "HNSW_SQ", "IVF_FLAT", "IVF_SQ8"])
def test_rust_definitions_are_buildable_by_real_reference(tmp_path, interpreters, kind):
    root = tmp_path / "c.db"
    ref = interpreters[PYTHONS[0]]
    run([sys.executable, "-c", SETUP, str(root), kind], cwd=tmp_path)
    assert (
        not list((root / "collections/c/partitions/_default/indexes").glob("*.idx"))
        if (root / "collections/c/partitions/_default/indexes").exists()
        else True
    )
    run([ref, "-c", READ, str(root), "COSINE", "ann"], cwd=tmp_path)
    assert list((root / "collections/c/partitions/_default/indexes").glob("*.idx"))
    run([sys.executable, "-c", READ, str(root), "COSINE"], cwd=tmp_path)


@pytest.mark.parametrize("pinned", [False, True, "windows"])
def test_real_sidecars_drop_and_metric_rebuild_obey_snapshot_contract(
    tmp_path, interpreters, pinned
):
    root = tmp_path / "c.db"
    ref = interpreters[PYTHONS[0]]
    run([ref, "-c", SETUP, str(root), "HNSW"], cwd=tmp_path)
    idx = root / "collections/c/partitions/_default/indexes"
    files = list(idx.glob("*.idx"))
    assert files
    original = {f.name: f.read_bytes() for f in files}
    if pinned:
        run(
            [
                ref,
                "-c",
                """
from milvus_lite.db import MilvusLite
import sys

d = MilvusLite(sys.argv[1])
d.create_snapshot("c", "s")
d.close()
""",
                str(root),
            ],
            cwd=tmp_path,
        )
        if pinned == "windows":
            metadata_files = list(
                (root / "collections/c/snapshots/metadata").glob("*.json")
            )
            assert len(metadata_files) == 1
            metadata = metadata_files[0]
            value = json.loads(metadata.read_text())
            for key in ["data_files", "delta_files", "index_files"]:
                for refs in value[key].values():
                    refs[:] = [path.replace("/", "\\") for path in refs]
            metadata.write_text(json.dumps(value))
    run(
        [
            sys.executable,
            "-c",
            """
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as m
import sys

c = MilvusClient(sys.argv[1], dedicated=True)
c.release_collection("c")
c.drop_index("c", "vector_idx")
c.close()
m.release_server(sys.argv[1])
""",
            str(root),
        ],
        cwd=tmp_path,
    )
    if pinned:
        assert {f.name: f.read_bytes() for f in idx.glob("*.idx")} == original
        before = (root / "collections/c/manifest.json").read_bytes()
        refuse = """
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as m

c = MilvusClient(sys.argv[1], dedicated=True)
p = c.prepare_index_params()
p.add_index("vector", index_type="HNSW", metric_type="L2")
try:
    c.create_index("c", p)
except Exception as e:
    assert e.code == 1 and "snapshot" in e.message, e
else:
    raise AssertionError("conflicting pinned definition was accepted")
finally:
    c.close()
    m.release_server(sys.argv[1])
"""
        run([sys.executable, "-c", refuse, str(root)], cwd=tmp_path)
        assert (root / "collections/c/manifest.json").read_bytes() == before
        run(
            [
                ref,
                "-c",
                """
from milvus_lite.db import MilvusLite
import sys

d = MilvusLite(sys.argv[1])
c = d.restore_snapshot("c", "s", "restored")
c.load()
assert c.num_entities == 4
d.close()
""",
                str(root),
            ],
            cwd=tmp_path,
        )
        run([sys.executable, "-c", CHANGE, str(root), "no", "COSINE"], cwd=tmp_path)
        assert {f.name: f.read_bytes() for f in idx.glob("*.idx")} == original
        run([ref, "-c", READ, str(root), "COSINE"], cwd=tmp_path)
    else:
        assert not list(idx.glob("*.idx"))
        run([sys.executable, "-c", CHANGE, str(root), "no", "L2"], cwd=tmp_path)
        run([ref, "-c", READ, str(root), "L2"], cwd=tmp_path)


@pytest.mark.parametrize(
    "kind", ["AUTOINDEX", "HNSW", "HNSW_SQ", "IVF_FLAT", "IVF_SQ8"]
)
@pytest.mark.parametrize("metric", ["BM25", "NONE"])
def test_unbuildable_metric_is_rejected_before_persisting_or_deleting_files(
    tmp_path, interpreters, kind, metric
):
    root = tmp_path / "c.db"
    ref = interpreters[PYTHONS[0]]
    check = """
import json
import sys
from pathlib import Path
import grpc
from pymilvus import MilvusClient, DataType
from milvus_lite.server_manager import server_manager_instance as manager

path, kind, metric = sys.argv[1:]
c = MilvusClient(path, dedicated=True)
try:
    schema = c.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    c.create_collection("c", schema=schema)
    c.insert(
        "c", [{"id": i, "vector": [float(i % 17), float(i % 23)]} for i in range(300)]
    )
    c.flush("c")
    collection = Path(path) / "collections/c"
    manifest = collection / "manifest.json"
    before = manifest.read_bytes()
    data = json.loads(before)
    source = Path(data["partitions"]["_default"]["data_files"][0]).stem
    indexes = collection / "partitions/_default/indexes"
    indexes.mkdir(exist_ok=True)
    stale = indexes / f"{source}.vector.{kind.lower()}.idx"
    stale.write_bytes(b"stale index must survive rejected creation")
    params = c.prepare_index_params()
    params.add_index("vector", index_type=kind, metric_type=metric)
    try:
        c.create_index("c", params)
    except grpc.RpcError as e:
        assert e.code() == grpc.StatusCode.UNIMPLEMENTED, e
        assert "requires COSINE / L2 / IP" in e.details(), e
    else:
        raise AssertionError("unbuildable metric was accepted")
    assert manifest.read_bytes() == before
    assert stale.read_bytes() == b"stale index must survive rejected creation"
    assert not (collection / "manifest.json.tmp").exists()
    assert not (collection / "milvus-lite-rust.index-intent.json").exists()
    # 用 3.x 能 load 的定义继续互通，确认拒绝未损伤原数据。
    params = c.prepare_index_params()
    params.add_index("vector", index_type="FLAT", metric_type="L2")
    c.create_index("c", params)
finally:
    c.close()
    manager.release_server(path)
"""
    run([sys.executable, "-c", check, str(root), kind, metric], cwd=tmp_path)
    read = """
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as manager

path = sys.argv[1]
c = MilvusClient(path, dedicated=True)
try:
    c.load_collection("c")
    assert len(c.query("c", output_fields=["id"], limit=500)) == 300
finally:
    c.close()
    manager.release_server(path)
"""
    run([ref, "-c", read, str(root)], cwd=tmp_path)


def test_legacy_unpinned_sidecars_are_removed_before_new_definition(
    tmp_path, interpreters
):
    root = tmp_path / "c.db"
    ref = interpreters[PYTHONS[0]]
    run([ref, "-c", SETUP, str(root), "HNSW"], cwd=tmp_path)
    manifest = root / "collections/c/manifest.json"
    data = json.loads(manifest.read_text())
    data["index_specs"] = {}
    manifest.write_text(json.dumps(data))
    files = list((root / "collections/c/partitions/_default/indexes").glob("*.idx"))
    assert files
    run([sys.executable, "-c", CHANGE, str(root), "no", "L2"], cwd=tmp_path)
    assert all(not f.exists() for f in files)
    run([ref, "-c", READ, str(root), "L2"], cwd=tmp_path)


def test_wal_only_drop_flush_difference_is_explicit(tmp_path, interpreters):
    original = tmp_path / "original.db"
    ref = interpreters[PYTHONS[0]]
    write = """
import os, sys
from pymilvus import MilvusClient, DataType

c = MilvusClient(sys.argv[1], dedicated=True)
s = MilvusClient.create_schema(auto_id=False)
s.add_field("id", DataType.INT64, is_primary=True)
s.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
c.create_collection("c", schema=s)
p = c.prepare_index_params()
p.add_index("vector", index_type="DISKANN", metric_type="L2")
p.add_index("id", index_type="INVERTED")
c.create_index("c", p)
c.insert("c", [{"id": 1, "vector": [1.0, 0.0]}])
os._exit(0)
"""
    run([ref, "-c", write, str(original)], cwd=tmp_path)
    check = """
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as m

path, expected = sys.argv[1:]
c = MilvusClient(path, dedicated=True)
try:
    c.drop_index("c", "id_idx")
    try:
        c.load_collection("c")
    except Exception as e:
        assert (
            expected == "fail"
            and e.code == 1
            and "unknown index_type: 'DISKANN'" in e.message
        ), e
    else:
        assert expected == "ok"
finally:
    c.close()
    m.release_server(path)
"""
    for python, expected, label in [
        (sys.executable, "fail", "rust"),
        (ref, "ok", "ref"),
    ]:
        copy = tmp_path / f"{label}.db"
        shutil.copytree(original, copy)
        run([python, "-c", check, str(copy), expected], cwd=tmp_path)


@pytest.fixture(scope="session")
def index_fault_executable():
    from test_cross_implementation import REPO
    import subprocess

    result = subprocess.run(
        [
            "cargo",
            "build",
            "-p",
            "milvus-lite-core",
            "--features",
            "fault-injection",
            "--example",
            "index_change_fault",
            "--message-format=json",
        ],
        cwd=REPO,
        text=True,
        capture_output=True,
    )
    assert result.returncode == 0, result.stderr
    artifacts = [
        json.loads(line) for line in result.stdout.splitlines() if line.startswith("{")
    ]
    executable = [
        v["executable"]
        for v in artifacts
        if v.get("reason") == "compiler-artifact"
        and v.get("target", {}).get("name") == "index_change_fault"
        and v.get("executable")
    ]
    assert len(executable) == 1, executable
    return executable[0]


def test_deleted_sidecars_before_manifest_failure_require_reference_rebuild(
    tmp_path, interpreters, index_fault_executable
):
    root = tmp_path / "c.db"
    ref = interpreters[PYTHONS[0]]
    run([ref, "-c", SETUP, str(root), "HNSW"], cwd=tmp_path)
    file = root / "collections/c/manifest.json"
    value = json.loads(file.read_text())
    value["index_specs"]["vector"]["build_params"] = {"M": "abc"}
    file.write_text(json.dumps(value))
    run([ref, "-c", READ, str(root), "COSINE"], cwd=tmp_path)
    before = file.read_bytes()
    sidecars = list((root / "collections/c/partitions/_default/indexes").glob("*.idx"))
    assert sidecars
    result = json.loads(run([index_fault_executable, str(root)], cwd=tmp_path))
    assert "BeforeReplace" in result["error"] and result["indexes"] == 1
    assert file.read_bytes() == before and all(not f.exists() for f in sidecars)
    error = """
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as m

c = MilvusClient(sys.argv[1], dedicated=True)
try:
    try:
        c.load_collection("c")
    except Exception as e:
        assert e.code == 1 and "invalid literal for int()" in e.message, e
    else:
        raise AssertionError("reference rebuilt an invalid HNSW definition")
finally:
    c.close()
    m.release_server(sys.argv[1])
"""
    run([ref, "-c", error, str(root)], cwd=tmp_path)
    run([sys.executable, "-c", READ, str(root), "COSINE"], cwd=tmp_path)


def test_private_intent_survives_reference_and_only_blocks_rust_index_changes(
    tmp_path, interpreters
):
    root = tmp_path / "c.db"
    ref = interpreters[PYTHONS[0]]
    run([ref, "-c", SETUP, str(root), "HNSW"], cwd=tmp_path)
    collection = root / "collections/c"
    marker = collection / "milvus-lite-rust.index-intent.json"
    files = [
        str(p.relative_to(collection))
        for p in (collection / "partitions/_default/indexes").glob("*.idx")
    ]
    assert files
    marker.write_text(
        json.dumps(
            {
                "format_version": 1,
                "operation": "drop_index",
                "field_names": ["vector"],
                "files": files,
                "boot_id": None,
                "boot_source": None,
                "auto_recovery": False,
            }
        )
    )
    before = marker.read_bytes()
    run([ref, "-c", READ, str(root), "COSINE"], cwd=tmp_path)
    run(
        [
            ref,
            "-c",
            """
from milvus_lite.db import MilvusLite
import sys

d = MilvusLite(sys.argv[1])
d.create_snapshot("c", "s")
c = d.restore_snapshot("c", "s", "restored")
c.load()
assert c.num_entities == 4
d.close()
""",
            str(root),
        ],
        cwd=tmp_path,
    )
    assert marker.read_bytes() == before
    assert not (root / "collections/restored" / marker.name).exists()
    check = """
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as m

c = MilvusClient(sys.argv[1], dedicated=True)
try:
    c.load_collection("c")
    assert len(c.get("c", [1, 2])) == 2
    c.release_collection("c")
    try:
        c.drop_index("c", "vector_idx")
    except Exception as e:
        assert e.code == 1 and "blocked" in e.message, e
    else:
        raise AssertionError("unknown boot marker was ignored")
    c.load_collection("c")
    c.insert("c", [{"id": 5, "vector": [3.0, 0.0]}])
    c.flush("c")
    assert len(c.get("c", [5])) == 1
finally:
    c.close()
    m.release_server(sys.argv[1])
"""
    run([sys.executable, "-c", check, str(root)], cwd=tmp_path)
    assert marker.read_bytes() == before
