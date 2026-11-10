"""text_match 与 enable_analyzer / enable_match / analyzer_params：逐项对照 3.x（0029）。"""

import grpc
import pytest
from pymilvus import (
    AnnSearchRequest,
    DataType,
    Function,
    FunctionScore,
    FunctionType,
    MilvusClient,
    RRFRanker,
)
from pymilvus.grpc_gen import common_pb2, milvus_pb2, milvus_pb2_grpc, schema_pb2

from compare import exact, run

BIG = "99999999999999999999"
ROWS = [
    dict(id=1, v=[1.0, 0.0], t="Hello, World!", m="hello world", j={"K": "Hello World", "中": "文", "n": 1.0, "b": True},
         a=["hello", "x"], n=1, f=1.5, b=True, ts="2025-01-01T00:00:00Z", dyn="hello there", d2=5),
    dict(id=2, v=[0.0, 1.0], t=None, m="foo", j=None, a=None, n=None, f=None, b=None, ts=None, dyn=None),
    dict(id=3, v=[1.0, 1.0], t="", m="bar", j={"p": "plain hello"}, a=[], n=2, f=2.5, b=False, ts="2025-01-02T00:00:00Z", d2=1),
    dict(id=4, v=[0.0, 0.5], t="ΣΑΣ İstanbul café résumé x²y foo_bar 中文", m="中文 baz", j={"k": [1, "hello"]}, n=3),
    dict(id=5, v=[2.0, 0.0], t="HELLO", m="qux", j={"z": "hello"}, n=4, d2=9),
]


def schema(dynamic=True):
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=dynamic)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("t", DataType.VARCHAR, max_length=256, nullable=True)
    s.add_field("m", DataType.VARCHAR, max_length=256, enable_analyzer=True, enable_match=True,
                analyzer_params={"tokenizer": {"type": "jieba"}, "filter": [{"type": "stop", "stop_words": ["hello"]}]})
    s.add_field("j", DataType.JSON, nullable=True)
    s.add_field("a", DataType.ARRAY, element_type=DataType.VARCHAR, max_capacity=4, max_length=16, nullable=True)
    s.add_field("n", DataType.INT64, nullable=True)
    s.add_field("f", DataType.FLOAT, nullable=True)
    s.add_field("b", DataType.BOOL, nullable=True)
    s.add_field("ts", DataType.TIMESTAMPTZ, nullable=True)
    return s


def create(diff, state, name="c", dynamic=True, rows=ROWS):
    index = MilvusClient.prepare_index_params()
    index.add_index(field_name="v", index_type="FLAT", metric_type="L2")
    diff.step("create", lambda c: c.create_collection(name, schema=schema(dynamic), index_params=index))
    diff.step("insert", lambda c: c.insert(name, rows))
    diff.step("load", lambda c: c.load_collection(name))
    if state == "flushed":
        diff.step("flush", lambda c: c.flush(name))


def ids(rows):
    return sorted(r["id"] for r in rows)


STATES = ["memtable", "flushed"]
MATCHING = """text_match(t, 'hello')
TEXT_MATCH(t, "hello")
text_match(t, 'missing hello')
text_match(t, 'world!')
text_match(t, '')
text_match(t, '!!!')
not text_match(t, 'hello')
not text_match(t, '')
text_match(m, 'hello')
text_match(m, '中文')
text_match(t, 'σασ')
text_match(t, 'ΣΑΣ')
text_match(t, 'σας')
text_match(t, 'i')
text_match(t, 'İstanbul')
text_match(t, 'stanbul')
text_match(t, 'cafe')
text_match(t, 'résumé')
text_match(t, 're')
text_match(t, 'sume')
text_match(t, 'x²y')
text_match(t, 'foo_bar')
text_match(t, 'foo')
text_match(j, 'hello')
text_match(j, 'k')
text_match(j, 'K')
text_match(j, '中')
text_match(j, 'u4e2d')
text_match(j, 'true')
text_match(j, '1')
text_match(j, '0')
text_match(j, 'plain')
text_match(a, 'hello')
text_match(n, '1')
text_match(f, '1')
text_match(b, 'true')
text_match(ts, '2025')
text_match(id, '1')
text_match(t, 'hello') and d2 > 1
text_match(t, 'hello') or d2 > 8
d2 > 1 and text_match(t, 'hello')
text_match(t, 'hello') and n > 0
not (text_match(t, 'hello') and n > 0)
text_match(t, 'hello') == true
text_match(t, 'hello') != true
true == text_match(t, 'hello')
text_match(t, 'hello') and array_length(a) > 0
text_match(t, 'hello') and j['K'] == 'Hello World'
text_match(t, 'hello') and t > 'A'
t > 'A' and text_match(t, 'hello')
n > 1 and text_match(n, '1')
text_match(t, 'hello') and f == 1.5
text_match(t, 'a\\'b')
text_match(t, 'hello\\nworld')
text_match(j, 'hello') and text_match(t, 'hello')""".split("\n") + [
    f"text_match(t, 'hello') and n == {BIG}",
    f"text_match(t, 'hello') or n == {BIG}",
]
# 排在 text_match 前的列式操作数先报错；这组结果依赖 3.x 的 hybrid 路线。
ORDER_ERRORS = [f"n == {BIG} and text_match(t, 'hello')", f"n > {BIG} or text_match(t, 'hello')"]
ERRORS = """text_match(v, 'x')
text_match(dyn, 'hello')
text_match(d2, 'hello')
text_match($meta, 'hello')
text_match(_seq, '1')
text_match(_partition, '1')
text_match(nosuch, 'hello')
text_match(j['K'], 'hello')
text_match(t, 'hello') == 1
text_match(t, 'hello') == 'a'
text_match(t, 'hello') in [true]
text_match(t, 'hello') is null
text_match(t, 'hello') like 'a'
text_match(t, 1)
text_match(t)
text_match(t, 'a', 'b')
text_match(nosuch, 'x') and nosuch2 > 1
t == 1 and text_match(t, 'hello')""".split("\n")


@pytest.mark.parametrize("state", STATES)
def test_filter_matrix(diff, state):
    create(diff, state)
    for expr in MATCHING:
        diff.step(expr, lambda c: ids(c.query("c", filter=expr, output_fields=["id"], limit=100)), exact)
    for expr in ORDER_ERRORS + ERRORS:
        diff.step(expr, lambda c: c.query("c", filter=expr, output_fields=["id"], limit=100), error=True)


def test_results_are_not_vacuous(diff):
    create(diff, "memtable")
    for expr, expected in [("text_match(t, 'hello')", [1, 5]), ("not text_match(t, 'hello')", [2, 3, 4]),
                           ("text_match(j, 'k')", [1, 4]), ("text_match(t, 'σας')", [4]), ("text_match(t, 'σασ')", [])]:
        assert ids(diff.rust.query("c", filter=expr, output_fields=["id"], limit=100)) == expected


@pytest.mark.parametrize("state", STATES)
def test_collection_without_dynamic_fields(diff, state):
    rows = [{k: v for k, v in r.items() if k not in ("dyn", "d2")} for r in ROWS]
    create(diff, state, dynamic=False, rows=rows)
    for expr in ["text_match(t, 'hello')", "not text_match(t, 'hello')"]:
        diff.step(expr, lambda c: ids(c.query("c", filter=expr, output_fields=["id"], limit=100)), exact)
    for expr in ["text_match(dyn, 'hello')", "text_match(nosuch, 'hello')", "text_match(_seq, '1')"]:
        diff.step(expr, lambda c: c.query("c", filter=expr, output_fields=["id"], limit=100), error=True)


@pytest.mark.parametrize("state", STATES)
def test_every_read_path_uses_the_filter(diff, state):
    create(diff, state)
    flt = "text_match(t, 'hello')"
    q = [[1.0, 0.0]]
    diff.step("search", lambda c: [[h["id"] for h in g] for g in c.search("c", data=q, filter=flt, limit=9, output_fields=["id"])], exact)
    diff.step("count", lambda c: c.query("c", filter=flt, output_fields=["count(*)"]), exact)
    diff.step("group", lambda c: [[h["id"] for h in g] for g in c.search("c", data=q, filter=flt, limit=9, group_by_field="n", output_fields=["id"])], exact)

    def hybrid(c):
        reqs = [AnnSearchRequest(q, "v", {"metric_type": "L2"}, 9, expr=flt)]
        return [[h["id"] for h in g] for g in c.hybrid_search("c", reqs, RRFRanker(), limit=9, output_fields=["id"])]

    diff.step("hybrid", hybrid, exact)

    def boosted(c):
        ranker = FunctionScore([Function("b", FunctionType.RERANK, [], params={"reranker": "boost", "weight": 0.01, "filter": flt})])
        return [[(h["id"], round(h["distance"], 6)) for h in g] for g in c.search("c", data=q, limit=9, ranker=ranker, search_params={"metric_type": "L2"}, output_fields=["id"])]

    diff.step("boost", boosted, exact)

    def iterate(c):
        iterator = c.query_iterator("c", batch_size=1, filter=flt, output_fields=["id"])
        found = []
        try:
            while page := iterator.next():
                found.extend(r["id"] for r in page)
        finally:
            iterator.close()
        return found

    diff.step("iterator", iterate, exact)
    diff.step("delete", lambda c: c.delete("c", filter="text_match(m, 'foo')"), exact)
    diff.step("after delete", lambda c: ids(c.query("c", filter="id > 0", output_fields=["id"], limit=9)), exact)


def test_partitions_and_multiple_segments(diff):
    create(diff, "memtable", rows=ROWS[:2])
    diff.step("flush one", lambda c: c.flush("c"))
    diff.step("partition", lambda c: c.create_partition("c", "p"))
    diff.step("insert p", lambda c: c.insert("c", ROWS[2:4], partition_name="p"))
    diff.step("flush two", lambda c: c.flush("c"))
    diff.step("insert last", lambda c: c.insert("c", ROWS[4:]))
    for expr in ["text_match(t, 'hello')", "text_match(j, 'hello') or d2 > 8"]:
        diff.step(expr, lambda c: ids(c.query("c", filter=expr, output_fields=["id"], limit=9)), exact)
        diff.step(f"p {expr}", lambda c: ids(c.query("c", filter=expr, partition_names=["p"], output_fields=["id"], limit=9)), exact)
    diff.step("upsert", lambda c: c.upsert("c", [{**ROWS[0], "t": "changed"}]))
    diff.step("after upsert", lambda c: ids(c.query("c", filter="text_match(t, 'hello')", output_fields=["id"], limit=9)), exact)


def test_boost_binds_id_to_the_primary_key_value(diff):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("$id", DataType.VARCHAR, max_length=32)
    index = MilvusClient.prepare_index_params()
    index.add_index(field_name="v", index_type="FLAT", metric_type="L2")
    diff.step("create", lambda c: c.create_collection("c", schema=s, index_params=index))
    diff.step("insert", lambda c: c.insert("c", [{"id": 1, "v": [1.0, 0.2], "$id": "123"}, {"id": 2, "v": [1.0, 0.1], "$id": "456"}]))

    def boosted(flt):
        def call(c):
            ranker = FunctionScore([Function("b", FunctionType.RERANK, [], params={"reranker": "boost", "weight": 0.0001, "filter": flt})])
            result = c.search("c", data=[[1.0, 0.0]], limit=9, ranker=ranker, search_params={"metric_type": "L2"}, output_fields=["*"])
            return [(h["id"], round(h["distance"], 7)) for h in result[0]]
        return call

    for flt in ["text_match(id, '123')", "text_match(id, '1')"]:
        diff.step(flt, boosted(flt), exact)


# ---- 建表属性：pymilvus 只保留部分类型上的属性，这里走原始 gRPC ----

@pytest.fixture
def stubs(rust_uri, ref_uri):
    channels = [grpc.insecure_channel(u.removeprefix("http://"), options=[("grpc.enable_http_proxy", 0)]) for u in (rust_uri, ref_uri)]
    try:
        yield [milvus_pb2_grpc.MilvusServiceStub(c) for c in channels]
    finally:
        for c in channels:
            c.close()


def pairs(params):
    return [common_pb2.KeyValuePair(key=k, value=v) for k, v in params.items()]


def raw_field(dtype, params, primary=False):
    extra = {
        DataType.VARCHAR: {"max_length": "32"},
        DataType.FLOAT_VECTOR: {"dim": "2"},
        DataType.ARRAY: {"max_capacity": "4"},
    }.get(dtype, {})
    field = schema_pb2.FieldSchema(name="t", data_type=dtype, is_primary_key=primary, type_params=pairs({**extra, **params}))
    if dtype == DataType.ARRAY:
        field.element_type = DataType.INT32
    return field


def create_raw(stub, name, field):
    fields = [field]
    if not field.is_primary_key:
        fields.append(schema_pb2.FieldSchema(name="id", data_type=DataType.INT64, is_primary_key=True))
    if field.data_type != DataType.FLOAT_VECTOR:
        fields.append(schema_pb2.FieldSchema(name="v", data_type=DataType.FLOAT_VECTOR, type_params=pairs({"dim": "2"})))
    blob = schema_pb2.CollectionSchema(name=name, fields=fields).SerializeToString()
    status = stub.CreateCollection(milvus_pb2.CreateCollectionRequest(collection_name=name, schema=blob))
    return status.code, status.error_code, status.reason


def described(stub, name):
    response = stub.DescribeCollection(milvus_pb2.DescribeCollectionRequest(collection_name=name))
    field = next(f for f in response.schema.fields if f.name == "t")
    return [(p.key, p.value) for p in field.type_params]


ACCEPTED = [
    {"enable_analyzer": "true"}, {"enable_analyzer": "True", "enable_match": "TRUE"}, {"enable_match": "1"}, {"enable_analyzer": "no"},
    {"enable_analyzer": "false", "enable_match": "false", "analyzer_params": '{"tokenizer":"standard"}'},
    {"analyzer_params": '{"b":1,"a":{"z":1.0,"y":"中"},"c":[1e5,null,1e16,1e-5,-0,-1e-999]}'},
    {"analyzer_params": "[1]"}, {"analyzer_params": "5"}, {"analyzer_params": '"x"'}, {"analyzer_params": "true"},
    {"analyzer_params": "{}"}, {"analyzer_params": "[]"}, {"analyzer_params": "0"}, {"analyzer_params": "-0"}, {"analyzer_params": "0.0"},
    {"analyzer_params": "1e-999"}, {"analyzer_params": '""'}, {"analyzer_params": "null"}, {"analyzer_params": "false"},
    {"analyzer_params": "not json"}, {"analyzer_params": ""}, {"analyzer_params": '{"a":1} x'},
    {"analyzer_params": '{"tokenizer":"nosuch"}'}, {"analyzer_params": '{"x":123456789012345678901234567890}'},
    {"analyzer_params": "1" * 400}, {"analyzer_params": '{"a":' + "1" * 4301 + "}"}, {"analyzer_params": '{"b":1,"a":2,"b":3}'},
    {"analyzer_params": "[" * 5000},
]
PARAM_IDS = [str(i) for i in range(len(ACCEPTED))]


@pytest.mark.parametrize("params", ACCEPTED, ids=PARAM_IDS)
def test_attribute_values_match_3x(diff, stubs, params):
    rust, ref = [create_raw(s, "c", raw_field(DataType.VARCHAR, params)) for s in stubs]
    assert rust == ref == (0, 0, "")
    assert described(stubs[0], "c") == described(stubs[1], "c")


@pytest.mark.parametrize("dtype", [DataType.BOOL, DataType.INT8, DataType.INT16, DataType.INT32, DataType.INT64, DataType.FLOAT, DataType.DOUBLE,
                                   DataType.VARCHAR, DataType.JSON, DataType.ARRAY, DataType.TIMESTAMPTZ, DataType.FLOAT_VECTOR])
def test_attributes_are_accepted_on_every_type(diff, stubs, dtype):
    params = {"enable_analyzer": "true", "enable_match": "true", "analyzer_params": '{"a":1}'}
    rust, ref = [create_raw(s, "c", raw_field(dtype, params)) for s in stubs]
    assert rust == ref == (0, 0, "")
    assert described(stubs[0], "c") == described(stubs[1], "c")
    assert ("enable_match", "true") in described(stubs[0], "c")


@pytest.mark.parametrize("dtype", [DataType.INT64, DataType.VARCHAR])
def test_attributes_on_the_primary_key(diff, stubs, dtype):
    field = raw_field(dtype, {"enable_analyzer": "true"}, primary=True)
    rust, ref = [create_raw(s, "c", field) for s in stubs]
    assert rust == ref == (0, 0, "")
    assert described(stubs[0], "c") == described(stubs[1], "c")


def test_analyzer_params_keep_input_key_order_before_reopen(diff, stubs):
    params = {"analyzer_params": '{"b":1,"a":2}'}
    for stub in stubs:
        assert create_raw(stub, "c", raw_field(DataType.VARCHAR, params))[0] == 0
    assert described(stubs[0], "c") == described(stubs[1], "c")
    assert ("analyzer_params", '{"b": 1, "a": 2}') in described(stubs[0], "c")


def test_sdk_attributes_round_trip(diff):
    create(diff, "memtable")
    diff.step("describe", lambda c: c.describe_collection("c")["fields"], exact)


@pytest.mark.parametrize("text,ref_ok", [
    ('{"x": NaN}', True), ("NaN", True), ("[Infinity]", True), ("-Infinity", True), ("1e999", True), ('{"x":1e999}', True),
    ("[" * 125 + "]" * 125, True),
    ('"\\ud800"', False), ('{"x":"\\ud800"}', False),
    ("[" * 3000 + "]" * 3000, False),
], ids=["nan-object", "nan", "inf-array", "neg-inf", "overflow", "overflow-object", "depth-125",
        "surrogate", "surrogate-object", "depth-3000"])
def test_unrepresentable_params_are_a_registered_difference(diff, stubs, text, ref_ok, request):
    # 3.x 写 schema 失败时留下集合目录，之后同名建表都报 File exists，所以每条用独立的名字。
    name = "bad_" + request.node.callspec.id.replace("-", "_")
    field = raw_field(DataType.VARCHAR, {"analyzer_params": text})
    with pytest.raises(grpc.RpcError) as caught:
        create_raw(stubs[0], name, field)
    assert caught.value.code() == grpc.StatusCode.UNIMPLEMENTED
    assert "analyzer_params that strict JSON cannot represent" in caught.value.details()
    assert (create_raw(stubs[1], name, field)[0] == 0) == ref_ok


def test_124_levels_are_accepted(diff, stubs):
    field = raw_field(DataType.VARCHAR, {"analyzer_params": "[" * 124 + "1" + "]" * 124})
    assert [create_raw(s, "deep", field) for s in stubs] == [(0, 0, "")] * 2
    assert described(stubs[0], "deep") == described(stubs[1], "deep")


def test_top_level_json_string_is_a_registered_difference(diff):
    """0004 / 0029：3.x 存解码后的无引号文本，本实现存带引号的 JSON 文本。"""
    rows = [{**ROWS[0], "j": '"via pymilvus\\nx"'}]
    create(diff, "memtable", rows=rows)
    query = lambda c, expr: ids(c.query("c", filter=expr, output_fields=["id"], limit=9))
    assert query(diff.rust, "text_match(j, 'nx')") == [1]
    assert query(diff.ref, "text_match(j, 'nx')") == []
    assert query(diff.rust, "text_match(j, 'x')") == []
    assert query(diff.ref, "text_match(j, 'x')") == [1]
    assert run(lambda c: query(c, "text_match(j, 'via')"), diff.rust) == run(lambda c: query(c, "text_match(j, 'via')"), diff.ref)
