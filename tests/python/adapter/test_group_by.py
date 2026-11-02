"""Phase 13 — Group By search tests.

复制自参照实现 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_group_by.py，Apache-2.0 许可。用例本体不变；
直接使用 3.x 内部 Collection 的 6 个引擎层用例（`TestGroupByEngine`）不复制，见 INVENTORY.md。
"""

from pymilvus import DataType, MilvusClient


def _create_grouped_collection(client, name):
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("category", DataType.VARCHAR, max_length=100)
    schema.add_field("dense", DataType.FLOAT_VECTOR, dim=4)
    client.create_collection(name, schema=schema)
    client.insert(name, [
        {"id": 1, "category": "tech", "dense": [1, 0, 0, 0]},
        {"id": 2, "category": "tech", "dense": [0.9, 0.1, 0, 0]},
        {"id": 3, "category": "tech", "dense": [0.8, 0.2, 0, 0]},
        {"id": 4, "category": "sports", "dense": [0, 1, 0, 0]},
        {"id": 5, "category": "sports", "dense": [0, 0.9, 0.1, 0]},
        {"id": 6, "category": "music", "dense": [0, 0, 1, 0]},
        {"id": 7, "category": "music", "dense": [0, 0, 0.9, 0.1]},
        {"id": 8, "category": "food", "dense": [0, 0, 0, 1]},
    ])
    idx = client.prepare_index_params()
    idx.add_index(field_name="dense", index_type="BRUTE_FORCE",
                  metric_type="COSINE", params={})
    client.create_index(name, idx)
    client.load_collection(name)


def test_grpc_group_by_basic(milvus_client):
    _create_grouped_collection(milvus_client, "gb_basic")
    results = milvus_client.search(
        "gb_basic",
        data=[[1, 0, 0, 0]],
        limit=3,
        group_by_field="category",
        group_size=1,
        output_fields=["category"],
    )
    categories = [h["entity"]["category"] for h in results[0]]
    assert len(set(categories)) == len(categories)  # unique per group
    milvus_client.drop_collection("gb_basic")


def test_grpc_group_by_group_size(milvus_client):
    _create_grouped_collection(milvus_client, "gb_size")
    results = milvus_client.search(
        "gb_size",
        data=[[1, 0, 0, 0]],
        limit=2,
        group_by_field="category",
        group_size=2,
        output_fields=["category"],
    )
    from collections import Counter
    counts = Counter(h["entity"]["category"] for h in results[0])
    for c in counts.values():
        assert c <= 2
    milvus_client.drop_collection("gb_size")


def test_grpc_group_by_strict(milvus_client):
    _create_grouped_collection(milvus_client, "gb_strict")
    results = milvus_client.search(
        "gb_strict",
        data=[[1, 0, 0, 0]],
        limit=10,
        group_by_field="category",
        group_size=3,
        strict_group_size=True,
        output_fields=["category"],
    )
    # Only "tech" has 3 docs
    from collections import Counter
    counts = Counter(h["entity"]["category"] for h in results[0])
    for c in counts.values():
        assert c == 3
    milvus_client.drop_collection("gb_strict")


def test_grpc_group_by_with_filter(milvus_client):
    _create_grouped_collection(milvus_client, "gb_filt")
    results = milvus_client.search(
        "gb_filt",
        data=[[1, 0, 0, 0]],
        limit=10,
        filter="id <= 5",
        group_by_field="category",
        group_size=1,
        output_fields=["category", "id"],
    )
    for h in results[0]:
        assert h["entity"]["id"] <= 5
    milvus_client.drop_collection("gb_filt")


def test_grpc_group_by_nullable_varchar(milvus_client):
    """Group-by on a nullable scalar field preserves real group values.

    Adapted from Milvus python_client query aggregation coverage for nullable
    group-by fields. Existing local tests only group by non-null fields.
    """
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("category", DataType.VARCHAR, max_length=100, nullable=True)
    schema.add_field("dense", DataType.FLOAT_VECTOR, dim=4)
    milvus_client.create_collection("gb_nullable", schema=schema)
    milvus_client.insert("gb_nullable", [
        {"id": 1, "category": "tech", "dense": [1, 0, 0, 0]},
        {"id": 2, "category": "tech", "dense": [0.9, 0.1, 0, 0]},
        {"id": 3, "category": "sports", "dense": [0, 1, 0, 0]},
        {"id": 4, "category": "music", "dense": [0, 0, 1, 0]},
        {"id": 5, "category": None, "dense": [0, 0, 0, 1]},
    ])
    idx = milvus_client.prepare_index_params()
    idx.add_index(field_name="dense", index_type="BRUTE_FORCE",
                  metric_type="COSINE", params={})
    milvus_client.create_index("gb_nullable", idx)
    milvus_client.load_collection("gb_nullable")

    results = milvus_client.search(
        "gb_nullable",
        data=[[1, 0, 0, 0]],
        limit=4,
        group_by_field="category",
        group_size=1,
        output_fields=["category"],
    )
    categories = [hit["entity"].get("category") for hit in results[0]]
    assert "tech" in categories
    assert len(categories) == len(set(categories))
    assert any(category is not None for category in categories)
    milvus_client.drop_collection("gb_nullable")
