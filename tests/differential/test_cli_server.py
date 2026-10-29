"""CLI 停服/重开以真实 3.x 为真值；信号与数值差异按 0012 单独固定。"""

import os
from pathlib import Path
import queue
import threading
import signal
import socket
import subprocess
import sys

import pytest
from pymilvus import MilvusClient

from compare import Diff

# 等待子进程启动（含参照实现导入 grpc、pyarrow 等）的上限，只防挂起；本机不到 1 秒，
# 慢的 Intel Mac 执行机在三个分片并行时实测超过 15 秒。
STARTUP_TIMEOUT = 60


class Cli:
    def __init__(self, python, path):
        self.path = path
        with socket.socket() as chosen:
            chosen.bind(("127.0.0.1", 0))
            port = chosen.getsockname()[1]
        self.log = (path.parent / (path.name + ".log")).open("w+")
        env = os.environ.copy()
        env["PYTHONUNBUFFERED"] = "1"
        self.process = subprocess.Popen([python, "-m", "milvus_lite", "server", "--data-dir", str(path), "--host", "127.0.0.1", "--port", str(port), "--max-workers", "1"], env=env, stdout=subprocess.PIPE, stderr=self.log, text=True, creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0)
        self.output = queue.Queue()
        def read_output():
            try:
                for value in self.process.stdout:
                    self.output.put(value)
            except Exception as error:
                self.output.put(error)
            finally:
                self.output.put(None)
        self.reader = threading.Thread(target=read_output, daemon=True)
        self.reader.start()
        try:
            line = self.output.get(timeout=STARTUP_TIMEOUT)
            if isinstance(line, Exception):
                raise line
            assert line is not None and f"listening on 127.0.0.1:{port}" in line, line
        except BaseException:
            self.close()
            raise
        self.reference = python != sys.executable
        self.client = MilvusClient(uri=f"http://127.0.0.1:{port}", dedicated=True)
    def stop(self):
        self.client.close()
        self.process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGINT)
        self.process.wait(timeout=30)
        self.reader.join(timeout=5)
        assert not self.reader.is_alive()
        lines = []
        while not self.output.empty():
            value = self.output.get_nowait()
            if isinstance(value, Exception):
                raise value
            if value is not None:
                lines.append(value)
        output = "".join(lines)
        self.log.seek(0)
        if os.name == "nt" and self.reference:
            # 0012 的已登记差异：参照不接管 SIGBREAK，本实现按正常停服处理。
            assert self.process.returncode != 0, self.log.read()
        else:
            assert self.process.returncode == 0, self.log.read()
            assert output == "\nShutting down...\n"
    def close(self):
        if hasattr(self, "client"):
            self.client.close()
        if self.process.poll() is None:
            self.process.kill(); self.process.wait()
        self.reader.join(timeout=5)
        assert not self.reader.is_alive()
        self.process.stdout.close()
        self.log.close()


def test_cli_stop_policy_flushes_or_recovers_the_same_logical_data(tmp_path):
    rust_path, ref_path = tmp_path / "rust", tmp_path / "reference"
    rust = Cli(sys.executable, rust_path)
    reference = Cli(os.environ["MILVUS_LITE_3X_PYTHON"], ref_path)
    try:
        diff = Diff(rust.client, reference.client)
        diff.step("create", lambda c: c.create_collection("c", dimension=2, metric_type="L2"))
        diff.step("insert", lambda c: c.insert("c", [{"id": 1, "vector": [1.0, 0.0], "category": "tech"}, {"id": 2, "vector": [0.0, 1.0], "category": "tech"}]))
        diff.step("upsert", lambda c: c.upsert("c", [{"id": 1, "vector": [0.0, 1.0], "category": "ai"}]))
        diff.step("delete", lambda c: c.delete("c", ids=[2]))
        diff.step("dynamic", lambda c: c.query("c", filter="category == 'ai'", output_fields=["id", "category"]))
        rust.stop(); reference.stop()
        rust.close(); reference.close()
        rust = Cli(sys.executable, rust_path)
        reference = Cli(os.environ["MILVUS_LITE_3X_PYTHON"], ref_path)
        diff = Diff(rust.client, reference.client)
        diff.step("load", lambda c: c.load_collection("c"))
        assert diff.step("get", lambda c: c.get("c", [1, 2], output_fields=["vector", "category"]))[1] == [{"id": 1, "vector": [0.0, 1.0], "category": "ai"}]
        assert diff.step("count", lambda c: c.query("c", filter="", output_fields=["count(*)"]))[1] == [{"count(*)": 1}]
        diff.step("search", lambda c: c.search("c", [[0.0, 1.0]], limit=1, search_params={"metric_type": "L2"}))
        rust.stop(); reference.stop()
    finally:
        rust.close(); reference.close()


@pytest.mark.parametrize("value", ["0", "-1"])
def test_cli_invalid_worker_rejection_is_registered_before_resource_creation(value, tmp_path):
    for label, python, code, exists in [("rust", sys.executable, 2, False), ("reference", os.environ["MILVUS_LITE_3X_PYTHON"], 1, True)]:
        path = tmp_path / label
        result = subprocess.run([python, "-m", "milvus_lite", "server", "--data-dir", str(path), "--max-workers", value], capture_output=True, text=True, timeout=STARTUP_TIMEOUT)
        assert result.returncode == code, result.stderr
        assert path.exists() == exists
        assert "listening" not in result.stdout
