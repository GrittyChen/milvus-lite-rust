# 复制自 milvus-lite 1320d26 的 tests/adapter/test_milvus_compat_v2.py（与 v3.2.1 相同），Apache-2.0 许可。
# 仅保留 INVENTORY.md 中已复用的用例（算术过滤，0032）；用例本体与 helper 保持上游原文。
# 直接导入依赖；检索按 FLAT 执行、不需要 faiss，移除上游的 faiss 跳过条件。
"""Milvus compatibility tests v2 — advanced filter expressions and
nullable field handling.

Adapted from milvus-io/milvus python_client/milvus_client_v2/
test_milvus_client_e2e.py and test_milvus_client_search_v2.py.

Focuses on NEW patterns not covered by test_milvus_compat.py:
  - IS NULL / IS NOT NULL filters
  - Nullable field insert + query + search
  - Arithmetic in filter expressions (field * 2 > 1.0)
  - Complex compound expressions (3+ conditions with NULL checks)
  - NOT operator combinations
  - LIKE with suffix and inner match
  - Dynamic field ($meta) access in search filters
  - Mixed metric types across separate searches
  - Large batch insert + search
  - Delete-then-verify lifecycle

All tests go through pymilvus → gRPC → MilvusLite engine.
"""

import numpy as np
from pymilvus import DataType, MilvusClient

DIM = 8
NB = 100


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _rng(seed=42):
    return np.random.default_rng(seed=seed)


def _nullable_schema(client):
    """Schema with multiple nullable scalar fields."""
    schema = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=DIM)
    schema.add_field("int_val", DataType.INT64)
    schema.add_field("float_val", DataType.FLOAT, nullable=True)
    schema.add_field("title", DataType.VARCHAR, max_length=128, nullable=True)
    schema.add_field("active", DataType.BOOL)
    schema.add_field("score", DataType.DOUBLE)
    return schema


def _gen_nullable_rows(n=NB):
    rng = _rng()
    rows = []
    for i in range(n):
        rows.append({
            "id": i,
            "vec": rng.standard_normal(DIM).astype(np.float32).tolist(),
            "int_val": i,
            "float_val": float(i) / 10 if i % 3 != 0 else None,   # ~33% null
            "title": f"doc_{i:04d}" if i % 4 != 0 else None,       # ~25% null
            "active": (i % 2 == 0),
            "score": float(i) * 0.01,
        })
    return rows


def _create_loaded_nullable(client, name, rows=None, metric="COSINE"):
    rows = rows or _gen_nullable_rows()
    client.create_collection(name, schema=_nullable_schema(client))
    client.insert(name, rows)
    idx = client.prepare_index_params()
    idx.add_index(field_name="vec", index_type="HNSW", metric_type=metric,
                  params={"M": 16, "efConstruction": 200})
    client.create_index(name, idx)
    client.load_collection(name)
    return rows

# ===========================================================================
# 3. Arithmetic in filter expressions
# ===========================================================================

class TestArithmeticFilters:
    def test_multiply_in_filter(self, milvus_client):
        rows = _create_loaded_nullable(milvus_client, "ar1")
        result = milvus_client.query(
            "ar1",
            filter="score * 100 > 50",
            output_fields=["id", "score"],
        )
        for r in result:
            assert r["score"] * 100 > 50
        expected = sum(1 for r in rows if r["score"] * 100 > 50)
        assert len(result) == expected
        milvus_client.drop_collection("ar1")

    def test_add_in_filter(self, milvus_client):
        rows = _create_loaded_nullable(milvus_client, "ar2")
        result = milvus_client.query(
            "ar2",
            filter="int_val + 10 > 50",
            output_fields=["id", "int_val"],
        )
        for r in result:
            assert r["int_val"] + 10 > 50
        expected = sum(1 for r in rows if r["int_val"] + 10 > 50)
        assert len(result) == expected
        milvus_client.drop_collection("ar2")

    def test_divide_in_filter(self, milvus_client):
        rows = _create_loaded_nullable(milvus_client, "ar3")
        result = milvus_client.query(
            "ar3",
            filter="int_val / 10 >= 5",
            output_fields=["id", "int_val"],
        )
        for r in result:
            assert r["int_val"] / 10 >= 5
        expected = sum(1 for r in rows if r["int_val"] / 10 >= 5)
        assert len(result) == expected
        milvus_client.drop_collection("ar3")

    def test_arithmetic_in_search_filter(self, milvus_client):
        rows = _create_loaded_nullable(milvus_client, "ar4")
        q = _rng().standard_normal((1, DIM)).astype(np.float32).tolist()
        res = milvus_client.search(
            "ar4", data=q, limit=50,
            filter="score * 100 > 80",
            output_fields=["score"],
        )
        for hit in res[0]:
            assert hit["entity"]["score"] * 100 > 80
        milvus_client.drop_collection("ar4")

