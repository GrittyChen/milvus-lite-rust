"""pymilvus contracts for ordinary sparse IP and Function-generated BM25 fields.

复制自参照实现 milvus-lite（1320d26）的 tests/adapter/test_sparse_field_contract.py，Apache-2.0 许可。用例本体不变；
`test_search_decoder_preserves_metric_omission` 直接调用 3.x 的翻译层（依赖内部模块），不复制，随它删去两条只为它
导入的内部模块与 `common_pb2` / `milvus_pb2`。见 INVENTORY.md。

Exercise ordinary and hybrid search against the shared local gRPC fixture
and real Engine, including field/metric validation and score direction.
"""

import pytest
from pymilvus import AnnSearchRequest, DataType, Function, FunctionType, MilvusClient, RRFRanker
from pymilvus.exceptions import MilvusException


def _setup(client, *, mixed=True):
    name = "sparse_field_contract"
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("sparse", DataType.SPARSE_FLOAT_VECTOR)
    rows = [
        {"id": 1, "sparse": {0: 1.0, 1: 100.0}},
        {"id": 2, "sparse": {0: 0.5}},
    ]
    if mixed:
        schema.add_field("dense", DataType.FLOAT_VECTOR, dim=2)
        schema.add_field("text", DataType.VARCHAR, max_length=256, enable_analyzer=True)
        schema.add_field("bm25", DataType.SPARSE_FLOAT_VECTOR)
        schema.add_function(Function(
            name="bm25_fn", input_field_names=["text"],
            output_field_names=["bm25"], function_type=FunctionType.BM25,
        ))
        rows[0].update(dense=[1.0, 0.0], text="alpha")
        rows[1].update(dense=[0.0, 1.0], text="beta")
    client.create_collection(name, schema=schema)
    client.insert(name, rows)
    indexes = client.prepare_index_params()
    if mixed:
        indexes.add_index(field_name="dense", index_type="BRUTE_FORCE", metric_type="COSINE")
        indexes.add_index(field_name="bm25", index_type="SPARSE_INVERTED_INDEX", metric_type="BM25")
    indexes.add_index(field_name="sparse", index_type="SPARSE_INVERTED_INDEX", metric_type="IP")
    client.create_index(name, indexes)
    client.load_collection(name)
    return name


@pytest.mark.parametrize("mixed", [False, True])
@pytest.mark.parametrize("explicit", [False, True])
def test_pymilvus_sparse_ip_scores_and_field_default(milvus_client, mixed, explicit):
    name = _setup(milvus_client, mixed=mixed)
    result = milvus_client.search(
        name, data=[{0: 1.0}], anns_field="sparse", limit=2,
        search_params={"metric_type": "IP"} if explicit else {},
    )
    assert [hit["id"] for hit in result[0]] == [1, 2]
    assert [hit["distance"] for hit in result[0]] == pytest.approx([1.0, 0.5])
    info = milvus_client.describe_index(name, index_name="sparse")
    assert info["metric_type"] == "IP"


def test_pymilvus_bm25_omitted_metric_uses_its_own_index(milvus_client):
    name = _setup(milvus_client)
    [hits] = milvus_client.search(name, data=["alpha"], anns_field="bm25", search_params={}, limit=2)
    assert [hit["id"] for hit in hits] == [1]
    assert hits[0]["distance"] < 0  # Preserve current BM25 public score convention.


@pytest.mark.parametrize("field,metric,data", [
    ("sparse", "IP", ["alpha"]),
    ("bm25", "BM25", [{0: 1.0}]),
    ("sparse", "BM25", [{0: 1.0}]),
    ("bm25", "IP", ["alpha"]),
])
def test_pymilvus_rejects_invalid_field_input_metric_combinations(milvus_client, field, metric, data):
    name = _setup(milvus_client)
    with pytest.raises(MilvusException, match=field):
        milvus_client.search(name, data=data, anns_field=field,
                             search_params={"metric_type": metric}, limit=2)


def test_hybrid_sparse_ip_route_preserves_inner_product_ranking(milvus_client):
    name = _setup(milvus_client)
    request = AnnSearchRequest(data=[{0: 1.0}], anns_field="sparse", param={}, limit=2)
    [hits] = milvus_client.hybrid_search(name, reqs=[request], ranker=RRFRanker(k=60), limit=2)
    assert [hit["id"] for hit in hits] == [1, 2]
    assert [hit["distance"] for hit in hits] == pytest.approx([1 / 61, 1 / 62])


def test_hybrid_resolves_sparse_and_bm25_defaults_independently(milvus_client):
    name = _setup(milvus_client)
    requests = [
        AnnSearchRequest(data=[{0: 1.0}], anns_field="sparse", param={}, limit=2),
        AnnSearchRequest(data=["alpha"], anns_field="bm25", param={}, limit=2),
    ]
    [hits] = milvus_client.hybrid_search(name, reqs=requests, ranker=RRFRanker(k=60), limit=2)
    assert [hit["id"] for hit in hits] == [1, 2]
    assert [hit["distance"] for hit in hits] == pytest.approx([2 / 61, 1 / 62])


def test_invalid_hybrid_route_fails_entire_request(milvus_client):
    name = _setup(milvus_client)
    requests = [
        AnnSearchRequest(data=[{0: 1.0}], anns_field="sparse", param={}, limit=2),
        AnnSearchRequest(data=[{0: 1.0}], anns_field="bm25", param={"metric_type": "BM25"}, limit=2),
    ]
    with pytest.raises(MilvusException, match="bm25"):
        milvus_client.hybrid_search(name, reqs=requests, ranker=RRFRanker(), limit=2)
