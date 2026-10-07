# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_auto_id.py，Apache-2.0 许可。
# 仅保留 INVENTORY.md 中已复用的用例；S1 表达式及断言保持上游原文。
"""Phase 15 — Auto ID tests.

Covers:
1. Engine: auto_id schema validation
2. Engine: insert without pk field, IDs auto-generated
3. Engine: multiple inserts produce strictly increasing IDs
4. Engine: auto-generated IDs survive flush + restart
5. gRPC: pymilvus create_collection(auto_id=True) + insert
6. gRPC: returned IDs are correct
7. gRPC: search/query on auto_id collection
"""

from pymilvus import DataType, MilvusClient


# ---------------------------------------------------------------------------
# Engine-level tests
# ---------------------------------------------------------------------------



# ---------------------------------------------------------------------------
# gRPC integration tests
# ---------------------------------------------------------------------------

def test_grpc_auto_id_create_and_insert(milvus_client):
    """pymilvus create_collection(auto_id=True) + insert without pk."""
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True, auto_id=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=4)
    schema.add_field("label", DataType.VARCHAR, max_length=100)
    milvus_client.create_collection("auto_id_test", schema=schema)

    result = milvus_client.insert("auto_id_test", [
        {"vec": [1, 0, 0, 0], "label": "first"},
        {"vec": [0, 1, 0, 0], "label": "second"},
        {"vec": [0, 0, 1, 0], "label": "third"},
    ])
    assert result["insert_count"] == 3
    # pymilvus returns the generated IDs in result["ids"]
    ids = result["ids"]
    assert len(ids) == 3
    assert all(isinstance(i, int) for i in ids)

    milvus_client.drop_collection("auto_id_test")


def test_grpc_auto_id_search(milvus_client):
    """Search on auto_id collection via pymilvus."""
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True, auto_id=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=4)
    milvus_client.create_collection("auto_search", schema=schema)

    milvus_client.insert("auto_search", [
        {"vec": [1, 0, 0, 0]},
        {"vec": [0, 1, 0, 0]},
        {"vec": [0, 0, 1, 0]},
    ])

    idx = milvus_client.prepare_index_params()
    idx.add_index(field_name="vec", index_type="BRUTE_FORCE",
                  metric_type="COSINE", params={})
    milvus_client.create_index("auto_search", idx)
    milvus_client.load_collection("auto_search")

    results = milvus_client.search(
        "auto_search", data=[[1, 0, 0, 0]], limit=3,
    )
    assert len(results[0]) == 3

    milvus_client.drop_collection("auto_search")


def test_grpc_auto_id_query(milvus_client):
    """Query on auto_id collection via pymilvus."""
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True, auto_id=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=4)
    schema.add_field("tag", DataType.INT64)
    milvus_client.create_collection("auto_query", schema=schema)

    result = milvus_client.insert("auto_query", [
        {"vec": [1, 0, 0, 0], "tag": 10},
        {"vec": [0, 1, 0, 0], "tag": 20},
    ])
    auto_ids = result["ids"]

    idx = milvus_client.prepare_index_params()
    idx.add_index(field_name="vec", index_type="BRUTE_FORCE",
                  metric_type="COSINE", params={})
    milvus_client.create_index("auto_query", idx)
    milvus_client.load_collection("auto_query")

    # Query by filter
    rows = milvus_client.query("auto_query", filter="tag == 10",
                               output_fields=["tag"], limit=10)
    assert len(rows) == 1
    assert rows[0]["tag"] == 10
    assert rows[0]["id"] == auto_ids[0]

    milvus_client.drop_collection("auto_query")


def test_grpc_auto_id_ids_increasing(milvus_client):
    """Multiple inserts produce increasing IDs."""
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True, auto_id=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=4)
    milvus_client.create_collection("auto_incr", schema=schema)

    r1 = milvus_client.insert("auto_incr", [{"vec": [1, 0, 0, 0]}])
    r2 = milvus_client.insert("auto_incr", [{"vec": [0, 1, 0, 0]}])
    assert r1["ids"][0] < r2["ids"][0]

    milvus_client.drop_collection("auto_incr")
