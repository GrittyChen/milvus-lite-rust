"""公共 function chain（0035）：Search 的 `function_chains`。

链用 [chainwire.py](chainwire.py) 的线格式编码构造，并入 SearchRequest 的 24 号字段后经普通 stub 发送：
pymilvus 2.6 的 SDK 与 proto 都没有 function chain，这样两条 SDK 线发出同一份请求。
期望的错误码与文案取自真实 3.x（`1320d26`）对同一请求的响应。
"""

from __future__ import annotations

import grpc
import pytest
from pymilvus import DataType, MilvusClient
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
    request = Prepare.search_requests_with_expr(collection_name=name, limit=3, data=[[0.0, 0.0]], **kwargs)
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


def assert_unimplemented(call, feature="function chains"):
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
        # rerank_model 的校验与 3.x 相同；通过校验后的处理见下一个用例。
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


def test_valid_chains_are_not_executed_yet(env):
    """第 1 步的中间状态（0035「实施步骤」）：通过全部校验的链返回 UNIMPLEMENTED，位置在 load 检查之前。
    合法的 rerank_model 是登记的差异（决策 B），位置相同。"""
    client, stub = env
    create(client, "c")
    create(client, "released", load=False)
    chains = [
        SCORE_PLUS_TS,
        w.chain(w.op("limit", limit=2, offset=1)),
        w.chain(map_op("t", w.expr("decay", Col("ts"), function="gauss", origin=20, scale=10)),
                map_op("$score", w.expr("round_decimal", Col("t"), decimal=2)), sort("s", desc=False)),
    ]
    for chain in chains:
        for name in ("c", "released"):
            assert_unimplemented(lambda: search(stub, name, chain))
        assert_unimplemented(lambda: search(stub, "c", chain, expr="ts >"))
    model = w.chain(map_op("$score", w.expr("rerank_model", Col("s"), provider="Cohere", queries=["q"],
                                          model_name="m")))
    for name in ("c", "released"):
        assert_unimplemented(lambda: search(stub, name, model), "function chains: rerank_model")
    # 没有链的请求不受影响。
    assert list(search(stub, "c").results.ids.int_id.data) == [1, 2, 3]
