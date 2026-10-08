"""差分测试：同一串操作分别发给 3.x 与本实现，逐步比较（0004「验收」第 1 项）。

3.x 与本包占用同一个 milvus_lite 模块，不能装在一起：3.x 装在另一个 venv 里，由环境变量
MILVUS_LITE_3X_PYTHON 指定它的 python，经 server3x.py 在独立进程中起服务。没有指定时直接报错，不跳过。
"""

import os
from contextlib import ExitStack
import shutil
import subprocess
import sys
import tempfile

import pytest
from pymilvus import MilvusClient

from compare import Diff
from milvus_lite.server_manager import ServerManager

ENV = "MILVUS_LITE_3X_PYTHON"
# 对照版本（AGENTS.md「参照实现」），升级时同步修改。
REF_VERSION = "3.2.1"


def pytest_configure(config):
    if not os.environ.get(ENV):
        raise pytest.UsageError(f"{ENV} must point to the python of a venv with milvus-lite 3.x installed")


@pytest.fixture(scope="session")
def workdir():
    path = tempfile.mkdtemp(prefix="milvus_lite_diff_")
    yield path
    shutil.rmtree(path, ignore_errors=True)


@pytest.fixture(scope="session")
def ref_uri(workdir):
    log = open(os.path.join(workdir, "server3x.log"), "wb")

    def dump_log():
        log.close()
        with open(log.name, encoding="utf-8", errors="replace") as f:
            sys.stderr.write(f.read())

    proc = subprocess.Popen(
        [os.environ[ENV], os.path.join(os.path.dirname(__file__), "server3x.py"), os.path.join(workdir, "3x")],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=log,
    )
    line = proc.stdout.readline().decode().split()
    if not line:
        proc.wait()
        dump_log()
        raise RuntimeError(f"3.x server exited with {proc.returncode}")
    port, version = line
    if version != REF_VERSION:
        proc.kill()
        log.close()
        raise RuntimeError(f"{ENV} has milvus-lite {version}, expected {REF_VERSION}")
    try:
        yield f"http://127.0.0.1:{int(port)}"
    finally:
        proc.stdin.close()
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        # 3.x 中途退出时之后的用例都会连不上；这里输出它的日志，并把退出码作为失败报出。
        if proc.returncode != 0:
            dump_log()
        log.close()
        assert proc.returncode == 0, f"3.x server exited with {proc.returncode}"


@pytest.fixture(scope="session")
def rust_uri(workdir):
    manager = ServerManager()
    uri = manager.start_and_get_uri(os.path.join(workdir, "rust.db"))
    assert uri is not None, "failed to start the in-process server"
    yield uri
    manager.release_all()


@pytest.fixture
def diff(rust_uri, ref_uri):
    # 默认共享池在 close 后仍保留闲置连接；跨重开用例的 keepalive 会触发
    # 3.x 的 Too many pings。fixture 自己拥有连接，清理时立即释放。
    with ExitStack() as clients:
        rust = MilvusClient(uri=rust_uri, dedicated=True)
        clients.callback(rust.close)
        ref = MilvusClient(uri=ref_uri, dedicated=True)
        clients.callback(ref.close)
        try:
            yield Diff(rust, ref)
        finally:
            for client in (rust, ref):
                for name in client.list_collections():
                    client.drop_collection(name)
