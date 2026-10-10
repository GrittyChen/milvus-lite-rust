"""CLI 的参数、资源与信号边界，使用当前安装产物。"""

import errno
import json
import os
from pathlib import Path
import queue
import signal
import socket
import subprocess
import sys
import threading
from types import SimpleNamespace

import grpc
import pytest
from pymilvus import MilvusClient
from pymilvus.grpc_gen import milvus_pb2, milvus_pb2_grpc
from milvus_lite import cmdline


def test_parser_defaults_and_repeated_arguments():
    parsed = cmdline.build_parser().parse_args(["server", "--data-dir", "d"])
    assert (parsed.host, parsed.port, parsed.max_workers) == ("0.0.0.0", 19530, 10)
    parsed = cmdline.build_parser().parse_args(["server", "--data-dir", "d", "--port", "1", "--port", "0"])
    assert parsed.port == 0


@pytest.mark.parametrize("option,value", [("--port", "-1"), ("--port", "65536"), ("--port", "abc"), ("--max-workers", "0"), ("--max-workers", "-1"), ("--max-workers", str(1 << 128))])
def test_invalid_parameters_never_create_a_directory(option, value, tmp_path):
    path = tmp_path / "absent.db"
    result = subprocess.run([sys.executable, "-m", "milvus_lite", "server", "--data-dir", str(path), option, value], capture_output=True, text=True, timeout=15)
    assert result.returncode == 2, result.stderr
    assert not path.exists()
    assert "listening" not in result.stdout


def test_invalid_bracketed_host_is_a_startup_error_without_resources(tmp_path):
    path = tmp_path / "absent.db"
    result = subprocess.run([sys.executable, "-m", "milvus_lite", "server", "--data-dir", str(path), "--host", "[bad]"], capture_output=True, text=True, timeout=15)
    assert result.returncode == 1, result.stderr
    assert "server failed:" in result.stderr
    assert not path.exists()
    assert "listening" not in result.stdout


@pytest.mark.parametrize("value", ["0", "bad", "-1", "", str(1 << 128)])
def test_invalid_tokio_configuration_is_an_ordinary_error_for_both_entrypoints(value, tmp_path):
    env = os.environ.copy()
    env["TOKIO_WORKER_THREADS"] = value
    cli = tmp_path / "cli.db"
    result = subprocess.run([sys.executable, "-m", "milvus_lite", "server", "--data-dir", str(cli), "--port", "0"], env=env, capture_output=True, text=True, timeout=15)
    assert result.returncode == 1
    assert "TOKIO_WORKER_THREADS" in result.stderr
    assert "PanicException" not in result.stderr
    assert not cli.exists()
    embedded = tmp_path / "embedded.db"
    code = """
import sys
from pymilvus import MilvusClient
try:
    MilvusClient(sys.argv[1])
except Exception as error:
    print(type(error).__name__)
else:
    raise AssertionError('invalid configuration succeeded')
"""
    result = subprocess.run([sys.executable, "-c", code, str(embedded)], env=env, capture_output=True, text=True, timeout=15)
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip()
    assert "PanicException" not in result.stderr
    assert not embedded.exists()


def test_real_server_branch_rejects_non_main_thread_before_resources(tmp_path):
    result = []
    path = tmp_path / "thread.db"
    worker = threading.Thread(target=lambda: result.append(cmdline.main(["server", "--data-dir", str(path)])))
    worker.start(); worker.join(timeout=5)
    assert not worker.is_alive()
    assert result == [1]
    assert not path.exists()


@pytest.mark.parametrize("primary,close_failure,start_stopped", [(False, False, False), (False, True, False), (True, True, False), (False, False, True)])
def test_cleanup_preserves_errors_signal_order_and_startup_cancellation(monkeypatch, capsys, primary, close_failure, start_stopped):
    events = []
    states = {}
    class Event:
        def set(self):
            pass
        def is_set(self):
            return start_stopped
        def wait(self, timeout):
            events.append(("wait", timeout))
            if primary:
                raise RuntimeError("primary failure")
            return True
    def signals(item, handler):
        old = states.get(item, signal.SIG_DFL)
        states[item] = handler
        return old
    class Native:
        bound_address = "[::1]:19531"
        @staticmethod
        def start(*args):
            events.append(("start", args))
            return Native()
        def stop(self, grace):
            events.append(("stop", grace))
            assert all(value == signal.SIG_DFL for value in states.values())
            if close_failure:
                raise RuntimeError("close failure")
    monkeypatch.setattr(cmdline.threading, "Event", Event)
    monkeypatch.setattr(cmdline.signal, "signal", signals)
    monkeypatch.setitem(sys.modules, "milvus_lite._native", SimpleNamespace(NativeServer=Native))
    code = cmdline.main(["server", "--data-dir", "d", "--host", "[::1]", "--port", "0", "--max-workers", "3"])
    output = capsys.readouterr()
    assert code == int(primary or close_failure)
    assert events[0][0] == "start"
    assert events[0][1][1:3] == ("::1", 0)
    assert events[0][1][-1] == 3
    assert events[-1] == ("stop", 5.0)
    assert all(value == signal.SIG_DFL for value in states.values())
    assert ("listening" in output.out) == (not start_stopped)
    if primary:
        assert "primary failure" in output.err and "shutdown also failed: close failure" in output.err
    elif close_failure:
        assert "server failed: close failure" in output.err


class Process:
    def __init__(self, path, extra=(), console=False):
        self.path = path
        self.log = (path.parent / (path.name + ".log")).open("w+")
        command = [str(Path(sys.executable).with_name("milvus-lite.exe" if os.name == "nt" else "milvus-lite"))] if console else [sys.executable, "-m", "milvus_lite"]
        self.process = subprocess.Popen([*command, "server", "--data-dir", str(path), "--host", "127.0.0.1", "--port", "0", *extra], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log, text=True, creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0)
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
            line = self.line()
            assert "listening on " in line, line
            self.address = line.split("listening on ", 1)[1].split(" ", 1)[0]
            assert not self.address.endswith(":0")
        except BaseException:
            self.close()
            raise
    def line(self):
        value = self.output.get(timeout=20)
        if isinstance(value, Exception):
            raise value
        assert value is not None, "CLI exited before printing a flushed line"
        return value
    def stop(self, sig=None):
        if sig is None:
            sig = signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGINT
        self.process.send_signal(sig)
        self.process.wait(timeout=30)
        self.reader.join(timeout=5)
        assert not self.reader.is_alive(), "CLI output reader did not finish"
        lines = []
        while not self.output.empty():
            value = self.output.get_nowait()
            if isinstance(value, Exception):
                raise value
            if value is not None:
                lines.append(value)
        self.log.seek(0)
        return self.process.returncode, "".join(lines), self.log.read()
    def close(self):
        if self.process.poll() is None:
            self.process.kill(); self.process.wait()
        self.reader.join(timeout=5)
        assert not self.reader.is_alive(), "CLI output reader did not finish"
        self.process.stdout.close()
        if self.process.stdin is not None:
            self.process.stdin.close()
        self.log.close()


def can_bind_address(family, address):
    try:
        with socket.socket(family) as probe:
            probe.bind((address, 0))
    except OSError as error:
        if error.errno not in (errno.EADDRNOTAVAIL, errno.EAFNOSUPPORT, errno.EPROTONOSUPPORT, errno.ENODEV):
            raise
        return False
    return True


def bindable_localhost_addresses():
    addresses = []
    for family, _, _, _, address in socket.getaddrinfo("localhost", 0, type=socket.SOCK_STREAM):
        item = (family, address[0])
        # hosts 中有地址不代表接口可用，特别是容器禁用 IPv6 后仍保留 ::1 的情形。
        if item not in addresses and can_bind_address(*item):
            addresses.append(item)
    assert addresses, "localhost has no bindable address"
    return addresses


def check_bind_failure(path, host, port=0):
    result = subprocess.run([sys.executable, "-m", "milvus_lite", "server", "--data-dir", str(path), "--host", host, "--port", str(port)], capture_output=True, text=True, timeout=15)
    assert result.returncode == 1 and "failed to bind" in result.stderr
    assert "listening" not in result.stdout


def check_empty_server_and_stop(server):
    # 直接使用 ready 地址，避免 SDK 的 URI 重组丢掉 IPv6 括号，以及回环请求被 HTTP 代理转发。
    with grpc.insecure_channel(server.address, options=[("grpc.enable_http_proxy", 0)]) as channel:
        grpc.channel_ready_future(channel).result(timeout=10)
        response = milvus_pb2_grpc.MilvusServiceStub(channel).ShowCollections(milvus_pb2.ShowCollectionsRequest(), timeout=5)
        assert response.status.error_code == 0 and response.status.code == 0
        assert list(response.collection_names) == []
        code, output, error = server.stop(signal.SIGTERM if os.name != "nt" else signal.CTRL_BREAK_EVENT)
        assert code == 0, error
        assert output == "\nShutting down...\n"


def test_localhost_ready_reports_the_only_bound_address(tmp_path):
    addresses = bindable_localhost_addresses()
    server = Process(tmp_path / "hostname.db", ["--host", "localhost"])
    try:
        host, port = server.address.rsplit(":", 1)
        host = host.strip("[]")
        assert host == addresses[0][1]
        # 同一 hostname 的其它解析地址没有额外 listener，不能假定 ready 代表全部地址。
        for family, address in addresses:
            if address != host:
                with socket.socket(family) as probe:
                    probe.settimeout(2)
                    assert probe.connect_ex((address, int(port))) != 0
        check_empty_server_and_stop(server)
    finally:
        server.close()


def test_localhost_tries_the_next_address_when_the_first_is_occupied(tmp_path):
    addresses = bindable_localhost_addresses()
    family, host = addresses[0]
    with socket.socket(family) as occupied:
        if family == socket.AF_INET6:
            occupied.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
        occupied.bind((host, 0))
        occupied.listen()
        port = occupied.getsockname()[1]
        if len(addresses) == 1:
            check_bind_failure(tmp_path / "occupied.db", "localhost", port)
            return
        server = Process(tmp_path / "fallback.db", ["--host", "localhost", "--port", str(port)])
        try:
            bound_host, bound_port = server.address.rsplit(":", 1)
            assert bound_host.strip("[]") in [address for _, address in addresses[1:]]
            assert int(bound_port) == port
            check_empty_server_and_stop(server)
        finally:
            server.close()


def test_bracketed_ipv6_ready_serves_and_stops(tmp_path):
    if not can_bind_address(socket.AF_INET6, "::1"):
        check_bind_failure(tmp_path / "ipv6.db", "[::1]")
        return
    server = Process(tmp_path / "ipv6.db", ["--host", "[::1]"])
    try:
        assert server.address.startswith("[::1]:")
        check_empty_server_and_stop(server)
    finally:
        server.close()


@pytest.mark.parametrize("stop_signal", [signal.SIGINT, signal.SIGTERM] if os.name != "nt" else [signal.CTRL_BREAK_EVENT])
def test_cli_signal_stops_flushes_and_reopens_with_real_client(stop_signal, tmp_path):
    path = tmp_path / "signals.db"
    server = Process(path, ["--max-workers", "1"])
    client = MilvusClient(uri=f"http://{server.address}", dedicated=True)
    try:
        # stdin EOF 本身不触发关闭，服务仍应能接受真实请求。
        server.process.stdin.close(); server.process.stdin = None
        client.create_collection("c", dimension=2, metric_type="L2")
        for pk in range(4):
            client.insert("c", [{"id": pk, "vector": [1.0, 0.0], "category": "tech"}])
            client.flush("c")
        client.upsert("c", [{"id": 0, "vector": [0.0, 1.0], "category": "ai"}])
        client.delete("c", ids=[1])
        assert [r["id"] for r in client.query("c", filter="category == 'ai'", output_fields=["id"])] == [0]
        assert client.search("c", [[0.0, 1.0]], limit=1, search_params={"metric_type": "L2"})[0][0]["id"] == 0
        client.close()
        code, output, error = server.stop(stop_signal)
        assert code == 0, error
        assert output == "\nShutting down...\n"
        reopened = MilvusClient(str(path), dedicated=True)
        try:
            reopened.load_collection("c")
            rows = list(reopened.get("c", [0, 1, 2, 3], output_fields=["category", "vector"]))
            assert [r["id"] for r in rows] == [0, 2, 3]
            assert rows[0]["category"] == "ai" and rows[0]["vector"] == [0.0, 1.0]
        finally:
            reopened.close()
            from milvus_lite.server_manager import server_manager_instance
            server_manager_instance.release_server(str(path))
    finally:
        client.close()
        server.close()


def test_storage_close_failure_makes_cli_exit_one_and_retains_wal(tmp_path):
    path = tmp_path / "failed.db"
    server = Process(path)
    client = MilvusClient(uri=f"http://{server.address}", dedicated=True)
    try:
        client.create_collection("c", dimension=2)
        client.insert("c", [{"id": 1, "vector": [1.0, 0.0]}])
        client.close()
        bad = path / "collections/c/manifest.json.tmp"
        bad.mkdir()
        code, _, error = server.stop()
        assert code == 1
        assert "storage shutdown failed" in error
        assert "close flush failed" in error
        assert list((path / "collections/c/wal").glob("*.arrow"))
        bad.rmdir()
        reopened = MilvusClient(str(path), dedicated=True)
        try:
            reopened.load_collection("c")
            assert [r["id"] for r in reopened.get("c", [1])] == [1]
        finally:
            reopened.close()
            from milvus_lite.server_manager import server_manager_instance
            server_manager_instance.release_server(str(path))
    finally:
        client.close(); server.close()


def test_cli_reports_bind_and_lock_failures_without_a_ready_line(tmp_path):
    with socket.socket() as occupied:
        occupied.bind(("127.0.0.1", 0)); occupied.listen()
        port = occupied.getsockname()[1]
        path = tmp_path / "bind.db"
        result = subprocess.run([sys.executable, "-m", "milvus_lite", "server", "--data-dir", str(path), "--host", "127.0.0.1", "--port", str(port)], capture_output=True, text=True, timeout=15)
        assert result.returncode == 1 and "failed to bind" in result.stderr
        assert "listening" not in result.stdout
    path = tmp_path / "lock.db"
    owner = Process(path)
    try:
        result = subprocess.run([sys.executable, "-m", "milvus_lite", "server", "--data-dir", str(path), "--port", "0"], capture_output=True, text=True, timeout=15)
        assert result.returncode == 1 and "lock" in result.stderr.lower()
        assert "listening" not in result.stdout
    finally:
        owner.stop(); owner.close()


@pytest.mark.parametrize("module", [False, True])
def test_installed_help_is_available_without_starting_a_server(module):
    command = [sys.executable, "-m", "milvus_lite"] if module else [str(Path(sys.executable).with_name("milvus-lite.exe" if os.name == "nt" else "milvus-lite"))]
    result = subprocess.run([*command, "--help"], capture_output=True, text=True, timeout=15)
    assert result.returncode == 0, result.stderr
    assert "server" in result.stdout and "dump" not in result.stdout
    assert "listening" not in result.stdout


def test_installed_console_script_reports_an_actual_address_and_serves_requests(tmp_path):
    server = Process(tmp_path / "console.db", console=True)
    client = MilvusClient(uri=f"http://{server.address}", dedicated=True)
    try:
        assert client.list_collections() == []
        code, output, error = server.stop()
        assert code == 0, error
        assert output == "\nShutting down...\n"
    finally:
        client.close(); server.close()
