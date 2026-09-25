# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_milvus_compat_v3.py，Apache-2.0 许可。
# 只保留 M1 范围内的用例（逐条状态见 INVENTORY.md），并删去随之不再使用的 import、helper 与分节标题。
# 改动：删去上游的 skipif（它导入了 3.x 内部模块，本文件的用例不用 HNSW）。
"""Milvus compatibility tests v3 -- edge cases extracted from
milvus-io/milvus python_client/milvus_client_v2/ test suite.

Adapted from:
  - test_milvus_client_e2e.py (end-to-end lifecycle)
  - test_milvus_client_search_pagination.py (offset pagination)
  - test_milvus_client_search_string.py (string/LIKE/IN filters)
  - test_milvus_client_search_by_pk.py (get-by-pk patterns)

Focuses on patterns NOT already covered by test_milvus_compat.py /
test_milvus_compat_v2.py:
  1. VARCHAR primary key with search filters
  2. Pagination consistency (offset pages match full search)
  3. Partition-scoped pagination
  4. JSON field query and search filters
  5. Multiple nullable scalar types in one schema
  6. String IN filter
  7. Mixed int64 + varchar comparison filter in search
  8. Search distance ordering verification (COSINE ascending)
  9. Delete-all lifecycle with nullable fields
 10. Chained range expression (e.g. 100 <= field < 200)
 11. Get with output_fields=["*"] (wildcard)
 12. Search with filter matching zero rows
 13. Upsert on VARCHAR pk
 14. Auto-ID insert and get-back
 15. Query with OR combining nullable fields
 16. Offset beyond data count returns empty

All tests use BRUTE_FORCE index + COSINE metric for deterministic,
fast results. Each test is self-contained (create + insert + index +
load + exercise + drop).

Skipped automatically when pymilvus / faiss-cpu is not installed.
"""

import numpy as np
import pytest
from pymilvus import DataType, MilvusClient

DIM = 4
NB = 30


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _rng(seed=7777):
    return np.random.default_rng(seed=seed)


def _index_and_load(client, name, field="vec", metric="COSINE"):
    """Create BRUTE_FORCE index and load."""
    idx = client.prepare_index_params()
    idx.add_index(field_name=field, index_type="BRUTE_FORCE",
                  metric_type=metric, params={})
    client.create_index(name, idx)
    client.load_collection(name)


def _simple_schema(client, dynamic=False):
    """id(INT64 PK), vec(FLOAT_VECTOR dim=4), val(FLOAT), tag(VARCHAR)."""
    schema = MilvusClient.create_schema(auto_id=False,
                                        enable_dynamic_field=dynamic)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=DIM)
    schema.add_field("val", DataType.FLOAT)
    schema.add_field("tag", DataType.VARCHAR, max_length=128)
    return schema


def _gen_simple_rows(n=NB, seed=7777):
    rng = _rng(seed)
    return [
        {
            "id": i,
            "vec": rng.standard_normal(DIM).astype(np.float32).tolist(),
            "val": float(i) * 1.0,
            "tag": f"tag_{i:03d}",
        }
        for i in range(n)
    ]


def _create_simple(client, name, rows=None, dynamic=False):
    """Create, insert, index, load. Returns rows."""
    rows = rows or _gen_simple_rows()
    client.create_collection(name, schema=_simple_schema(client, dynamic))
    client.insert(name, rows)
    _index_and_load(client, name)
    return rows


# ===========================================================================
# 2. Pagination consistency -- offset pages match full search
# ===========================================================================

class TestPaginationConsistency:
    """Adapted from test_search_float_vectors_with_pagination_default."""

    def test_paginated_search_matches_full(self, milvus_client):
        """Two pages (offset=0 + offset=5) should equal top-10."""
        rows = _create_simple(milvus_client, "pg1")

        q = _rng(99).standard_normal((1, DIM)).astype(np.float32).tolist()

        # Full search: top 10
        full_res = milvus_client.search("pg1", data=q, limit=10)
        full_ids = [h["id"] for h in full_res[0]]

        # Page 1: offset=0, limit=5
        page1 = milvus_client.search(
            "pg1", data=q, limit=5,
            search_params={"metric_type": "COSINE", "offset": 0},
        )
        page1_ids = [h["id"] for h in page1[0]]

        # Page 2: offset=5, limit=5
        page2 = milvus_client.search(
            "pg1", data=q, limit=5,
            search_params={"metric_type": "COSINE", "offset": 5},
        )
        page2_ids = [h["id"] for h in page2[0]]

        assert page1_ids + page2_ids == full_ids
        milvus_client.drop_collection("pg1")

    def test_offset_beyond_count_returns_empty(self, milvus_client):
        """Offset >= total rows -> empty results.
        Adapted from test_search_offset_beyond_count."""
        _create_simple(milvus_client, "pg2")
        q = _rng(99).standard_normal((1, DIM)).astype(np.float32).tolist()

        res = milvus_client.search(
            "pg2", data=q, limit=5,
            search_params={"metric_type": "COSINE", "offset": NB + 100},
        )
        assert len(res[0]) == 0
        milvus_client.drop_collection("pg2")

# ===========================================================================
# 8. Search distance ordering verification
# ===========================================================================

class TestDistanceOrdering:
    """Adapted from test_milvus_client_e2e search distance verification."""

    def test_cosine_scores_descending(self, milvus_client):
        """COSINE search scores must be in descending order."""
        _create_simple(milvus_client, "do1")
        q = _rng(42).standard_normal((1, DIM)).astype(np.float32).tolist()

        res = milvus_client.search("do1", data=q, limit=NB)
        distances = [h["distance"] for h in res[0]]
        assert distances == sorted(distances, reverse=True), \
            "COSINE scores should be in descending order"
        milvus_client.drop_collection("do1")

    def test_self_query_cosine_score_near_one(self, milvus_client):
        """Searching with an inserted vector should return itself at
        score ~1 (COSINE)."""
        rows = _create_simple(milvus_client, "do2")
        q = [rows[5]["vec"]]
        res = milvus_client.search("do2", data=q, limit=1)
        assert res[0][0]["id"] == 5
        assert res[0][0]["distance"] == pytest.approx(1.0, abs=1e-4)
        milvus_client.drop_collection("do2")


# ===========================================================================
# 11. Get with output_fields=["*"] (wildcard)
# ===========================================================================

class TestGetWithOutputFields:
    """Adapted from test_search_by_pk_with_output_fields_all."""

    def test_get_explicit_fields(self, milvus_client):
        """get(ids, output_fields=[...]) returns requested fields."""
        _create_simple(milvus_client, "gw1")
        result = milvus_client.get("gw1", ids=[0, 1],
                                   output_fields=["val", "tag"])
        assert len(result) == 2
        for r in result:
            assert "id" in r
            assert "val" in r
            assert "tag" in r
        milvus_client.drop_collection("gw1")


# ===========================================================================
# 13. Auto-ID insert and retrieve
# ===========================================================================

class TestAutoIdViaGrpc:
    """Adapted from test_milvus_client_search_by_pk auto_id patterns."""

    def test_auto_id_insert_returns_ids(self, milvus_client):
        """Insert with auto_id, verify returned PKs can be used to get."""
        schema = MilvusClient.create_schema(auto_id=True,
                                            enable_dynamic_field=False)
        schema.add_field("id", DataType.INT64, is_primary=True,
                         auto_id=True)
        schema.add_field("vec", DataType.FLOAT_VECTOR, dim=DIM)
        schema.add_field("label", DataType.VARCHAR, max_length=64)
        milvus_client.create_collection("ai1", schema=schema)

        rows = [
            {"vec": [float(i), 0, 0, 0], "label": f"item_{i}"}
            for i in range(10)
        ]
        res = milvus_client.insert("ai1", rows)
        assert res["insert_count"] == 10
        pks = res["ids"]
        assert len(pks) == 10

        _index_and_load(milvus_client, "ai1")

        # Get by returned PKs with explicit output_fields
        got = milvus_client.get("ai1", ids=pks[:3],
                                output_fields=["label"])
        assert len(got) == 3
        labels = sorted(r["label"] for r in got)
        assert labels == sorted([f"item_{i}" for i in range(3)])
        milvus_client.drop_collection("ai1")


# ===========================================================================
# 14. Multi-query search (nq > 1)
# ===========================================================================

class TestMultiQuerySearch:
    """Adapted from test_search_multi_query in test_milvus_compat.py
    but with BRUTE_FORCE and self-query verification."""

    def test_multi_nq_self_query(self, milvus_client):
        """Search with 3 query vectors, each should find itself."""
        rows = _create_simple(milvus_client, "mq1")

        qs = [rows[0]["vec"], rows[10]["vec"], rows[20]["vec"]]
        res = milvus_client.search("mq1", data=qs, limit=1)
        assert len(res) == 3
        assert res[0][0]["id"] == 0
        assert res[1][0]["id"] == 10
        assert res[2][0]["id"] == 20
        milvus_client.drop_collection("mq1")


# ===========================================================================
# 16. Get with missing IDs returns partial
# ===========================================================================

class TestGetPartialMiss:
    """Adapted from test_get_missing_ids_returns_empty -- extend to
    mixed existing + non-existing IDs."""

    def test_get_mixed_existing_and_missing(self, milvus_client):
        """Get with some valid IDs + some missing -> only valid returned."""
        _create_simple(milvus_client, "gp1")
        result = milvus_client.get("gp1", ids=[0, 1, 9999, 2])
        returned_ids = sorted(r["id"] for r in result)
        assert returned_ids == [0, 1, 2]
        milvus_client.drop_collection("gp1")

    def test_get_all_missing(self, milvus_client):
        """Get with all non-existing IDs -> empty result."""
        _create_simple(milvus_client, "gp2")
        result = milvus_client.get("gp2", ids=[9997, 9998, 9999])
        assert result == []
        milvus_client.drop_collection("gp2")
