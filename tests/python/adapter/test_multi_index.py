"""Phase 18 — Multi-vector independent index tests.

Covers:
1. Engine: create separate indexes on dense + sparse fields
2. Engine: load builds both indexes
3. Engine: drop one index, keep the other
4. gRPC: pymilvus create_index on two vector fields
5. gRPC: DescribeIndex returns multiple descriptions
6. gRPC: search on each field works after loading

复制自参照实现 milvus-lite（1320d26）的 tests/adapter/test_multi_index.py，Apache-2.0 许可。用例本体不变；
`TestMultiIndexEngine` 的 4 条直接构造 3.x 的引擎对象（依赖内部模块），不复制，随它删去只为它使用的
`closing`、`tempfile`、`pytest` 与 Engine 的分节标题。见 INVENTORY.md。
"""

from pymilvus import DataType, Function, FunctionType, MilvusClient


# ---------------------------------------------------------------------------
# gRPC integration tests
# ---------------------------------------------------------------------------

def _create_multi_idx_collection(client, name):
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("text", DataType.VARCHAR, max_length=65535,
                     enable_analyzer=True)
    schema.add_field("dense", DataType.FLOAT_VECTOR, dim=4)
    schema.add_field("sparse", DataType.SPARSE_FLOAT_VECTOR)
    schema.add_function(Function(
        name="bm25", function_type=FunctionType.BM25,
        input_field_names=["text"], output_field_names=["sparse"],
    ))
    client.create_collection(name, schema=schema)
    client.insert(name, [
        {"id": 1, "text": "python programming", "dense": [1, 0, 0, 0]},
        {"id": 2, "text": "java programming", "dense": [0, 1, 0, 0]},
        {"id": 3, "text": "machine learning", "dense": [0, 0, 1, 0]},
    ])


def test_grpc_multi_index_create(milvus_client):
    _create_multi_idx_collection(milvus_client, "mi_create")
    idx = milvus_client.prepare_index_params()
    idx.add_index(field_name="dense", index_type="BRUTE_FORCE",
                  metric_type="COSINE", params={})
    idx.add_index(field_name="sparse", index_type="SPARSE_INVERTED_INDEX",
                  metric_type="BM25", params={})
    milvus_client.create_index("mi_create", idx)
    milvus_client.load_collection("mi_create")

    # Both indexes should be queryable
    indexes = milvus_client.list_indexes("mi_create")
    assert len(indexes) >= 2

    milvus_client.drop_collection("mi_create")


def test_grpc_multi_index_search_both(milvus_client):
    _create_multi_idx_collection(milvus_client, "mi_search")
    idx = milvus_client.prepare_index_params()
    idx.add_index(field_name="dense", index_type="BRUTE_FORCE",
                  metric_type="COSINE", params={})
    milvus_client.create_index("mi_search", idx)
    milvus_client.load_collection("mi_search")

    # Dense search
    r1 = milvus_client.search("mi_search", data=[[1, 0, 0, 0]], limit=3)
    assert len(r1[0]) == 3

    # BM25 search (no index needed — on-the-fly)
    q = "python"
    r2 = milvus_client.search("mi_search", data=[q], anns_field="sparse",
                              search_params={"metric_type": "BM25"}, limit=3)
    assert len(r2[0]) >= 1

    milvus_client.drop_collection("mi_search")


def test_grpc_drop_one_index(milvus_client):
    _create_multi_idx_collection(milvus_client, "mi_drop")
    idx = milvus_client.prepare_index_params()
    idx.add_index(field_name="dense", index_type="BRUTE_FORCE",
                  metric_type="COSINE", params={})
    idx.add_index(field_name="sparse", index_type="SPARSE_INVERTED_INDEX",
                  metric_type="BM25", params={})
    milvus_client.create_index("mi_drop", idx)

    # Drop requires released state (Milvus semantics).
    milvus_client.release_collection("mi_drop")
    # Drop only sparse index
    milvus_client.drop_index("mi_drop", index_name="sparse_idx")

    # Dense index still exists
    indexes = milvus_client.list_indexes("mi_drop")
    assert len(indexes) >= 1

    milvus_client.drop_collection("mi_drop")
