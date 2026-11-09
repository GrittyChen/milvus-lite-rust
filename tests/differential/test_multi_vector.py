"""多向量字段：目标字段、投影、索引、Hybrid 与写入错误，逐项对照 3.x（0028）。"""

import grpc
import pytest
from pymilvus import AnnSearchRequest, DataType, MilvusClient, RRFRanker, WeightedRanker
from pymilvus.grpc_gen import milvus_pb2, milvus_pb2_grpc, schema_pb2

from compare import exact, hits, run


def schema(order=("v1", "v2"), dims=(4, 8)):
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("s", DataType.VARCHAR, max_length=16)
    for name, dim in zip(order, dims):
        s.add_field(name, DataType.FLOAT_VECTOR, dim=dim)
    return s


def unit(dim, at, length=1.0):
    v = [0.0] * dim
    v[at] = length
    return v


ROWS = [
    {"id": i, "s": f"s{i % 2}", "v1": unit(4, i % 4, 1.0 + i / 10), "v2": unit(8, (7 - i) % 8, 2.0 + i / 10)}
    for i in range(6)
]
V1 = {r["id"]: r["v1"] for r in ROWS}
V2 = {r["id"]: r["v2"] for r in ROWS}
# 相似度被截断到 1 以内，查询和候选共线时错误的范数看不出来，所以两个查询都不与任何候选共线。
Q1 = [[0.6, 0.8, 0.0, 0.0]]
Q2 = [[0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.6, 0.8]]


def flat(*definitions):
    params = MilvusClient.prepare_index_params()
    for field, metric in definitions:
        params.add_index(field_name=field, index_type="FLAT", metric_type=metric)
    return params


def create(diff, name="c", index=None, flushed=False, **kw):
    diff.step("create", lambda c: c.create_collection(name, schema=schema(**kw), index_params=index))
    diff.step("insert", lambda c: c.insert(name, ROWS))
    if index is not None:
        diff.step("load", lambda c: c.load_collection(name))
    if flushed:
        diff.step("flush", lambda c: c.flush(name))


STATES = ["memtable", "flushed"]


@pytest.mark.parametrize("flushed", STATES)
@pytest.mark.parametrize("field,query,vectors,metric,k", [
    ("v1", Q1, V1, "COSINE", 2),
    ("v2", Q2, V2, "COSINE", 2),
    ("v2", Q2, V2, "L2", 3),
    ("v2", Q2, V2, "IP", 2),
    ("v1", Q1, V1, "L2", 3),
])
def test_search_uses_the_target_field(diff, flushed, field, query, vectors, metric, k):
    create(diff, flushed=flushed == "flushed")
    diff.step("search", lambda c: c.search("c", query, anns_field=field, limit=k, output_fields=["s"],
                                           search_params={"metric_type": metric}),
              hits(metric, query, vectors, k=k))


@pytest.mark.parametrize("flushed", STATES)
def test_default_field_is_the_first_vector_in_schema_order(diff, flushed):
    create(diff, flushed=flushed == "flushed")
    diff.step("first", lambda c: c.search("c", Q1, limit=2, output_fields=["s"]), hits("COSINE", Q1, V1, k=2))


def test_declaring_the_longer_vector_first_changes_the_default(diff):
    create(diff, order=("v2", "v1"), dims=(8, 4))
    diff.step("v2_default", lambda c: c.search("c", Q2, limit=2, output_fields=["s"]), hits("COSINE", Q2, V2, k=2))


@pytest.mark.parametrize("field", ["v1", "v2"])
def test_query_vector_of_the_wrong_dimension_keeps_the_registered_message_difference(diff, field):
    create(diff)
    wrong = Q2 if field == "v1" else Q1
    rust = run(lambda c: c.search("c", wrong, anns_field=field, limit=1), diff.rust)
    ref = run(lambda c: c.search("c", wrong, anns_field=field, limit=1), diff.ref)
    assert rust[:3] == ("err", 1, 0) == ref[:3]
    assert field in rust[3]


@pytest.mark.parametrize("field,error", [("nope", "not found"), ("s", "not a vector")])
def test_invalid_target_field(diff, field, error):
    create(diff)
    out = diff.step("search", lambda c: c.search("c", Q1, anns_field=field, limit=1), error=True)
    assert error in out[3]


OUTPUTS = [{}, {"output_fields": ["*"]}, {"output_fields": ["v2"]}, {"output_fields": ["v2", "v1"]}, {"output_fields": ["s"]}]


@pytest.mark.parametrize("flushed", STATES)
@pytest.mark.parametrize("kw", OUTPUTS, ids=["default", "star", "v2", "both", "s"])
def test_projection_of_every_vector_field(diff, flushed, kw):
    create(diff, flushed=flushed == "flushed")
    diff.step("search", lambda c: c.search("c", Q2, anns_field="v2", limit=1, **kw), hits("COSINE", Q2, V2, k=1))
    diff.step("query", lambda c: c.query("c", filter="id == 1", **kw), exact)
    diff.step("get", lambda c: c.get("c", ids=[1], **kw), exact)


@pytest.mark.parametrize("expr", ["v2 is null", "v2 == 1", "v1 > 0"])
def test_no_vector_field_is_filterable(diff, expr):
    create(diff)
    diff.step("query", lambda c: c.query("c", filter=expr), error=True)


@pytest.mark.parametrize("field", ["v1", "v2"])
def test_no_vector_field_can_group(diff, field):
    create(diff)
    out = diff.step("search", lambda c: c.search("c", Q1, anns_field="v1", limit=2, group_by_field=field), error=True)
    assert field in out[3]


@pytest.mark.parametrize("flushed", STATES)
def test_partition_key_collection(diff, flushed):
    def make(c):
        s = schema()
        s.add_field("k", DataType.INT64, is_partition_key=True)
        c.create_collection("c", schema=s)
    diff.step("create", make)
    rows = [{**r, "k": r["id"] % 3} for r in ROWS]
    diff.step("insert", lambda c: c.insert("c", rows)["insert_count"], exact)
    if flushed == "flushed":
        diff.step("flush", lambda c: c.flush("c"))
    diff.step("search", lambda c: c.search("c", Q2, anns_field="v2", limit=3, output_fields=["k"]),
              hits("COSINE", Q2, V2, k=3))
    new_v2 = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]
    diff.step("upsert", lambda c: c.upsert("c", [{"id": 1, "k": 1, "s": "u", "v1": unit(4, 0), "v2": new_v2}])["upsert_count"], exact)
    diff.step("read", lambda c: c.query("c", filter="id == 1", output_fields=["*"]), exact)
    diff.step("search upserted", lambda c: c.search("c", Q2, anns_field="v2", limit=2, output_fields=["k"]),
              hits("COSINE", Q2, {**V2, 1: new_v2}, k=2))


@pytest.mark.parametrize("flushed", STATES)
def test_explicit_partition(diff, flushed):
    create(diff)
    diff.step("partition", lambda c: c.create_partition("c", "p1"))
    extra = {"id": 100, "s": "p", "v1": unit(4, 1, 3.0), "v2": unit(8, 6, 7.0)}
    diff.step("insert", lambda c: c.insert("c", [extra], partition_name="p1")["insert_count"], exact)
    if flushed == "flushed":
        diff.step("flush", lambda c: c.flush("c"))
    vectors = {**V2, 100: extra["v2"]}
    diff.step("only p1", lambda c: c.search("c", Q2, anns_field="v2", limit=1, partition_names=["p1"],
                                            output_fields=["s"]), hits("COSINE", Q2, vectors, k=1))
    diff.step("all", lambda c: c.search("c", Q2, anns_field="v2", limit=3, output_fields=["s"]),
              hits("COSINE", Q2, vectors, k=3))
    diff.step("drop", lambda c: (c.release_collection("c"), c.drop_partition("c", "p1"), c.load_collection("c"))[1])
    diff.step("after drop", lambda c: c.search("c", Q2, anns_field="v2", limit=2, output_fields=["s"]),
              hits("COSINE", Q2, V2, k=2))


def test_group_range_filter_and_offset_on_a_later_field(diff):
    create(diff)
    diff.step("group", lambda c: c.search("c", Q2, anns_field="v2", limit=2, group_by_field="s",
                                          output_fields=["s"], search_params={"metric_type": "IP"}),
              hits("IP", Q2, V2, k=2))
    diff.step("range", lambda c: c.search("c", Q2, anns_field="v2", limit=3, output_fields=["s"],
                                          search_params={"metric_type": "L2",
                                                         "params": {"radius": 100.0, "range_filter": 0.0}}),
              hits("L2", Q2, V2, k=3))
    diff.step("filter", lambda c: c.search("c", Q2, anns_field="v2", limit=2, filter="id > 2", output_fields=["s"],
                                           search_params={"metric_type": "IP"}),
              hits("IP", Q2, V2, k=2))
    diff.step("offset", lambda c: c.search("c", Q2, anns_field="v2", limit=2, output_fields=["s"],
                                           search_params={"metric_type": "IP", "offset": 1}),
              hits("IP", Q2, V2, k=2, offset=1))


def test_each_field_has_its_own_index_and_metric(diff):
    create(diff, index=flat(("v1", "L2"), ("v2", "IP")))
    diff.step("v1", lambda c: c.search("c", Q1, limit=2, output_fields=["s"]), hits("L2", Q1, V1, k=2))
    diff.step("v2", lambda c: c.search("c", Q2, anns_field="v2", limit=2, output_fields=["s"]), hits("IP", Q2, V2, k=2))
    diff.step("list", lambda c: sorted(c.list_indexes("c")), exact)
    diff.step("list_field", lambda c: c.list_indexes("c", field_name="v2"), exact)
    for name in ("v1", "v2", "v2_idx"):
        diff.step(f"describe {name}", lambda c, n=name: c.describe_index("c", n), exact)


def test_a_field_without_an_index_defaults_to_cosine(diff):
    create(diff, index=flat(("v1", "L2")))
    diff.step("v1 metric", lambda c: c.search("c", Q1, limit=2, output_fields=["s"]), hits("L2", Q1, V1, k=2))
    diff.step("v2 metric", lambda c: c.search("c", Q2, anns_field="v2", limit=2, output_fields=["s"]),
              hits("COSINE", Q2, V2, k=2))


def test_indexing_a_second_field_keeps_the_collection_loaded(diff):
    create(diff, index=flat(("v1", "L2")))
    diff.step("create v2", lambda c: c.create_index("c", flat(("v2", "IP"))))
    diff.step("state", lambda c: c.get_load_state("c"), exact)
    diff.step("search", lambda c: c.search("c", Q2, anns_field="v2", limit=2, output_fields=["s"]),
              hits("IP", Q2, V2, k=2))


@pytest.mark.parametrize("dropped,left", [("v2_idx", ["v1"]), ("v2", [])])
def test_drop_index_by_name(diff, dropped, left):
    # 只有 `<字段名>_idx` 定向删除；字段名删除全部索引（0014）。
    create(diff, index=flat(("v1", "L2"), ("v2", "IP")))
    diff.step("drop while loaded", lambda c: c.drop_index("c", dropped), error=True)
    diff.step("release", lambda c: c.release_collection("c"))
    diff.step("released search", lambda c: c.search("c", Q2, anns_field="v2", limit=1), error=True)
    diff.step("drop", lambda c: c.drop_index("c", dropped))
    listed = diff.step("list", lambda c: sorted(c.list_indexes("c")), exact)
    assert listed == ("ok", left)
    diff.step("load", lambda c: c.load_collection("c"))
    metric = "L2" if left else "COSINE"
    diff.step("v1", lambda c: c.search("c", Q1, limit=2, output_fields=["s"]), hits(metric, Q1, V1, k=2))


@pytest.mark.parametrize("flushed", STATES)
def test_hybrid_routes_use_their_own_fields(diff, flushed):
    create(diff, index=flat(("v1", "L2"), ("v2", "COSINE")), flushed=flushed == "flushed")
    v1 = AnnSearchRequest(Q1, "v1", {"metric_type": "L2"}, 5)
    v2 = AnnSearchRequest(Q2, "v2", {"metric_type": "COSINE"}, 5)
    for label, ranker in [("rrf", RRFRanker()), ("weighted", WeightedRanker(0.3, 0.7))]:
        diff.step(label, lambda c, r=ranker: c.hybrid_search("c", [v1, v2], r, limit=4, output_fields=["s", "v2"]),
                  scored)


def test_hybrid_same_field_twice_and_invalid_route(diff):
    create(diff, index=flat(("v1", "L2"), ("v2", "COSINE")))
    a = AnnSearchRequest(Q1, "v1", {"metric_type": "L2"}, 5)
    b = AnnSearchRequest([unit(4, 1)], "v1", {"metric_type": "L2"}, 5)
    diff.step("twice", lambda c: c.hybrid_search("c", [a, b], RRFRanker(), limit=3), scored)
    bad = AnnSearchRequest(Q1, "nope", {"metric_type": "L2"}, 5)
    diff.step("unknown", lambda c: c.hybrid_search("c", [a, bad], RRFRanker(), limit=3), error=True)


def scored(a, b):
    """Hybrid 的分数按字段合并：同分时的先后、0004 的允许误差都已在距离检索的用例覆盖，这里比结果与分数。"""
    if a[0] != "ok" or b[0] != "ok":
        return exact(a, b)
    for qa, qb in zip(a[1], b[1], strict=True):
        if [h["id"] for h in qa] != [h["id"] for h in qb] or [h["entity"] for h in qa] != [h["entity"] for h in qb]:
            return "hits differ"
        if any(abs(x["distance"] - y["distance"]) > 1e-5 for x, y in zip(qa, qb, strict=True)):
            return "scores differ"
    return None


@pytest.mark.parametrize("flushed", STATES)
def test_upsert_keeps_or_replaces_each_vector_independently(diff, flushed):
    create(diff, flushed=flushed == "flushed")
    new_v2 = [9.0] * 8
    diff.step("only v2", lambda c: c.upsert("c", [{"id": 1, "v2": new_v2}], partial_update=True)["upsert_count"], exact)
    diff.step("read v2", lambda c: c.query("c", filter="id == 1", output_fields=["*"]), exact)
    diff.step("only v1", lambda c: c.upsert("c", [{"id": 1, "v1": [7.0] * 4}], partial_update=True)["upsert_count"], exact)
    diff.step("read v1", lambda c: c.query("c", filter="id == 1", output_fields=["*"]), exact)
    diff.step("flush", lambda c: c.flush("c"))
    q = [unit(8, 0)]
    diff.step("search", lambda c: c.search("c", q, anns_field="v2", limit=1, output_fields=["v1", "v2"]),
              hits("COSINE", q, {**V2, 1: new_v2}, k=1))


@pytest.mark.parametrize("n", [3, 11])
def test_no_limit_on_the_number_of_vector_fields(diff, n):
    def create_many(c):
        s = MilvusClient.create_schema(auto_id=False)
        s.add_field("id", DataType.INT64, is_primary=True)
        for i in range(n):
            s.add_field(f"v{i}", DataType.FLOAT_VECTOR, dim=2)
        c.create_collection("many", schema=s)
    diff.step("create", create_many)
    row = {f"v{i}": [3.0 * (i + 1), 4.0 * (i + 1)] for i in range(n)}
    diff.step("insert", lambda c: c.insert("many", [{"id": 1, **row}])["insert_count"], exact)
    q = [[1.0, 0.0]]
    diff.step("last", lambda c: c.search("many", q, anns_field=f"v{n - 1}", limit=1, output_fields=["id"]),
              hits("COSINE", q, {1: row[f"v{n - 1}"]}, k=1))


# ---------------------------------------------------------------- 写入错误（原始 RPC，SDK 绕不过的情形）


def ids(*values):
    return schema_pb2.FieldData(field_name="id", type=5, scalars=schema_pb2.ScalarField(long_data=schema_pb2.LongArray(data=list(values))))


def texts(n=1):
    return schema_pb2.FieldData(field_name="s", type=21, scalars=schema_pb2.ScalarField(string_data=schema_pb2.StringArray(data=["x"] * n)))


def vectors(name, dim, data):
    return schema_pb2.FieldData(field_name=name, type=101, vectors=schema_pb2.VectorField(dim=dim, float_vector=schema_pb2.FloatArray(data=data)))


def scalar_column(name):
    return schema_pb2.FieldData(field_name=name, type=5, scalars=schema_pb2.ScalarField(long_data=schema_pb2.LongArray(data=[1])))


@pytest.fixture
def stubs(rust_uri, ref_uri):
    channels = [grpc.insecure_channel(u.removeprefix("http://"), options=[("grpc.enable_http_proxy", 0)]) for u in (rust_uri, ref_uri)]
    try:
        yield [milvus_pb2_grpc.MilvusServiceStub(c) for c in channels]
    finally:
        for c in channels:
            c.close()


def insert(stub, fields, n=1, upsert=False):
    cls = milvus_pb2.UpsertRequest if upsert else milvus_pb2.InsertRequest
    call = stub.Upsert if upsert else stub.Insert
    status = call(cls(collection_name="c", fields_data=fields, num_rows=n), timeout=30).status
    return status.code, status.error_code, status.reason


GOOD_V1 = vectors("v1", 4, unit(4, 0))
GOOD_V2 = vectors("v2", 8, unit(8, 7))


@pytest.mark.parametrize("fields", [
    pytest.param([ids(100), texts(), GOOD_V1, vectors("v2", 4, unit(4, 0))], id="later-wrong-dim"),
    pytest.param([ids(100), texts(), vectors("v1", 8, unit(8, 0)), GOOD_V2], id="first-wrong-dim"),
    pytest.param([ids(100), texts(), vectors("v1", 8, unit(8, 0)), vectors("v2", 4, unit(4, 0))], id="both-wrong-dim"),
    pytest.param([ids(100), texts(), GOOD_V2], id="missing-first"),
    pytest.param([ids(100), texts()], id="missing-both"),
    pytest.param([ids(100), GOOD_V1], id="missing-scalar-before-later-vector"),
    pytest.param([ids(100), texts(), GOOD_V1, vectors("v2", 8, [1.0] * 4)], id="later-short-data"),
    pytest.param([ids(100), texts(), GOOD_V1, vectors("v2", 0, [])], id="later-zero-dim"),
])
def test_write_errors_match_3x(diff, stubs, fields):
    create(diff)
    rust, ref = insert(stubs[0], fields), insert(stubs[1], fields)
    assert rust == ref
    assert rust[0] != 0
    diff.step("no row added", lambda c: c.query("c", filter="id == 100"), exact)


@pytest.mark.parametrize("fields,reason", [
    pytest.param([ids(100), texts(), GOOD_V1], "vector field 'v2' missing or None", id="absent"),
])
def test_missing_later_vector_is_rejected_unlike_3x(diff, stubs, fields, reason):
    """登记的差异（0028）：3.x 接受这种写入，flush 后输出该字段的读取失败。"""
    create(diff)
    assert insert(stubs[0], fields) == (6, 6, reason)
    assert insert(stubs[1], fields) == (0, 0, "")


def test_missing_later_vector_in_a_new_upsert_row_is_rejected_unlike_3x(diff):
    create(diff)
    row = [{"id": 50, "s": "n", "v1": unit(4, 0)}]
    rust = run(lambda c: c.upsert("c", row, partial_update=True)["upsert_count"], diff.rust)
    ref = run(lambda c: c.upsert("c", row, partial_update=True)["upsert_count"], diff.ref)
    assert rust == ("err", 6, 6, "vector field 'v2' missing or None")
    assert ref == ("ok", 1)


def test_scalar_column_for_a_later_vector_keeps_the_registered_message_difference(diff, stubs):
    create(diff)
    fields = [ids(100), texts(), GOOD_V1, scalar_column("v2")]
    assert insert(stubs[0], fields) == (6, 6, "vector field 'v2' must be list/tuple, got int")
    assert insert(stubs[1], fields)[0] == 1
