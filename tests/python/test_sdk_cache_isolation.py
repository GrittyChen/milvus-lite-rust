"""用例边界上复用固定端口时，SDK 必须按新数据库的 schema 编码。"""

import sys

import pytest
from pymilvus import DataType, MilvusClient

from milvus_lite import __version__
from milvus_lite._native import NativeServer


def schema(client, nullable):
    result = client.create_schema(auto_id=False, enable_dynamic_field=False)
    result.add_field("id", DataType.INT64, is_primary=True)
    result.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    result.add_field("n", DataType.INT64, nullable=nullable)
    return result


@pytest.fixture(scope="module")
def retired_endpoint(tmp_path_factory):
    # 模拟此前已经结束的用例；模块级准备发生在函数级隔离之前。
    cache = sys.modules.get("pymilvus.client.cache")
    if cache is not None:
        cache.GlobalCache._reset_for_testing()
    root = tmp_path_factory.mktemp("sdk-cache")
    server = NativeServer.start(str(root / "old"), "127.0.0.1", 0, __version__)
    port = server.port
    client = None
    try:
        client = MilvusClient(f"http://127.0.0.1:{port}", dedicated=True)
        client.create_collection("c", schema=schema(client, False))
        assert client.insert("c", [{"id": 0, "vector": [0.0, 0.0], "n": 0}])["insert_count"] == 1
    finally:
        if client is not None:
            client.close()
        server.stop(2.0)
    return root, port


def test_fresh_databases_reusing_endpoint_have_isolated_sdk_schema(retired_endpoint):
    root, port = retired_endpoint
    server = NativeServer.start(str(root / "new"), "127.0.0.1", port, __version__)
    client = None
    try:
        assert server.port == port
        client = MilvusClient(f"http://127.0.0.1:{port}", dedicated=True)
        client.create_collection("c", schema=schema(client, True))
        field = next(f for f in client.describe_collection("c")["fields"] if f["name"] == "n")
        assert field["nullable"]
        rows = [
            {"id": i, "vector": [float(i), 0.0], "n": None if i % 2 == 0 else i}
            for i in range(16)
        ]
        assert client.insert("c", rows)["insert_count"] == 16
        client.load_collection("c")
        assert list(client.get("c", list(range(16)), output_fields=["n"])) == [
            {"id": row["id"], "n": row["n"]} for row in rows
        ]
    finally:
        if client is not None:
            client.close()
        server.stop(2.0)
