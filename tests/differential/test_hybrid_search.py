"""0026 第 3 步：HybridSearch 分别发给本实现与 3.x（43d1257），逐项比较原始响应。

比较方式与 test_rerank_boost.py 相同（原始 ids、scores、topks、group_by_field_value 与 fields_data，分数容差 1e-5）。
SDK 写不出的请求（rank_params 的原始 JSON、子请求自带的 function_score、路由级的时区、空的 requests）经 `mod`
直接改写 HybridSearchRequest。model 与候选真正用到的 random_score 是登记的差异（0026 第 10 节），这里不比较。
"""

from pymilvus import AnnSearchRequest, DataType, Function, FunctionScore, FunctionType, RRFRanker, WeightedRanker
from pymilvus.client.prepare import Prepare
from pymilvus.grpc_gen import common_pb2

from test_rerank_boost import QUERY, Clients, boost, vector
from test_rerank_l2 import decay, fn
from test_reopen import pair  # noqa: F401  （fixture）


class HybridClients(Clients):
    """发 HybridSearch，并允许改写发出的请求。"""

    method = "hybrid_search"

    def __init__(self, diff):
        super().__init__(diff)
        self.mod = None
        for client in self.clients:
            stub = client._get_connection()._stub
            inner = stub.HybridSearch

            def hybrid(request, *args, _inner=inner, **kwargs):
                if self.mod is not None:
                    self.mod(request)
                return _inner(request, *args, **kwargs)

            stub.HybridSearch = hybrid

    def check(self, label, *args, mod=None, **kwargs):
        self.mod = mod
        try:
            super().check(label, *args, **kwargs)
        finally:
            self.mod = None


# HybridSearchRequest.function_chains（字段 15）里追加一个空的 FunctionChain。pymilvus 2.6 的 proto 没有这个字段，
# 直接按字段号写入字节：3.0 线解析成该字段，2.6 线作为未知字段原样发出，两条线发出的请求相同。
def add_function_chain(request):
    request.MergeFromString(bytes([15 << 3 | 2, 0]))


def route(data=QUERY, limit=8, expr=None, metric=None):
    return AnnSearchRequest(data=data, anns_field="v", param={"metric_type": metric} if metric else {}, limit=limit,
                            expr=expr)


def rank_param(key, value):
    def mod(request):
        for p in request.rank_params:
            if p.key == key:
                p.value = value
                return
        request.rank_params.append(common_pb2.KeyValuePair(key=key, value=value))
    return mod


def route_param(index, key, value):
    def mod(request):
        params = request.requests[index].search_params
        for p in params:
            if p.key == key:
                p.value = value
                return
        params.append(common_pb2.KeyValuePair(key=key, value=value))
    return mod


def route_score(index, ranker):
    """第 index 路自带的 function_score（SDK 的 AnnSearchRequest 写不出）。"""
    return lambda request: request.requests[index].function_score.CopyFrom(Prepare.function_score_schema(ranker))


def chain(*mods):
    def mod(request):
        for m in mods:
            m(request)
    return mod


def test_hybrid_ties_match_3x(pair):
    """无分组时同分按 `$id` 升序（VARCHAR 按码点）；GroupByOp 按首次出现。严格断言顺序（0026 的 6 个 Hybrid `tie *`）。"""
    c = HybridClients(pair.open())
    names = ["k9", "k10", "k2", "Z", "a", "é", "中", "k1"]
    tie = c.create([("g", DataType.INT64, {})],
                   [{"id": n, "v": [1.0, 0.0], "g": i % 2, "age": 5} for i, n in enumerate(names)])
    c.check("rrf", tie, [route()], RRFRanker(), limit=8, output_fields=["g"])
    c.check("rrf two routes", tie, [route(), route()], RRFRanker(), limit=8, output_fields=["g"])
    c.check("weighted zero", tie, [route()], WeightedRanker(0), limit=8, output_fields=["g"])
    c.check("weighted", tie, [route()], WeightedRanker(1), limit=8, output_fields=["g"])
    c.check("group", tie, [route()], WeightedRanker(1), limit=2, group_by_field="g", group_size=2, output_fields=["g"])
    c.check("group order", tie, [route()], WeightedRanker(1), limit=8, group_by_field="g", group_size=8,
            output_fields=["g"])
    c.finish(6)


def test_hybrid_merges_windows_and_routes_match_3x(pair):
    c = HybridClients(pair.open())
    total = 0
    # 每路的距离各不相同，避免并列跨过候选窗口边界（0004「决策」）；两路的查询不同，候选部分重叠。
    for metric, first, second in (("COSINE", [[1.0, 0.0]], [[1.0, 1.2]]), ("IP", [[1.0, 1.0]], [[2.0, 1.0]]),
                                  ("L2", [[1.0, 0.0]], [[1.0, 1.7]])):
        name = c.create([("cat", DataType.INT64, {}), ("price", DataType.DOUBLE, {})],
                        [{"id": i, "v": vector(i), "age": i % 17, "cat": i % 3, "price": (i % 11) * 1.5} for i in range(60)],
                        metric=metric, pk=DataType.INT64)
        two = [route(first, 6), route(second, 5)]
        rankers = {
            "rrf": RRFRanker(),
            "rrf k=1": RRFRanker(k=1),
            "weighted": WeightedRanker(0.3, 0.7),
            "weighted raw": WeightedRanker(0.3, 0.7, norm_score=False),
            "fs rrf": fn("rrf", k=5),
            "fs weighted": fn("weighted", weights=[0.4, 0.6]),
            "gauss": decay(),
            "exp sum": decay("price", "exp", 3, 2, offset=1, decay=0.3, score_mode="sum"),
            "linear avg": decay("cat", "linear", 1, 4, decay=0.2, score_mode="avg"),
        }
        for label, ranker in rankers.items():
            c.check(f"{metric} {label}", name, two, ranker, limit=5, output_fields=["age"])
            c.check(f"{metric} {label} offset", name, two, ranker, limit=3, offset=2)
            c.check(f"{metric} {label} group", name, two, ranker, limit=2, group_by_field="cat", group_size=2,
                    output_fields=["cat"])
            c.check(f"{metric} {label} nq", name, [route(first + second, 4), route(second + first, 4)], ranker, limit=3)
            c.check(f"{metric} {label} empty route", name, [route(first, 6), route(second, 5, expr="age < 0")], ranker,
                    limit=4)
            total += 5
        # 顶层 boost 放大各路窗口、并入各路；子请求自带的 boost 只作用在那一路，子请求的 L2 只解码。
        c.check(f"{metric} top boost", name, two, FunctionScore([boost(3.0, "cat == 1").functions[0], fn("rrf")]),
                limit=4, output_fields=["cat"])
        c.check(f"{metric} top boost decay", name, two, FunctionScore([boost(0.5, "cat == 2").functions[0], decay()]),
                limit=4, offset=1)
        c.check(f"{metric} route boost", name, two, RRFRanker(), limit=4,
                mod=route_score(1, FunctionScore([boost(5.0, "age == 3").functions[0]])))
        c.check(f"{metric} route boost + top boost", name, two, boost(2.0, "cat == 0", boost_mode="sum"), limit=4,
                mod=route_score(0, FunctionScore([boost(0.1, "age > 5").functions[0]])))
        c.check(f"{metric} route L2 ignored", name, two, RRFRanker(), limit=4,
                mod=route_score(0, FunctionScore([fn("weighted", weights=[5])])))
        total += 5
    # 两路度量不同（路由的 metric_type）：weighted 不归一化时 L2 一路按方向转换。
    name = c.create([("cat", DataType.INT64, {})], [{"id": i, "v": vector(i), "age": i % 17, "cat": i % 3} for i in range(60)],
                    pk=DataType.INT64)
    mixed = [route([[1.0, 0.0]], 6), route([[1.0, 1.7]], 6, metric="L2")]
    for label, ranker in (("weighted raw", WeightedRanker(0.5, 0.5, norm_score=False)), ("weighted", WeightedRanker(0.5, 0.5)),
                          ("decay sum", decay(score_mode="sum")), ("rrf", RRFRanker())):
        c.check(f"mixed {label}", name, mixed, ranker, limit=6)
        total += 1
    # 各路的窗口：路由 limit 就是候选数；分组与 boost 才放大。
    for limits in ((3, 8), (0, 5), (-1, 5)):
        reqs = [route([[1.0, 0.0]], 10), route([[1.0, 1.7]], 10)]
        md = chain(route_param(0, "topk", str(limits[0])), route_param(1, "topk", str(limits[1])))
        c.check(f"route limits {limits}", name, reqs, decay(), limit=10, mod=md)
        c.check(f"route limits {limits} group", name, reqs, RRFRanker(), limit=2, group_by_field="cat", group_size=2,
                mod=md)
        total += 2
    # rank_params 的 limit / offset 的 Python 切片与 `limit <= 0`（绕过 SDK 的客户端校验）。
    for limit, offset in ((0, 0), (-1, 1), (3, -1), (2, 1000), (-3, 1)):
        md = chain(rank_param("limit", str(limit)), rank_param("offset", str(offset)))
        c.check(f"rank limit {limit} offset {offset}", name, [route(), route([[1.0, 1.7]])], RRFRanker(), mod=md)
        c.check(f"rank group limit {limit} offset {offset}", name, [route(), route([[1.0, 1.7]])], RRFRanker(),
                group_by_field="cat", mod=md)
        total += 2
    # rank_params 路径不校验：k 与 weights 原值参与运算，类型错误在用到时才报。
    for raw in ('{"k": 0}', '{"k": -1}', '{"k": -1.5}', '{"k": true}', '{"k": "5"}', '{"k": null}', '{"k": 1e400}',
                '{"k": 9007199254740993}', '"x"', "[]"):
        c.check(f"rank rrf params {raw}", name, [route(), route([[1.0, 1.7]])], RRFRanker(), limit=4,
                mod=rank_param("params", raw))
        total += 1
    for raw in ('{"weights": [2, -1]}', '{"weights": [1, true]}', '{"weights": "ab"}', '{"weights": 5}',
                '{"weights": [0.5, "0.5"]}', '{"weights": [0.5, null]}', '{"weights": [0.5]}',
                '{"weights": [0.5, 0.5], "norm_score": "false"}', '{"weights": [0.5, 0.5], "norm_score": 0}'):
        c.check(f"rank weighted params {raw}", name, [route(), route([[1.0, 1.7]], metric="L2")],
                WeightedRanker(0.5, 0.5), limit=4, mod=rank_param("params", raw))
        total += 1
    c.check("rank k str no candidates", name, [route(expr="age < 0")], RRFRanker(), mod=rank_param("params", '{"k": "5"}'))
    c.check("rank weights 5 no candidates", name, [route(expr="age < 0")], WeightedRanker(1),
            mod=rank_param("params", '{"weights": 5}'))
    total += 2
    # 错误先后：rank_params 的 int() → function_score 解码 → 逐路（子请求解析、过滤）→ 合并链（strategy、L2 校验）→
    # 各路查询数。顶层有 L2 时忽略 strategy。
    unknown = Function("u", FunctionType.RERANK, [], params={"reranker": "nope"})
    errors = [
        ("limit bad + fs bad", [route()], unknown, rank_param("limit", '"x"')),
        ("group_size bad", [route()], RRFRanker(), rank_param("group_size", "[]")),
        ("fs bad + route bad filter", [route(expr="age ==")], unknown, None),
        ("strategy bad + route bad filter", [route(expr="age ==")], RRFRanker(), rank_param("strategy", '"foo"')),
        ("strategy bad", [route()], RRFRanker(), rank_param("strategy", '"RRF"')),
        ("strategy ignored with L2", [route()], decay(), rank_param("strategy", '"foo"')),
        ("params not object", [route()], RRFRanker(), rank_param("params", '"x"')),
        ("route2 missing topk + route1 bad filter", [route(expr="age =="), route()], RRFRanker(),
         lambda r: r.requests[1].ClearField("search_params")),
        ("route1 missing topk + route2 bad filter", [route(), route(expr="age ==")], RRFRanker(),
         lambda r: r.requests[0].ClearField("search_params")),
        ("route boost filter bad", [route(), route()], RRFRanker(),
         route_score(1, FunctionScore([boost(2.0, "age ==").functions[0]]))),
        ("route L2 decode error", [route(), route()], RRFRanker(), route_score(0, FunctionScore([fn("rrf", k="abc")]))),
        ("fs k range", [route()], fn("rrf", k=0), None),
        ("fs weights length", [route(), route()], fn("weighted", weights=[0.5]), None),
        ("fs decay field", [route()], decay("nosuch"), None),
        ("nq mismatch", [route([[1.0, 0.0], [0.0, 1.0]]), route()], WeightedRanker(0.5, 0.5), None),
        ("nq mismatch + weights length", [route([[1.0, 0.0], [0.0, 1.0]]), route()], WeightedRanker(0.2, 0.3, 0.5), None),
        ("function_chains", [route()], RRFRanker(), add_function_chain),
        ("no requests", [route()], RRFRanker(), lambda r: r.ClearField("requests")),
        ("no requests fs weighted", [route()], fn("weighted", weights=[0.5]), lambda r: r.ClearField("requests")),
    ]
    for label, reqs, ranker, md in errors:
        c.check(f"error {label}", name, reqs, ranker, limit=3, mod=md)
        total += 1
    c.finish(total)


def test_hybrid_wide_integer_scores_match_3x(pair):
    """同名动态键把超出 int64 的整数盖到 `$score` 上、把多个命中并成同一个 `$id`：decay 的 sum / avg 是精确的
    Python 整数运算（保留符号，平均按整数真除法正确舍入），超出 float64 的部分按 3.x 报错。"""
    c = HybridClients(pair.open())
    total = 0
    for wide in (2**70, 10**400):
        # `$score` 写成 ±1111111，写入时改写成 ±wide；`$id` 是 i % 3，三行并成一个 `$id`。
        scores = [1111111, -1111111, 7, 1111111, -1111111, 5, 1111111, 3]
        name = c.create([], [{"id": i, "v": vector(i), "age": 0, "$id": i % 3, "$score": s} for i, s in enumerate(scores)],
                        dynamic=True, pk=DataType.INT64,
                        meta_patch={"from": b"1111111", "to": str(wide).encode()})
        two = [route(limit=8), route([[1.0, 0.1]], 8)]
        for mode in ("max", "sum", "avg"):
            ranker = decay(origin=0, scale=100, score_mode=mode)
            c.check(f"{wide} {mode}", name, two, ranker, limit=8, output_fields=["$score", "$id"])
            c.check(f"{wide} {mode} one route", name, two[:1], ranker, limit=8, output_fields=["$score", "$id"])
            c.check(f"{wide} {mode} mixed", name, [two[0], route([[1.0, 0.1]], 8, metric="L2")], ranker, limit=8,
                    output_fields=["$score", "$id"])
            total += 3
        c.check(f"{wide} weighted raw", name, two, WeightedRanker(0.5, 0.5, norm_score=False), limit=8,
                output_fields=["$score", "$id"])
        c.check(f"{wide} weighted int weights", name, two, WeightedRanker(0.5, 0.5), limit=8, output_fields=["$score"],
                mod=rank_param("params", '{"weights": [1, 2]}'))
        c.check(f"{wide} rrf", name, two, RRFRanker(), limit=8, output_fields=["$score", "$id"])
        total += 3
    c.finish(total)


def test_hybrid_timezone_and_output_match_3x(pair):
    """输出的时区取 rank_params 的，没有则取第一个带它的路由；过滤用 `路由的 || rank_params 的`。"""
    c = HybridClients(pair.open())
    total = 0
    name = c.create([("ts", DataType.TIMESTAMPTZ, {}), ("cat", DataType.INT64, {})],
                    [{"id": i, "v": vector(i), "age": i % 17, "cat": i % 3,
                      "ts": f"2025-01-0{1 + i % 3}T0{i % 10}:00:00Z"} for i in range(30)],
                    dynamic=True, pk=DataType.INT64)
    two = [route(limit=6, expr="ts > ISO '2025-01-02T00:00:00'"), route([[1.0, 1.2]], 6)]
    cases = [
        ("none", None),
        ("rank tz", rank_param("timezone", '"Asia/Shanghai"')),
        ("rank tz + time_fields", chain(rank_param("timezone", '"Asia/Shanghai"'), rank_param("time_fields", '"year,hour"'))),
        ("route2 tz output", route_param(1, "timezone", '"America/New_York"')),
        ("route1 tz filter", route_param(0, "timezone", '"Asia/Tokyo"')),
        ("rank vs route", chain(rank_param("timezone", '"UTC"'), route_param(0, "timezone", '"Asia/Tokyo"'))),
        ("route time_fields", route_param(1, "time_fields", '"day hour"')),
        ("rank tz bad", rank_param("timezone", '"Nope/Zone"')),
        ("route tz bad no filter", route_param(1, "timezone", '"Nope/Zone"')),
        ("route tz bad filter", route_param(0, "timezone", '"Nope/Zone"')),
    ]
    for label, md in cases:
        c.check(f"tz {label}", name, two, RRFRanker(), limit=4, output_fields=["ts"], mod=md)
        total += 1
    for label, fields in (("star", ["*"]), ("none", None), ("meta", ["$meta"]), ("pk", ["id"]), ("vector", ["v", "cat"])):
        kwargs = {} if fields is None else {"output_fields": fields}
        c.check(f"output {label}", name, two, decay(), limit=3, **kwargs)
        c.check(f"output {label} group", name, two, RRFRanker(), limit=2, group_by_field="cat", group_size=1, **kwargs)
        total += 2
    c.finish(total)


def test_hybrid_non_finite_decay_factor_in_meta_match_3x(pair):
    """decay 因子是 NaN 时（字段值为 +inf、`scale=1e308` 使 `scale²` 溢出），动态 `$meta` 里的 `_decay_score`
    按 `json.dumps` 写成 `NaN`；同名动态键 `$score` 为 None 的行得分为 0，不触发 num_combine 的检查。"""
    c = HybridClients(pair.open())
    rows = []
    for i in range(8):
        row = {"id": i, "v": vector(i), "age": 0, "x": float("inf") if i % 2 else float(i)}
        if i % 2:
            row["$score"] = None
        if i == 3:
            row["_decay_score"] = 7
        rows.append(row)
    name = c.create([("x", DataType.DOUBLE, {})], rows, dynamic=True, pk=DataType.INT64)
    ranker = decay("x", origin=0, scale=1e308)
    total = 0
    for fields in (["*"], ["_decay_score"], ["$score", "_decay_score"], ["$score", "_decay_score", "x"], None):
        kwargs = {} if fields is None else {"output_fields": fields}
        c.check(f"output {fields}", name, [route(limit=8)], ranker, limit=8, **kwargs)
        c.check(f"output {fields} two routes", name, [route(limit=8), route([[1.0, 0.3]], 8)], ranker, limit=8, **kwargs)
        total += 2
    c.finish(total)

