# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_milvus_compat_search.py，Apache-2.0 许可。
# 仅保留 INVENTORY.md 中已复用的用例；S1 表达式及断言保持上游原文。
"""Milvus compatibility test suite — search features.

Migrated from milvus/tests/python_client/milvus_client_v2/ test files:
- test_milvus_client_search_json.py (JSON filter in search)
- test_milvus_client_search_pagination.py (search pagination)
- test_milvus_client_search_text_match.py (text_match filter)
- test_milvus_client_partial_update.py (partial upsert via gRPC)
"""

import numpy as np
import pytest

from pymilvus import DataType, MilvusClient


DIM = 16
rng = np.random.default_rng(seed=42)


# ===========================================================================
# JSON filter in search
# ===========================================================================



# ===========================================================================
# Search pagination
# ===========================================================================

class TestSearchPagination:
    @pytest.fixture(autouse=True)
    def _setup(self, milvus_client):
        self.client = milvus_client
        schema = MilvusClient.create_schema(auto_id=False)
        schema.add_field("id", DataType.INT64, is_primary=True)
        schema.add_field("vec", DataType.FLOAT_VECTOR, dim=DIM)

        idx = milvus_client.prepare_index_params()
        idx.add_index(field_name="vec", index_type="FLAT", metric_type="COSINE")
        milvus_client.create_collection("pag_s", schema=schema, index_params=idx)

        vecs = rng.random((200, DIM)).astype(np.float32)
        rows = [{"id": i, "vec": vecs[i].tolist()} for i in range(200)]
        milvus_client.insert("pag_s", rows)
        milvus_client.load_collection("pag_s")
        self.query_vec = vecs[0].tolist()

    def test_paginated_search_matches_full(self):
        """Page 1 + Page 2 should equal top-20 full search."""
        full = self.client.search("pag_s", data=[self.query_vec], limit=20)
        p1 = self.client.search("pag_s", data=[self.query_vec], limit=10,
                                search_params={"offset": 0})
        p2 = self.client.search("pag_s", data=[self.query_vec], limit=10,
                                search_params={"offset": 10})
        full_ids = [h["id"] for h in full[0]]
        paged_ids = [h["id"] for h in p1[0]] + [h["id"] for h in p2[0]]
        assert full_ids == paged_ids

    def test_search_offset_beyond_total(self):
        res = self.client.search("pag_s", data=[self.query_vec], limit=10,
                                 search_params={"offset": 9999})
        assert len(res[0]) == 0

    def test_search_pagination_with_filter(self):
        full = self.client.search("pag_s", data=[self.query_vec], limit=20,
                                  filter="id >= 100")
        p1 = self.client.search("pag_s", data=[self.query_vec], limit=10,
                                filter="id >= 100",
                                search_params={"offset": 0})
        p2 = self.client.search("pag_s", data=[self.query_vec], limit=10,
                                filter="id >= 100",
                                search_params={"offset": 10})
        full_ids = [h["id"] for h in full[0]]
        paged_ids = [h["id"] for h in p1[0]] + [h["id"] for h in p2[0]]
        assert full_ids == paged_ids



# ===========================================================================
# Text match filter in search
# ===========================================================================


class TestSearchTextMatch:
    @pytest.fixture(autouse=True)
    def _setup(self, milvus_client):
        self.client = milvus_client
        schema = MilvusClient.create_schema(auto_id=False)
        schema.add_field("id", DataType.INT64, is_primary=True)
        schema.add_field("vec", DataType.FLOAT_VECTOR, dim=DIM)
        schema.add_field("text", DataType.VARCHAR, max_length=1024,
                         enable_analyzer=True, enable_match=True,
                         analyzer_params={"tokenizer": "standard"})

        idx = milvus_client.prepare_index_params()
        idx.add_index(field_name="vec", index_type="FLAT", metric_type="COSINE")
        milvus_client.create_collection("tm_s", schema=schema, index_params=idx)
        milvus_client.insert("tm_s", [
            {"id": 1, "vec": [1.0] + [0.0] * (DIM - 1),
             "text": "python programming language"},
            {"id": 2, "vec": [0.0, 1.0] + [0.0] * (DIM - 2),
             "text": "java programming language"},
            {"id": 3, "vec": [0.0, 0.0, 1.0] + [0.0] * (DIM - 3),
             "text": "machine learning algorithms"},
            {"id": 4, "vec": [0.0] * (DIM - 1) + [1.0],
             "text": "deep learning neural networks"},
        ])
        milvus_client.load_collection("tm_s")

    def test_search_with_text_match_single_token(self):
        q = [[1.0] + [0.0] * (DIM - 1)]
        res = self.client.search("tm_s", data=q, limit=10,
                                 filter='text_match(text, "python")',
                                 output_fields=["text"])
        assert len(res[0]) == 1
        assert res[0][0]["id"] == 1

    def test_search_with_text_match_multi_token(self):
        q = [[1.0] + [0.0] * (DIM - 1)]
        res = self.client.search("tm_s", data=q, limit=10,
                                 filter='text_match(text, "programming")',
                                 output_fields=["text"])
        ids = {h["id"] for h in res[0]}
        assert ids == {1, 2}

    def test_query_with_text_match(self):
        res = self.client.query("tm_s",
                                filter='text_match(text, "learning")',
                                output_fields=["text"], limit=10)
        ids = {r["id"] for r in res}
        assert ids == {3, 4}

    def test_search_text_match_combined_with_scalar(self):
        q = [[1.0] + [0.0] * (DIM - 1)]
        res = self.client.search("tm_s", data=q, limit=10,
                                 filter='text_match(text, "learning") and id >= 4',
                                 output_fields=["text"])
        assert len(res[0]) == 1
        assert res[0][0]["id"] == 4


# ===========================================================================
# Partial update via gRPC upsert
# ===========================================================================
