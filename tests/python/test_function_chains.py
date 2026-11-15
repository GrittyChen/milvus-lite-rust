"""公共 function chain（0035）：Search 的 `function_chains`。

链用 [chainwire.py](chainwire.py) 的线格式编码构造，并入 SearchRequest 的 24 号字段后经普通 stub 发送：
pymilvus 2.6 的 SDK 与 proto 都没有 function chain，这样两条 SDK 线发出同一份请求。
期望的错误码、文案、顺序与分数取自真实 3.x（`1320d26`）对同一请求的响应；与 3.x 不同的三处
（`*` / `$meta` 投影、合法的 `rerank_model`、按名为 `$score` / `$id` 的字段分组，见 0035「与 3.x 的差异」）
各自单列一个用例。
"""

from __future__ import annotations

import json

import grpc
import pytest
from pymilvus import DataType, Function, FunctionType, MilvusClient
from pymilvus.client.prepare import Prepare
from pymilvus.grpc_gen import milvus_pb2, milvus_pb2_grpc

import chainwire as w
from chainwire import Col
from milvus_lite.server_manager import ServerManager


@pytest.fixture
def env(tmp_path):
    manager = ServerManager()
    uri = manager.start_and_get_uri(str(tmp_path / "t.db"))
    channel = grpc.insecure_channel(uri.removeprefix("http://"))
    client = MilvusClient(uri=uri)
    yield client, milvus_pb2_grpc.MilvusServiceStub(channel)
    client.close()
    channel.close()
    manager.release_all()


def create(client, name, *, pk="id", load=True):
    schema = client.create_schema(auto_id=False, enable_dynamic_field=True)
    schema.add_field(pk, DataType.INT64, is_primary=True)
    schema.add_field("ts", DataType.INT64)
    schema.add_field("s", DataType.VARCHAR, max_length=32)
    schema.add_field("j", DataType.JSON)
    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    index = client.prepare_index_params()
    index.add_index(field_name="vector", index_type="FLAT", metric_type="L2", params={})
    client.create_collection(name, schema=schema, index_params=index)
    client.insert(name, [
        {pk: i, "ts": 10 * i, "s": "abc"[i - 1], "j": {"k": i}, "dyn": i, "vector": [0.01 * i, 0.0]}
        for i in (1, 2, 3)
    ])
    if load:
        client.load_collection(name)
    else:
        client.release_collection(name)


def search(stub, name, *chains, raw=None, function_score=False, **kwargs):
    kwargs.setdefault("anns_field", "vector")
    kwargs.setdefault("param", {"metric_type": "L2", "params": {}})
    kwargs.setdefault("limit", 3)
    kwargs.setdefault("data", [[0.0, 0.0]])
    request = Prepare.search_requests_with_expr(collection_name=name, **kwargs)
    w.attach(request, *chains)
    if function_score:
        request.function_score.SetInParent()
    for key, value in (raw or {}).items():
        request.search_params.add(key=key, value=value)
    return stub.Search(request)


def status(response):
    return response.status.code, response.status.reason


def num_combine(*args, **params):
    return w.expr("num_combine", *args, **params)


def map_op(output, expr):
    return w.op("map", expr=expr, outputs=[output])


def sort(column, **params):
    inputs = [column] + [params[k] for k in ("tie_break_col",) if k in params]
    return w.op("sort", inputs=inputs, column=column, **params)


SCORE_PLUS_TS_OPS = (
    map_op("$score", num_combine(Col("$score"), Col("ts"), mode="sum")),
    sort("$score", desc=True, tie_break_col="$id"),
)
SCORE_PLUS_TS = w.chain(*SCORE_PLUS_TS_OPS)
BAD_LIMIT = w.chain(w.op("limit", limit=0))
BAD_LIMIT_MESSAGE = "function chain op[0]: function chain limit must be a positive integer"


def assert_unimplemented(call, feature):
    with pytest.raises(grpc.RpcError) as info:
        call()
    assert info.value.code() == grpc.StatusCode.UNIMPLEMENTED
    assert info.value.details() == (
        f"MilvusLite does not support {feature}: not supported in this build of milvus-lite-rust yet")


def test_chain_decoding_and_validation_errors(env):
    client, stub = env
    create(client, "c")
    unset = w.chain(map_op("$score", num_combine(Col("ts"), None)))
    cases = [
        # 解码
        (w.chain(w.op("")), "function chain op[0] name must not be empty"),
        (w.chain(map_op("x", w.expr("", Col("ts")))), "function chain op[0] expression name must not be empty"),
        (unset, "function chain op[0].expr.args[1].literal: value is not set"),
        (w.chain(map_op("x", num_combine(Col("ts"), Col("ts"), mode=None))),
         "function chain op[0].expr.params['mode']: value is not set"),
        (w.chain(map_op("", num_combine(Col("$score"), Col("ts")))),
         "function chain op[0] output[0] name must not be empty"),
        (w.chain(*SCORE_PLUS_TS_OPS, stage=99), "unknown function chain stage value: 99"),
        # 后面算子的解码错误先于前面算子的校验错误。
        (w.chain(map_op("$id", num_combine(Col("ts"))), w.op("")), "function chain op[1] name must not be empty"),
        # 链
        (w.chain(*SCORE_PLUS_TS_OPS, stage=3),
         "function chain stage FunctionChainStageL0Rerank is not supported in search request"),
        (w.chain(), "function chain must contain at least one operator"),
        (w.chain(w.op("filter")), "function chain op[0]: unsupported function chain operator: filter"),
        # map
        (w.chain(map_op("$id", num_combine(Col("$score"), Col("ts")))),
         "function chain op[0]: system output '$id' is not writable by L2 rerank function chain"),
        (w.chain(map_op("ts", num_combine(Col("$score"), Col("ts")))),
         "function chain op[0]: function chain output 'ts' conflicts with a collection field"),
        (w.chain(map_op("$score", num_combine(Col("$score"), Col("dyn")))),
         "function chain op[0] expression num_combine: function chain input 'dyn' is neither a previous output "
         "nor a collection field"),
        (w.chain(map_op("$score", num_combine(Col("$score"), Col("$seg_offset")))),
         "function chain op[0] expression num_combine: system input '$seg_offset' is not supported by "
         "L2 rerank function chain"),
        (w.chain(map_op("$score", num_combine(Col("$score"), Col("j")))),
         "function chain op[0] expression num_combine: function chain input field 'j' has unsupported type json"),
        (w.chain(map_op("$score", num_combine(Col("$score"), Col("s")))),
         "function chain op[0] expression num_combine: num_combine requires at least two numeric arguments"),
        (w.chain(map_op("$score", num_combine(Col("$score"), [1]))),
         "function chain op[0] expression num_combine: unsupported function chain literal type: tuple"),
        (w.chain(map_op("$score", num_combine(Col("ts"), Col("ts"), mode="weighted", weights=[1]))),
         "function chain op[0] expression num_combine: weighted num_combine requires 2 weights"),
        (w.chain(map_op("$score", w.expr("decay", Col("ts"), function="gauss", origin=1, scale=0))),
         "function chain op[0] expression decay: decay requires scale > 0, offset >= 0, and 0 < decay < 1"),
        (w.chain(map_op("$score", w.expr("round_decimal", Col("$score"), decimal=7))),
         "function chain op[0] expression round_decimal: round_decimal decimal must be an integer in [0, 6]"),
        (w.chain(map_op("$score", w.expr("xgboost", Col("ts")))),
         "function chain op[0] expression xgboost: unsupported function chain expression: xgboost"),
        # sort / limit
        (w.chain(w.op("sort", inputs=[], column="ts")),
         "function chain op[0]: sort inputs must match column and tie_break_col"),
        (w.chain(sort("ts", desc=1)), "function chain op[0]: sort desc must be a boolean"),
        (w.chain(sort("vector")),
         "function chain op[0]: function chain input field 'vector' has unsupported type float_vector"),
        (BAD_LIMIT, BAD_LIMIT_MESSAGE),
        (w.chain(w.op("limit", limit=2, offset=-1)),
         "function chain op[0]: function chain offset must be a non-negative integer"),
        # rerank_model 的校验与 3.x 相同；通过校验后的处理见 test_valid_rerank_model_is_unimplemented。
        (w.chain(map_op("$score", w.expr("rerank_model", Col("s"), provider="zz", queries=["q"]))),
         "function chain op[0] expression rerank_model: Unknown rerank provider: 'zz'. Supported: 'cohere'"),
        (w.chain(map_op("$score", w.expr("rerank_model", Col("s"), provider="cohere", queries=["q"], Base_URL="x"))),
         "function chain op[0] expression rerank_model: rerank_model endpoint must be configured on the server"),
        (w.chain(map_op("$score", w.expr("rerank_model", Col("s"), provider="cohere", queries=["q", "r"]))),
         "function chain op[0] expression rerank_model: rerank_model query count 2 does not match search nq 1"),
        (w.chain(map_op("$score", w.expr("rerank_model", Col("s"), provider="cohere", queries=["q"], zz=1, aa=2))),
         "function chain op[0] expression rerank_model: unsupported rerank_model parameter 'aa'"),
    ]
    for chain, message in cases:
        assert status(search(stub, "c", chain)) == (6, message)

    # 主键名与系统列同名的 collection 可以建，带链检索时被拒绝。
    for pk in ("$score", "$id"):
        name = "pk" + pk[1:]
        create(client, name, pk=pk)
        assert status(search(stub, name, w.chain(w.op("limit", limit=1)))) == (
            6, f"function chain primary key field '{pk}' conflicts with reserved system column '{pk}'")
        assert search(stub, name).status.code == 0


def test_chain_conflicts_and_error_order(env):
    client, stub = env
    create(client, "c")
    create(client, "released", load=False)

    assert status(search(stub, "c", SCORE_PLUS_TS, function_score=True)) == (
        6, "function_score and function_chains cannot be used together")
    assert status(search(stub, "c", SCORE_PLUS_TS, SCORE_PLUS_TS)) == (
        6, "ordinary search supports exactly one function chain")
    # 冲突与条数先于链本身的错误。
    assert status(search(stub, "c", BAD_LIMIT, BAD_LIMIT)) == (
        6, "ordinary search supports exactly one function chain")
    assert status(search(stub, "c", BAD_LIMIT, function_score=True, raw={"order_by_fields": '"ts:asc"'})) == (
        6, "function_score and function_chains cannot be used together")
    assert status(search(stub, "c", BAD_LIMIT, BAD_LIMIT, function_score=True,
                         raw={"order_by_fields": '"ts:asc"'})) == (
        6, "function_score and function_chains cannot be used together")
    assert status(search(stub, "c", BAD_LIMIT, BAD_LIMIT, raw={"order_by_fields": '"ts:asc"'})) == (
        6, "order_by and function rerank cannot be used together")

    # order_by_fields：值按 JSON 解析后为真才冲突；没有链时不起作用。
    conflict = (6, "order_by and function rerank cannot be used together")
    # `NaN` 与 `1e999` 在 Python 的 `json.loads` 里是 nan / inf，都为真。
    for value in ('"ts:asc"', " ", "ts", "[1]", "1", "NaN", "1e999"):
        assert status(search(stub, "c", BAD_LIMIT, raw={"order_by_fields": value})) == conflict
        assert status(search(stub, "released", BAD_LIMIT, raw={"order_by_fields": value})) == conflict
        assert search(stub, "c", raw={"order_by_fields": value}).status.code == 0
    for value in ("", "0", "[]", "null", '""', "{}", "false", "0.0"):
        assert status(search(stub, "c", BAD_LIMIT, raw={"order_by_fields": value})) == (6, BAD_LIMIT_MESSAGE)

    # 先后：collection 不存在、anns_field 先于链；链先于 load 检查、度量是否匹配与过滤。
    assert search(stub, "nope", BAD_LIMIT).status.code == 100
    assert status(search(stub, "c", BAD_LIMIT, anns_field="nope")) == (6, "anns_field 'nope' not found in schema")
    assert status(search(stub, "released", BAD_LIMIT)) == (6, BAD_LIMIT_MESSAGE)
    assert status(search(stub, "c", BAD_LIMIT, expr="ts >")) == (6, BAD_LIMIT_MESSAGE)
    assert status(search(stub, "c", BAD_LIMIT, param={"metric_type": "IP", "params": {}})) == (
        6, BAD_LIMIT_MESSAGE)
    assert search(stub, "released").status.code == 101


def test_hybrid_search_rejects_function_chains(env):
    client, stub = env
    create(client, "c")
    sub = Prepare.search_requests_with_expr(
        collection_name="c", anns_field="vector", param={"metric_type": "L2", "params": {}}, limit=3,
        data=[[0.0, 0.0]])
    request = milvus_pb2.HybridSearchRequest(collection_name="c", requests=[sub])
    request.rank_params.add(key="strategy", value="rrf")
    request.rank_params.add(key="limit", value="3")
    assert stub.HybridSearch(request).status.code == 0
    # HybridSearchRequest 的 function_chains 是 15 号字段。
    request.MergeFromString(w._field(15, SCORE_PLUS_TS))
    assert status(stub.HybridSearch(request)) == (6, "function_chains is not supported for hybrid search yet")


# ---- 执行（第 2 步）----

def create_rich(client, name, *, metric="L2", load=True, extra=None):
    """四行：ts 有并列，n 与 tz 有空值，dyn / tmp 是动态键。`extra` 再加一个 DOUBLE 字段。"""
    schema = client.create_schema(auto_id=False, enable_dynamic_field=True)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("ts", DataType.INT64)
    schema.add_field("f", DataType.FLOAT)
    schema.add_field("s", DataType.VARCHAR, max_length=32)
    schema.add_field("tz", DataType.TIMESTAMPTZ, nullable=True)
    schema.add_field("n", DataType.INT64, nullable=True)
    schema.add_field("j", DataType.JSON)
    schema.add_field("arr", DataType.ARRAY, element_type=DataType.INT64, max_capacity=4)
    if extra:
        schema.add_field(extra, DataType.DOUBLE)
    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    index = client.prepare_index_params()
    index.add_index(field_name="vector", index_type="FLAT", metric_type=metric, params={})
    client.create_collection(name, schema=schema, index_params=index)
    rows = []
    data = [(10, 0.1, "b", "2026-01-01T00:00:00Z", 5), (20, 0.25, "a", None, None),
            (30, 0.5, "c", "2026-01-03T00:00:00+08:00", 7), (20, 2.5, "a", "2026-01-02T00:00:00Z", 5)]
    for i, (ts, f, s, tz, n) in enumerate(data, 1):
        row = {"id": i, "ts": ts, "f": f, "s": s, "tz": tz, "n": n, "j": {"k": i}, "arr": [i],
               "vector": [0.01 * i, 0.0], "dyn": i * 100, "tmp": "dynamic-tmp"}
        if extra:
            row[extra] = i + 0.5
        rows.append(row)
    client.insert(name, rows)
    if load:
        client.load_collection(name)
    else:
        client.release_collection(name)


def decode(response):
    """响应里与断言有关的部分；字段按名字给出逐行的值（空值为 None）。"""
    assert status(response) == (0, ""), status(response)
    results = response.results
    fields = {}
    for field in results.fields_data:
        if field.HasField("vectors") and field.vectors.HasField("sparse_float_vector"):
            values = list(field.vectors.sparse_float_vector.contents)
        elif field.HasField("vectors"):
            dim = field.vectors.dim
            flat = list(field.vectors.float_vector.data)
            values = [tuple(flat[i:i + dim]) for i in range(0, len(flat), dim)]
        else:
            data = getattr(field.scalars, field.scalars.WhichOneof("data")).data
            values = [json.loads(v) if isinstance(v, bytes) else str(v) if hasattr(v, "ListFields") else v
                      for v in data]
        if field.valid_data:
            values = [v if ok else None for v, ok in zip(values, field.valid_data)]
        fields[field.field_name] = values
    group = None
    if results.HasField("group_by_field_value"):
        group = list(results.group_by_field_value.scalars.long_data.data)
    return {
        "ids": list(results.ids.int_id.data),
        "scores": list(results.scores),
        "topks": list(results.topks),
        "fields": fields,
        "output_fields": list(results.output_fields),
        "group": group,
    }


def ids_scores(response):
    out = decode(response)
    return out["ids"], pytest.approx(out["scores"], rel=1e-6)


def reordered(plain, ids):
    """不带链的响应 `plain` 按 `ids` 的顺序重排后的字段。"""
    at = [plain["ids"].index(i) for i in ids]
    return {name: [values[i] for i in at] for name, values in plain["fields"].items()}


def decimal(column, places):
    return w.expr("round_decimal", Col(column), decimal=places)


TS_DESC = w.chain(sort("ts", desc=True, tie_break_col="$id"))
# 临时列 `tmp` 与动态键同名：只在链内可见。
TEMP_COLLISION = w.chain(map_op("tmp", num_combine(Col("$score"), Col("ts"))), sort("tmp", desc=True),
                         w.op("limit", limit=3))
LIMIT_ONLY = w.chain(w.op("limit", limit=10))


def test_chain_projection_matches_search_without_chain(env):
    """投影合同（0035「投影」）：带链时返回的字段与同一请求不带链时相同，链只改变顺序、行数与分数；
    链读了但没点名的字段、临时列都不返回。"""
    client, stub = env
    create_rich(client, "c")
    outputs = [None, ["ts"], ["f"], ["dyn"], ["tmp"], ["id"], ["id", "ts"], ["ts", "ts"], ["n", "tz"],
               ["vector", "j", "arr"], ["nosuch"], ["dyn", "nosuch", "s"]]
    for output in outputs:
        plain = decode(search(stub, "c", limit=4, output_fields=output))
        assert plain["ids"] == [1, 2, 3, 4]
        for chain, ids in ((TS_DESC, [3, 2, 4, 1]), (TEMP_COLLISION, [3, 4, 2]), (LIMIT_ONLY, [1, 2, 3, 4])):
            chained = decode(search(stub, "c", chain, limit=4, output_fields=output))
            assert chained["ids"] == ids, output
            assert chained["topks"] == [len(ids)]
            assert chained["output_fields"] == plain["output_fields"], output
            assert chained["fields"] == reordered(plain, ids), output
    # 链读 ts，请求只点名 f：ts 不返回。
    assert list(decode(search(stub, "c", TS_DESC, output_fields=["f"]))["fields"]) == ["f", "$meta"]
    # 点名与动态键同名的临时列：返回的是动态键原来的值。
    chained = decode(search(stub, "c", TEMP_COLLISION, output_fields=["tmp"]))
    assert chained["fields"] == {"$meta": [{"tmp": "dynamic-tmp"}] * 3}

    # flush 之后（命中来自 segment）同样成立。
    client.flush("c")
    for output in (None, ["ts"], ["dyn", "n"]):
        plain = decode(search(stub, "c", limit=4, output_fields=output))
        chained = decode(search(stub, "c", TS_DESC, limit=4, output_fields=output))
        assert chained["ids"] == [3, 2, 4, 1]
        assert chained["fields"] == reordered(plain, [3, 2, 4, 1])


def test_chain_star_and_meta_projection_is_not_degraded(env):
    """登记的差异（0035 决策 A）：`output_fields` 含 `*` 或 `$meta` 时，3.x 带链的响应把 schema 字段写成
    类型默认值、`$meta` 只剩点名的键；本实现与不带链时相同。"""
    client, stub = env
    create_rich(client, "c")
    for output in (["*"], ["$meta"], ["*", "ts"], ["$meta", "dyn"], ["$meta", "ts"]):
        plain = decode(search(stub, "c", limit=4, output_fields=output))
        for chain, ids in ((TS_DESC, [3, 2, 4, 1]), (LIMIT_ONLY, [1, 2, 3, 4])):
            chained = decode(search(stub, "c", chain, limit=4, output_fields=output))
            assert chained["ids"] == ids
            assert chained["output_fields"] == plain["output_fields"]
            assert chained["fields"] == reordered(plain, ids), output
    full = decode(search(stub, "c", TS_DESC, limit=4, output_fields=["*"]))["fields"]
    assert full["ts"] == [30, 20, 20, 10]
    assert full["$meta"][0] == {"dyn": 300, "tmp": "dynamic-tmp"}


def test_chain_strips_fields_named_like_system_columns(env):
    """3.x 的带链响应不返回名为 `$id` / `$score` 的名字：同名的 schema 字段是空值占位，动态键被去掉
    （与 0026 的 L2 链相同）；不带链时照常返回。"""
    client, stub = env
    for name, extra in (("score", "$score"), ("ident", "$id")):
        create_rich(client, name, extra=extra)
        plain = decode(search(stub, name, output_fields=[extra, "ts"]))
        assert plain["fields"][extra] == [1.5, 2.5, 3.5]
        for output in ([extra, "ts"], None):
            chained = decode(search(stub, name, TS_DESC, output_fields=output))
            assert chained["ids"] == [3, 2, 1]
            assert chained["fields"][extra] == [None, None, None]
            assert chained["fields"]["ts"] == [30, 20, 10]
        # 链里读到的 `$score` / `$id` 是分数与主键，不是同名字段；写 `$score` 与同名字段冲突。
        chain = w.chain(map_op("$score", num_combine(Col("$score"), Col("$id"), mode="max")))
        if extra == "$score":
            assert status(search(stub, name, chain)) == (
                6, "function chain op[0]: function chain output '$score' conflicts with a collection field")
        else:
            assert ids_scores(search(stub, name, chain)) == ([1, 2, 3], [1.0, 2.0, 3.0])

    schema = client.create_schema(auto_id=False, enable_dynamic_field=True)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("ts", DataType.INT64)
    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    index = client.prepare_index_params()
    index.add_index(field_name="vector", index_type="FLAT", metric_type="L2", params={})
    client.create_collection("dynamic", schema=schema, index_params=index)
    client.insert("dynamic", [{"id": i, "ts": 10 * i, "$score": i + 0.5, "$id": f"x{i}", "k": i,
                               "vector": [0.01 * i, 0.0]} for i in (1, 2, 3)])
    client.load_collection("dynamic")
    assert decode(search(stub, "dynamic", output_fields=["$score", "$id", "k"]))["fields"] == {
        "$meta": [{"$score": i + 0.5, "$id": f"x{i}", "k": i} for i in (1, 2, 3)]}
    assert decode(search(stub, "dynamic", TS_DESC, output_fields=["$score", "$id", "k"]))["fields"] == {
        "$meta": [{"k": 3}, {"k": 2}, {"k": 1}]}
    assert decode(search(stub, "dynamic", TS_DESC))["fields"]["$meta"] == [{"k": 3}, {"k": 2}, {"k": 1}]


def test_grouping_by_fields_named_like_system_columns(env):
    """登记的差异（0035 决策 G）：按名为 `$score` / `$id` 的字段分组时，3.x 在链执行前用分组值盖掉行里的
    `$score` / `$id`（分数变成分组值或报 float() 错误，`$id` 报 unknown 或被换成别的行的主键）；本实现不盖，
    分组值照常返回，分数与 id 不受影响。名为这两个名字的字段在 entity 里仍是空值占位。"""
    client, stub = env
    schema = client.create_schema(auto_id=False, enable_dynamic_field=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("ts", DataType.INT64)
    schema.add_field("$score", DataType.INT64)
    schema.add_field("$id", DataType.VARCHAR, max_length=8)
    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    index = client.prepare_index_params()
    index.add_index(field_name="vector", index_type="FLAT", metric_type="L2", params={})
    client.create_collection("grouped", schema=schema, index_params=index)
    client.insert("grouped", [{"id": i, "ts": 10 * i, "$score": i % 2, "$id": f"g{i % 2}",
                               "vector": [0.01 * i, 0.0]} for i in (1, 2, 3, 4)])
    client.load_collection("grouped")
    for field in ("$score", "$id"):
        response = search(stub, "grouped", TS_DESC, limit=4, raw={"group_by_field": field, "group_size": "2"},
                          output_fields=[field, "ts"])
        assert status(response) == (0, "")
        group = response.results.group_by_field_value
        assert group.field_name == field
        values = (list(group.scalars.long_data.data) if field == "$score"
                  else list(group.scalars.string_data.data))
        assert list(response.results.ids.int_id.data) == [4, 3, 2, 1]
        assert values == ([0, 1, 0, 1] if field == "$score" else ["g0", "g1", "g0", "g1"])
        assert decode(response)["fields"][field] == [None] * 4
        assert decode(response)["scores"] == pytest.approx([0.0016, 0.0009, 0.0004, 0.0001], rel=1e-5)


def test_chain_scores_for_each_metric(env):
    client, stub = env
    plus_ts = SCORE_PLUS_TS
    create_rich(client, "l2")
    create_rich(client, "ip", metric="IP")
    create_rich(client, "cos", metric="COSINE")
    assert ids_scores(search(stub, "l2", plus_ts, limit=4)) == (
        [3, 4, 2, 1], [30.000900268554688, 20.00160026550293, 20.00040054321289, 10.000100135803223])
    assert ids_scores(search(stub, "ip", plus_ts, limit=4, data=[[1.0, 0.0]],
                             param={"metric_type": "IP", "params": {}})) == (
        [3, 4, 2, 1], [30.030000686645508, 20.040000915527344, 20.020000457763672, 10.010000228881836])
    plus_f = w.chain(map_op("$score", num_combine(Col("$score"), Col("f"))),
                     sort("$score", desc=True, tie_break_col="$id"))
    assert ids_scores(search(stub, "cos", plus_f, limit=4, data=[[1.0, 0.0]],
                             param={"metric_type": "COSINE", "params": {}})) == (
        [4, 3, 2, 1], [3.5, 1.5, 1.25, 1.100000023841858])

    # BM25：链的入口取 `-distance`，之后不取回负号；只有 limit 的链也让分数变正。稀疏 IP 不变号。
    schema = client.create_schema(auto_id=False, enable_dynamic_field=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("ts", DataType.INT64)
    schema.add_field("text", DataType.VARCHAR, max_length=200, enable_analyzer=True)
    schema.add_field("sp", DataType.SPARSE_FLOAT_VECTOR)
    schema.add_field("sv", DataType.SPARSE_FLOAT_VECTOR)
    schema.add_function(Function(name="bm25", function_type=FunctionType.BM25, input_field_names=["text"],
                                 output_field_names=["sp"]))
    index = client.prepare_index_params()
    index.add_index(field_name="sp", index_type="SPARSE_INVERTED_INDEX", metric_type="BM25", params={})
    index.add_index(field_name="sv", index_type="SPARSE_INVERTED_INDEX", metric_type="IP", params={})
    client.create_collection("fts", schema=schema, index_params=index)
    client.insert("fts", [
        {"id": 1, "ts": 3, "text": "apple banana", "sv": {1: 0.5, 2: 0.25}},
        {"id": 2, "ts": 2, "text": "apple apple cherry", "sv": {1: 1.0}},
        {"id": 3, "ts": 1, "text": "banana cherry durian", "sv": {2: 2.0}},
    ])
    client.load_collection("fts")
    bm25 = dict(anns_field="sp", param={"metric_type": "BM25", "params": {}}, data=["apple"])
    sparse = dict(anns_field="sv", param={"metric_type": "IP", "params": {}}, data=[{1: 1.0, 2: 1.0}])
    limit_only = w.chain(w.op("limit", limit=3))
    by_score = w.chain(sort("$score", desc=True, tie_break_col="$id"))
    rounded = w.chain(map_op("$score", decimal("$score", 2)))
    by_text = w.chain(sort("text", desc=False))
    assert ids_scores(search(stub, "fts", **bm25)) == ([2, 1], [-0.6454985737800598, -0.529581606388092])
    positive = ([2, 1], [0.6454985737800598, 0.529581606388092])
    assert ids_scores(search(stub, "fts", limit_only, **bm25)) == positive
    assert ids_scores(search(stub, "fts", by_score, **bm25)) == positive
    assert ids_scores(search(stub, "fts", by_text, **bm25)) == positive
    assert ids_scores(search(stub, "fts", plus_ts, **bm25)) == ([1, 2], [3.5295815467834473, 2.645498514175415])
    assert ids_scores(search(stub, "fts", rounded, **bm25)) == ([2, 1], [0.6499999761581421, 0.5299999713897705])
    assert ids_scores(search(stub, "fts", limit_only, raw={"round_decimal": "2"}, **bm25)) == (
        [2, 1], [0.6499999761581421, 0.5299999713897705])
    for chain in (None, limit_only, by_score, rounded):
        chains = [chain] if chain else []
        assert ids_scores(search(stub, "fts", *chains, **sparse)) == ([3, 2, 1], [2.0, 1.0, 0.75])
    assert ids_scores(search(stub, "fts", plus_ts, **sparse)) == ([1, 2, 3], [3.75, 3.0, 3.0])
    assert ids_scores(search(stub, "fts", by_text, **sparse)) == ([2, 1, 3], [1.0, 0.75, 2.0])
    for kwargs in (bm25, sparse):
        assert status(search(stub, "fts", w.chain(sort("sv")), **kwargs)) == (
            6, "function chain op[0]: function chain input field 'sv' has unsupported type sparse_float_vector")


def test_chain_expressions_sorting_and_search_parameters(env):
    client, stub = env
    create_rich(client, "c")
    plus_ts_top2 = w.chain(*SCORE_PLUS_TS_OPS, w.op("limit", limit=2))
    top2 = ([3, 4], [30.000900268554688, 20.00160026550293])

    # 表达式
    def scores(expr, **kwargs):
        return ids_scores(search(stub, "c", w.chain(map_op("$score", expr)), limit=4, **kwargs))

    in_order = [1, 2, 3, 4]
    assert scores(num_combine(Col("ts"), Col("$id"), mode="multiply")) == (in_order, [10.0, 40.0, 90.0, 80.0])
    assert scores(num_combine(Col("ts"), Col("f"), 15, mode="min")) == (
        in_order, [0.10000000149011612, 0.25, 0.5, 2.5])
    assert scores(num_combine(Col("ts"), Col("$id"), mode="max")) == (in_order, [10.0, 20.0, 30.0, 20.0])
    assert scores(num_combine(1e16, 1.0, -1e16, Col("ts"), mode="avg")) == (in_order, [2.75, 5.25, 7.75, 5.25])
    assert scores(num_combine(Col("ts"), Col("f"), Col("$id"), mode="weighted", weights=[0.1, 0.2, 0.7])) == (
        in_order, [1.7200000286102295, 3.450000047683716, 5.199999809265137, 5.300000190734863])
    # 整数精确相加（超过 int64），分数按 float32 返回。
    big = 9223372036854775807
    assert scores(num_combine(Col("ts"), big, big)) == (in_order, [1.8446744073709552e19] * 4)
    # 可空字段：num_combine 与 decay 把 None 当 0.0。
    assert scores(num_combine(Col("n"), Col("ts"))) == (in_order, [15.0, 0.0, 37.0, 25.0])
    assert scores(w.expr("decay", Col("n"), function="linear", origin=5, scale=4)) == (
        in_order, [1.0, 0.0, 0.75, 1.0])
    assert scores(w.expr("decay", Col("ts"), function="gauss", origin=20, scale=10)) == (
        in_order, [0.5, 1.0, 0.5, 1.0])
    assert scores(w.expr("decay", Col("f"), function="exp", origin=0.1, scale=1.5, offset=0.1, decay=0.3)) == (
        in_order, [1.0, 0.9606621861457825, 0.7860031127929688, 0.157852903008461])
    # TIMESTAMPTZ 取 epoch 秒。
    assert scores(w.expr("decay", Col("tz"), function="linear", origin=1767225600, scale=86400 * 3)) == (
        in_order, [1.0, 0.0, 0.7222222222222222, 0.8333333333333334])
    # 超出 float32 的分数是 inf。
    assert decode(search(stub, "c", w.chain(map_op("$score", num_combine(Col("ts"), -1e300, mode="multiply")))))[
        "scores"] == [float("-inf")] * 3

    # 排序：各种列，None 总在最后，平局列升序，没有平局列时保持原顺序。
    def order(*args, **params):
        return decode(search(stub, "c", w.chain(sort(*args, **params)), limit=4))["ids"]

    assert order("s", desc=False) == [2, 4, 1, 3]
    assert order("s", desc=True, tie_break_col="$id") == [3, 1, 2, 4]
    assert order("tz", desc=False) == [1, 4, 3, 2]
    assert order("tz", desc=True) == [3, 4, 1, 2]
    assert order("n", desc=False) == [1, 4, 3, 2]
    assert order("n", desc=True, tie_break_col="$id") == [3, 1, 4, 2]
    assert order("f") == [4, 3, 2, 1]
    assert order("id") == [4, 3, 2, 1]
    assert order("ts", desc=True, tie_break_col="s") == [3, 2, 4, 1]
    # 不排序的链保持引擎的顺序；临时列里的 None 排在最后，再截断。
    assert decode(search(stub, "c", w.chain(map_op("x", decimal("n", 0)), sort("x", desc=True, tie_break_col="$id"),
                                           w.op("limit", limit=2)), limit=4))["ids"] == [3, 1]

    # 引擎按普通检索执行，链只作用在返回的命中上：不放大候选窗口。
    assert ids_scores(search(stub, "c", SCORE_PLUS_TS, limit=2)) == (
        [2, 1], [20.00040054321289, 10.000100135803223])
    assert ids_scores(search(stub, "c", plus_ts_top2, limit=3, raw={"offset": "1"})) == top2
    assert ids_scores(search(stub, "c", plus_ts_top2, limit=4, expr="ts >= 20")) == top2
    assert decode(search(stub, "c", plus_ts_top2, limit=4, expr="ts > 1000"))["topks"] == [0]
    ranged = decode(search(stub, "c", TS_DESC, limit=4,
                           param={"metric_type": "L2", "params": {"radius": 0.001, "range_filter": 0.0}}))
    assert ranged["ids"] == [3, 2, 1]
    # 多个查询各自一个 chunk。
    many = decode(search(stub, "c", plus_ts_top2, limit=4, data=[[0.0, 0.0], [0.04, 0.0], [0.02, 0.0]],
                         output_fields=["ts"]))
    assert (many["topks"], many["ids"]) == ([2, 2, 2], [3, 4, 3, 2, 3, 4])
    assert many["fields"]["ts"] == [30, 20, 30, 20, 30, 20]
    assert many["scores"] == pytest.approx(
        [30.000900268554688, 20.00160026550293, 30.000099182128906, 20.00040054321289, 30.000099182128906,
         20.00040054321289], rel=1e-6)

    # 分组：引擎先分组，分组值随行走。
    grouped = decode(search(stub, "c", SCORE_PLUS_TS, limit=4, raw={"group_by_field": "ts"}, output_fields=["f"]))
    assert (grouped["ids"], grouped["group"]) == ([3, 2, 1], [30, 20, 10])
    assert grouped["fields"]["f"] == pytest.approx([0.5, 0.25, 0.1])
    grouped = decode(search(stub, "c", w.chain(sort("$id", desc=True)), limit=4,
                            raw={"group_by_field": "ts", "group_size": "2"}))
    assert (grouped["ids"], grouped["group"]) == ([4, 3, 2, 1], [20, 30, 20, 10])

    # search 级的 round_decimal 在链之后；整数分数照常取整。
    assert ids_scores(search(stub, "c", w.chain(map_op("$score", num_combine(Col("$score"), Col("f")))), limit=4,
                             round_decimal=1)) == (in_order, [0.1, 0.3, 0.5, 2.5])
    assert scores(num_combine(Col("ts"), Col("$id"), mode="max"), round_decimal=1) == (
        in_order, [10.0, 20.0, 30.0, 20.0])

    # time_fields 与时区照常作用在输出上。
    timed = decode(search(stub, "c", w.chain(sort("tz", desc=True)), limit=4, output_fields=["tz"],
                          raw={"time_fields": "year,hour", "timezone": "Asia/Shanghai"}))
    assert timed["ids"] == [3, 4, 1, 2]
    assert timed["fields"]["tz"][3] is None

    # 带 iterator 参数的 SearchRequest（pymilvus search_iterator 两个版本发出的键）：3.x 与本实现都不区分。
    v1 = {"iterator": "True"}
    v2 = {"iterator": "True", "search_iter_v2": "True", "search_iter_batch_size": "2", "search_iter_id": "abc",
          "search_iter_last_bound": "0.0001"}
    for raw in (v1, v2):
        assert ids_scores(search(stub, "c", plus_ts_top2, limit=4, raw=raw)) == top2
        assert decode(search(stub, "c", limit=4, raw=raw))["ids"] == in_order


def test_chain_runtime_errors_and_their_order(env):
    client, stub = env
    create_rich(client, "c")
    create_rich(client, "released", load=False)
    non_finite = w.chain(map_op("$score", num_combine(Col("ts"), 1e308, mode="multiply")))
    none_score = w.chain(map_op("$score", decimal("n", 2)))
    float_none = (1, "float() argument must be a string or a real number, not 'NoneType'")
    round_none = (1, "type NoneType doesn't define __round__ method")
    bad_zone = {"timezone": "Not/AZone"}

    # 通过校验的链：load 检查、过滤等引擎内的检查先于链的运行期错误。
    for chain in (SCORE_PLUS_TS, non_finite, none_score):
        assert search(stub, "released", chain).status.code == 101
        assert search(stub, "c", chain, expr="ts >").status.code == 6
        assert search(stub, "c", chain, raw={"group_by_field": "nosuch"}).status.code != 0
    assert status(search(stub, "c", non_finite)) == (1, "num_combine produced a non-finite result")
    # 运行期错误只看返回的命中：limit 把出错的行挡在引擎的结果之外。
    assert status(search(stub, "c", w.chain(map_op("$score", num_combine(Col("f"), 2e38, mode="multiply"))),
                         limit=4)) == (0, "")
    huge = w.chain(map_op("$score", num_combine(Col("ts"), 1e307, mode="multiply")))
    assert status(search(stub, "c", huge, limit=1)) == (0, "")
    assert status(search(stub, "c", huge, limit=2)) == (1, "num_combine produced a non-finite result")
    assert status(search(stub, "c", w.chain(map_op("$score", w.expr(
        "decay", Col("ts"), function="gauss", origin=0, scale=1e-200))))) == (1, "float division by zero")

    # `$score` 为 None：search 级的 round() 先于时区校验，float() 晚于时区校验。
    assert status(search(stub, "c", none_score, limit=1)) == (0, "")
    assert status(search(stub, "c", none_score)) == float_none
    assert status(search(stub, "c", none_score, round_decimal=2)) == round_none
    assert status(search(stub, "c", none_score, round_decimal=0)) == round_none
    zone = status(search(stub, "c", none_score, raw=bad_zone))
    assert zone[0] != 0 and zone not in (float_none, round_none)
    assert status(search(stub, "c", none_score, round_decimal=2, raw=bad_zone)) == round_none
    # 非法的 time_fields 分量在编码字段时才报错，晚于 float()；没有 None 分数时它照常报错。
    bad_parts = {"time_fields": "year,nosuch"}
    assert status(search(stub, "c", none_score, output_fields=["tz"], raw=bad_parts)) == float_none
    assert status(search(stub, "c", none_score, output_fields=["tz"], round_decimal=2, raw=bad_parts)) == round_none
    assert search(stub, "c", w.chain(w.op("limit", limit=3)), output_fields=["tz"], raw=bad_parts).status.code == 6
    # None 被链排到最后并截掉后不报错；第二个查询里的 None 同样报错。
    assert decode(search(stub, "c", w.chain(map_op("$score", decimal("n", 2)), sort("$score", desc=True),
                                           w.op("limit", limit=3)), limit=4))["ids"] == [3, 1, 4]
    assert status(search(stub, "c", none_score, limit=2, data=[[0.0, 0.0], [0.0, 0.0]])) == float_none


def test_valid_rerank_model_is_unimplemented(env):
    """登记的差异（0035 决策 B）：通过全部校验的 `rerank_model` 返回 UNIMPLEMENTED，位置在 load 检查之前
    （3.x 在同一位置创建 provider）。"""
    client, stub = env
    create(client, "c")
    create(client, "released", load=False)
    model = w.chain(map_op("$score", w.expr("rerank_model", Col("s"), provider="Cohere", queries=["q"],
                                          model_name="m")))
    for name in ("c", "released"):
        assert_unimplemented(lambda: search(stub, name, model), "function chains: rerank_model")
    assert_unimplemented(lambda: search(stub, "c", model, expr="ts >"), "function chains: rerank_model")
    # 没有链的请求不受影响。
    assert list(search(stub, "c").results.ids.int_id.data) == [1, 2, 3]
