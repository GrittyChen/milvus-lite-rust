# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_grpc_crud.py，Apache-2.0 许可。
# 只保留 M1 范围内的用例（逐条状态见 INVENTORY.md），并删去随之不再使用的 import、helper 与分节标题。
# 改动：`<pk> == N` 换成 `<pk> in [N]`（M1 只识别 `<主键> in [...]`）。
"""Phase 10.3 — pymilvus CRUD end-to-end tests.

The 4 RPCs added in Phase 10.3 (Insert / Upsert / Delete / Query)
drive the following pymilvus client API surface:

    client.insert(name, data)
    client.upsert(name, data)
    client.delete(name, ids=[...])
    client.delete(name, filter="...")
    client.query(name, filter="...", output_fields=[...])
    client.get(name, ids=[...], output_fields=[...])

Skipped automatically when pymilvus / grpcio is not installed.
"""

import pytest
from pymilvus import DataType, MilvusClient


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _make_schema():
    schema = MilvusClient.create_schema(
        auto_id=False, enable_dynamic_field=False
    )
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=4)
    schema.add_field("title", DataType.VARCHAR, max_length=64)
    schema.add_field("score", DataType.FLOAT)
    schema.add_field("active", DataType.BOOL)
    return schema


def _row(i, active=True):
    return {
        "id": i,
        "vec": [float(i), float(i + 1), float(i + 2), float(i + 3)],
        "title": f"t{i}",
        "score": float(i) / 10,
        "active": active,
    }


@pytest.fixture
def populated_client(milvus_client):
    """A client with a 'demo' collection holding 5 records."""
    milvus_client.create_collection("demo", schema=_make_schema())
    rows = [_row(i, active=(i % 2 == 0)) for i in range(5)]
    milvus_client.insert("demo", rows)
    return milvus_client


# ---------------------------------------------------------------------------
# Insert
# ---------------------------------------------------------------------------

def test_insert_returns_inserted_ids(milvus_client):
    milvus_client.create_collection("demo", schema=_make_schema())
    rows = [_row(i) for i in range(5)]
    res = milvus_client.insert("demo", rows)
    assert res["insert_count"] == 5
    assert sorted(res["ids"]) == [0, 1, 2, 3, 4]


def test_insert_into_unknown_collection_raises(milvus_client):
    with pytest.raises(Exception):
        milvus_client.insert("ghost", [_row(0)])


def test_insert_round_trip_preserves_field_values(populated_client):
    # 上游为 filter="id == 2"：M1 只识别 <pk> in [...]，单个主键时二者结果相同
    rows = populated_client.query(
        "demo", filter="id in [2]", output_fields=["title", "score", "active"]
    )
    assert len(rows) == 1
    r = rows[0]
    assert r["title"] == "t2"
    assert abs(r["score"] - 0.2) < 1e-6
    assert r["active"] is True


# ---------------------------------------------------------------------------
# Query — id-based path → routed to col.get
# ---------------------------------------------------------------------------

def test_query_by_id_in_list(populated_client):
    rows = populated_client.query(
        "demo", filter="id in [0, 2, 4]", output_fields=["title"]
    )
    titles = sorted(r["title"] for r in rows)
    assert titles == ["t0", "t2", "t4"]


def test_query_by_id_with_output_fields(populated_client):
    rows = populated_client.query(
        "demo", filter="id in [1]", output_fields=["score", "active"]
    )
    assert len(rows) == 1
    # pk is always returned + the requested fields
    assert "id" in rows[0]
    assert "score" in rows[0]
    assert "active" in rows[0]


# ---------------------------------------------------------------------------
# Get (pymilvus client.get → routed through Query RPC)
# ---------------------------------------------------------------------------

def test_get_by_ids(populated_client):
    rows = populated_client.get("demo", ids=[1, 3])
    ids = sorted(r["id"] for r in rows)
    assert ids == [1, 3]


def test_get_by_ids_with_output_fields(populated_client):
    rows = populated_client.get("demo", ids=[2], output_fields=["title"])
    assert len(rows) == 1
    assert rows[0]["id"] == 2
    assert rows[0]["title"] == "t2"


def test_get_missing_ids_returns_empty(populated_client):
    rows = populated_client.get("demo", ids=[999])
    assert rows == []
