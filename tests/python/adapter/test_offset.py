# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_offset.py，Apache-2.0 许可。
# 只保留 M1 范围内的用例（逐条状态见 INVENTORY.md），并删去随之不再使用的 import、helper 与分节标题。
"""Phase 17 — Search/Query offset pagination tests.

Covers:
1. Engine search offset
2. Engine query offset
3. gRPC search offset via pymilvus
4. gRPC query offset via pymilvus
5. offset + limit combination
6. offset beyond result count → empty
"""

from pymilvus import DataType, MilvusClient


# ---------------------------------------------------------------------------
# gRPC integration tests
# ---------------------------------------------------------------------------

def _setup(client, name, n=20):
    schema = MilvusClient.create_schema()
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=4)
    client.create_collection(name, schema=schema)
    client.insert(name, [
        {"id": i, "vec": [float(20 - i), 0, 0, 0]} for i in range(n)
    ])
    idx = client.prepare_index_params()
    idx.add_index(field_name="vec", index_type="BRUTE_FORCE",
                  metric_type="COSINE", params={})
    client.create_index(name, idx)
    client.load_collection(name)


def test_grpc_search_offset(milvus_client):
    _setup(milvus_client, "off_search")
    all_res = milvus_client.search("off_search", data=[[20, 0, 0, 0]], limit=20)
    off_res = milvus_client.search("off_search", data=[[20, 0, 0, 0]],
                                   limit=5, offset=3)
    assert len(off_res[0]) <= 5
    # Offset results should not contain the top-3 best matches
    top3_ids = {h["id"] for h in all_res[0][:3]}
    for h in off_res[0]:
        assert h["id"] not in top3_ids
    milvus_client.drop_collection("off_search")


def test_grpc_search_offset_empty(milvus_client):
    _setup(milvus_client, "off_empty", n=5)
    results = milvus_client.search("off_empty", data=[[20, 0, 0, 0]],
                                   limit=5, offset=100)
    assert len(results[0]) == 0
    milvus_client.drop_collection("off_empty")
