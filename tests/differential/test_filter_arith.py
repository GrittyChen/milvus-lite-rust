"""0032 过滤算术的实际 RPC：Query / count / 分页 / iterator / Search / 稀疏检索 / HybridSearch / Delete，
覆盖 MemTable、flush 与重开，对照真实 3.x / 1320d26。

数据分两批：先写入并 flush 的 segment，再写入留在 MemTable 的行，所以同一个表达式在两个数据源上可能走
不同的路线（hybrid 的整源回退），稀疏检索在 MemTable 上逐行走 Python 路线（double），在 segment 上走
Arrow 路线（float32），这些都是 3.x 自身的行为。
"""

import pytest
from pymilvus import AnnSearchRequest, DataType, MilvusClient, RRFRanker

from compare import hits, run
from test_reopen import pair

BIG = 2**62
VECTORS = {i: [float(i), 0.0] for i in range(1, 7)}
SEGMENT = [
    {"id": 1, "i8": 127, "n": 5, "f": 1.1, "d": 0.0, "s": "ab", "sp": {1: 1.0},
     "j": {"a": 2, "s": "ab", "l": [1, "x"], "f": 1.5}, "x": BIG, "y": 2},
    {"id": 2, "i8": -128, "n": None, "f": None, "d": -0.0, "s": "c", "sp": {1: 2.0},
     "j": {"a": -3, "s": "", "l": [], "f": -0.0}, "x": 3, "y": 0},
    {"id": 3, "i8": 1, "n": 7, "f": 1.5, "d": 1.5, "s": "", "sp": {1: 3.0, 2: 1.0},
     "j": {"a": 0, "s": "x", "l": [0.5], "f": 0.0}, "x": -1, "y": 4},
]
MEMTABLE = [
    {"id": 4, "i8": 2, "n": BIG, "f": 2.5, "d": 1e308, "s": "abc", "sp": {1: 0.5},
     "j": {"a": BIG, "s": "é", "l": [1, [2]], "f": 1e308}, "x": BIG, "y": 2},
    {"id": 5, "i8": 0, "n": -1, "f": 0.1, "d": 2.0, "s": "a", "sp": {2: 1.5},
     "j": [1, 2], "x": "s", "y": 1},
    {"id": 6, "i8": -1, "n": None, "f": -2.5, "d": -1.0, "s": "zz", "sp": {1: 1.0, 2: 2.0},
     "j": {"a": None}, "y": 3},
]
ROWS = SEGMENT + MEMTABLE
SPARSE_QUERY = [{1: 1.0, 2: 1.0}]


def schema():
    value = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=True)
    value.add_field("id", DataType.INT64, is_primary=True)
    value.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    value.add_field("sp", DataType.SPARSE_FLOAT_VECTOR)
    value.add_field("i8", DataType.INT8)
    value.add_field("n", DataType.INT64, nullable=True)
    value.add_field("f", DataType.FLOAT, nullable=True)
    value.add_field("d", DataType.DOUBLE)
    value.add_field("s", DataType.VARCHAR, max_length=16)
    value.add_field("j", DataType.JSON)
    return value


def indexes():
    params = MilvusClient.prepare_index_params()
    params.add_index(field_name="vector", index_type="FLAT", metric_type="L2")
    params.add_index(field_name="sp", index_type="SPARSE_INVERTED_INDEX", metric_type="IP")
    return params


def rows(batch):
    return [{**row, "vector": VECTORS[row["id"]]} for row in batch]


def create(diff, name):
    diff.step("create", lambda c: c.create_collection(name, schema=schema(), index_params=indexes()))
    diff.step("segment", lambda c: c.insert(name, rows(SEGMENT)))
    diff.step("flush", lambda c: c.flush(name))
    diff.step("memtable", lambda c: c.insert(name, rows(MEMTABLE)))


def pages(name, expression):
    def collect(c):
        iterator = c.query_iterator(name, batch_size=2, filter=expression, output_fields=["id"])
        try:
            result = []
            for _ in range(10):
                page = list(iterator.next())
                result.append(page)
                if not page:
                    return result
            raise AssertionError("iterator did not end")
        finally:
            iterator.close()
    return collect


def hybrid(name, expression):
    requests = [
        AnnSearchRequest([[0.0, 0.0]], "vector", {"metric_type": "L2"}, 6, expr=expression),
        AnnSearchRequest(SPARSE_QUERY, "sp", {"metric_type": "IP"}, 6, expr=expression),
    ]
    return lambda c: c.hybrid_search(name, requests, RRFRanker(), limit=6, output_fields=["id"])


EXPRESSIONS = [
    # Arrow：回绕、带检查的转换、float32、除零与 -0.0
    "i8 + i8 == -2", "i8 * 2 < 0", "n * 2 < 0", "i8 / 2 >= 0.5", "-n < 0", "i8 - n < 0",
    "f * f == 1.21", "f * f > 1.2", "f * f > 0.0100000005", "f + d > 1", "d / 0 > 1", "d / 0 < 0",
    "-d == 0", "(i8 + 1) * 2 == 4", "d * 10 > 1e308",
    # hybrid：segment 上 x 全是整数（Arrow，int64 回绕），MemTable 上含字符串（整源回退 Python，精确整数）
    "x * x == 0", "x / y > 0", "x + 1 > 2", "x * 2 == x + x", "y * 1.5 > 2", "x * 1.0 > 0",
    # Python：精确整数、字符串与列表、除零、JSON 路径
    "j['a'] * 2 == 4", "j['a'] * j['a'] > 1000", "j['a'] / 3 == -1.0", "j['s'] * 2 == 'abab'",
    "j['s'] + j['s'] like 'ab%'", "j['l'] + j['l'] == j['l'] * 2", "j['l'] * 2 > j['l']",
    "j['f'] / 0 > 0", "$meta['x'] / 3 > 1", "$meta['x'] * $meta['y'] > 2", "-j['a'] < 0",
]


@pytest.mark.parametrize("expression", EXPRESSIONS)
def test_arithmetic_across_consumers_sources_and_reopen(pair, expression):
    name = "arith_rpc"
    diff = pair.open()
    create(diff, name)
    for stage in ("mixed", "committed", "reopened"):
        if stage == "committed":
            diff.step("flush_all", lambda c: c.flush(name))
        if stage == "reopened":
            pair.close()
            diff = pair.open()
            diff.step("load", lambda c: c.load_collection(name))
        expected = diff.step(stage + "_query", lambda c: c.query(name, filter=expression, output_fields=["id"]))[1]
        assert diff.step(stage + "_count", lambda c: c.query(name, filter=expression, output_fields=["count(*)"])) == ("ok", [{"count(*)": len(expected)}])
        diff.step(stage + "_page", lambda c: c.query(name, filter=expression, output_fields=["id"], limit=2, offset=1))
        diff.step(stage + "_iterator", pages(name, expression))
        diff.step(stage + "_search", lambda c: c.search(name, [[0.0, 0.0]], anns_field="vector", filter=expression, limit=6, search_params={"metric_type": "L2"}), hits("L2", [[0.0, 0.0]], VECTORS, k=len(expected)))
        diff.step(stage + "_sparse", lambda c: c.search(name, SPARSE_QUERY, anns_field="sp", filter=expression, limit=6, search_params={"metric_type": "IP"}, output_fields=["id"]))
        diff.step(stage + "_hybrid", hybrid(name, expression))
    diff.step("delete", lambda c: c.delete(name, filter=expression))
    diff.step("remaining", lambda c: c.query(name, filter="", output_fields=["id"]))


# 本实现的资源上限（0032 登记的差异）：3.x 分配失败报 MemoryError（code 1、空文案）。
RESOURCE = "filter value exceeds the 67108864-byte limit"


# 第二项是成功的步骤：稀疏检索在 MemTable 上逐行走 Python 路线（double，不做 Arrow 的转换检查），
# MemTable 中 n = 2^62 引起的 Arrow 转换失败在那里不出现，segment 上又没有越界值（3.x 自身的行为）。
ARROW_ONLY = {"sparse"}


@pytest.mark.parametrize("expression,succeeds,resource", [
    # Arrow：MemTable 中 n = 2^62 转 float32 / double 失败（segment 先求值且有命中）
    ("n + f > 0", ARROW_ONLY, False),
    ("n / 2 > 0", ARROW_ONLY, False),
    ("i8 + 9223372036854775808 > 0", set(), False),
    # Python：int 转 float 溢出、重复次数放不进 Py_ssize_t
    ("j['f'] + " + str(2**1024) + " > 0", set(), False),
    ("$meta['x'] * 1180591620717411303424 > 0", set(), False),
    ("id == 1 or j['s'] * 4611686018427387904 == ''", set(), True),
])
def test_errors_fail_every_consumer_and_never_publish_a_delete(pair, expression, succeeds, resource):
    name = "arith_error"
    diff = pair.open()
    create(diff, name)
    before = diff.step("before", lambda c: c.query(name, filter="", output_fields=["id"]))
    steps = {
        "query": lambda c: c.query(name, filter=expression, output_fields=["id"]),
        "count": lambda c: c.query(name, filter=expression, output_fields=["count(*)"]),
        "search": lambda c: c.search(name, [[0.0, 0.0]], anns_field="vector", filter=expression, limit=6),
        "sparse": lambda c: c.search(name, SPARSE_QUERY, anns_field="sp", filter=expression, limit=6),
        "hybrid": hybrid(name, expression),
        "delete": lambda c: c.delete(name, filter=expression),
    }
    for step, fn in steps.items():
        if resource:
            rust, ref = run(fn, diff.rust), run(fn, diff.ref)
            assert rust[0] == "err" and rust[1] == 1 and rust[3] == RESOURCE, (step, rust)
            assert ref[0] == "err" and ref[1] == 1 and ref[3] == "", (step, ref)
        else:
            diff.step(step, fn, error=step not in succeeds)
    assert diff.step("after", lambda c: c.query(name, filter="", output_fields=["id"])) == before
    diff.step("flush_all", lambda c: c.flush(name))
    assert diff.step("after_flush", lambda c: c.query(name, filter="", output_fields=["id"])) == before


def test_dead_rows_change_hybrid_inference_and_arithmetic_values(pair):
    # 0006 / 0014 登记的「只看活行」：3.x 的类型推断仍看到已删除的字符串行，整源回退 Python（精确整数，
    # 2^124 != 0）；本实现只看活行，x 全是整数，走 Arrow（int64 回绕为 0）。删除之前两边一致。
    name = "arith_dead"
    diff = pair.open()
    diff.step("create", lambda c: c.create_collection(name, schema=schema(), index_params=indexes()))
    batch = [{**SEGMENT[0], "id": 1, "x": "s"}, {**SEGMENT[0], "id": 2, "x": BIG}]
    diff.step("insert", lambda c: c.insert(name, rows(batch)))
    diff.step("flush", lambda c: c.flush(name))
    query = lambda c: c.query(name, filter="x * x == 0", output_fields=["id"])
    assert diff.step("live", query) == ("ok", [])
    diff.step("delete", lambda c: c.delete(name, ids=[1]))
    for stage in ("deleted", "reopened"):
        if stage == "reopened":
            diff.step("flush_delete", lambda c: c.flush(name))
            pair.close()
            diff = pair.open()
            diff.step("load", lambda c: c.load_collection(name))
        assert run(query, diff.rust) == ("ok", [{"id": 2}]), stage
        assert run(query, diff.ref) == ("ok", []), stage


def test_same_expression_takes_different_routes_per_source(pair):
    # 两边一致之外，固定 3.x 的两个现象确实出现：
    # - hybrid：segment 上 x 全是整数，走 Arrow，2^62 * 2^62 回绕为 0；MemTable 上 x 含字符串，整源回退
    #   Python，精确整数不为 0。
    # - 稀疏检索：MemTable 逐行走 Python（double），0.1f 的平方不大于阈值；flush 后走 Arrow（float32），
    #   平方舍入后大于阈值。普通 Query 在两处都走 Arrow。
    name = "arith_routes"
    diff = pair.open()
    create(diff, name)
    ids = lambda result: sorted(hit["id"] for hit in result[1][0])
    square = "x * x == 0"
    assert diff.step("hybrid_mixed", lambda c: c.query(name, filter=square, output_fields=["id"])) == ("ok", [{"id": 1}])
    narrow = "f * f > 0.0100000005"
    sparse = lambda c: c.search(name, SPARSE_QUERY, anns_field="sp", filter=narrow, limit=6, search_params={"metric_type": "IP"})
    assert diff.step("query_mixed", lambda c: c.query(name, filter=narrow, output_fields=["id"])) == ("ok", [{"id": 1}, {"id": 3}, {"id": 4}, {"id": 5}, {"id": 6}])
    assert ids(diff.step("sparse_mixed", sparse)) == [1, 3, 4, 6]
    diff.step("flush_all", lambda c: c.flush(name))
    assert ids(diff.step("sparse_committed", sparse)) == [1, 3, 4, 5, 6]
    assert diff.step("hybrid_committed", lambda c: c.query(name, filter=square, output_fields=["id"])) == ("ok", [{"id": 1}])
