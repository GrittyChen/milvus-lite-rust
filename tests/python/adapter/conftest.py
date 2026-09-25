"""本目录的用例复制自参照实现 milvus-lite v3.2.1（43d1257）的 tests/adapter/，Apache-2.0 许可。

这里替换上游 conftest.py 中的两个 fixture，用例本体不变：
- grpc_server：上游经 start_server_in_thread 在进程内起 3.x 服务，返回 (port, db)；这里经本包的
  server_manager 起服务，db 一项为 None（依赖 3.x 内部 db 对象的用例不复制，见 INVENTORY.md）。
- milvus_client 的清理：上游先 list_databases / using_database 再逐库删除，而 using_database
  发出的 DescribeDatabase 在 M1 返回 UNIMPLEMENTED，清理会被跳过；M1 只有 default 一个数据库，
  所以直接删除其中的全部 collection。
"""

import os
import shutil
import tempfile

import pytest
from pymilvus import MilvusClient

from milvus_lite.server_manager import ServerManager


@pytest.fixture(scope="session")
def grpc_server():
    tmpdir = tempfile.mkdtemp(prefix="milvus_lite_adapter_")
    manager = ServerManager()
    uri = manager.start_and_get_uri(os.path.join(tmpdir, "adapter.db"))
    assert uri is not None, "failed to start the in-process server"
    try:
        yield int(uri.rsplit(":", 1)[1]), None
    finally:
        manager.release_all()
        shutil.rmtree(tmpdir, ignore_errors=True)


@pytest.fixture
def milvus_client(grpc_server):
    port, _db = grpc_server
    client = MilvusClient(uri=f"http://127.0.0.1:{port}")
    try:
        yield client
    finally:
        for name in client.list_collections():
            client.drop_collection(name)
        client.close()
