"""隔离 FI 产物控制维护暂停，验证 CLI 的后续默认信号及崩溃恢复。"""

import os
import queue
import threading
import signal
import subprocess
import sys

import pytest
from pymilvus import MilvusClient
from milvus_lite._native import NativeServer


@pytest.mark.parametrize("kind", ["SIGINT", "SIGTERM"] if os.name != "nt" else ["SIGBREAK"])
def test_cli_default_signal_after_stop_checkpoint_terminates_and_recovers(kind, tmp_path):
    if not hasattr(NativeServer, "_pause_compaction"):
        pytest.fail("isolated fault-injection wheel required")
    path = tmp_path / "second.db"
    code = """
import sys,signal,threading
from milvus_lite import _native,cmdline
Original=_native.NativeServer
class Wrapper:
    def __init__(self,native): self.native=native
    @property
    def bound_address(self): return self.native.bound_address
    def stop(self,grace):
        assert signal.getsignal(signal.SIGINT)==signal.SIG_DFL
        assert signal.getsignal(signal.SIGTERM)==signal.SIG_DFL
        print('DEFAULT',flush=True)
        self.native.stop(grace)
        raise AssertionError('paused maintenance unexpectedly finished')
class Factory:
    @staticmethod
    def start(*args):
        native=Original.start(*args)
        native._pause_compaction('before_commit')
        def observe():
            native._wait_compaction_pause(30)
            print('PAUSED',flush=True)
        threading.Thread(target=observe,daemon=True).start()
        return Wrapper(native)
_native.NativeServer=Factory
raise SystemExit(cmdline.main(['server','--data-dir',sys.argv[1],'--host','127.0.0.1','--port','0']))
"""
    log = (tmp_path / "signals.log").open("w+")
    process = subprocess.Popen([sys.executable, "-u", "-c", code, str(path)], text=True, stdout=subprocess.PIPE, stderr=log, creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0)
    output_lines = queue.Queue()
    def read_output():
        for value in process.stdout:
            output_lines.put(value)
        output_lines.put(None)
    reader = threading.Thread(target=read_output, daemon=True)
    reader.start()
    def line():
        value = output_lines.get(timeout=30)
        assert value is not None, "CLI exited before signal checkpoint"
        return value.strip()
    try:
        ready = line()
        assert "listening on " in ready, ready
        address = ready.split("listening on ", 1)[1].split(" ", 1)[0]
        client = MilvusClient(uri=f"http://{address}", dedicated=True)
        try:
            client.create_collection("c", dimension=2, metric_type="L2")
            for pk in range(4):
                client.insert("c", [{"id": pk, "vector": [1.0, 0.0]}])
                client.flush("c")
        finally:
            client.close()
        assert line() == "PAUSED"
        stop = signal.CTRL_BREAK_EVENT if os.name == "nt" else getattr(signal, kind)
        process.send_signal(stop)
        output = []
        while True:
            value = line()
            output.append(value)
            if value == "DEFAULT":
                break
            assert len(output) < 5, output
        assert "Shutting down..." in output
        process.send_signal(stop)
        process.wait(timeout=15)
        if os.name == "nt":
            assert process.returncode != 0
        else:
            assert process.returncode == -stop
        reopened = NativeServer.start(str(path), "127.0.0.1", 0, "test")
        client = MilvusClient(uri=f"http://127.0.0.1:{reopened.port}", dedicated=True)
        try:
            client.load_collection("c")
            assert [r["id"] for r in client.get("c", [0, 1, 2, 3])] == [0, 1, 2, 3]
        finally:
            client.close(); reopened.stop(2)
    finally:
        if process.poll() is None:
            process.kill(); process.wait()
        reader.join(timeout=5)
        assert not reader.is_alive()
        log.close()
