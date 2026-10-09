"""隔离测试产物验证正常停服等待期间的 Python 信号处理。"""

import json
import selectors
import signal
import subprocess
import sys
import time

import pytest
from milvus_lite._native import NativeServer


def test_ctrl_c_is_observed_after_the_native_stop_wait_returns(tmp_path):
    if not hasattr(NativeServer, "_pause_compaction"):
        pytest.fail("isolated fault-injection wheel required")
    code = """
import json,sys,threading,time
from pymilvus import MilvusClient
from milvus_lite._native import NativeServer
server=NativeServer.start(sys.argv[1],"127.0.0.1",0,"test")
client=MilvusClient(uri=f"http://127.0.0.1:{server.port}",dedicated=True)
client.create_collection("c",dimension=2,metric_type="L2")
server._wait_for_maintenance("c",15)
server._pause_compaction("before_commit")
for pk in range(4):
    client.insert("c",[{"id":pk,"vector":[1.0,0.0]}])
    client.flush("c")
server._wait_compaction_pause(15)
client.close()
def resume():
    time.sleep(2)
    server._resume_compaction()
worker=threading.Thread(target=resume)
worker.start()
print("READY",flush=True)
start=time.monotonic()
interrupted=False
try:
    server.stop(2)
except KeyboardInterrupt:
    interrupted=True
elapsed=time.monotonic()-start
worker.join()
print(json.dumps({"interrupted":interrupted,"elapsed":elapsed}),flush=True)
"""
    process = subprocess.Popen([sys.executable,"-u","-c",code,str(tmp_path / "signal.db")],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
    selector = selectors.DefaultSelector()
    selector.register(process.stdout,selectors.EVENT_READ)
    try:
        assert selector.select(20)
        assert process.stdout.readline().strip()=="READY"
        time.sleep(0.15)
        process.send_signal(signal.SIGINT)
        stdout,stderr=process.communicate(timeout=20)
        assert process.returncode==0,stderr
        result=json.loads(stdout)
        assert result["interrupted"]
        assert result["elapsed"]>=1.5,result
    finally:
        selector.close()
        if process.poll() is None:
            process.kill()
            process.wait()
