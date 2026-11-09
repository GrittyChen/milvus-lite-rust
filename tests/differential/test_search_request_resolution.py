"""按目标字段确定度量、错误先后、Search 的主键投影与带分区的 count(*)，逐项对照 3.x（0027）。"""

import math
import struct

import grpc
import pytest
from pymilvus import AnnSearchRequest, DataType, MilvusClient, RRFRanker
from pymilvus.grpc_gen import common_pb2, milvus_pb2, milvus_pb2_grpc

from compare import hits, run
from test_scenarios import index

UNSET = object()


def schema():
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("cat", DataType.VARCHAR, max_length=16)
    return s


ROWS = [
    {"id": 1, "vector": [1.0, 0.0], "cat": "a"},
    {"id": 2, "vector": [3.0, 0.0], "cat": "b"},
    {"id": 3, "vector": [0.0, 1.0], "cat": "a"},
]


def create(diff, name, metric="L2", load=True):
    params = index(metric) if metric else None
    diff.step("create", lambda c: c.create_collection(name, schema=schema(), index_params=params))
    diff.step("insert", lambda c: c.insert(name, ROWS))
    if metric:
        diff.step("load", lambda c: c.load_collection(name))
    if not load:
        diff.step("release", lambda c: c.release_collection(name))


def sub_request(name, *, expr="", metric=UNSET, anns="vector", topk="2", group=None):
    placeholder = common_pb2.PlaceholderGroup(placeholders=[common_pb2.PlaceholderValue(
        tag="$0", type=101, values=[struct.pack("<ff", 1.0, 0.0)])])
    params = [("anns_field", anns), ("topk", topk)]
    if metric is not UNSET:
        params.append(("metric_type", metric))
    if group is not None:
        params.append(("group_by_field", group))
    return milvus_pb2.SearchRequest(
        collection_name=name, dsl=expr, dsl_type=1, placeholder_group=placeholder.SerializeToString(),
        search_params=[common_pb2.KeyValuePair(key=k, value=v) for k, v in params],
    )


def hybrid_request(name, subs):
    rank = [("strategy", "rrf"), ("params", '{"k":60}'), ("limit", "2"), ("round_decimal", "-1"), ("offset", "0")]
    return milvus_pb2.HybridSearchRequest(
        collection_name=name, requests=subs,
        rank_params=[common_pb2.KeyValuePair(key=k, value=v) for k, v in rank],
    )


def outcome(reply):
    status = reply.status
    results = reply.results
    ids = list(results.ids.int_id.data) if results.ids.HasField("int_id") else []
    return (status.code, status.error_code, status.reason, ids), list(results.scores)


def assert_same(rust, ref):
    rust_key, rust_scores = outcome(rust)
    ref_key, ref_scores = outcome(ref)
    assert rust_key == ref_key
    assert len(rust_scores) == len(ref_scores)
    for a, b in zip(rust_scores, ref_scores):
        assert math.isclose(a, b, rel_tol=0, abs_tol=1e-5), (rust_scores, ref_scores)


@pytest.fixture
def raw(rust_uri, ref_uri):
    channels = [
        grpc.insecure_channel(uri.removeprefix("http://"), options=[("grpc.enable_http_proxy", 0)])
        for uri in (rust_uri, ref_uri)
    ]
    try:
        yield [milvus_pb2_grpc.MilvusServiceStub(channel) for channel in channels]
    finally:
        for channel in channels:
            channel.close()


@pytest.mark.parametrize("metric", ["l2", "Ip", "cOsInE", "COSINE", "", "null", UNSET, "BAD", "bm25", "none",
                                    "123", "1.5", "true", "[1]", '{"a":1}',
                                    "NaN", "Infinity", "-Infinity", " NaN ", '"NaN"'])
@pytest.mark.parametrize("index_metric", [None, "L2", "IP"])
def test_search_metric_forms(diff, raw, metric, index_metric):
    create(diff, "m", index_metric)
    req = sub_request("m", metric=metric)
    assert_same(raw[0].Search(req), raw[1].Search(req))


@pytest.mark.parametrize("metric", ["l2", "", "null", UNSET, "BAD", "123", "NaN"])
def test_hybrid_route_metric_forms(diff, raw, metric):
    create(diff, "m", "L2")
    req = hybrid_request("m", [sub_request("m", metric=metric)])
    assert_same(raw[0].HybridSearch(req), raw[1].HybridSearch(req))


@pytest.mark.parametrize("request_kwargs", [
    pytest.param({"anns": "nope"}, id="anns"),
    pytest.param({"anns": "cat"}, id="anns-not-vector"),
    pytest.param({"metric": "BAD"}, id="metric"),
    pytest.param({"metric": "BAD", "anns": "nope"}, id="anns-before-metric"),
    pytest.param({"metric": "BAD", "expr": "id = 1"}, id="metric-before-filter"),
    pytest.param({"metric": "BAD", "group": "nope"}, id="metric-before-group"),
    pytest.param({"anns": "nope", "group": "nope"}, id="anns-before-group"),
    pytest.param({"expr": "id = 1", "group": "nope"}, id="group-before-filter"),
    pytest.param({"metric": "123", "topk": "x"}, id="topk-before-metric-type"),
    pytest.param({"metric": "NaN", "anns": "nope"}, id="nonfinite-metric-type-before-anns"),
])
@pytest.mark.parametrize("loaded", [True, False], ids=["loaded", "released"])
def test_search_error_order(diff, raw, request_kwargs, loaded):
    create(diff, "o", "L2", load=loaded)
    req = sub_request("o", **request_kwargs)
    assert_same(raw[0].Search(req), raw[1].Search(req))


def with_function_chains_conflict(req):
    req.function_score.SetInParent()
    # SearchRequest.function_chains（字段 24）里的空 FunctionChain；pymilvus 2.6 的 proto 没有这个字段，按字段号写入。
    req.MergeFromString(bytes([24 << 3 & 0x7F | 0x80 | 2, 24 << 3 >> 7, 0]))
    return req


@pytest.mark.parametrize("request_kwargs", [
    pytest.param({}, id="conflict"),
    pytest.param({"metric": "BAD"}, id="metric-before-conflict"),
    pytest.param({"anns": "nope"}, id="anns-before-conflict"),
    pytest.param({"expr": "id = 1"}, id="conflict-before-filter"),
])
@pytest.mark.parametrize("loaded", [True, False], ids=["loaded", "released"])
def test_function_chains_conflict_order(diff, raw, request_kwargs, loaded):
    create(diff, "fc", "L2", load=loaded)
    req = with_function_chains_conflict(sub_request("fc", **request_kwargs))
    rust, ref = raw[0].Search(req), raw[1].Search(req)
    assert_same(rust, ref)
    assert ref.status.code != 0


@pytest.mark.parametrize("routes", [
    pytest.param([{"expr": "id = 1"}, {"metric": "BAD"}], id="first-filter-before-second-metric"),
    pytest.param([{"metric": "BAD"}, {"expr": "id = 1"}], id="first-metric"),
    pytest.param([{}, {"anns": "nope"}], id="second-anns"),
    pytest.param([{"anns": "nope"}], id="anns"),
])
@pytest.mark.parametrize("loaded", [True, False], ids=["loaded", "released"])
def test_hybrid_error_order(diff, raw, routes, loaded):
    create(diff, "h", "L2", load=loaded)
    req = hybrid_request("h", [sub_request("h", **route) for route in routes])
    assert_same(raw[0].HybridSearch(req), raw[1].HybridSearch(req))


@pytest.mark.parametrize("requested", [UNSET, "L2"], ids=["omitted", "explicit"])
def test_none_metric_index_only_fails_without_a_request_metric(diff, raw, requested):
    diff.step("create", lambda c: c.create_collection("n", schema=schema()))
    diff.step("insert", lambda c: c.insert("n", ROWS))
    def add_index(c):
        params = c.prepare_index_params()
        params.add_index("vector", index_type="FLAT", metric_type="NONE")
        c.create_index("n", params)
    diff.step("index", add_index)
    diff.step("load", lambda c: c.load_collection("n"))
    req = sub_request("n", metric=requested)
    assert_same(raw[0].Search(req), raw[1].Search(req))


@pytest.mark.parametrize("metric", ["cosine", "Ip", "bad", ""])
def test_create_index_metric_case(diff, metric):
    diff.step("create", lambda c: c.create_collection("ci", schema=schema()))
    def add_index(c):
        params = c.prepare_index_params()
        params.add_index("vector", index_type="FLAT", metric_type=metric)
        c.create_index("ci", params)
    rust, ref = run(add_index, diff.rust), run(add_index, diff.ref)
    assert rust == ref
    if rust[0] == "ok":
        diff.step("describe", lambda c: c.describe_index("ci", c.list_indexes("ci")[0]))


@pytest.mark.parametrize("output_fields", [None, [], ["cat"], ["id"], ["id", "cat"], ["cat", "id"], ["*"], ["id", "id"]],
                         ids=str)
def test_search_primary_key_projection(diff, output_fields):
    create(diff, "p", "L2")
    vectors = {row["id"]: row["vector"] for row in ROWS}
    diff.step("search", lambda c: c.search("p", data=[[1.0, 0.0]], limit=2, output_fields=output_fields),
              hits("L2", [[1.0, 0.0]], vectors, k=2))


@pytest.mark.parametrize("output_fields", [["cat"], ["id", "cat"], ["*"]], ids=str)
def test_hybrid_primary_key_projection(diff, output_fields):
    create(diff, "p", "L2")
    def search(c):
        req = AnnSearchRequest(data=[[1.0, 0.0]], anns_field="vector", param={"metric_type": "L2"}, limit=2)
        return c.hybrid_search("p", reqs=[req], ranker=RRFRanker(), limit=2, output_fields=output_fields)
    diff.step("hybrid", search)


@pytest.mark.parametrize("names", [["p1"], ["_default", "p1"], ["p1", "p1"], ["_default", "p1", "_default"], ["nope"],
                                   ["p1", "nope"]], ids=str)
@pytest.mark.parametrize("flushed", [False, True], ids=["active", "flushed"])
@pytest.mark.parametrize("loaded", [True, False], ids=["loaded", "released"])
def test_unfiltered_count_with_partition_names(diff, names, loaded, flushed):
    create(diff, "cp", "L2")
    diff.step("partition", lambda c: c.create_partition("cp", "p1"))
    diff.step("insert_p1", lambda c: c.insert("cp", [
        {"id": 100 + i, "vector": [1.0, 0.0], "cat": "p"} for i in range(4)
    ], partition_name="p1"))
    diff.step("delete_one", lambda c: c.delete("cp", ids=[100]))
    diff.step("same_pk_in_p1", lambda c: c.insert("cp", [{"id": 1, "vector": [1.0, 0.0], "cat": "p"}], partition_name="p1"))
    if flushed:
        diff.step("flush", lambda c: c.flush("cp"))
    if not loaded:
        diff.step("release", lambda c: c.release_collection("cp"))
    fails = "nope" in names
    diff.step("count", lambda c: c.query("cp", filter="", output_fields=["count(*)"], partition_names=names), error=fails)
