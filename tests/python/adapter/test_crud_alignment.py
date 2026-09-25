# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_crud_alignment.py，Apache-2.0 许可。
# 只保留 M1 范围内的用例（逐条状态见 INVENTORY.md），并删去随之不再使用的 import、helper 与分节标题。
# 改动：`<pk> == N` 换成 `<pk> in [N]`（M1 只识别 `<主键> in [...]`）。
"""CRUD alignment tests — gaps fixed between MilvusLite and pymilvus.

1. delete(filter=...) without load
2. query(output_fields=["count(*)"])
3. get(ids, output_fields=["field"])
4. search(round_decimal=N)
5. query(output_fields=["*"]) wildcard expansion
"""

from pymilvus import DataType, MilvusClient


def _setup(client, name, n=10):
    schema = MilvusClient.create_schema()
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=4)
    schema.add_field("tag", DataType.VARCHAR, max_length=64)
    schema.add_field("score", DataType.INT64)
    client.create_collection(name, schema=schema)
    client.insert(name, [
        {"id": i, "vec": [float(i), 0, 0, 0], "tag": f"t{i % 3}", "score": i * 10}
        for i in range(n)
    ])
    return name


def _load(client, name):
    idx = client.prepare_index_params()
    idx.add_index(field_name="vec", index_type="BRUTE_FORCE",
                  metric_type="COSINE", params={})
    client.create_index(name, idx)
    client.load_collection(name)


# ---------------------------------------------------------------------------
# 2. count(*) aggregation
# ---------------------------------------------------------------------------

def test_query_count_star_no_filter(milvus_client):
    """count(*) with empty filter returns total count."""
    _setup(milvus_client, "cnt3")
    _load(milvus_client, "cnt3")
    result = milvus_client.query("cnt3", filter="",
                                 output_fields=["count(*)"])
    assert result[0]["count(*)"] == 10
    milvus_client.drop_collection("cnt3")


# ---------------------------------------------------------------------------
# 3. get() with output_fields
# ---------------------------------------------------------------------------

def test_get_with_output_fields(milvus_client):
    """get(ids, output_fields=[...]) returns only requested fields."""
    _setup(milvus_client, "getof")
    _load(milvus_client, "getof")
    rows = milvus_client.get("getof", ids=[0, 1],
                             output_fields=["tag"])
    assert len(rows) == 2
    for r in rows:
        assert "tag" in r
        assert "id" in r  # pk always included
        # score should NOT be in the result
        assert "score" not in r
    milvus_client.drop_collection("getof")


# ---------------------------------------------------------------------------
# 4. search round_decimal
# ---------------------------------------------------------------------------

def test_search_round_decimal(milvus_client):
    """search(round_decimal=2) rounds distance values."""
    _setup(milvus_client, "rd1")
    _load(milvus_client, "rd1")
    results = milvus_client.search(
        "rd1", data=[[1.0, 0.0, 0.0, 0.0]], limit=5,
        search_params={"metric_type": "COSINE"},
        round_decimal=2,
    )
    for hit in results[0]:
        d = hit["distance"]
        # Check that distance is rounded to 2 decimal places
        assert d == round(d, 2)
    milvus_client.drop_collection("rd1")


# ---------------------------------------------------------------------------
# 5. output_fields=["*"] wildcard
# ---------------------------------------------------------------------------

def test_query_wildcard_output_fields(milvus_client):
    """query(output_fields=['*']) returns all schema fields."""
    _setup(milvus_client, "wc1")
    _load(milvus_client, "wc1")
    # 上游为 filter="id == 0"：M1 只识别 <pk> in [...]，单个主键时二者结果相同
    rows = milvus_client.query("wc1", filter="id in [0]",
                               output_fields=["*"])
    assert len(rows) == 1
    r = rows[0]
    assert "id" in r
    assert "tag" in r
    assert "score" in r
    assert "vec" in r
    milvus_client.drop_collection("wc1")
