# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_milvus_compat.py，Apache-2.0 许可。
# 只保留 M1 范围内的用例（逐条状态见 INVENTORY.md），并删去随之不再使用的 import、helper 与分节标题。
# 改动：HNSW 换成 FLAT（M1 只有 FLAT，用例少于 1 万行时 3.x 不会 flush，结果不变）。
# 改动：删去上游因 HNSW 需要 faiss 而加的 skipif（它导入了 3.x 内部模块）。
"""Milvus pymilvus compatibility tests — adapted from milvus-io/milvus
python_client/milvus_client/ test suite.

Covers the "can MilvusLite pass the same test as a real Milvus?" surface:
- Full CRUD lifecycle (insert → search → query → get → delete)
- All supported scalar types (INT8/16/32/64, FLOAT, DOUBLE, VARCHAR, BOOL)
- VARCHAR primary key
- Filter expression coverage (cmp / AND / OR / NOT / IN / LIKE / IS NULL)
- Upsert semantics (overwrite existing pk, insert new pk)
- Multi-query search
- Output fields projection
- Partition insert + partition-scoped search / query
- Delete by IDs + delete by filter expression
- Index lifecycle (create → load → search → release → drop → re-create)
- Dynamic field ($meta) access

Test style: each test is self-contained (create → insert → exercise
→ drop) so failures are isolated. No shared state across tests.

Skipped automatically when pymilvus / faiss-cpu is not installed.
"""

import numpy as np
import pytest
from pymilvus import DataType, MilvusClient

DIM = 8
NB = 200  # small but enough to exercise filters meaningfully


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _rng():
    return np.random.default_rng(seed=19530)


def _gen_rows(n=NB, dim=DIM, rng=None):
    """Generate n rows with id, vec, float_field, varchar, bool fields."""
    rng = rng or _rng()
    return [
        {
            "id": i,
            "vec": rng.standard_normal(dim).astype(np.float32).tolist(),
            "float_field": float(i) * 1.0,
            "varchar": f"str_{i:04d}",
            "bool_field": (i % 2 == 0),
        }
        for i in range(n)
    ]


def _schema(client, dynamic=False):
    schema = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=dynamic)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=DIM)
    schema.add_field("float_field", DataType.FLOAT)
    schema.add_field("varchar", DataType.VARCHAR, max_length=128)
    schema.add_field("bool_field", DataType.BOOL)
    return schema


def _create_loaded(client, name, rows=None, metric="COSINE", dynamic=False):
    """Helper: create collection, insert, create index, load. Returns rows."""
    rows = rows or _gen_rows()
    client.create_collection(name, schema=_schema(client, dynamic=dynamic))
    client.insert(name, rows)
    idx = client.prepare_index_params()
    # 上游为 HNSW（M=16、efConstruction=200）。M1 只有 FLAT；用例都少于 1 万行，3.x 不会
    # flush，一直在 memtable 上暴力检索，换成 FLAT 不改变结果。上游因 HNSW 需要 faiss 而加的
    # skipif 随之去掉。
    idx.add_index(field_name="vec", index_type="FLAT", metric_type=metric,
                  params={})
    client.create_index(name, idx)
    client.load_collection(name)
    return rows


# ===========================================================================
# 1. INSERT — basic, type coverage, varchar pk
# ===========================================================================

class TestInsert:
    def test_insert_string_pk(self, milvus_client):
        schema = MilvusClient.create_schema(auto_id=False)
        schema.add_field("pk", DataType.VARCHAR, is_primary=True, max_length=64)
        schema.add_field("vec", DataType.FLOAT_VECTOR, dim=4)
        milvus_client.create_collection("strpk", schema=schema)
        rows = [{"pk": f"doc_{i}", "vec": [float(i)] * 4} for i in range(10)]
        res = milvus_client.insert("strpk", rows)
        assert res["insert_count"] == 10
        milvus_client.drop_collection("strpk")

# ===========================================================================
# 2. SEARCH — basic, filters, metrics, multi-query, output_fields
# ===========================================================================

class TestSearch:
    def test_search_basic(self, milvus_client):
        rows = _create_loaded(milvus_client, "srch1")
        q = _rng().standard_normal((1, DIM)).astype(np.float32).tolist()
        res = milvus_client.search("srch1", data=q, limit=10)
        assert len(res) == 1
        assert len(res[0]) == 10
        milvus_client.drop_collection("srch1")

    def test_search_self_query_l2(self, milvus_client):
        rows = _create_loaded(milvus_client, "srch_l2", metric="L2")
        # Query with the exact vector of id=50
        q = [rows[50]["vec"]]
        res = milvus_client.search("srch_l2", data=q, limit=1)
        assert res[0][0]["id"] == 50
        assert res[0][0]["distance"] < 1e-3
        milvus_client.drop_collection("srch_l2")

    @pytest.mark.parametrize("metric", ["COSINE", "L2", "IP"])
    def test_search_metric_types(self, milvus_client, metric):
        _create_loaded(milvus_client, f"met_{metric}", metric=metric)
        q = _rng().standard_normal((1, DIM)).astype(np.float32).tolist()
        res = milvus_client.search(f"met_{metric}", data=q, limit=5)
        assert len(res[0]) == 5
        milvus_client.drop_collection(f"met_{metric}")

    def test_search_multi_query(self, milvus_client):
        rows = _create_loaded(milvus_client, "multi_q", metric="L2")
        qs = [rows[0]["vec"], rows[99]["vec"], rows[199]["vec"]]
        res = milvus_client.search("multi_q", data=qs, limit=3)
        assert len(res) == 3
        assert res[0][0]["id"] == 0
        assert res[1][0]["id"] == 99
        assert res[2][0]["id"] == 199
        milvus_client.drop_collection("multi_q")

    def test_search_with_output_fields(self, milvus_client):
        _create_loaded(milvus_client, "out_f")
        q = _rng().standard_normal((1, DIM)).astype(np.float32).tolist()
        res = milvus_client.search(
            "out_f", data=q, limit=5,
            output_fields=["varchar", "float_field"],
        )
        for hit in res[0]:
            assert "varchar" in hit["entity"]
            assert "float_field" in hit["entity"]
        milvus_client.drop_collection("out_f")

    def test_search_results_sorted_by_cosine_score_descending(self, milvus_client):
        _create_loaded(milvus_client, "sorted")
        q = _rng().standard_normal((1, DIM)).astype(np.float32).tolist()
        res = milvus_client.search("sorted", data=q, limit=20)
        distances = [hit["distance"] for hit in res[0]]
        assert distances == sorted(distances, reverse=True)
        milvus_client.drop_collection("sorted")


# ===========================================================================
# 3. QUERY — filter expressions, output_fields, limit, get by id
# ===========================================================================

class TestQuery:
    def test_query_id_in_list(self, milvus_client):
        _create_loaded(milvus_client, "qid")
        rows = milvus_client.query(
            "qid", filter="id in [5, 10, 15]",
            output_fields=["id", "varchar"],
        )
        ids = sorted(r["id"] for r in rows)
        assert ids == [5, 10, 15]
        milvus_client.drop_collection("qid")

    def test_get_by_ids(self, milvus_client):
        _create_loaded(milvus_client, "gid")
        rows = milvus_client.get("gid", ids=[0, 50, 199])
        assert sorted(r["id"] for r in rows) == [0, 50, 199]
        milvus_client.drop_collection("gid")

    def test_get_missing_ids_returns_empty(self, milvus_client):
        _create_loaded(milvus_client, "gm")
        rows = milvus_client.get("gm", ids=[9999])
        assert rows == []
        milvus_client.drop_collection("gm")

# ===========================================================================
# 10. COLLECTION LIFECYCLE — comprehensive
# ===========================================================================

class TestCollectionLifecycle:
    def test_create_describe_drop(self, milvus_client):
        milvus_client.create_collection("cdl", schema=_schema(milvus_client))
        desc = milvus_client.describe_collection("cdl")
        field_names = sorted(f["name"] for f in desc["fields"])
        assert field_names == sorted(["id", "vec", "float_field", "varchar", "bool_field"])
        milvus_client.drop_collection("cdl")
        assert not milvus_client.has_collection("cdl")

    def test_create_duplicate_raises(self, milvus_client):
        milvus_client.create_collection("dup", schema=_schema(milvus_client))
        with pytest.raises(Exception):
            milvus_client.create_collection("dup", schema=_schema(milvus_client))
        milvus_client.drop_collection("dup")

    def test_list_multiple_collections(self, milvus_client):
        for name in ["c_alpha", "c_beta", "c_gamma"]:
            milvus_client.create_collection(name, schema=_schema(milvus_client))
        names = milvus_client.list_collections()
        assert set(names) >= {"c_alpha", "c_beta", "c_gamma"}
        for name in ["c_alpha", "c_beta", "c_gamma"]:
            milvus_client.drop_collection(name)
