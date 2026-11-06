"""0026 第 1 步：Search + boost 分别发给本实现与 3.x（43d1257），逐项比较原始响应。

比较原始的 ids、scores、topks、group_by_field_value 与 fields_data，不经 SDK 的 Hit（字段叫 `id` 时它的键被覆盖，
分数还被客户端取整）。分数容差取 0026 测试矩阵的 1e-5（相对与绝对，近零时比 0004 的 L2 / IP 下限 1e-6 宽）；其余逐项相等。
部分命中时排序比较的报错，操作数顺序取决于 Timsort 的比较序列，只比较错误码与固定的一段文案（0026「测试矩阵」）。
"""

import json
import math

from pymilvus import DataType, Function, FunctionScore, FunctionType, MilvusClient
from pymilvus.exceptions import MilvusException

from test_reopen import pair  # noqa: F401  （fixture）


def install_spy(client):
    connection = client._get_connection()
    raw = []
    insert = connection._stub.Insert
    meta_patch = {}

    def values(scalars):
        kind = scalars.WhichOneof("data")
        if kind is None or kind == "array_data":
            return kind
        return [v.decode() if isinstance(v, bytes) else v for v in getattr(scalars, kind).data]

    def spy(request, *args, _rpc, **kwargs):
        reply = _rpc(request, *args, **kwargs)
        data, group = reply.results, reply.results.group_by_field_value
        fields = []
        for f in data.fields_data:
            kind = f.WhichOneof("field")
            content = values(f.scalars) if kind == "scalars" else (f.vectors.dim, list(f.vectors.float_vector.data))
            fields.append([f.field_name, f.type, kind, content, list(f.valid_data)])
        raw.append({
            "status": [reply.status.code, reply.status.reason],
            "top_k": data.top_k,
            "topks": list(data.topks),
            "ids": list(data.ids.int_id.data) or list(data.ids.str_id.data),
            "scores": list(data.scores),
            "output_fields": list(data.output_fields),
            "fields": fields,
            "group": [group.field_name, group.type, values(group.scalars), list(group.valid_data)]
            if data.HasField("group_by_field_value") else None,
        })
        return reply

    # SDK 写不进超出 int64 的动态值，这里改写请求里的 `$meta` 文本。
    def insert_spy(*args, **kwargs):
        request = args[0] if args else kwargs["request"]
        if meta_patch:
            for f in request.fields_data:
                if f.field_name == "$meta":
                    patched = [bytes(v).replace(meta_patch["from"], meta_patch["to"])
                               for v in f.scalars.json_data.data]
                    del f.scalars.json_data.data[:]
                    f.scalars.json_data.data.extend(patched)
        return insert(*args, **kwargs)

    # Search 与 HybridSearch 的响应是同一种消息，记录方式相同。
    for name in ("Search", "HybridSearch"):
        rpc = getattr(connection._stub, name)
        setattr(connection._stub, name, lambda request, *a, _rpc=rpc, **k: spy(request, *a, _rpc=_rpc, **k))
    connection._stub.Insert = insert_spy
    return raw, meta_patch


def same(a, b):
    if isinstance(a, float) or isinstance(b, float):
        if isinstance(a, bool) or isinstance(b, bool) or not isinstance(a, (int, float)) or not isinstance(b, (int, float)):
            return False
        if math.isnan(a) or math.isnan(b):
            return math.isnan(a) and math.isnan(b)
        return math.isclose(a, b, rel_tol=1e-5, abs_tol=1e-5)
    if isinstance(a, (list, tuple)) and isinstance(b, (list, tuple)):
        return len(a) == len(b) and all(same(x, y) for x, y in zip(a, b))
    if isinstance(a, dict) and isinstance(b, dict):
        return a.keys() == b.keys() and all(same(a[k], b[k]) for k in a)
    return a == b


ORDER_ERROR = "not supported between instances of"


def normalized(outcome):
    if "error" in outcome and ORDER_ERROR in outcome["error"][1]:
        return {"error": [outcome["error"][0], ORDER_ERROR]}
    return outcome


class Clients:
    method = "search"

    def __init__(self, diff):
        self.clients = [diff.rust, diff.ref]
        self.spies = [install_spy(c) for c in self.clients]
        self.failures = []
        self.collections = 0
        self.count = 0

    def create(self, fields, rows, metric="COSINE", dynamic=False, pk=DataType.VARCHAR, vector="v", dim=2,
               meta_patch=None):
        self.collections += 1
        name = f"c{self.collections}"
        for client, (_, patch) in zip(self.clients, self.spies):
            schema = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=dynamic)
            schema.add_field("id", pk, is_primary=True, **({"max_length": 500} if pk == DataType.VARCHAR else {}))
            for field, data_type, kwargs in fields:
                schema.add_field(field, data_type, **kwargs)
            schema.add_field("age", DataType.INT32)
            schema.add_field(vector, DataType.FLOAT_VECTOR, dim=dim)
            index = client.prepare_index_params()
            index.add_index(vector, index_type="FLAT", metric_type=metric)
            client.create_collection(name, schema=schema, index_params=index)
            patch.clear()
            patch.update(meta_patch or {})
            client.insert(name, rows)
            patch.clear()
            client.load_collection(name)
        return name

    def check(self, label, *args, **kwargs):
        outcomes = []
        for client, (raw, _) in zip(self.clients, self.spies):
            raw.clear()
            try:
                getattr(client, self.method)(*args, **kwargs)
                outcomes.append(raw[-1])
            except MilvusException as e:
                outcomes.append({"error": [e.code, str(e.message)]})
        self.count += 1
        rust, ref = normalized(outcomes[0]), normalized(outcomes[1])
        if not same(rust, ref):
            self.failures.append(f"{label}\n rust {json.dumps(rust)[:600]}\n ref  {json.dumps(ref)[:600]}")

    def finish(self, expected):
        assert not self.failures, f"{len(self.failures)} / {self.count}:\n" + "\n".join(self.failures[:20])
        assert self.count == expected


def boost(weight=2.0, expr=None, **params):
    p = {"reranker": "boost", "weight": weight}
    if expr is not None:
        p["filter"] = expr
    return FunctionScore([Function("b", FunctionType.RERANK, [], params=p)], params=params or None)


QUERY = [[1.0, 0.0]]


def vector(i):
    return [1.0, round(0.05 * i, 3)]


def test_boost_ties_are_broken_by_id_like_3x(pair):
    """同分时按 `$id` 升序（VARCHAR 按码点），不是首次出现或引擎的并列顺序。严格断言 ids 的顺序。"""
    c = Clients(pair.open())
    names = ["k9", "k10", "k2", "Z", "a", "é", "中", "k1"]
    tie = c.create([("g", DataType.INT64, {})],
                   [{"id": n, "v": [1.0, 0.0], "g": i % 2, "age": 5} for i, n in enumerate(names)])
    c.check("all", tie, data=QUERY, limit=8, ranker=boost(), output_fields=["g"])
    c.check("some", tie, data=QUERY, limit=8, ranker=boost(2.0, "g == 1"), output_fields=["g"])
    c.check("group", tie, data=QUERY, limit=8, ranker=boost(), group_by_field="g", group_size=8, output_fields=["g"])
    c.check("plain", tie, data=QUERY, limit=8, output_fields=["g"])
    c.check("plain group", tie, data=QUERY, limit=8, group_by_field="g", group_size=8, output_fields=["g"])
    c.finish(5)


def test_boost_window_offset_group_and_scores_match_3x(pair):
    c = Clients(pair.open())
    total = 0
    # 查询让每行的距离都不同：并列跨过候选窗口边界时 3.x 由 argpartition 任取，这一点普通检索相同（0004「决策」）。
    for metric, query, queries in (("COSINE", QUERY, [[1.0, 0.0], [0.0, 1.0], [1.0, 1.0]]),
                                   ("IP", [[1.0, 1.0]], [[1.0, 1.0], [0.0, 1.0], [2.0, 1.0]]),
                                   ("L2", QUERY, [[1.0, 0.0], [0.0, 1.0], [1.0, 1.0]])):
        name = c.create([("cat", DataType.INT64, {})],
                        [{"id": i, "v": vector(i), "age": i, "cat": i % 3} for i in range(60)],
                        metric=metric, pk=DataType.INT64)
        for weight, expr in ((2.0, None), (0.5, "age > 3"), (0.0, "age == 19"), (0.0, "age == 20"), (2.0, "age > 1000")):
            c.check(f"{metric} {weight} {expr}", name, data=query, limit=2, ranker=boost(weight, expr), output_fields=["age"])
            c.check(f"{metric} {weight} {expr} offset", name, data=query, limit=2, offset=3, ranker=boost(weight, expr))
            c.check(f"{metric} {weight} {expr} group", name, data=query, limit=2, ranker=boost(weight, expr),
                    group_by_field="cat", group_size=3, output_fields=["cat"])
            c.check(f"{metric} {weight} {expr} group offset", name, data=query, limit=2, offset=1,
                    ranker=boost(weight, expr), group_by_field="cat", group_size=3, strict_group_size=True)
            c.check(f"{metric} {weight} {expr} round", name, data=query, limit=2, ranker=boost(weight, expr), round_decimal=2)
            total += 5
        for mode, function_mode in (("sum", "sum"), ("multiply", "sum"), ("sum", "multiply"), ("Multiply", "Multiply")):
            functions = [Function(n, FunctionType.RERANK, [], params={"reranker": "boost", "weight": w, "filter": f})
                         for n, w, f in (("a", 0.1, "age > 3"), ("b", 0.2, "age > 5"), ("c", 0.3, ""))]
            c.check(f"{metric} modes {mode} {function_mode}", name, data=query, limit=5,
                    ranker=FunctionScore(functions, params={"boost_mode": mode, "function_mode": function_mode}))
            total += 1
        c.check(f"{metric} nq", name, data=queries, limit=3, offset=1, ranker=boost(2.0, "age > 3"))
        c.check(f"{metric} range", name, data=query, limit=3, ranker=boost(2.0, "age > 3"), output_fields=["age"],
                search_params={"radius": 1e9 if metric == "L2" else -1e9})
        total += 2
    # float32 的求和顺序不同：两边的分数只在 0004 的误差内相等，包括舍入之后。
    d8 = [-1.5922501, 5.4084558, 2.1465912, 3.5537271, -6.5382862, -1.2961364, 7.8397546, 14.9343119]
    for metric in ("L2", "COSINE", "IP"):
        name = c.create([], [{"id": "k0", "age": 0, "v": d8}, {"id": "k1", "age": 1, "v": [x * 0.7 for x in d8]}], metric=metric, dim=8)
        q = [[0.0] * 8] if metric == "L2" else [[0.5, -1.25, 3.0, 0.1, 7.7, -2.2, 0.3, 1.9]]
        for rounding in (-1, 4, 6):
            c.check(f"d8 {metric} plain {rounding}", name, data=q, limit=2, round_decimal=rounding)
            c.check(f"d8 {metric} boost {rounding}", name, data=q, limit=2, ranker=boost(2.0), round_decimal=rounding)
            total += 2
    c.finish(total)


def test_boost_same_named_fields_match_3x(pair):
    """`$id` / `$distance` 同名字段按扁平行模型覆盖、再剥除；过滤里的主键读到覆盖后的原生值。"""
    c = Clients(pair.open())
    total = 0
    types = {
        "varchar_num": (DataType.VARCHAR, {"max_length": 16}, lambda i: str(i % 3)),
        "varchar": (DataType.VARCHAR, {"max_length": 16}, lambda i: f"s{i % 3}"),
        "int64": (DataType.INT64, {}, lambda i: 100 + i),
        "bool": (DataType.BOOL, {}, lambda i: bool(i % 2)),
        "double": (DataType.DOUBLE, {}, lambda i: i + 0.5),
        "nan": (DataType.DOUBLE, {}, lambda i: math.nan if i == 0 else float(i)),
        "inf": (DataType.DOUBLE, {}, lambda i: math.inf if i == 0 else float(i)),
        "json": (DataType.JSON, {}, lambda i: {"k": i}),
        "array": (DataType.ARRAY, {"element_type": DataType.INT64, "max_capacity": 2}, lambda i: [i]),
        "array_nan": (DataType.ARRAY, {"element_type": DataType.DOUBLE, "max_capacity": 2}, lambda i: [math.nan, i + 0.5]),
        "timestamptz": (DataType.TIMESTAMPTZ, {}, lambda i: f"2025-01-0{1 + i % 3}T00:00:00Z"),
    }
    filters = (None, "age > 100", "age > 2", "id is null", "id is not null", "id == 1", "id != 1", "id > '1'",
               "id == 's1'", "id == '2025-01-01 00:00:00+00:00'", "id > '2025-01-01 00:00:00+00:00'",
               "id like 's%'", "id in ['s1', '2025-01-01 00:00:00+00:00']", "not (id == 'a')")
    for type_name, (data_type, kwargs, value) in types.items():
        for key in ("$id", "$distance"):
            name = c.create([(key, data_type, kwargs)],
                            [{"id": f"k{i}", "v": vector(i), "age": i, key: value(i)} for i in range(6)])
            for expr in filters:
                c.check(f"{type_name} {key} {expr}", name, data=QUERY, limit=4, ranker=boost(2.0, expr), output_fields=["age", key])
                total += 1
            c.check(f"{type_name} {key} group", name, data=QUERY, limit=4, ranker=boost(), group_by_field=key, output_fields=["age"])
            total += 1
    # 动态键：只有在投影内才覆盖；其余动态字段（包括叫 pk 的键）在 boost 过滤里不可见。
    for pk, make_id in ((DataType.INT64, lambda i: i), (DataType.VARCHAR, lambda i: f"k{i}")):
        name = c.create([], [{"id": make_id(i), "v": vector(i), "age": i, "$id": 100 + i, "pk": 5} for i in range(4)], dynamic=True, pk=pk)
        for output in (None, ["*"], ["$id"], ["age"], ["pk"]):
            for expr in ("id > 10 and pk > 0", "pk > 0", "$meta['pk'] > 0", "$meta['$id'] > 0", "id > 10", "id == 'k1'"):
                c.check(f"pk key {pk} {output} {expr}", name, data=QUERY, limit=4, ranker=boost(2.0, expr), output_fields=output)
                total += 1
    for label, row_value in (("str", "x"), ("list", [1, "a"]), ("dict", {"a": 1}), ("bool", True), ("null", None), ("float", 1.5)):
        for key in ("$id", "$distance"):
            for metric in ("COSINE", "IP", "L2"):
                name = c.create([], [{"id": f"k{i}", "v": vector(i), "age": i, key: row_value} for i in range(4)], metric=metric, dynamic=True)
                for expr in (None, "age > 100", "id == 'x'", "id is null", "id is not null", "id == 1.5", "id like 'x%'"):
                    c.check(f"{label} {key} {metric} {expr}", name, data=QUERY, limit=4, ranker=boost(2.0, expr), output_fields=["*"])
                    total += 1
    # 超出 int64 的动态值只能经原始请求写入：VARCHAR 主键的 str() 精确，整数主键在 protobuf 处越界。
    placeholder = 987654321
    for text in (str(2**70), str(-2**70), "[%d, 1.5]" % 2**70, "1" + "0" * 400, "1" + "0" * 200):
        patch = {"from": str(placeholder).encode(), "to": text.encode()}
        for key in ("$id", "$distance"):
            for metric, pk, make_id in (("COSINE", DataType.VARCHAR, lambda i: f"k{i}"), ("L2", DataType.INT64, lambda i: i), ("IP", DataType.VARCHAR, lambda i: f"k{i}")):
                name = c.create([], [{"id": make_id(i), "v": vector(i), "age": i, key: placeholder} for i in range(3)],
                                metric=metric, dynamic=True, pk=pk, meta_patch=patch)
                for expr in (None, "age > 100"):
                    c.check(f"wide {text[:12]} {key} {metric} {expr}", name, data=QUERY, limit=3, ranker=boost(2.0, expr), output_fields=["*"])
                    total += 1
    # 向量字段本身叫 `$id` / `$distance`。
    for key in ("$id", "$distance"):
        for pk, make_id in ((DataType.VARCHAR, lambda i: f"k{i}"), (DataType.INT64, lambda i: i)):
            name = c.create([], [{"id": make_id(i), "age": i, key: vector(i)} for i in range(4)], pk=pk, vector=key)
            for output in (None, [], [key], ["*"], ["age"]):
                c.check(f"vector {key} {pk} {output}", name, data=QUERY, limit=3, ranker=boost(), output_fields=output)
                c.check(f"vector {key} {pk} {output} none", name, data=QUERY, limit=3, ranker=boost(2.0, "age > 100"), output_fields=output)
                total += 2
            c.check(f"vector {key} {pk} offset", name, data=QUERY, limit=2, offset=1, ranker=boost(), output_fields=[key])
            c.check(f"vector {key} {pk} round", name, data=QUERY, limit=2, ranker=boost(), output_fields=[key], round_decimal=2)
            c.check(f"vector {key} {pk} group", name, data=QUERY, limit=2, ranker=boost(), group_by_field="age", output_fields=[key])
            c.check(f"vector {key} {pk} plain", name, data=QUERY, limit=2, output_fields=[key])
            total += 4
    c.finish(total)
