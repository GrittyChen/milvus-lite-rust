# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_grpc_error_mapping.py，Apache-2.0 许可。
# 只保留 M1 范围内的用例（逐条状态见 INVENTORY.md），并删去随之不再使用的 import、helper 与分节标题。
"""Phase 10.6 — error code mapping + UNIMPLEMENTED stub tests.

Validates that:
1. MilvusLiteError subclasses produce the right grpc Status codes
   (both legacy error_code and new code fields) so pymilvus clients
   see the expected exception types.
2. Explicitly-stubbed UNIMPLEMENTED RPCs return a friendly reason
   string (not the generic "Method not implemented!" from the base).
"""

import pytest
from pymilvus import DataType, MilvusClient


# ---------------------------------------------------------------------------
# Integration: pymilvus sees the right exceptions via gRPC
# ---------------------------------------------------------------------------

def test_drop_nonexistent_is_idempotent(milvus_client):
    """Dropping a non-existent collection should silently succeed (Milvus compat)."""
    milvus_client.drop_collection("ghost")  # no error


def test_search_after_release_reports_not_loaded(milvus_client):
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=4)
    milvus_client.create_collection("demo", schema=schema)
    milvus_client.insert("demo", [
        {"id": 1, "vec": [1.0, 0.0, 0.0, 0.0]},
    ])
    idx = milvus_client.prepare_index_params()
    idx.add_index(field_name="vec", index_type="BRUTE_FORCE",
                  metric_type="L2", params={})
    milvus_client.create_index("demo", idx)
    milvus_client.release_collection("demo")
    with pytest.raises(Exception) as exc_info:
        milvus_client.search("demo", data=[[1.0, 0.0, 0.0, 0.0]], limit=1)
    assert "load" in str(exc_info.value).lower()
