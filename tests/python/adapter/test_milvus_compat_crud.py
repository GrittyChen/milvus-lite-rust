# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_milvus_compat_crud.py，Apache-2.0 许可。
# 仅保留 INVENTORY.md 中已复用的用例；S1 表达式及断言保持上游原文。
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

import numpy as np
import pytest

from pymilvus import DataType, MilvusClient


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _schema(dim=128, auto_id=False, enable_dynamic=False):
    schema = MilvusClient.create_schema(auto_id=auto_id,
                                        enable_dynamic_field=enable_dynamic)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=dim)
    schema.add_field("title", DataType.VARCHAR, max_length=256, nullable=True)
    schema.add_field("score", DataType.FLOAT, nullable=True)
    schema.add_field("active", DataType.BOOL, nullable=True)
    return schema


def _schema_string_pk(dim=128):
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.VARCHAR, is_primary=True, max_length=128)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=dim)
    return schema


DIM = 16
NB = 200  # keep small for test speed
rng = np.random.default_rng(seed=42)


def _gen_rows(nb=NB, dim=DIM, id_offset=0):
    vecs = rng.random((nb, dim)).astype(np.float32)
    return [
        {"id": id_offset + i, "vec": vecs[i].tolist(),
         "title": f"doc_{id_offset + i}", "score": float(i) / nb,
         "active": i % 2 == 0}
        for i in range(nb)
    ]


def _create_and_load(client, name, dim=DIM, rows=None, **schema_kw):
    schema = _schema(dim=dim, **schema_kw)
    idx = client.prepare_index_params()
    idx.add_index(field_name="vec", index_type="FLAT", metric_type="COSINE")
    client.create_collection(name, schema=schema, index_params=idx)
    if rows is None:
        rows = _gen_rows(dim=dim)
    if rows:
        client.insert(name, rows)
    client.load_collection(name)
    return rows


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
        res = milvus_client.query("ins_strpk", filter='id == "hello"')
        assert len(res) == 1


# ===========================================================================
# Upsert
# ===========================================================================



# ===========================================================================
# Delete
# ===========================================================================



# ===========================================================================
# Query
# ===========================================================================

class TestQuery:
















    def test_query_not_exist_collection_raises(self, milvus_client):
        with pytest.raises(Exception):
            milvus_client.query("nonexistent_col_xyz", filter="id >= 0",
                                limit=10)


# ===========================================================================
# Get
# ===========================================================================



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


# ===========================================================================
# Partition
# ===========================================================================



# ===========================================================================
# Default value
# ===========================================================================



# ===========================================================================
# Nullable
# ===========================================================================
