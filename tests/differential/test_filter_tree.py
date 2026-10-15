"""切片 B 的实际 RPC、状态和失败删除，对照真实 3.x / 43d1257。"""

import json
import operator

import pytest
from pymilvus import DataType, MilvusClient

from compare import hits
from test_reopen import pair
from test_scenarios import index


def schema():
    value = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=True)
    value.add_field("id", DataType.INT64, is_primary=True)
    value.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    value.add_field("age", DataType.INT64)
    value.add_field("title", DataType.VARCHAR, max_length=128)
    value.add_field("j", DataType.JSON)
    return value


ROWS = [
    {"id": 1, "age": 1, "title": "a%b", "vector": [0.0, 0.0], "j": {"a": 1, "arr": [1, True, None]}, "x": 1},
    {"id": 2, "age": 2, "title": "a\\_b", "vector": [1.0, 0.0], "j": {"a": None, "arr": []}, "x": "a"},
    {"id": 3, "age": 3, "title": "a\n", "vector": [2.0, 0.0], "j": [1, True, None], "x": None},
    {"id": 4, "age": 4, "title": "中文😀", "vector": [3.0, 0.0], "j": {}},
]


@pytest.mark.parametrize("expression", [
    "age in [1, 3]", "age not in [1, 3]", "not (age in [1, 3])", "age == id",
    "age > 1 or id == 1", "true and age > 1", "title like 'a%'", "title like 'a\\\\%b'",
    "j['a'] == 1", "j['a'] is null", "exists j['a']", "not (j['a'] == 1)",
    "json_contains(j['arr'], 1)", "json_contains_all(j['arr'], [1, true])",
    "json_contains_any(j['arr'], [])", "not json_contains(j['missing'],1)",
    "array_length(j) == 3", "x not in [1]", "not(x like 'a%')",
    "json_contains(j['arr'], _seq)", "json_contains(j, ghost)", "json_contains(j, $meta['ghost'])",
])
def test_tree_forms_across_query_count_search_pagination_and_delete(diff, expression):
    name = "tree_rpc"
    diff.step("create", lambda c: c.create_collection(name, schema=schema(), index_params=index("L2")))
    diff.step("insert", lambda c: c.insert(name, ROWS))
    for stage in ("active", "committed"):
        if stage == "committed":
            diff.step("flush", lambda c: c.flush(name))
        result = diff.step(stage + "_query", lambda c: c.query(name, filter=expression, output_fields=["id", "title"]))
        expected = result[1]
        assert diff.step(stage + "_count", lambda c: c.query(name, filter=expression, output_fields=["count(*)"])) == ("ok", [{"count(*)": len(expected)}])
        diff.step(stage + "_page", lambda c: c.query(name, filter=expression, output_fields=["id"], limit=1, offset=1))
        diff.step(stage + "_search", lambda c: c.search(name, [[0.0, 0.0]], filter=expression, limit=4, search_params={"metric_type": "L2"}), hits("L2", [[0.0, 0.0]], {r["id"]: r["vector"] for r in ROWS}, k=len(expected)))
    diff.step("delete", lambda c: c.delete(name, filter=expression))
    diff.step("remaining", lambda c: c.query(name, output_fields=["*"]))


@pytest.mark.parametrize("expression,error", [
    ("true or json_contains(j['arr'],[1])", False),
    ("false and json_contains(j['arr'],[1])", False),
    ("json_contains(j['arr'],[1])", True),
    ("true or age == 9223372036854775808", True),
    ("true or j['a'] == 9223372036854775808", False),
    ("id == 1 or (j['a'] == 1 and x in [1])", True),
])
def test_execution_errors_and_short_circuit_do_not_publish_failed_deletes(diff, expression, error):
    name = "tree_failure"
    diff.step("create", lambda c: c.create_collection(name, schema=schema(), index_params=index("L2")))
    diff.step("committed", lambda c: c.insert(name, [ROWS[0]]))
    diff.step("flush", lambda c: c.flush(name))
    later = {**ROWS[1], "j": {"a": 1, "arr": [1]}, "x": [1]}
    diff.step("active", lambda c: c.insert(name, [later]))
    before = diff.step("before", lambda c: c.query(name, output_fields=["*"]))
    diff.step("query", lambda c: c.query(name, filter=expression, output_fields=["*"]), error=error)
    diff.step("count", lambda c: c.query(name, filter=expression, output_fields=["count(*)"]), error=error)
    diff.step("search", lambda c: c.search(name, [[0.0, 0.0]], filter=expression, limit=2), error=error)
    diff.step("delete", lambda c: c.delete(name, filter=expression), error=error)
    if error:
        assert diff.step("after", lambda c: c.query(name, output_fields=["*"])) == before


@pytest.mark.parametrize("raw", [
    '{"x":NaN}', '{"x":Infinity}', '{"x":1e999}', '{"x":"\\ud800"}',
    '{"x":"\\ud83d\\ude00"}', '{"x":1,"x":2}', '{"x":}', '{"x":[NaN]}',
])
def test_string_json_decoder_runs_through_real_sdk_and_reopen(pair, raw):
    diff = pair.open()
    name = "tree_decode"
    diff.step("create", lambda c: c.create_collection(name, schema=schema(), index_params=index("L2")))
    diff.step("insert", lambda c: c.insert(name, [{**ROWS[0], "blob": raw, "other": raw}]))
    for expression in ["blob['x'] is null", "blob['x'] == other['x']", "not (blob['x'] == 1)", "blob['x'] like '_'", "json_contains(blob['x'], other['x'])"]:
        diff.step(expression, lambda c: c.query(name, filter=expression, output_fields=["id"]))
    pair.close()
    diff = pair.open()
    diff.step("load", lambda c: c.load_collection(name))
    diff.step("reopened", lambda c: c.query(name, filter="blob['x'] == other['x']", output_fields=["id"]))


@pytest.mark.parametrize("op", ["<", "<=", ">", ">="])
@pytest.mark.parametrize("negate", [False, True])
@pytest.mark.parametrize("operands", ["containers", "elements", "dynamic"])
def test_nan_ordering_and_type_errors_across_all_consumers(pair, op, negate, operands):
    raw_values = [
        '{"x":[NaN],"y":[1]}', '{"x":[[NaN]],"y":[[1]]}',
        '{"x":[2],"y":[NaN]}', '{"x":[NaN],"y":[NaN]}',
        '{"x":[null],"y":[1]}', '{"x":["a"],"y":[1]}',
        '{"x":[1],"y":[2]}', '{"x":[1],"y":[1]}',
        '{"x":[],"y":[1]}', '{"x":[1e999],"y":[Infinity]}',
    ]
    rows = [
        {**ROWS[0], "id": i, "vector": [float(i), 0.0], "blob": raw, "y": [1]}
        for i, raw in enumerate(raw_values)
    ]
    left, right = {
        "containers": ("blob['x']", "blob['y']"),
        "elements": ("blob['x'][0]", "blob['y'][0]"),
        "dynamic": ("blob['x']", "y"),
    }[operands]
    expression = f"{left} {op} {right}"
    if negate:
        expression = f"not ({expression})"
    operation = {"<": operator.lt, "<=": operator.le, ">": operator.gt, ">=": operator.ge}[op]
    expected = []
    for i, raw in enumerate(raw_values):
        values = json.loads(raw)
        a, b = values["x"], [1] if operands == "dynamic" else values["y"]
        try:
            if operands == "elements":
                a, b = a[0], b[0]
            matched = None if a is None or b is None else operation(a, b)
        except (TypeError, IndexError):
            matched = None
        if matched is not None and (not matched if negate else matched):
            expected.append({"id": i})
    diff = pair.open()
    name = "tree_nan_order"
    diff.step("create", lambda c: c.create_collection(name, schema=schema(), index_params=index("L2")))
    diff.step("insert", lambda c: c.insert(name, rows))
    for stage in ("active", "committed", "reopened"):
        if stage == "committed":
            diff.step("flush", lambda c: c.flush(name))
        if stage == "reopened":
            pair.close()
            diff = pair.open()
            diff.step("load", lambda c: c.load_collection(name))
        assert diff.step(stage + "_query", lambda c: c.query(name, filter=expression, output_fields=["id"])) == ("ok", expected)
        assert diff.step(stage + "_count", lambda c: c.query(name, filter=expression, output_fields=["count(*)"])) == ("ok", [{"count(*)": len(expected)}])
        diff.step(stage + "_search", lambda c: c.search(name, [[0.0, 0.0]], filter=expression, limit=len(rows), search_params={"metric_type": "L2"}), hits("L2", [[0.0, 0.0]], {r["id"]: r["vector"] for r in rows}, k=len(expected)))
    diff.step("delete", lambda c: c.delete(name, filter=expression))
    remaining = [{"id": row["id"]} for row in rows if {"id": row["id"]} not in expected]
    assert diff.step("remaining", lambda c: c.query(name, output_fields=["id"])) == ("ok", remaining)


@pytest.mark.parametrize("expression", [
    "exists f", "exists d", "exists small", "exists flag", "exists title", "exists vector",
    "vector == 1", "not (vector == 1)", "vector['a'] is null", "vector in [1]",
    "true or vector == 1", "json_contains(vector, 1)",
])
def test_scalar_filter_type_diagnostics_and_failed_delete(diff, expression):
    value = schema()
    for name, dtype in [("f", DataType.FLOAT), ("d", DataType.DOUBLE), ("small", DataType.INT8), ("flag", DataType.BOOL)]:
        value.add_field(name, dtype)
    name = "tree_type_error"
    diff.step("create", lambda c: c.create_collection(name, schema=value, index_params=index("L2")))
    diff.step("insert", lambda c: c.insert(name, [{**ROWS[0], "f": 1.0, "d": 2.0, "small": 1, "flag": True}]))
    before = diff.step("before", lambda c: c.query(name, output_fields=["*"]))
    result = diff.step("query", lambda c: c.query(name, filter=expression, output_fields=["id"]), error=True)
    assert result[:3] == ("err", 6, 6)
    diff.step("count", lambda c: c.query(name, filter=expression, output_fields=["count(*)"]), error=True)
    diff.step("search", lambda c: c.search(name, [[0.0, 0.0]], filter=expression, limit=1), error=True)
    diff.step("delete", lambda c: c.delete(name, filter=expression), error=True)
    assert diff.step("after", lambda c: c.query(name, output_fields=["*"])) == before


def test_large_decoded_objects_keep_duplicate_and_equality_semantics(pair):
    entries = [f'"key{i}":{i}' for i in range(2000)]
    left = "{" + ",".join([*entries, '"key0":-1', '"\\ud800":1', '"\\ud800":2']) + "}"
    right = "{" + ",".join(['"\\ud800":2', *reversed(entries[1:]), '"key0":-1']) + "}"
    raw = '{"x":' + left + ',"y":' + right + '}'
    ordinary = {f"key{i}": i for i in range(2000)}
    ordinary["key0"] = -1
    diff = pair.open()
    name = "tree_object"
    diff.step("create", lambda c: c.create_collection(name, schema=schema(), index_params=index("L2")))
    rows = [
        {**ROWS[0], "blob": raw},
        {**ROWS[1], "blob": raw + " trailing"},
        {**ROWS[2], "blob": json.dumps({"x": ordinary, "y": ordinary}), "other": ordinary},
    ]
    diff.step("insert", lambda c: c.insert(name, rows))
    for expression, expected in [
        ("blob['x'] == blob['y']", [{"id": 1}, {"id": 3}]),
        ("blob['x'] != blob['y']", []),
        ("blob['x'] == other", [{"id": 3}]),
        ("blob['x']['key0'] == -1", [{"id": 1}, {"id": 3}]),
        ("blob['x']['key1999'] == 1999", [{"id": 1}, {"id": 3}]),
    ]:
        assert diff.step(expression, lambda c: c.query(name, filter=expression, output_fields=["id"])) == ("ok", expected)
    diff.step("flush", lambda c: c.flush(name))
    pair.close()
    diff = pair.open()
    diff.step("load", lambda c: c.load_collection(name))
    assert diff.step("reopened", lambda c: c.query(name, filter="blob['x'] == blob['y']", output_fields=["id"])) == ("ok", [{"id": 1}, {"id": 3}])
