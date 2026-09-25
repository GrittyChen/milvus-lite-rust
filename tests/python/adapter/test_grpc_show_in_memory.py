# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_grpc_show_in_memory.py，Apache-2.0 许可。
# 只保留 M1 范围内的用例（逐条状态见 INVENTORY.md），并删去随之不再使用的 import、helper 与分节标题。
"""Compatibility tests for ShowCollections/ShowPartitions InMemory responses."""

import grpc
import pytest
from pymilvus import DataType, MilvusClient
from pymilvus.grpc_gen import milvus_pb2, milvus_pb2_grpc


def _make_schema():
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=4)
    return schema


def _make_index_params(client):
    params = client.prepare_index_params()
    params.add_index(
        field_name="vec",
        index_type="BRUTE_FORCE",
        metric_type="L2",
        params={},
    )
    return params


@pytest.fixture
def raw_stub(grpc_server):
    port, _db = grpc_server
    channel = grpc.insecure_channel(f"127.0.0.1:{port}")
    try:
        yield milvus_pb2_grpc.MilvusServiceStub(channel)
    finally:
        channel.close()


def test_show_collections_in_memory_reports_loaded_percentage(
    milvus_client, raw_stub
):
    milvus_client.create_collection("demo", schema=_make_schema())

    response = raw_stub.ShowCollections(
        milvus_pb2.ShowCollectionsRequest(
            type=milvus_pb2.ShowType.InMemory,
            collection_names=["demo"],
        )
    )

    assert list(response.collection_names) == ["demo"]
    assert list(response.inMemory_percentages) == [100]
    assert list(response.query_service_available) == [True]
    assert list(response.shards_num) == [1]


def test_show_collections_in_memory_reports_released_percentage(
    milvus_client, raw_stub
):
    milvus_client.create_collection("demo", schema=_make_schema())
    milvus_client.create_index("demo", _make_index_params(milvus_client))
    milvus_client.release_collection("demo")

    response = raw_stub.ShowCollections(
        milvus_pb2.ShowCollectionsRequest(
            type=milvus_pb2.ShowType.InMemory,
            collection_names=["demo"],
        )
    )

    assert list(response.collection_names) == ["demo"]
    assert list(response.inMemory_percentages) == [0]
    assert list(response.query_service_available) == [False]
    assert list(response.shards_num) == [1]


def test_show_collections_in_memory_filters_requested_names(
    milvus_client, raw_stub
):
    milvus_client.create_collection("alpha", schema=_make_schema())
    milvus_client.create_collection("beta", schema=_make_schema())

    response = raw_stub.ShowCollections(
        milvus_pb2.ShowCollectionsRequest(
            type=milvus_pb2.ShowType.InMemory,
            collection_names=["beta"],
        )
    )

    assert list(response.collection_names) == ["beta"]
    assert list(response.inMemory_percentages) == [100]
