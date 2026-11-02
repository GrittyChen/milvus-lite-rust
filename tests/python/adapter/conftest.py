"""本目录的用例复制自参照实现 milvus-lite v3.2.1（43d1257）的 tests/adapter/，Apache-2.0 许可。

这里替换上游 conftest.py 中的 grpc_server fixture，用例本体不变：
- grpc_server：上游经 start_server_in_thread 在进程内起 3.x 服务，返回 (port, db)；这里经本包的
  server_manager 起服务，db 一项为 None（依赖 3.x 内部 db 对象的用例不复制，见 INVENTORY.md）。
- milvus_client 的清理与上游一致：先清空每个数据库内的 collection，再删除全部非默认数据库。
"""

import os
import shutil
import tempfile

import pytest
from pymilvus import MilvusClient

from milvus_lite.server_manager import ServerManager


def _cleanup_all_databases(client):
    try:
        database_names = client.list_databases()
    except Exception:
        database_names = ["default"]

    for database_name in database_names:
        try:
            client.using_database(database_name)
        except Exception:
            continue
        try:
            collection_names = client.list_collections()
        except Exception:
            collection_names = []
        for collection_name in collection_names:
            try:
                client.drop_collection(collection_name)
            except Exception:
                pass

    try:
        client.using_database("default")
    except Exception:
        pass

    for database_name in database_names:
        if database_name != "default":
            try:
                client.drop_database(database_name)
            except Exception:
                pass


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
        _cleanup_all_databases(client)
        client.close()
