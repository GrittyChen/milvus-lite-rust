"""0035：Search 的公共 function chain 分别发给本实现与 3.x（`1320d26`），逐项比较原始响应。

链用 [chainwire.py](../python/chainwire.py) 的线格式编码写进 SearchRequest 的 24 号字段，经各自连接的普通 stub
发送：pymilvus 2.6 的 SDK 与 proto 都没有 function chain，两条 SDK 线因此发出同一份字节（0035「测试机制」）。
比较原始的状态、ids、scores、topks、output_fields、fields_data 与分组值；分数容差与 test_rerank_boost.py 相同。

三项登记差异（0035「与 3.x 的差异」）不做相等比较，而是两边各自断言：3.x 一侧断言 3.x 的表现，本实现一侧断言合同。
- A：`output_fields` 含 `*` / `$meta` 时 3.x 返回类型默认值与残缺的 `$meta`；本实现与不带链时相同。
- B：合法的 `rerank_model` 在 3.x 报缺 key（本测试要求环境里没有 `COHERE_API_KEY`，不调用外部服务）；本实现
  返回 UNIMPLEMENTED。
- G：按名为 `$score` / `$id` 的字段分组时 3.x 用分组值盖掉虚拟列；本实现不盖。
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

import grpc
from pymilvus import DataType, Function, FunctionType
from pymilvus.client.prepare import Prepare
from pymilvus.grpc_gen import milvus_pb2

from test_rerank_boost import same
from test_reopen import pair  # noqa: F401  （fixture）

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))
import chainwire as w  # noqa: E402
from chainwire import Col  # noqa: E402

S = Col("$score")


def nc(*args, **params):
    return w.expr("num_combine", *args, **params)


def mapped(output, expr):
    return w.op("map", expr=expr, outputs=[output])


def sort(column, **params):
    inputs = [column] + [params[k] for k in ("tie_break_col",) if k in params]
    return w.op("sort", inputs=inputs, column=column, **params)


def limit(n, **params):
    return w.op("limit", limit=n, **params)


def decay(column, **params):
    return w.expr("decay", Col(column), **params)


def rounded(column, places):
    return w.expr("round_decimal", Col(column), decimal=places)


def rerank(column, **params):
    return w.expr("rerank_model", Col(column), **params)


TS_DESC = w.chain(sort("ts", desc=True, tie_break_col="$id"))
PLUS_TS = w.chain(mapped("$score", nc(S, Col("ts"))), sort("$score", desc=True, tie_break_col="$id"))
PLUS_TS_TOP2 = w.chain(mapped("$score", nc(S, Col("ts"))), sort("$score", desc=True, tie_break_col="$id"), limit(2))
TEMP_COLLISION = w.chain(mapped("tmp", nc(S, Col("ts"))), sort("tmp", desc=True), limit(3))
LIMIT_ONLY = w.chain(limit(10))
BAD_LIMIT = w.chain(limit(0))
NON_FINITE = w.chain(mapped("$score", nc(Col("ts"), 1e308, mode="multiply")))
NONE_SCORE = w.chain(mapped("$score", rounded("n", 2)))
VALID_MODEL = w.chain(mapped("$score", rerank("s", provider="cohere", queries=["q"])))


def snapshot(response):
    """响应里可比较的全部内容；JSON 与 `$meta` 保留原始文本。"""
    if response.status.code != 0:
        return {"status": [response.status.code, response.status.reason]}
    results = response.results
    fields = []
    for field in results.fields_data:
        if field.HasField("vectors"):
            vectors = field.vectors
            if vectors.HasField("sparse_float_vector"):
                content = list(vectors.sparse_float_vector.contents)
            else:
                flat = list(vectors.float_vector.data)
                content = [flat[i:i + vectors.dim] for i in range(0, len(flat), vectors.dim)]
        else:
            kind = field.scalars.WhichOneof("data")
            data = getattr(field.scalars, kind).data if kind else []
            content = [v.decode() if isinstance(v, bytes) else str(v) if hasattr(v, "ListFields") else v
                       for v in data]
        fields.append([field.field_name, field.type, content, list(field.valid_data)])
    group = None
    if results.HasField("group_by_field_value"):
        g = results.group_by_field_value
        kind = g.scalars.WhichOneof("data")
        group = [g.field_name, list(getattr(g.scalars, kind).data) if kind else [], list(g.valid_data)]
    return {
        "topks": list(results.topks),
        "ids": list(results.ids.int_id.data) or list(results.ids.str_id.data),
        "scores": list(results.scores),
        "output_fields": list(results.output_fields),
        "fields": fields,
        "group": group,
    }


def values(outcome):
    """按字段名给出逐行的值（空值为 None）。"""
    out = {}
    for name, _, content, valid in outcome["fields"]:
        out[name] = [v if ok else None for v, ok in zip(content, valid)] if valid else content
    return out


def reordered(plain, ids):
    at = [plain["ids"].index(i) for i in ids]
    return {name: [column[i] for i in at] for name, column in values(plain).items()}


class Chains:
    def __init__(self, diff):
        self.clients = [diff.rust, diff.ref]
        self.stubs = [c._get_connection()._stub for c in self.clients]
        self.failures = []
        self.count = 0

    def both(self, action):
        for client in self.clients:
            action(client)

    def call(self, rpc, request):
        out = []
        for stub in self.stubs:
            try:
                out.append(snapshot(getattr(stub, rpc)(request)))
            except grpc.RpcError as error:
                out.append({"rpc": [error.code().name, error.details()]})
        return out

    def search(self, name, *chains, raw=None, function_score=False, **kwargs):
        kwargs.setdefault("anns_field", "vector")
        kwargs.setdefault("param", {"metric_type": "L2", "params": {}})
        kwargs.setdefault("limit", 4)
        kwargs.setdefault("data", [[0.0, 0.0]])
        request = Prepare.search_requests_with_expr(collection_name=name, **kwargs)
        w.attach(request, *chains)
        if function_score:
            request.function_score.SetInParent()
        for key, value in (raw or {}).items():
            request.search_params.add(key=key, value=value)
        return self.call("Search", request)

    def compare(self, label, outcomes):
        self.count += 1
        rust, ref = outcomes
        if not same(rust, ref):
            self.failures.append(f"{label}\n rust {json.dumps(rust)[:700]}\n ref  {json.dumps(ref)[:700]}")
        return rust

    def check(self, label, name, *chains, **kwargs):
        return self.compare(label, self.search(name, *chains, **kwargs))

    def finish(self, expected):
        assert not self.failures, f"{len(self.failures)} / {self.count}:\n" + "\n".join(self.failures[:20])
        assert self.count == expected


def create_rich(client, name, *, metric="L2", load=True, extra=None, partition=False):
    """四行：ts 有并列，n 与 tz 有空值，dyn / tmp 是动态键；`partition` 时另在分区 p 里加两行。"""
    schema = client.create_schema(auto_id=False, enable_dynamic_field=True)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("ts", DataType.INT64)
    schema.add_field("f", DataType.FLOAT)
    schema.add_field("s", DataType.VARCHAR, max_length=32)
    schema.add_field("tz", DataType.TIMESTAMPTZ, nullable=True)
    schema.add_field("n", DataType.INT64, nullable=True)
    schema.add_field("b", DataType.BOOL)
    schema.add_field("j", DataType.JSON)
    schema.add_field("arr", DataType.ARRAY, element_type=DataType.INT64, max_capacity=4)
    if extra:
        schema.add_field(extra, DataType.DOUBLE)
    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    index = client.prepare_index_params()
    index.add_index(field_name="vector", index_type="FLAT", metric_type=metric, params={})
    client.create_collection(name, schema=schema, index_params=index)
    data = [(10, 0.1, "b", "2026-01-01T00:00:00Z", 5), (20, 0.25, "a", None, None),
            (30, 0.5, "c", "2026-01-03T00:00:00+08:00", 7), (20, 2.5, "a", "2026-01-02T00:00:00Z", 5)]
    rows = []
    for i, (ts, f, s, tz, n) in enumerate(data, 1):
        row = {"id": i, "ts": ts, "f": f, "s": s, "tz": tz, "n": n, "b": i % 2 == 0, "j": {"k": i}, "arr": [i],
               "vector": [0.01 * i, 0.0], "dyn": i * 100, "tmp": "dynamic-tmp"}
        if extra:
            row[extra] = i + 0.5
        rows.append(row)
    client.insert(name, rows)
    if partition:
        client.create_partition(name, "p")
        client.insert(name, [{**rows[0], "id": 10 + i, "ts": 5 * i, "vector": [0.015 * i, 0.0]} for i in (1, 2)],
                      partition_name="p")
    if load:
        client.load_collection(name)
    else:
        client.release_collection(name)


PROJECTIONS = [None, ["ts"], ["f"], ["dyn"], ["tmp"], ["id"], ["id", "ts"], ["ts", "ts"], ["n", "tz"],
               ["vector", "j", "arr"], ["nosuch"], ["dyn", "nosuch", "s"], ["b"]]


def test_function_chain_projection_and_parameters_match_3x(pair):
    """投影表的每一格、表达式与排序、检索参数的组合；MemTable 与 flush 之后各一遍。"""
    c = Chains(pair.open())
    c.both(lambda client: create_rich(client, "c", partition=True))
    expressions = [
        nc(Col("ts"), Col("$id"), mode="multiply"), nc(Col("ts"), Col("f"), 15, mode="min"),
        nc(Col("ts"), Col("$id"), mode="max"), nc(1e16, 1.0, -1e16, Col("ts"), mode="avg"),
        nc(Col("ts"), Col("f"), Col("$id"), mode="weighted", weights=[0.1, 0.2, 0.7]),
        nc(Col("ts"), 9223372036854775807, 9223372036854775807), nc(Col("n"), Col("ts")),
        decay("n", function="linear", origin=5, scale=4), decay("ts", function="gauss", origin=20, scale=10),
        decay("f", function="exp", origin=0.1, scale=1.5, offset=0.1, decay=0.3),
        decay("tz", function="linear", origin=1767225600, scale=86400 * 3), decay("id", function="exp", origin=2, scale=3),
        rounded("f", 1), rounded("ts", 3), nc(Col("ts"), -1e300, mode="multiply"), nc(Col("f"), 3e38, mode="multiply"),
    ]
    sorts = [("s", {"desc": False}), ("s", {"desc": True, "tie_break_col": "$id"}), ("tz", {"desc": False}),
             ("tz", {"desc": True}), ("n", {"desc": False}), ("n", {"desc": True, "tie_break_col": "$id"}),
             ("f", {}), ("id", {}), ("ts", {"desc": True, "tie_break_col": "s"}), ("b", {"tie_break_col": "f"}),
             ("$id", {"desc": True}), ("$score", {"desc": False})]
    params = [
        {"limit": 2}, {"limit": 3, "raw": {"offset": "1"}}, {"expr": "ts >= 20"}, {"expr": "ts > 1000"},
        {"partition_names": ["p"]}, {"partition_names": ["_default", "p"], "limit": 6},
        {"param": {"metric_type": "L2", "params": {"radius": 0.001, "range_filter": 0.0}}},
        {"data": [[0.0, 0.0], [0.04, 0.0], [0.02, 0.0]], "output_fields": ["ts"]},
        {"raw": {"group_by_field": "ts"}, "output_fields": ["f"]},
        {"raw": {"group_by_field": "ts", "group_size": "2"}}, {"raw": {"group_by_field": "s", "group_size": "3",
                                                                       "strict_group_size": "true"}},
        {"round_decimal": 1}, {"round_decimal": 0},
        {"output_fields": ["tz"], "raw": {"time_fields": "year,hour", "timezone": "Asia/Shanghai"}},
        {"output_fields": ["tz", "ts"], "raw": {"timezone": "America/New_York"}},
        {"raw": {"iterator": "True"}},
        {"raw": {"iterator": "True", "search_iter_v2": "True", "search_iter_batch_size": "2",
                 "search_iter_id": "abc", "search_iter_last_bound": "0.0001"}},
    ]
    chains = [TS_DESC, PLUS_TS, PLUS_TS_TOP2, TEMP_COLLISION, LIMIT_ONLY,
              w.chain(mapped("x", rounded("n", 0)), sort("x", desc=True, tie_break_col="$id"), limit(2))]
    for source in ("memtable", "flushed"):
        for output in PROJECTIONS:
            plain = c.check(f"{source} plain {output}", "c", output_fields=output)
            for index, chain in enumerate(chains):
                chained = c.check(f"{source} projection {output} chain {index}", "c", chain, output_fields=output)
                # 投影合同：链读了但没点名的字段、临时列都不返回，字段与不带链时相同。
                assert chained["output_fields"] == plain["output_fields"]
                assert values(chained) == reordered(plain, chained["ids"])
        for index, expr in enumerate(expressions):
            c.check(f"{source} expression {index}", "c", w.chain(mapped("$score", expr)))
            c.check(f"{source} expression {index} sorted", "c",
                    w.chain(mapped("t", expr), sort("t", tie_break_col="$id"), limit(3)), output_fields=["ts"])
        for column, extra in sorts:
            c.check(f"{source} sort {column} {extra}", "c", w.chain(sort(column, **extra)))
        for index, kwargs in enumerate(params):
            for chain in (PLUS_TS_TOP2, TS_DESC, LIMIT_ONLY):
                c.check(f"{source} params {index} {kwargs}", "c", chain, **kwargs)
        c.both(lambda client: client.flush("c"))
    c.finish(2 * (len(PROJECTIONS) * (1 + len(chains)) + 2 * len(expressions) + len(sorts) + 3 * len(params)))


def test_function_chain_metrics_match_3x(pair):
    """三种 dense 度量、BM25（带任何链都变号）与稀疏 IP；稀疏字段作链的输入被拒绝。"""
    c = Chains(pair.open())
    for metric in ("L2", "IP", "COSINE"):
        c.both(lambda client, m=metric: create_rich(client, m.lower(), metric=m))

    def fts(client):
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
            {"id": 4, "ts": 1, "text": "apple durian", "sv": {3: 1.0}},
        ])
        client.load_collection("fts")

    c.both(fts)
    by_score = w.chain(sort("$score", desc=True, tie_break_col="$id"))
    plus_f = w.chain(mapped("$score", nc(S, Col("f"))), sort("$score", desc=True, tie_break_col="$id"))
    dense = [None, LIMIT_ONLY, by_score, PLUS_TS, plus_f, w.chain(mapped("$score", rounded("$score", 2)))]
    for metric in ("L2", "IP", "COSINE"):
        for index, chain in enumerate(dense):
            chains = [chain] if chain else []
            for data in ([[0.0, 0.0]], [[1.0, 0.0]], [[0.3, -0.7], [0.01, 0.0]]):
                c.check(f"{metric} {index} {data}", metric.lower(), *chains, data=data,
                        param={"metric_type": metric, "params": {}}, output_fields=["ts"])
    bm25 = dict(anns_field="sp", param={"metric_type": "BM25", "params": {}}, data=["apple"], output_fields=["ts"])
    sparse = dict(anns_field="sv", param={"metric_type": "IP", "params": {}}, data=[{1: 1.0, 2: 1.0}],
                  output_fields=["text"])
    by_text = w.chain(sort("text", desc=False))
    plus_ts = w.chain(mapped("$score", nc(S, Col("ts"))), sort("$score", desc=True, tie_break_col="$id"))
    sparse_chains = [None, LIMIT_ONLY, by_score, by_text, plus_ts, w.chain(mapped("$score", rounded("$score", 2))),
                     w.chain(sort("sv")), w.chain(mapped("$score", nc(S, Col("sp"))))]
    for kwargs in (bm25, sparse):
        for index, chain in enumerate(sparse_chains):
            chains = [chain] if chain else []
            c.check(f"{kwargs['anns_field']} {index}", "fts", *chains, **kwargs)
            c.check(f"{kwargs['anns_field']} {index} round", "fts", *chains, round_decimal=2, **kwargs)
        c.check(f"{kwargs['anns_field']} two queries", "fts", PLUS_TS_TOP2,
                **{**kwargs, "data": kwargs["data"] * 2})
    c.finish(3 * len(dense) * 3 + 2 * (2 * len(sparse_chains) + 1))


def test_function_chain_errors_and_their_order_match_3x(pair):
    """解码与校验错误、冲突、order_by_fields、错误先后、运行期错误、`None` 分数与 HybridSearch。"""
    c = Chains(pair.open())
    c.both(lambda client: create_rich(client, "c"))
    c.both(lambda client: create_rich(client, "released", load=False))
    invalid = [
        w.chain(w.op("")), w.chain(mapped("x", w.expr("", Col("ts")))), w.chain(mapped("x", nc(Col("ts"), None))),
        w.chain(mapped("", nc(S, Col("ts")))), w.chain(limit(1), stage=99), w.chain(mapped("$id", nc(S)), w.op("")),
        w.chain(limit(1), stage=3), w.chain(), w.chain(w.op("filter")), w.chain(mapped("ts", nc(S, Col("ts")))),
        w.chain(mapped("$score", nc(S, Col("dyn")))), w.chain(mapped("$score", nc(S, Col("j")))),
        w.chain(mapped("$score", nc(S, Col("s")))), w.chain(mapped("$score", nc(S, Col("ts"), mode="weighted"))),
        w.chain(mapped("$score", decay("ts", function="gauss", origin=1, scale=0))),
        w.chain(mapped("$score", rounded("$score", 7))), w.chain(sort("vector")), w.chain(sort("ts", desc=1)),
        w.chain(limit(2, offset=-1)), BAD_LIMIT,
        w.chain(mapped("$score", rerank("s", provider="zz", queries=["q"]))),
        w.chain(mapped("$score", rerank("s", provider="cohere", queries=["q"], Base_URL="x"))),
        w.chain(mapped("$score", rerank("s", provider="cohere", queries=["q", "r"]))),
        w.chain(mapped("$score", rerank("s", provider="cohere", queries=["q"], zz=1, aa=2))),
        w.chain(mapped("$score", rerank("ts", provider="cohere", queries=["q"]))),
        w.chain(mapped("$score", rerank("s", provider="cohere", queries=[]))),
    ]
    for index, chain in enumerate(invalid):
        c.check(f"invalid {index}", "c", chain)
        c.check(f"invalid {index} released", "released", chain)
    # 冲突与条数，order_by_fields 的各种取值（带链与不带链）。
    c.check("function_score", "c", PLUS_TS, function_score=True)
    c.check("two chains", "c", PLUS_TS, PLUS_TS)
    c.check("two bad chains", "c", BAD_LIMIT, BAD_LIMIT)
    c.check("function_score order_by", "c", BAD_LIMIT, function_score=True, raw={"order_by_fields": '"ts:asc"'})
    c.check("order_by two chains", "c", BAD_LIMIT, BAD_LIMIT, raw={"order_by_fields": '"ts:asc"'})
    order_by = ['"ts:asc"', " ", "ts", "[1]", "1", "NaN", "1e999", "", "0", "[]", "null", '""', "{}", "false", "0.0"]
    for value in order_by:
        for chains in ((BAD_LIMIT,), (PLUS_TS,), ()):
            c.check(f"order_by {value!r} {len(chains)}", "c", *chains, raw={"order_by_fields": value})
        c.check(f"order_by {value!r} released", "released", BAD_LIMIT, raw={"order_by_fields": value})
    # 错误先后：每种情形分别配非法链、合法链与运行期出错的链。
    situations = [
        ("missing", {"name": "nope"}), ("anns_field", {"anns_field": "nope"}),
        ("metric", {"param": {"metric_type": "IP", "params": {}}}), ("released", {"name": "released"}),
        ("filter", {"expr": "ts >"}), ("filter field", {"expr": "nosuch > 1"}), ("output", {"output_fields": ["nosuch"]}),
        ("group", {"raw": {"group_by_field": "nosuch"}}), ("group json", {"raw": {"group_by_field": "j"}}),
        ("partition", {"partition_names": ["nope"]}), ("bad zone", {"raw": {"timezone": "Not/AZone"}}),
        ("bad parts", {"output_fields": ["tz"], "raw": {"time_fields": "year,nosuch"}}),
        ("round", {"round_decimal": 7}), ("offset", {"raw": {"offset": "-1"}}),
    ]
    for label, kwargs in situations:
        kwargs = dict(kwargs)
        name = kwargs.pop("name", "c")
        for index, chain in enumerate((BAD_LIMIT, PLUS_TS, NON_FINITE, NONE_SCORE)):
            c.check(f"order {label} {index}", name, chain, **kwargs)
    # 运行期错误只看返回的命中；`None` 分数的先后。
    runtime = [
        (NON_FINITE, {}), (w.chain(mapped("$score", nc(Col("f"), 2e38, mode="multiply"))), {}),
        (w.chain(mapped("$score", nc(Col("ts"), 1e307, mode="multiply"))), {"limit": 1}),
        (w.chain(mapped("$score", nc(Col("ts"), 1e307, mode="multiply"))), {"limit": 2}),
        (w.chain(mapped("$score", decay("ts", function="gauss", origin=0, scale=1e-200))), {}),
        (NONE_SCORE, {"limit": 1}), (NONE_SCORE, {}), (NONE_SCORE, {"round_decimal": 2}),
        (NONE_SCORE, {"round_decimal": 0}), (NONE_SCORE, {"round_decimal": 2, "raw": {"timezone": "Not/AZone"}}),
        (NONE_SCORE, {"output_fields": ["tz"], "raw": {"time_fields": "year,nosuch"}}),
        (NONE_SCORE, {"output_fields": ["tz"], "round_decimal": 2, "raw": {"time_fields": "year,nosuch"}}),
        (LIMIT_ONLY, {"output_fields": ["tz"], "raw": {"time_fields": "year,nosuch"}}),
        (w.chain(mapped("$score", rounded("n", 2)), sort("$score", desc=True), limit(3)), {}),
        (NONE_SCORE, {"limit": 2, "data": [[0.0, 0.0], [0.0, 0.0]]}),
        (w.chain(mapped("t", nc(S, 1)), mapped("$score", nc(Col("t"), 1e308, mode="multiply"))),
         {"data": [[0.0, 0.0], [0.03, 0.0]]}),
    ]
    for index, (chain, kwargs) in enumerate(runtime):
        c.check(f"runtime {index}", "c", chain, **kwargs)
    # 带链检索的 bad zone：没有 `None` 分数时照常报请求时区的错误。
    zone = c.search("c", NONE_SCORE, raw={"timezone": "Not/AZone"})
    c.compare("none score bad zone", zone)
    # HybridSearch 不接受 function_chains（15 号字段）。
    sub = Prepare.search_requests_with_expr(collection_name="c", anns_field="vector",
                                            param={"metric_type": "L2", "params": {}}, limit=3, data=[[0.0, 0.0]])
    hybrid = milvus_pb2.HybridSearchRequest(collection_name="c", requests=[sub])
    hybrid.rank_params.add(key="strategy", value="rrf")
    hybrid.rank_params.add(key="limit", value="3")
    c.compare("hybrid", c.call("HybridSearch", hybrid))
    hybrid.MergeFromString(w._field(15, PLUS_TS))
    c.compare("hybrid chain", c.call("HybridSearch", hybrid))
    c.finish(2 * len(invalid) + 5 + 4 * len(order_by) + 4 * len(situations) + len(runtime) + 3)


def test_registered_differences(pair):
    """三项登记差异：两边各自断言。"""
    assert "COHERE_API_KEY" not in os.environ, "3.x 会用这个 key 调用外部服务；运行差分前先取消它"
    c = Chains(pair.open())
    c.both(lambda client: create_rich(client, "c"))
    c.both(lambda client: create_rich(client, "released", load=False))

    # A：`*` / `$meta` 投影。
    for output in (["*"], ["$meta"], ["*", "ts"], ["$meta", "dyn"], ["$meta", "ts"]):
        rust_plain, ref_plain = c.search("c", output_fields=output)
        assert same(rust_plain, ref_plain)
        for chain in (TS_DESC, LIMIT_ONLY):
            rust, ref = c.search("c", chain, output_fields=output)
            assert rust["ids"] == ref["ids"] and same(rust["scores"], ref["scores"])
            assert values(rust) == reordered(rust_plain, rust["ids"])
            assert values(ref) != reordered(ref_plain, ref["ids"])
            raw = {name: (content, valid) for name, _, content, valid in ref["fields"]}
            meta = [json.loads(v) for v in raw["$meta"][0]]
            assert meta == [{"dyn": 100 * i} if "dyn" in output else {} for i in ref["ids"]]
            if "*" in output:
                # 没有显式点名的非主键字段：原始值是类型默认值，valid_data 全为假（SDK 读成 None）。
                defaults = {"ts": 0, "f": 0.0, "s": "", "tz": "", "n": 0, "b": False, "j": "null",
                            "arr": "long_data {\n}\n", "vector": [0.0, 0.0]}
                for name, default in defaults.items():
                    if name in output:
                        assert raw[name][0] == values(rust)[name]
                    else:
                        assert raw[name] == ([default] * 4, [False] * 4), name
                assert raw["id"][0] == ref["ids"]

    # B：合法的 rerank_model。
    for name in ("c", "released"):
        rust, ref = c.search(name, VALID_MODEL)
        assert rust == {"rpc": ["UNIMPLEMENTED", "MilvusLite does not support function chains: rerank_model: "
                                "not supported in this build of milvus-lite-rust yet"]}
        assert ref["status"][0] == 1 and ref["status"][1].startswith("Cohere API key is required"), ref

    # G：按名为 `$score` / `$id` 的字段分组。
    def grouped(client):
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

    c.both(grouped)
    raw = {"group_size": "2"}
    rust, ref = c.search("grouped", TS_DESC, raw={**raw, "group_by_field": "$score"}, output_fields=["$score", "ts"])
    assert rust["ids"] == ref["ids"] == [4, 3, 2, 1]
    assert same(rust["scores"], [0.0016, 0.0009, 0.0004, 0.0001])
    assert ref["scores"] == [0.0, 1.0, 0.0, 1.0]
    assert rust["group"] == ref["group"] == ["$score", [0, 1, 0, 1], []]
    rust, ref = c.search("grouped", TS_DESC, raw={**raw, "group_by_field": "$id"}, output_fields=["$id", "ts"])
    assert rust["ids"] == [4, 3, 2, 1] and rust["group"] == ["$id", ["g0", "g1", "g0", "g1"], []]
    assert ref["status"] == [1, "function chain runtime output chunk 0 row 0 has unknown $id value 'g0'"], ref
    # 不带链时两边相同。
    for field in ("$score", "$id"):
        c.check(f"group {field} without chain", "grouped", raw={**raw, "group_by_field": field})
    c.finish(2)
