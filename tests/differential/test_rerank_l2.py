"""0026 第 2 步：Search + L2（rrf / weighted / decay）分别发给本实现与 3.x（43d1257），逐项比较原始响应。

比较方式与 test_rerank_boost.py 相同（原始 ids、scores、topks、group_by_field_value 与 fields_data，分数容差 1e-5）。
SDK 写不出的参数类型（JSON 布尔、数字字符串、超出 SDK 校验的 round_decimal）经 `mod` 直接改写请求。
model 通过全部校验后本实现返回 UNIMPLEMENTED，是登记的差异（0026 第 10 节），这里不比较。
"""

from pymilvus import DataType, Function, FunctionScore, FunctionType, MilvusClient
from pymilvus.grpc_gen import common_pb2

from test_rerank_boost import QUERY, Clients, boost, vector
from test_reopen import pair  # noqa: F401  （fixture）


class L2Clients(Clients):
    """在 test_rerank_boost 的比较之外，允许改写发出的 SearchRequest。"""

    def __init__(self, diff):
        super().__init__(diff)
        self.mod = None
        for client in self.clients:
            stub = client._get_connection()._stub
            inner = stub.Search

            def search(request, *args, _inner=inner, **kwargs):
                if self.mod is not None:
                    self.mod(request)
                return _inner(request, *args, **kwargs)

            stub.Search = search

    def check(self, label, *args, mod=None, **kwargs):
        self.mod = mod
        try:
            super().check(label, *args, **kwargs)
        finally:
            self.mod = None


def fn(reranker, inputs=(), **params):
    return Function("r", FunctionType.RERANK, list(inputs), params={"reranker": reranker, **params})


def decay(field="age", function="gauss", origin=0, scale=8, **params):
    return fn("decay", [field], function=function, origin=origin, scale=scale, **params)


def setp(key, value, index=0):
    """把第 index 个函数的参数 key 改成原始文本 value（没有就追加）。"""
    def mod(request):
        params = request.function_score.functions[index].params
        for p in params:
            if p.key == key:
                p.value = value
                return
        params.append(common_pb2.KeyValuePair(key=key, value=value))
    return mod


def search_param(key, value):
    def mod(request):
        for p in request.search_params:
            if p.key == key:
                p.value = value
                return
        request.search_params.append(common_pb2.KeyValuePair(key=key, value=value))
    return mod


def test_l2_ties_match_3x(pair):
    """无分组时同分按 `$id` 升序（VARCHAR 按码点）；GroupByOp 没有 `$id` 平局规则，按首次出现。严格断言顺序。"""
    c = L2Clients(pair.open())
    names = ["k9", "k10", "k2", "Z", "a", "é", "中", "k1"]
    tie = c.create([("g", DataType.INT64, {})],
                   [{"id": n, "v": [1.0, 0.0], "g": i % 2, "age": 5} for i, n in enumerate(names)])
    c.check("decay", tie, data=QUERY, limit=8, ranker=decay(), output_fields=["g"])
    c.check("decay group", tie, data=QUERY, limit=8, ranker=decay(), group_by_field="g", group_size=8, output_fields=["g"])
    c.check("weighted", tie, data=QUERY, limit=8, ranker=fn("weighted", weights=[0.5]), output_fields=["g"])
    c.check("weighted group", tie, data=QUERY, limit=8, ranker=fn("weighted", weights=[0.5]), group_by_field="g",
            group_size=8, output_fields=["g"])
    c.finish(4)


def test_l2_functions_windows_and_errors_match_3x(pair):
    c = L2Clients(pair.open())
    total = 0
    # 每行的距离都不同，避免并列跨过候选窗口边界（0004「决策」）。
    # nq 的每个查询同样让距离各不相同（L2 的 [1, 3.1] 在全部行之外）。
    for metric, query, queries in (("COSINE", QUERY, [[1.0, 0.0], [0.0, 1.0]]), ("IP", [[1.0, 1.0]], [[1.0, 1.0], [2.0, 1.0]]),
                                   ("L2", QUERY, [[1.0, 0.0], [1.0, 3.1]])):
        name = c.create([("cat", DataType.INT64, {}), ("price", DataType.DOUBLE, {})],
                        [{"id": i, "v": vector(i), "age": i % 17, "cat": i % 3, "price": (i % 11) * 1.5} for i in range(60)],
                        metric=metric, pk=DataType.INT64)
        rankers = {
            "rrf": fn("rrf"),
            "rrf k=1": fn("rrf", k=1),
            "weighted": fn("weighted", weights=[0.7]),
            "weighted norm": fn("weighted", weights=[0.7], norm_score=True),
            "gauss": decay(),
            "exp sum": decay("price", "exp", 3, 2, offset=1, decay=0.3, score_mode="sum"),
            "linear avg": decay("cat", "linear", 1, 4, decay=0.2, score_mode="avg"),
        }
        for label, ranker in rankers.items():
            c.check(f"{metric} {label}", name, data=query, limit=5, ranker=ranker, output_fields=["age"])
            c.check(f"{metric} {label} offset", name, data=query, limit=3, offset=2, ranker=ranker)
            c.check(f"{metric} {label} group", name, data=query, limit=2, ranker=ranker, group_by_field="cat",
                    group_size=2, strict_group_size=True, output_fields=["cat"])
            c.check(f"{metric} {label} nq", name, data=queries, limit=3, ranker=ranker)
            c.check(f"{metric} {label} round", name, data=query, limit=3, ranker=ranker, round_decimal=3)
            c.check(f"{metric} {label} boost", name, data=query, limit=4,
                    ranker=FunctionScore([boost(3.0, "cat == 1").functions[0], ranker]), output_fields=["cat"])
            c.check(f"{metric} {label} empty", name, data=query, limit=4, ranker=ranker, filter="age < 0",
                    group_by_field="cat")
            total += 7
        # 归一化开关：decay 的 `"False"` 字符串为真，JSON false 为假；weighted 的字符串按大小写不敏感的布尔解析。
        for raw in ("false", "0", '"False"', "true"):
            c.check(f"{metric} decay norm {raw}", name, data=query, limit=4, ranker=decay(), mod=setp("norm_score", raw))
            total += 1
        for raw in ("false", '"TRUE"'):
            c.check(f"{metric} weighted norm {raw}", name, data=query, limit=4, ranker=fn("weighted", weights=[0.6]),
                    group_by_field="cat", mod=setp("norm_score", raw))
            total += 1
    name = c.create([("cat", DataType.INT64, {})],
                    [{"id": i, "v": vector(i), "age": i % 17, "cat": i % 3} for i in range(60)], pk=DataType.INT64)
    # limit / offset / group_size 的 Python 切片（绕过 SDK 的客户端校验）。
    for limit, offset in ((0, 5), (-1, 5), (-3, 5), (0, 0), (3, -1), (3, 1000)):
        def mk(request, limit=limit, offset=offset):
            search_param("topk", str(limit))(request)
            search_param("offset", str(offset))(request)
        c.check(f"limit {limit} offset {offset}", name, data=QUERY, ranker=fn("rrf"), mod=mk)
        c.check(f"group limit {limit} offset {offset}", name, data=QUERY, ranker=decay(), group_by_field="cat", mod=mk)
        total += 2
    for size in ("-1", "0", "1000"):
        c.check(f"group_size {size}", name, data=QUERY, limit=2, ranker=fn("rrf"), group_by_field="cat",
                mod=search_param("group_size", size))
        total += 1
    for rounding in ("7", "10", "-2", "0"):
        c.check(f"round {rounding}", name, data=QUERY, limit=3, ranker=decay(), mod=search_param("round_decimal", rounding))
        total += 1
    # 参数的原始 JSON 类型与校验先后。
    raw_cases = [
        ("rrf", fn("rrf", k=60), [("k", raw) for raw in ("true", "false", '" 5 "', "16384", "1e400", "null", '"abc"')]),
        ("weighted", fn("weighted", weights=[0.7]),
         [("weights", raw) for raw in ('"[0.7]"', '["0.5"]', "[true]", "[]", "5", '["x"]', "[1.5]", "[0.5, 0.5]")]
         + [("norm_score", raw) for raw in ('"x"', "1", "null")]),
        ("decay", decay(), [(key, raw) for key, values in (
            ("origin", ('"5"', "true", '"x"', "null")), ("scale", ("true", '"5"', "0", "1e400")),
            ("offset", ("true", "-1", '"5"')), ("decay", ("true", '"0.5"', "1", "0.999")),
            ("function", ('"GAUSS"', '"exp"', "5", "[1]")), ("score_mode", ('"MAX"', '"min"', "5"))) for raw in values]),
    ]
    for label, ranker, edits in raw_cases:
        for key, raw in edits:
            c.check(f"raw {label} {key}={raw}", name, data=QUERY, limit=3, ranker=ranker, mod=setp(key, raw))
            total += 1
    c.check("score_mode MAX no hits", name, data=QUERY, limit=3, ranker=decay(), filter="age < 0",
            mod=setp("score_mode", '"MAX"'))
    c.check("decay non numeric", name, data=QUERY, limit=3, ranker=decay("v"))
    c.check("decay unknown field", name, data=QUERY, limit=3, ranker=decay("nosuch"))
    c.check("decay two inputs", name, data=QUERY, limit=3, ranker=fn("decay", ["age", "cat"], function="gauss", origin=0, scale=1))
    c.check("two L2", name, data=QUERY, limit=3, ranker=FunctionScore([fn("rrf"), decay()]))
    c.check("unknown reranker", name, data=QUERY, limit=3, ranker=fn("nope"))
    c.check("model bad field", name, data=QUERY, limit=3, ranker=fn("model", ["age"], provider="cohere", queries=["q"]))
    c.check("model unknown provider", name, data=QUERY, limit=3, ranker=fn("model", ["id"], provider="nosuch", queries=["q"]))
    c.check("k bad + bad anns", name, data=QUERY, limit=3, ranker=fn("rrf", k=0), anns_field="nosuch")
    c.check("k abc + bad anns", name, data=QUERY, limit=3, ranker=fn("rrf", k="abc"), anns_field="nosuch")
    c.check("k bad + bad filter", name, data=QUERY, limit=3, ranker=fn("rrf", k=0), filter="cat ==")
    c.check("k bad + boost filter bad no hits", name, data=QUERY, limit=3, filter="age < 0",
            ranker=FunctionScore([boost(2.0, "cat ==").functions[0], fn("rrf", k=0)]))
    c.check("k bad no hits", name, data=QUERY, limit=3, filter="age < 0", ranker=fn("rrf", k=0))
    c.check("group vector + k bad", name, data=QUERY, limit=3, ranker=fn("rrf", k=0), group_by_field="v")
    c.check("group vector", name, data=QUERY, limit=3, ranker=fn("rrf"), group_by_field="v")
    total += 15
    # 候选窗口：id 即距离名次，标记行 age=0 / tag=1。Search + L2 的窗口是 max(limit + offset, limit·10)，
    # 有分组再并上 limit·group_size·3；有 boost 时引擎再乘 10。
    marks = {10, 49, 50, 51, 79, 80, 81, 120, 150, 151, 199, 200, 201, 299, 300}
    win = c.create([("tag", DataType.INT64, {}), ("grp", DataType.VARCHAR, {"max_length": 8})],
                   [{"id": i, "v": [round(i * 0.01, 4), 0.0], "age": 0 if i in marks else 1000, "tag": int(i in marks),
                     "grp": "g%d" % (i // 40)} for i in range(400)], metric="L2", pk=DataType.INT64)
    q0 = [[0.0, 0.0]]
    for limit in (1, 5, 10):
        for offset in (0, 3, 30):
            c.check(f"win decay {limit} {offset}", win, data=q0, limit=limit, offset=offset, ranker=decay(scale=5))
            c.check(f"win boost+decay {limit} {offset}", win, data=q0, limit=limit, offset=offset,
                    ranker=FunctionScore([boost(0.0001, "tag == 1").functions[0], decay(scale=5)]))
            total += 2
    for limit, size in ((2, 1), (5, 2), (1, 20)):
        c.check(f"win rrf group {limit} {size}", win, data=q0, limit=limit, ranker=fn("rrf"), group_by_field="grp",
                group_size=size)
        c.check(f"win weighted group {limit} {size}", win, data=q0, limit=limit, ranker=fn("weighted", weights=[1.0]),
                group_by_field="grp", group_size=size)
        total += 2
    c.finish(total)


def test_l2_same_named_fields_match_3x(pair):
    """链式候选行的扁平模型：`$id` / `$score` 先写，entity 再覆盖；`_decay_score`、`$group_score` 由链写入；
    `$id`、`$score`、`$group_score` 输出时剥掉（同名字段是空值占位）。"""
    c = L2Clients(pair.open())
    total = 0
    rankers = {"decay": decay(scale=100), "rrf": fn("rrf"), "weighted": fn("weighted", weights=[1.0])}
    for key in ("$id", "$score", "_decay_score", "$group_score", "$distance"):
        for type_name, data_type, kwargs, value in (("int64", DataType.INT64, {}, lambda i: 100 + i % 3),
                                                    ("double", DataType.DOUBLE, {}, lambda i: float(i % 3)),
                                                    ("varchar", DataType.VARCHAR, {"max_length": 8}, lambda i: f"s{i % 3}")):
            name = c.create([(key, data_type, kwargs)],
                            [{"id": f"k{i}", "v": vector(i), "age": i, key: value(i)} for i in range(10)])
            for label, ranker in rankers.items():
                for output in (None, ["age"], [key]):
                    c.check(f"{key} {type_name} {label} {output}", name, data=QUERY, limit=5, ranker=ranker, output_fields=output)
                    total += 1
                c.check(f"{key} {type_name} {label} group", name, data=QUERY, limit=10, ranker=ranker,
                        group_by_field=key, group_size=2, output_fields=["age"])
                total += 1
            c.check(f"{key} {type_name} boost+decay", name, data=QUERY, limit=5, output_fields=[key],
                    ranker=FunctionScore([boost(2.0, "age > 1").functions[0], decay(scale=100)]))
            total += 1
    # 动态键：在投影内才覆盖；decay 的因子按投影写进 `$meta`，新 key 排在最后。
    rows = [{"id": i, "v": vector(i), "age": i, "dyn": i % 3, "$id": 1000 + i % 3, "$score": float(i), "_decay_score": 7}
            for i in range(10)]
    name = c.create([], rows, dynamic=True, pk=DataType.INT64)
    plain = c.create([], [{"id": i, "v": vector(i), "age": i, "dyn": i % 3} for i in range(10)], dynamic=True, pk=DataType.INT64)
    for label, ranker in rankers.items():
        for output in (None, [], ["*"], ["$id"], ["$score"], ["_decay_score"], ["dyn"], ["id"]):
            c.check(f"dyn {label} {output}", name, data=QUERY, limit=5, ranker=ranker, output_fields=output)
            c.check(f"plain dyn {label} {output}", plain, data=QUERY, limit=5, ranker=ranker, output_fields=output)
            total += 2
        for field in ("dyn", "$id", "$score", "_decay_score", "id", "nosuch"):
            c.check(f"dyn {label} group {field}", name, data=QUERY, limit=5, ranker=ranker, group_by_field=field,
                    group_size=2, output_fields=["dyn"])
            total += 1
    # 主键叫虚拟列名：主键不在 entity 里，组键与组值按设计第 7 节的主键表。
    for key in ("$score", "_decay_score", "$group_score"):
        for client in c.clients:
            schema = MilvusClient.create_schema(auto_id=False)
            schema.add_field(key, DataType.VARCHAR, is_primary=True, max_length=16)
            schema.add_field("age", DataType.INT32)
            schema.add_field("v", DataType.FLOAT_VECTOR, dim=2)
            index = client.prepare_index_params()
            index.add_index("v", index_type="FLAT", metric_type="COSINE")
            client.create_collection(f"pk_{key}", schema=schema, index_params=index)
            client.insert(f"pk_{key}", [{key: f"k{i}", "v": vector(i), "age": i} for i in range(6)])
            client.load_collection(f"pk_{key}")
        for label, ranker in rankers.items():
            c.check(f"pk {key} {label} group", f"pk_{key}", data=QUERY, limit=4, ranker=ranker, group_by_field=key,
                    group_size=1, output_fields=["age"])
            c.check(f"pk {key} {label}", f"pk_{key}", data=QUERY, limit=4, ranker=ranker, output_fields=[key, "age"])
            total += 2
    c.finish(total)


def test_l2_computed_values_match_3x(pair):
    """链算出的值（组得分、decay 因子）写进同名字段时按 3.x 的 `_coerce_for` / `json.dumps` 输出：
    INT8 / INT16 不收窄，JSON 的非有限浮点是 NaN / Infinity；宽整数的 `$score` 是 Python 整数。"""
    c = L2Clients(pair.open())
    total = 0
    # 分数各不相同（IP 160·(160 + 0.01·i)），组得分远超 INT8 / INT16 的范围。
    for data_type, scale in ((DataType.INT8, 160.0), (DataType.INT16, 200.0)):
        for key in ("$group_score", "_decay_score"):
            name = c.create([(key, data_type, {})],
                            [{"id": i, "v": [scale + 0.01 * i, 0.0], "age": i, key: i % 3} for i in range(10)],
                            metric="IP", pk=DataType.INT64)
            query = [[scale, 0.0]]
            for label, ranker in (("weighted", fn("weighted", weights=[1.0])), ("rrf", fn("rrf")),
                                  ("decay", decay(scale=100))):
                c.check(f"{data_type} {key} {label} group", name, data=query, limit=10, ranker=ranker,
                        group_by_field=key, group_size=5, output_fields=[key])
                c.check(f"{data_type} {key} {label}", name, data=query, limit=5, ranker=ranker, output_fields=[key])
                total += 2
    # JSON 同名字段接收 NaN / ±inf 的组得分：boost 让全部命中的分数相同，组内与组间都没有 NaN 排序问题。
    name = c.create([("$group_score", DataType.JSON, {})],
                    [{"id": i, "v": vector(i), "age": i, "$group_score": {"k": i % 2}} for i in range(6)],
                    metric="IP", pk=DataType.INT64)
    for weight in ("nan", "inf", "-inf", 2.0):
        ranker = FunctionScore([boost(weight).functions[0], fn("weighted", weights=[1.0])])
        c.check(f"json group {weight}", name, data=[[1.0, 1.0]], limit=4, ranker=ranker,
                group_by_field="$group_score", group_size=3, output_fields=["$group_score"])
        total += 1
    # 宽整数与超出 float64 的 `$score`（只能经原始请求写入 `$meta`）。
    placeholder = 987654321
    for text in (str(2**70), str(-2**70), "1" + "0" * 400, "1e400"):
        patch = {"from": str(placeholder).encode(), "to": text.encode()}
        name = c.create([], [{"id": i, "v": vector(i), "age": i, "$score": placeholder} for i in range(4)],
                        dynamic=True, pk=DataType.INT64, meta_patch=patch)
        rankers = {
            "decay": decay(scale=100),
            "decay sum": decay(scale=100, score_mode="sum"),
            "decay avg": decay(scale=100, score_mode="avg"),
            "weighted": fn("weighted", weights=[0.5]),
            "weighted norm": fn("weighted", weights=[0.5], norm_score=True),
            "rrf": fn("rrf"),
        }
        for label, ranker in rankers.items():
            c.check(f"wide {text[:10]} {label}", name, data=QUERY, limit=3, ranker=ranker, output_fields=["$score"])
            total += 1
        c.check(f"wide {text[:10]} decay norm", name, data=QUERY, limit=3, ranker=decay(scale=100),
                output_fields=["$score"], mod=setp("norm_score", "true"))
        total += 1
    c.finish(total)


def test_l2_non_finite_decay_factor_in_meta_match_3x(pair):
    """Search + decay：NaN 因子写进动态 `$meta` 的 `_decay_score` 时是 `NaN`（`json.dumps`），不是丢掉这个 key。"""
    c = L2Clients(pair.open())
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
        c.check(f"output {fields}", name, data=QUERY, limit=8, ranker=ranker, **kwargs)
        c.check(f"output {fields} group", name, data=QUERY, limit=8, ranker=ranker, group_by_field="age", group_size=8,
                **kwargs)
        total += 2
    c.finish(total)

