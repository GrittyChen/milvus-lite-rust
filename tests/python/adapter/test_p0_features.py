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
