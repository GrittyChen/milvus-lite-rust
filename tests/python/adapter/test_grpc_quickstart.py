# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_grpc_quickstart.py，Apache-2.0 许可。
# 只保留 M1 范围内的用例（逐条状态见 INVENTORY.md），并删去随之不再使用的 import、helper 与分节标题。
# 改动：删去上游因 HNSW 需要 faiss 而加的 skipif（它导入了 3.x 内部模块）。
"""Phase 10.5 — pymilvus quickstart full end-to-end smoke test.

This is the Phase 10 completion marker. If this test passes, the
MilvusLite gRPC adapter supports the Milvus "hello world" workflow
that every pymilvus user expects on first contact:

    connect → create_collection → insert → flush →
    create_index → load → search → query → get → delete →
    release → drop_collection

The test exercises EVERY RPC layer implemented across 10.1-10.5.

Skipped if pymilvus / grpcio / faiss-cpu is not installed.
"""

from pymilvus import DataType, MilvusClient



def test_quickstart_multi_query_search(grpc_server):
    """Multi-query search with nq > 1 — covers the per-query topks
    flattening in build_search_result_data."""
    port, _db = grpc_server
    client = MilvusClient(uri=f"http://127.0.0.1:{port}")

    try:
        schema = MilvusClient.create_schema(auto_id=False)
        schema.add_field("id", DataType.INT64, is_primary=True)
        schema.add_field("vec", DataType.FLOAT_VECTOR, dim=4)

        client.create_collection("multi", schema=schema)
        client.insert("multi", [
            {"id": i, "vec": [float(i), 0.0, 0.0, 0.0]}
            for i in range(50)
        ])
        idx = client.prepare_index_params()
        idx.add_index(field_name="vec", index_type="HNSW",
                      metric_type="L2", params={"M": 16})
        client.create_index("multi", idx)
        client.load_collection("multi")

        results = client.search(
            "multi",
            data=[
                [0.0, 0.0, 0.0, 0.0],
                [25.0, 0.0, 0.0, 0.0],
                [49.0, 0.0, 0.0, 0.0],
            ],
            limit=3,
        )
        assert len(results) == 3
        assert results[0][0]["id"] == 0   # closest to [0,0,0,0]
        assert results[1][0]["id"] == 25  # closest to [25,0,0,0]
        assert results[2][0]["id"] == 49  # closest to [49,0,0,0]

        client.drop_collection("multi")
    finally:
        client.close()
