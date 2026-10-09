"""自动合并与 compaction RPC 的真实客户端入口。"""

import json

import pytest
from pymilvus import MilvusClient
from milvus_lite._native import NativeServer
from milvus_lite.server_manager import ServerManager


@pytest.mark.parametrize("name", ["c", "missing"])
def test_compaction_rpc_acknowledges_without_checking_collection(name, tmp_path):
    manager = ServerManager()
    uri = manager.start_and_get_uri(str(tmp_path / "rpc.db"))
    client = MilvusClient(uri=uri, dedicated=True)
    try:
        client.create_collection("c", dimension=2, metric_type="L2")
        identifier = client.compact(name)
        assert identifier == 0
        assert client.get_compaction_state(identifier) == "Completed"
    finally:
        client.close()
        manager.release_all()


def test_normal_stop_merges_committed_files_and_reopen_reads_all_rows(tmp_path):
    path = tmp_path / "data.db"
    manager = ServerManager()
    uri = manager.start_and_get_uri(str(path))
    client = MilvusClient(uri=uri, dedicated=True)
    try:
        client.create_collection("c", dimension=2, metric_type="L2")
        for pk in range(4):
            client.insert("c", [{"id": pk, "vector": [1.0, 0.0]}])
            client.flush("c")
    finally:
        client.close()
        manager.release_all()
    manifest = json.loads((path / "collections/c/manifest.json").read_text())
    assert len(manifest["partitions"]["_default"]["data_files"]) == 1
    uri = manager.start_and_get_uri(str(path))
    client = MilvusClient(uri=uri, dedicated=True)
    try:
        client.load_collection("c")
        assert [row["id"] for row in client.get("c", [0, 1, 2, 3])] == [0, 1, 2, 3]
    finally:
        client.close()
        manager.release_all()


def test_release_native_has_no_compaction_test_entrypoints():
    for name in ("_wait_for_maintenance", "_pause_compaction", "_wait_compaction_pause", "_resume_compaction", "_inject_compaction_fault"):
        assert not hasattr(NativeServer, name), name


@pytest.mark.parametrize("diagnostics", [None, "0", "1"])
def test_success_diagnostics_require_an_explicit_opt_in(diagnostics, tmp_path):
    import os
    import subprocess
    import sys
    environment = os.environ.copy()
    environment.pop("MILVUS_LITE_COMPACTION_DIAGNOSTICS", None)
    if diagnostics is not None:
        environment["MILVUS_LITE_COMPACTION_DIAGNOSTICS"] = diagnostics
    code = """
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import ServerManager
manager=ServerManager()
client=MilvusClient(uri=manager.start_and_get_uri(sys.argv[1]),dedicated=True)
try:
    client.create_collection("c",dimension=2)
    for pk in range(4):
        client.insert("c",[{"id":pk,"vector":[1.0,0.0]}])
        client.flush("c")
    assert client.query("c",filter="",output_fields=["count(*)"])[0]["count(*)"]==4
finally:
    client.close()
    manager.release_all()
"""
    result = subprocess.run([sys.executable, "-c", code, str(tmp_path / "diagnostics.db")], capture_output=True, text=True, env=environment, timeout=30)
    assert result.returncode == 0, result.stderr
    for marker in ("compaction completed:", "compaction publication:"):
        assert (marker in result.stderr) == (diagnostics == "1"), result.stderr
