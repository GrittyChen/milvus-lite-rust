# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_function_chain.py，Apache-2.0 许可。
# 只保留 M1 范围内的用例（逐条状态见 INVENTORY.md），并删去随之不再使用的 import、helper 与分节标题。
import uuid

import pytest

from pymilvus import DataType, MilvusClient


def _collection_name(prefix: str) -> str:
    return f"{prefix}_{uuid.uuid4().hex[:10]}"


def _create_collection(client: MilvusClient, name: str, metric: str = "IP"):
    schema = client.create_schema(
        auto_id=False,
        enable_dynamic_field=False,
    )
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    schema.add_field("popularity", DataType.FLOAT)
    schema.add_field("title", DataType.VARCHAR, max_length=128)
    schema.add_field("category", DataType.VARCHAR, max_length=32)

    index_params = client.prepare_index_params()
    index_params.add_index(
        field_name="vector",
        index_type="BRUTE_FORCE",
        metric_type=metric,
        params={},
    )
    client.create_collection(
        name,
        schema=schema,
        index_params=index_params,
    )
    client.insert(
        name,
        [
            {
                "id": 1,
                "vector": [0.8, 0.6],
                "popularity": 1.0,
                "title": "one",
                "category": "a",
            },
            {
                "id": 2,
                "vector": [1.0, 0.0],
                "popularity": 10.0,
                "title": "two",
                "category": "a",
            },
            {
                "id": 3,
                "vector": [0.0, 1.0],
                "popularity": 5.0,
                "title": "three",
                "category": "b",
            },
            {
                "id": 4,
                "vector": [-1.0, 0.0],
                "popularity": 2.0,
                "title": "four",
                "category": "c",
            },
        ],
    )
    client.load_collection(name)


def test_search_without_function_chain_preserves_legacy_behavior(milvus_client):
    name = _collection_name("function_chain_legacy")
    _create_collection(milvus_client, name)

    result = milvus_client.search(
        collection_name=name,
        data=[[1.0, 0.0]],
        anns_field="vector",
        search_params={"metric_type": "IP", "params": {}},
        limit=3,
        output_fields=["title"],
    )

    assert [hit["id"] for hit in result[0]] == [2, 1, 3]
    assert [hit["distance"] for hit in result[0]] == pytest.approx(
        [1.0, 0.8, 0.0]
    )
    assert [hit["entity"]["title"] for hit in result[0]] == [
        "two",
        "one",
        "three",
    ]
