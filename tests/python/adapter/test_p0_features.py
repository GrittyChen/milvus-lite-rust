# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_p0_features.py，Apache-2.0 许可。
# 只保留 M1 范围内的用例（逐条状态见 INVENTORY.md），并删去随之不再使用的 import、helper 与分节标题。
"""P0 compatibility features: aliases, truncate, indexes, and statistics."""

from pymilvus import DataType, MilvusClient


def _create_basic_collection(client: MilvusClient, name: str) -> None:
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=4)
    schema.add_field("tag", DataType.VARCHAR, max_length=32)
    client.create_collection(name, schema=schema)


def test_alias_lifecycle_and_alias_reads(milvus_client):
    _create_basic_collection(milvus_client, "alias_a")
    _create_basic_collection(milvus_client, "alias_b")
    milvus_client.insert("alias_a", [
        {"id": 1, "vec": [1.0, 0.0, 0.0, 0.0], "tag": "a"},
    ])
    milvus_client.insert("alias_b", [
        {"id": 2, "vec": [0.0, 1.0, 0.0, 0.0], "tag": "b"},
    ])

    milvus_client.create_alias("alias_a", "current_alias")
    info = milvus_client.describe_alias("current_alias")
    assert info["collection_name"] == "alias_a"

    rows = milvus_client.query(
        "current_alias", filter="id == 1", output_fields=["tag"]
    )
    assert rows == [{"id": 1, "tag": "a"}]

    listed = milvus_client.list_aliases("alias_a")
    assert "current_alias" in listed["aliases"]

    milvus_client.alter_alias("alias_b", "current_alias")
    assert milvus_client.describe_alias("current_alias")["collection_name"] == "alias_b"
    rows = milvus_client.query(
        "current_alias", filter="id == 2", output_fields=["tag"]
    )
    assert rows == [{"id": 2, "tag": "b"}]

    milvus_client.drop_alias("current_alias")
    assert "current_alias" not in milvus_client.list_aliases()["aliases"]


def test_list_indexes_returns_index_names(milvus_client):
    _create_basic_collection(milvus_client, "idx_names")
    idx = milvus_client.prepare_index_params()
    idx.add_index(
        field_name="vec",
        index_type="BRUTE_FORCE",
        metric_type="COSINE",
        params={},
    )
    milvus_client.create_index("idx_names", idx)

    assert milvus_client.list_indexes("idx_names") == ["vec"]
