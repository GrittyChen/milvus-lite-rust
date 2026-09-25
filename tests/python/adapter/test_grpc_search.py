# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_grpc_search.py，Apache-2.0 许可。
# 只保留 M1 范围内的用例（逐条状态见 INVENTORY.md），并删去随之不再使用的 import、helper 与分节标题。
# 改动：HNSW 换成 FLAT（M1 只有 FLAT，用例少于 1 万行时 3.x 不会 flush，结果不变）。
"""Phase 10.4 — pymilvus search end-to-end tests.

Validates the Search RPC: SearchRequest decoding (placeholder_group +
search_params), engine dispatch, and SearchResultData encoding. Both
HNSW and BRUTE_FORCE backends are tested. Skipped if pymilvus or
faiss-cpu is missing.
"""

import pytest
from pymilvus import DataType, MilvusClient


def _make_schema():
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=4)
    schema.add_field("title", DataType.VARCHAR, max_length=64)
    schema.add_field("score", DataType.FLOAT)
    schema.add_field("active", DataType.BOOL)
    return schema


# 上游为 _hnsw_idx（HNSW，M=16、efConstruction=200）。M1 只有 FLAT；用例都少于 1 万行，
# 3.x 不会 flush，一直在 memtable 上暴力检索，换成 FLAT 不改变结果。
def _flat_idx(client):
    idx = client.prepare_index_params()
    idx.add_index(
        field_name="vec",
        index_type="FLAT",
        metric_type="COSINE",
        params={},
    )
    return idx


def _brute_idx(client, metric="L2"):
    idx = client.prepare_index_params()
    idx.add_index(
        field_name="vec",
        index_type="BRUTE_FORCE",
        metric_type=metric,
        params={},
    )
    return idx


@pytest.fixture
def loaded_collection(milvus_client):
    """Collection with 20 records, FLAT index, loaded ready for search."""
    milvus_client.create_collection("demo", schema=_make_schema())
    rows = [
        {
            "id": i,
            "vec": [float(i), float(i + 1), 0.0, 0.0],
            "title": f"t{i:02d}",
            "score": float(i) / 10,
            "active": (i % 2 == 0),
        }
        for i in range(20)
    ]
    milvus_client.insert("demo", rows)
    milvus_client.create_index("demo", _flat_idx(milvus_client))
    milvus_client.load_collection("demo")
    return milvus_client


# ---------------------------------------------------------------------------
# Basic search shape
# ---------------------------------------------------------------------------

def test_search_returns_top_k(loaded_collection):
    res = loaded_collection.search(
        "demo", data=[[1.0, 2.0, 0.0, 0.0]], limit=5
    )
    assert len(res) == 1            # nq = 1
    assert len(res[0]) == 5         # top_k = 5


def test_search_multiple_queries(loaded_collection):
    res = loaded_collection.search(
        "demo",
        data=[
            [1.0, 2.0, 0.0, 0.0],
            [10.0, 11.0, 0.0, 0.0],
        ],
        limit=3,
    )
    assert len(res) == 2
    assert len(res[0]) == 3
    assert len(res[1]) == 3


def test_search_self_query_returns_self_top1(loaded_collection):
    """Querying with one of the indexed vectors should return that
    pk at rank 1 — basic correctness check that the placeholder
    decoding got the bytes right."""
    res = loaded_collection.search(
        "demo", data=[[5.0, 6.0, 0.0, 0.0]], limit=1
    )
    assert res[0][0]["id"] == 5


# ---------------------------------------------------------------------------
# Output fields
# ---------------------------------------------------------------------------

def test_search_with_output_fields(loaded_collection):
    res = loaded_collection.search(
        "demo",
        data=[[5.0, 6.0, 0.0, 0.0]],
        limit=3,
        output_fields=["title", "score"],
    )
    for hit in res[0]:
        assert "title" in hit["entity"]
        assert "score" in hit["entity"]


def test_search_default_output_fields_is_empty(loaded_collection):
    """When no output_fields are requested, pymilvus shows just id +
    distance, no entity fields."""
    res = loaded_collection.search(
        "demo", data=[[5.0, 6.0, 0.0, 0.0]], limit=3
    )
    for hit in res[0]:
        # entity may be empty {} or may contain just the id field
        assert "id" in hit or "id" in hit.get("entity", {})


# ---------------------------------------------------------------------------
# Loaded state guard
# ---------------------------------------------------------------------------

def test_search_after_release_raises(loaded_collection):
    loaded_collection.release_collection("demo")
    with pytest.raises(Exception):
        loaded_collection.search("demo", data=[[1.0, 2.0, 0.0, 0.0]], limit=1)


def test_search_unknown_collection_raises(milvus_client):
    with pytest.raises(Exception):
        milvus_client.search("ghost", data=[[1.0, 2.0, 0.0, 0.0]], limit=1)


# ---------------------------------------------------------------------------
# BRUTE_FORCE backend (tests the factory routing too)
# ---------------------------------------------------------------------------

def test_search_brute_force_backend(milvus_client):
    milvus_client.create_collection("brute", schema=_make_schema())
    milvus_client.insert("brute", [
        {"id": i, "vec": [float(i), 0.0, 0.0, 0.0], "title": f"t{i}",
         "score": 0.0, "active": True}
        for i in range(10)
    ])
    milvus_client.create_index("brute", _brute_idx(milvus_client, metric="L2"))
    milvus_client.load_collection("brute")
    res = milvus_client.search("brute", data=[[3.0, 0.0, 0.0, 0.0]], limit=3)
    assert len(res[0]) == 3
    # L2 self-distance is 0 for exact match
    assert res[0][0]["id"] == 3


# ---------------------------------------------------------------------------
# Distances are present and reasonable
# ---------------------------------------------------------------------------

def test_search_distances_returned(loaded_collection):
    res = loaded_collection.search(
        "demo", data=[[5.0, 6.0, 0.0, 0.0]], limit=5
    )
    for hit in res[0]:
        assert "distance" in hit
        assert isinstance(hit["distance"], (int, float))


def test_search_results_sorted_by_cosine_score_descending(loaded_collection):
    res = loaded_collection.search(
        "demo", data=[[5.0, 6.0, 0.0, 0.0]], limit=10
    )
    distances = [hit["distance"] for hit in res[0]]
    assert distances == sorted(distances, reverse=True)
