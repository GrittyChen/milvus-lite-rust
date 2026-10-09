"""真实子进程退出覆盖合并 manifest 提交点两侧，使用隔离测试 wheel。"""

import json
from pathlib import Path
import subprocess
import sys

import pytest
from milvus_lite._native import NativeServer


@pytest.mark.parametrize("phase,files", [("exit_before_replace",4),("exit_after_replace",1)])
def test_process_exit_preserves_the_manifest_boundary_and_all_rows(phase,files,tmp_path):
    if not hasattr(NativeServer,"_inject_compaction_fault"):
        pytest.fail("isolated fault-injection wheel required")
    database=tmp_path / "crash.db"
    code="""
import sys
from pymilvus import MilvusClient
from milvus_lite._native import NativeServer
server=NativeServer.start(sys.argv[1],"127.0.0.1",0,"test")
client=MilvusClient(uri=f"http://127.0.0.1:{server.port}",dedicated=True)
client.create_collection("c",dimension=2,metric_type="L2")
server._wait_for_maintenance("c",15)
for pk in range(3):
    client.insert("c",[{"id":pk,"vector":[float(pk+1),0.0]}])
    client.flush("c")
    server._wait_for_maintenance("c",15)
server._inject_compaction_fault(sys.argv[2])
client.insert("c",[{"id":3,"vector":[4.0,0.0]}])
client.flush("c")
server._wait_for_maintenance("c",15)
raise AssertionError("process exit seam was not reached")
"""
    process=subprocess.run([sys.executable,"-c",code,str(database),phase],capture_output=True,text=True,timeout=30)
    assert process.returncode==86,(process.stdout,process.stderr)
    manifest=json.loads((database / "collections/c/manifest.json").read_text())
    assert len(manifest["partitions"]["_default"]["data_files"])==files
    from pymilvus import MilvusClient
    server=NativeServer.start(str(database),"127.0.0.1",0,"test")
    client=MilvusClient(uri=f"http://127.0.0.1:{server.port}",dedicated=True)
    try:
        client.load_collection("c")
        rows=client.get("c",[0,1,2,3],output_fields=["vector"])
        assert [r["id"] for r in rows]==[0,1,2,3]
        assert [r["vector"] for r in rows]==[[1.0,0.0],[2.0,0.0],[3.0,0.0],[4.0,0.0]]
        assert server._wait_for_maintenance("c",15)["failed"]==0
    finally:
        client.close(); server.stop(2)
