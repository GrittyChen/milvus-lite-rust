# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_issue_fixes.py，Apache-2.0 许可。
# 仅保留 INVENTORY.md 中已复用的用例；S1 表达式及断言保持上游原文。
# 直接导入依赖；FLAT 不需要 faiss，移除上游的 faiss 跳过条件。
"""Tests for GitHub issue fixes #1-#7."""

import numpy as np
import pytest

from pymilvus import MilvusClient, DataType


# ---------------------------------------------------------------------------
# #1: Dynamic field values returned in query output
# ---------------------------------------------------------------------------

def test_dynamic_field_query_output(milvus_client):
    """Issue #1: Dynamic field values must be returned in query output."""
    schema = MilvusClient.create_schema(enable_dynamic_field=True)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=4)
    milvus_client.create_collection("dyn_test", schema=schema)
    milvus_client.insert("dyn_test", [
        {"id": 1, "vec": [1, 0, 0, 0], "category": "tech", "level": 5},
        {"id": 2, "vec": [0, 1, 0, 0], "category": "news", "level": 3},
    ])
    idx = milvus_client.prepare_index_params()
    idx.add_index(field_name="vec", index_type="FLAT",
                  metric_type="COSINE", params={})
    milvus_client.create_index("dyn_test", idx)
    milvus_client.load_collection("dyn_test")

    # Query with dynamic field in output_fields
    rows = milvus_client.query("dyn_test", filter="id >= 1",
                                output_fields=["category", "level"], limit=10)
    assert len(rows) == 2
    for r in rows:
        assert "category" in r
        assert "level" in r
    assert rows[0]["category"] in ("tech", "news")

    # Search with dynamic field in output_fields
    results = milvus_client.search("dyn_test", data=[[1, 0, 0, 0]], limit=2,
                                    output_fields=["category"])
    assert "category" in results[0][0]["entity"]

    milvus_client.drop_collection("dyn_test")


# ---------------------------------------------------------------------------
# #2: Multiple FLOAT_VECTOR fields
# ---------------------------------------------------------------------------



# ---------------------------------------------------------------------------
# #3: IP distance sign
# ---------------------------------------------------------------------------



# ---------------------------------------------------------------------------
# #4: gRPC large batch insert
# ---------------------------------------------------------------------------

def test_grpc_large_batch_insert(milvus_client):
    """Issue #4: Insert ~10K vectors should not hit 4MB gRPC limit."""
    schema = MilvusClient.create_schema()
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=128)
    milvus_client.create_collection("big_batch", schema=schema)

    data = [
        {"id": i, "vec": np.random.rand(128).tolist()}
        for i in range(5000)
    ]
    # Should not raise RESOURCE_EXHAUSTED
    result = milvus_client.insert("big_batch", data)
    assert result["insert_count"] == 5000

    milvus_client.drop_collection("big_batch")


# ---------------------------------------------------------------------------
# #6: describe_index with field_name
# ---------------------------------------------------------------------------

def test_describe_index_by_field_name(milvus_client):
    """Issue #6: describe_index should work with field_name parameter."""
    schema = MilvusClient.create_schema()
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=4)
    milvus_client.create_collection("idx_desc", schema=schema)

    idx = milvus_client.prepare_index_params()
    idx.add_index(field_name="vec", index_type="FLAT",
                  metric_type="COSINE", params={})
    milvus_client.create_index("idx_desc", idx)

    # Should not raise TypeError
    info = milvus_client.describe_index("idx_desc", index_name="vec")
    assert info is not None

    milvus_client.drop_collection("idx_desc")


# ---------------------------------------------------------------------------
# #7: AUTOINDEX uses HNSW when faiss available
# ---------------------------------------------------------------------------



# ---------------------------------------------------------------------------
# #343: raw distance semantics should match Milvus server
# ---------------------------------------------------------------------------

def test_raw_distance_semantics_match_milvus_server(milvus_client):
    """Issue #343: COSINE is similarity and L2 is squared distance."""
    expected = {
        "COSINE": [(1, 1.0), (2, 0.0)],
        "IP": [(1, 1.0), (2, 0.0)],
        "L2": [(1, 0.0), (2, 2.0)],
    }

    for metric, metric_expected in expected.items():
        collection = f"metric_semantics_{metric.lower()}"
        milvus_client.create_collection(
            collection_name=collection,
            dimension=2,
            metric_type=metric,
            auto_id=False,
        )
        milvus_client.insert(
            collection_name=collection,
            data=[
                {"id": 1, "vector": [1.0, 0.0]},
                {"id": 2, "vector": [0.0, 1.0]},
            ],
        )

        results = milvus_client.search(
            collection_name=collection,
            data=[[1.0, 0.0]],
            anns_field="vector",
            limit=2,
            search_params={"metric_type": metric},
            output_fields=["id"],
        )

        actual = [
            (row.get("id") or row["entity"]["id"], row["distance"])
            for row in results[0]
        ]
        assert [row[0] for row in actual] == [row[0] for row in metric_expected]
        for (_, distance), (_, expected_distance) in zip(actual, metric_expected):
            assert distance == pytest.approx(expected_distance)


# ---------------------------------------------------------------------------
# #347 / #348: dynamic wildcard output and array filters
# ---------------------------------------------------------------------------

def _create_dynamic_compat_collection(client, name):
    schema = MilvusClient.create_schema(enable_dynamic_field=True)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("text", DataType.VARCHAR, max_length=128)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=2)
    client.create_collection(name, schema=schema)
    client.insert(name, [{
        "id": 1,
        "text": "foo",
        "vec": [0.0, 0.0],
        "page": 1,
        "source": "test",
        "array_field": [0, 1, 2],
    }])
    index = client.prepare_index_params()
    index.add_index(
        field_name="vec", index_type="FLAT", metric_type="L2", params={}
    )
    client.create_index(name, index)
    client.load_collection(name)


@pytest.mark.parametrize(
    "suffix, fields",
    [
        ("star", ["*"]),
        ("meta", ["$meta"]),
        ("mixed", ["text", "$meta"]),
    ],
)
def test_dynamic_wildcard_and_meta_search_output(milvus_client, suffix, fields):
    name = f"dynamic_output_347_{suffix}"
    _create_dynamic_compat_collection(milvus_client, name)
    result = milvus_client.search(
        name,
        data=[[0.0, 0.0]],
        anns_field="vec",
        search_params={"metric_type": "L2", "params": {}},
        limit=1,
        output_fields=fields,
    )
    entity = result[0][0]["entity"]
    assert entity["page"] == 1
    assert entity["source"] == "test"

    rows = milvus_client.query(
        name,
        filter="id == 1",
        output_fields=fields,
        limit=1,
    )
    assert rows[0]["page"] == 1
    assert rows[0]["source"] == "test"
    milvus_client.drop_collection(name)
