"""0015 标量空值、默认值与三路线：真实参照 / 43d1257。"""

import copy
import json
from pathlib import Path

import pytest
from pymilvus import DataType, MilvusClient

from compare import hits
from test_reopen import pair
from test_scenarios import index


def schema():
    value = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=True)
    value.add_field("id", DataType.INT64, is_primary=True)
    value.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    for name, dtype in [("x", DataType.INT64), ("y", DataType.DOUBLE), ("b", DataType.BOOL),
                        ("s", DataType.VARCHAR), ("j", DataType.JSON)]:
        value.add_field(name, dtype, nullable=True, **({"max_length": 32} if dtype == DataType.VARCHAR else {}))
    value.add_field("zero", DataType.INT64, default_value=0)
    value.add_field("flag", DataType.BOOL, default_value=False)
    value.add_field("title", DataType.VARCHAR, max_length=8, default_value="")
    return value


ROWS = [
    {"id": 1, "vector": [1.0, 0.0], "x": None, "y": None, "b": None, "s": None, "j": None, "d": 1},
    {"id": 2, "vector": [2.0, 0.0], "x": 7, "y": 1.5, "b": True, "s": "abc", "j": {"k": 1, "arr": [1]}, "d": None},
    {"id": 3, "vector": [3.0, 0.0], "x": 0, "y": 0.0, "b": False, "s": "", "j": {}, "d": {}},
    {"id": 4, "vector": [4.0, 0.0], "d": {"k": 1}},
]


@pytest.mark.parametrize("indexed", [False, True])
@pytest.mark.parametrize("expression", [
    "x is null", "x is not null", "not (x == 7)", "x in [7]", "x not in [7]", "x not in []",
    "x == zero", "b is null or s like 'a%'", "j is null", "j['k'] is null",
    "x == 7 or j['k'] == 1", "x is null or d == 1", "not json_contains(j['arr'],1)",
])
def test_null_routes_all_consumers_and_reopen(pair, indexed, expression):
    diff = pair.open()
    diff.step("create", lambda c: c.create_collection("c", schema=schema(), index_params=index("L2")))
    if indexed:
        def scalar_index(c):
            params = c.prepare_index_params()
            params.add_index("x", index_type="INVERTED")
            c.create_index("c", params)
        diff.step("scalar_index", scalar_index)
    diff.step("insert", lambda c: c.insert("c", copy.deepcopy(ROWS)))
    for stage in ("active", "flushed", "reopened"):
        if stage == "flushed":
            diff.step("flush", lambda c: c.flush("c"))
        elif stage == "reopened":
            pair.close()
            diff = pair.open()
            diff.step("load", lambda c: c.load_collection("c"))
        result = diff.step(stage + "_query", lambda c: c.query("c", filter=expression, output_fields=["*"]))
        count = len(result[1])
        assert diff.step(stage + "_count", lambda c: c.query("c", filter=expression, output_fields=["count(*)"])) == ("ok", [{"count(*)": count}])
        diff.step(stage + "_page", lambda c: c.query("c", filter=expression, output_fields=["x", "j"], limit=1, offset=1))
        diff.step(stage + "_search", lambda c: c.search("c", [[0.0, 0.0], [0.0, 0.0]], filter=expression, limit=4, output_fields=["x", "j"], search_params={"metric_type": "L2"}),
                  hits("L2", [[0.0, 0.0], [0.0, 0.0]], {r["id"]: r["vector"] for r in ROWS}, k=count))
    diff.step("delete", lambda c: c.delete("c", filter=expression))
    diff.step("remaining", lambda c: c.query("c", output_fields=["*"]))


def test_failed_nullable_cast_delete_does_not_change_wal_rows_or_sequence(pair):
    diff = pair.open()
    diff.step("create", lambda c: c.create_collection("c", schema=schema(), index_params=index("L2")))
    diff.step("initial", lambda c: c.insert("c", copy.deepcopy(ROWS[:2])))
    diff.step("flush", lambda c: c.flush("c"))
    wide = {**ROWS[2], "x": 9007199254740993}
    diff.step("wide", lambda c: c.insert("c", copy.deepcopy([wide])))
    before = diff.step("before", lambda c: c.query("c", output_fields=["*"]))
    root = Path(pair.rust_path)
    files = lambda: {str(p.relative_to(root)): p.read_bytes() for p in root.rglob("*") if p.is_file()}
    previous = files()
    expr = "x == 1.0"
    for name, operation in [
        ("query", lambda c: c.query("c", filter=expr, output_fields=["*"])),
        ("count", lambda c: c.query("c", filter=expr, output_fields=["count(*)"])),
        ("search", lambda c: c.search("c", [[0.0, 0.0]], filter=expr, limit=3)),
        ("delete", lambda c: c.delete("c", filter=expr)),
    ]:
        diff.step(name, operation, error=True)
    assert files() == previous
    assert diff.step("after", lambda c: c.query("c", output_fields=["*"])) == before
    diff.step("next", lambda c: c.insert("c", copy.deepcopy([ROWS[3]])))
    diff.step("flush_after", lambda c: c.flush("c"))
    for path in (pair.rust_path, pair.ref_path):
        assert json.loads((Path(path) / "collections/c/manifest.json").read_text())["current_seq"] == 4
