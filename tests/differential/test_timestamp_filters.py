"""0025 第 2 步：TIMESTAMPTZ 的过滤（比较、`in`、时间戳 ± 区间、类型检查、三个求值后端的差异）对照 3.x（43d1257）。

表达式与结果类别见 timestamp_filter_cases.py（由真实 3.x 生成）。已登记的差异：不涉及时间值的算术仍是
UNIMPLEMENTED（0006 第 1 项：范围外构造先于其语义检查），3.x 对这些表达式给出结果或语义错误。
"""

from pymilvus import DataType, MilvusClient

from compare import run, same
from test_reopen import pair  # noqa: F401  （fixture）
from test_scenarios import index
from timestamp_filter_cases import CASES, HUGE

V = [0.0, 0.0]
ROWS = [
    {"id": 1, "vector": V, "ts": "2025-01-01T00:00:00Z", "ts2": "2025-01-02T00:00:00Z",
     "j": {"a": 1, "when": "2025-01-01T00:00:00Z"}, "n": 3, "dynw": "2025-01-01T00:00:00Z", "dynn": 7, "dynf": 1.5, "arr": [1, 2], "dmicro": 1735689600000000, "dn": None},
    {"id": 2, "vector": V, "ts": "0001-01-01T00:00:00Z", "ts2": None, "j": {"a": 2}, "n": None, "dynn": -5, "arr": [3], "dmicro": 7, "dn": None},
    {"id": 3, "vector": V, "ts": "9999-12-31T23:59:59.999999Z", "ts2": "2024-01-01T00:00:00Z", "j": None, "n": 1, "dynn": 100, "dn": None},
    {"id": 4, "vector": V, "ts": None, "ts2": None, "j": {"a": 1}, "n": 2, "dn": None},
]
# 含不涉及时间值的算术（`n + 1`、`-n`）且解析阶段没有错误：本实现 UNIMPLEMENTED，先于任何语义检查。
# 解析阶段的错误（无偏移 / 非法字面量）两边相同，不在此列。
GENERAL_ARITHMETIC = {
    "-n > 0", "n + 1 > 2", "n + 1 > 2 and ts > 5", "ts > 5 and n + 1 > 2", "ts + 1 > 2 and n + 1 > 2", "(n + 1) + ts > 5",
}


def create(client, name):
    schema = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=True)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    schema.add_field("ts", DataType.TIMESTAMPTZ, nullable=True)
    schema.add_field("ts2", DataType.TIMESTAMPTZ, nullable=True)
    schema.add_field("j", DataType.JSON, nullable=True)
    schema.add_field("n", DataType.INT64, nullable=True)
    schema.add_field("arr", DataType.ARRAY, element_type=DataType.INT64, max_capacity=4, nullable=True)
    client.create_collection(name, schema=schema, index_params=index("L2"))
    client.load_collection(name)


def ids(client, expr, name="t"):
    return sorted(row["id"] for row in client.query(name, filter=expr, output_fields=["id"]))


def test_filter_expressions_match_3x(pair):
    d = pair.open()
    d.step("create", lambda c: create(c, "t"))
    d.step("insert", lambda c: c.insert("t", ROWS))
    kinds = {"rows": 0, "empty": 0, "error": 0}
    unimplemented = set()
    for expr, kind in CASES:
        expr = expr.replace("<HUGE>", HUGE)
        a, b = run(lambda c: ids(c, expr), d.rust), run(lambda c: ids(c, expr), d.ref)
        shown = expr if len(expr) < 200 else expr[:80] + "…"
        got = "error" if b[0] != "ok" else ("rows" if b[1] else "empty")
        assert got == kind, f"{shown}: 3.x gave {b!r}, table says {kind}"
        if a[:2] == ("rpc", "UNIMPLEMENTED"):
            assert shown in GENERAL_ARITHMETIC, f"{shown}: {a!r}"
            unimplemented.add(shown)
            continue
        assert same(a, b), f"{shown}\n  rust: {a!r}\n  3.x:  {b!r}"
        kinds[kind] += 1
    # 三类结果都有足够多的样本，不会因为数据写错而两边一起报错或一起为空。
    assert unimplemented == GENERAL_ARITHMETIC
    assert kinds["rows"] > 50 and kinds["empty"] > 15 and kinds["error"] > 50, kinds


def test_filter_in_other_requests_and_empty_collections_match_3x(pair):
    d = pair.open()
    d.step("create", lambda c: create(c, "t"))
    d.step("create empty", lambda c: create(c, "e"))
    d.step("insert", lambda c: c.insert("t", ROWS))
    big = "ts + INTERVAL 'P10000000000D' > ISO '2025-01-01T00:00:00Z'"
    # 区间字面量只在真正对行求值时才检查（空集合上成功），count 与 search 同样会求值。
    d.step("empty big interval", lambda c: ids(c, big, "e"))
    d.step("empty big interval python", lambda c: ids(c, big + " and j['a'] > 0", "e"))
    d.step("count big interval", lambda c: c.query("t", filter=big, output_fields=["count(*)"]), error=True)
    d.step("search big interval", lambda c: c.search("t", data=[V], limit=5, filter=big), error=True)
    ok = "ts > ISO '2024-12-31T00:00:00Z'"
    d.step("count", lambda c: c.query("t", filter=ok, output_fields=["count(*)"]))
    d.step("search", lambda c: [[h["id"] for h in x] for x in c.search("t", data=[V], limit=5, filter=ok)])
    d.step("query output", lambda c: sorted((r["id"], r["ts"]) for r in c.query("t", filter=ok, output_fields=["id", "ts"])))
    # 动态整数列配 ISO 列表：Arrow 把 value_set 转成 int64 微秒比较，各类请求结果一致。这些请求放在删除之前，
    # 否则命中的行已被删掉，三边都是空结果。
    hit = "dmicro in [ISO '2025-01-01T00:00:00Z']"
    counted = d.step("dynamic count", lambda c: c.query("t", filter=hit, output_fields=["count(*)"]))
    assert counted == ("ok", [{"count(*)": 1}])
    found = d.step("dynamic search", lambda c: [[h["id"] for h in x] for x in c.search("t", data=[V], limit=5, filter=hit)])
    assert found == ("ok", [[1]])
    other = d.step("dynamic not in", lambda c: ids(c, "dmicro not in [ISO '2025-01-01T00:00:00Z']"))
    assert other == ("ok", [2, 3, 4])
    seven = "dmicro in [ISO '1970-01-01T00:00:00.000007Z']"
    left = d.step("dynamic delete", lambda c: (c.delete("t", filter=seven), ids(c, "id >= 0"))[1])
    assert left == ("ok", [1, 3, 4])
    left = d.step("delete", lambda c: (c.delete("t", filter="ts2 < ISO '2024-06-01T00:00:00Z'"), ids(c, "id >= 0"))[1])
    assert left == ("ok", [1, 4])
    left = d.step("delete by arithmetic", lambda c: (c.delete("t", filter="ts + INTERVAL 'P1D' == ISO '2025-01-02T00:00:00Z'"), ids(c, "id >= 0"))[1])
    assert left == ("ok", [4])
    d.step("delete naive", lambda c: c.delete("t", filter="ts < ISO '2025-01-01T00:00:00'"), error=True)
