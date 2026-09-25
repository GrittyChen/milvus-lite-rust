# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_milvus_compat_search.py，Apache-2.0 许可。
# 只保留 M1 范围内的用例（逐条状态见 INVENTORY.md），并删去随之不再使用的 import、helper 与分节标题。
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
