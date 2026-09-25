# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_milvus_compat_crud.py，Apache-2.0 许可。
# 只保留 M1 范围内的用例（逐条状态见 INVENTORY.md），并删去随之不再使用的 import、helper 与分节标题。
# 改动：`<pk> == N` 换成 `<pk> in [N]`（M1 只识别 `<主键> in [...]`）。
"""Milvus compatibility test suite — CRUD operations.

Migrated from milvus/tests/python_client/milvus_client/ test files:
- test_milvus_client_insert.py
- test_milvus_client_upsert.py
- test_milvus_client_delete.py
- test_milvus_client_query.py
- test_milvus_client_search.py

Covers core insert/upsert/delete/query/search/get round-trips that
any pymilvus-compatible backend must pass.
"""

import pytest

from pymilvus import DataType, MilvusClient


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _schema_string_pk(dim=128):
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.VARCHAR, is_primary=True, max_length=128)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=dim)
    return schema


DIM = 16


# ===========================================================================
# Insert
# ===========================================================================

class TestInsert:
    def test_insert_string_pk(self, milvus_client):
        schema = _schema_string_pk(dim=DIM)
        idx = milvus_client.prepare_index_params()
        idx.add_index(field_name="vec", index_type="FLAT", metric_type="COSINE")
        milvus_client.create_collection("ins_strpk", schema=schema, index_params=idx)
        milvus_client.insert("ins_strpk", [
            {"id": "hello", "vec": [0.1] * DIM},
            {"id": "world", "vec": [0.2] * DIM},
        ])
        milvus_client.load_collection("ins_strpk")
        # 上游为 filter='id == "hello"'：M1 只识别 <pk> in [...]，单个主键时二者结果相同
        res = milvus_client.query("ins_strpk", filter='id in ["hello"]')
        assert len(res) == 1


# ===========================================================================
# Search
# ===========================================================================

class TestSearch:
    def test_search_string_pk(self, milvus_client):
        schema = _schema_string_pk(dim=DIM)
        idx = milvus_client.prepare_index_params()
        idx.add_index(field_name="vec", index_type="FLAT", metric_type="COSINE")
        milvus_client.create_collection("s_strpk", schema=schema, index_params=idx)
        milvus_client.insert("s_strpk", [
            {"id": "alpha", "vec": [1.0] + [0.0] * (DIM - 1)},
            {"id": "beta", "vec": [0.0, 1.0] + [0.0] * (DIM - 2)},
        ])
        milvus_client.load_collection("s_strpk")
        res = milvus_client.search("s_strpk",
                                   data=[[1.0] + [0.0] * (DIM - 1)], limit=1)
        assert res[0][0]["id"] == "alpha"

    def test_search_nonexistent_collection_raises(self, milvus_client):
        with pytest.raises(Exception):
            milvus_client.search("ghost_col", data=[[0.1] * DIM], limit=5)
