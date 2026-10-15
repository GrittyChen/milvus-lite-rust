"""真实 3.x / 43d1257 的切片 B 组件真值；--verify 不改写语料。"""

import argparse
import json
import re
from pathlib import Path
import subprocess

import milvus_lite
import pyarrow as pa
from milvus_lite.schema.types import CollectionSchema, DataType, FieldSchema
from milvus_lite.search.filter.eval import evaluate
from milvus_lite.search.filter.parser import parse_expr
from milvus_lite.search.filter.semantic import compile_expr

ROOT = Path(__file__).resolve().parents[2]
OUTPUT = ROOT / "crates/core/tests/fixtures/filter_tree.json"
FIELDS = [
    ("id", "INT64"), ("age", "INT64"), ("small", "INT8"), ("short", "INT16"), ("mid", "INT32"),
    ("f", "FLOAT"), ("d", "DOUBLE"), ("flag", "BOOL"), ("s", "VARCHAR"),
    ("j", "JSON"), ("vector", "FLOAT_VECTOR"),
]
BASE = {
    "id": [1, 2, 3, 4], "age": [0, 1, 2**53 + 1, -2**53 - 1],
    "small": [0, 1, 127, -128], "short": [0, 1, 32767, -32768], "mid": [0, 1, 2**24 + 1, -2**24 - 1],
    "f": [0.0, 1.0, 1.1, 1.5], "d": [-0.0, 1.0, 1.1, 1.5],
    "flag": [True, False, True, False], "s": ["a%b", "a\\_b", "a\n", "中文😀"],
    "j": [json.dumps(x, ensure_ascii=False) for x in [
        {"a": 1, "arr": [1, True, None], "s": "a%b"},
        {"a": None, "arr": [], "s": 7}, [1, True, None], None,
    ]],
    "vector": [[1.0, 0.0]] * 4,
}


def expressions():
    result = [
        "flag", "true", "false", "not flag", "id > 1 or flag", "flag and id > 1",
        "id == age", "f == d", "small < mid", "mid == f", "age < d",
        "id > 1 == flag", "(id > 1) == (age > 1)", "true == false",
        "id in [1, 3]", "id not in [1, 3]", "not (id in [1, 3])", "id in []",
        "small in [-0.0]", "small in [-0.0, 129.0]", "mid in [-0.0, 129.0]",
        "short in [-0.0]", "short in [-0.0, 32769.0]", "short == mid",
        "flag in [true]", "flag not in []", "s in ['a%b', '中文😀']", "s not in ['a%b']",
        "f in [1.1]", "d in [1.1]", "f in [16777217]", "d in [9007199254740993]",
        "f in [1, 1.1]", "d not in [-0.0]", "f == 1e999", "id != 1e999",
        "flag or age > 0.0", "true or age == 9223372036854775808",
        "false and age == 9223372036854775808", "not (age > 0.0)",
        "s is null", "s is not null", "j is null", "j == 'null'", "j in ['null']",
        "x in [1]", "x not in [1]", "x not in []", "not (x in [1])", "x == y",
        "x < y", "x is null", "exists x", "not (x == 1)",
        "x == 1 or age > 0.0", "x in [1] or age == 9223372036854775808",
        "x == y or age == 9223372036854775808", "x in [true]", "x in ['1']",
        "x in [9007199254740993, 1.5]", "x in [9223372036854775808]",
        "x like 'a%'", "not (x like 'a%')", "x like 'a%' or age == 9223372036854775808",
        "true or x like 'a%'", "x is null or age == 9223372036854775808",
        "__meta__x == 'sentinel' or x == 1", "x == 9223372036854775808 or __meta__x == 'sentinel'",
        "j['a'] == 1", "j['a'] is null", "not (j['a'] == 1)", "exists j['a']",
        "j['arr'][0] == true", "j['arr'][9999999999999999999999] is null",
        "j['arr'] == x", "j['arr'] < x", "s['a'] is null", "id['a'] is null",
        "x['a'] is null", "x['a'] == 1", "x['a']['b'] == 1",
        "not (j['s'] like 'a%')", "not json_contains(j['missing'], 1)",
        "json_contains(j['arr'], 1)", "json_contains(j['arr'], true)",
        "json_contains_all(j['arr'], [])", "json_contains_any(j['arr'], [])",
        "json_contains_all(j['arr'], [1, true])", "array_contains(j['arr'], x)",
        "json_contains(j['arr'], [1])", "true or json_contains(j['arr'], [1])",
        "false and json_contains(j['arr'], [1])", "array_contains_all(j['arr'], x)",
        "array_length(j) >= 0", "array_length(x) == 0", "json_contains(j, ghost)",
        "json_contains(j, _seq)", "array_contains_all(j['arr'], vector)",
        "exists j", "exists id", "exists f", "exists d", "exists small", "exists short",
        "exists mid", "exists flag", "exists s", "exists vector",
        "vector == 1", "not (vector == 1)", "vector['a'] is null", "vector in [1]",
        "true or vector == 1", "json_contains(vector, 1)",
        "id and ghost > 1", "id or ghost > 1",
        "not id", "id like 'a%'", "id in ['a']", "id in [1,'a']",
        "array_length(id) > 0", "json_contains(id,1)", "j['a'] in [1]",
        "array_length(j['arr']) > 0", "_seq > 0", "$meta['x'] == 1",
    ]
    patterns = ["", "%", "_", "a\\", "a\\%b", "a\\_b", "%_", "中文_", "a%_b"]
    for pattern in patterns:
        literal = json.dumps(pattern, ensure_ascii=False)
        result += [f"s like {literal}", f"x like {literal}", f"j['s'] like {literal}"]
    return result


def cases():
    groups = [
        [1, 2, None, 0], [1.0, -0.0, None, 1.5], [True, False, None, True],
        ["a%b", "1", None, "a\n"], [1, "1", None, 0], [None] * 4,
        [[1], [], None, [True]], [{"a": 1}, {}, None, {"a": "1"}],
        [2**63, 1, None, 2], [2**53 + 1, 1.5, None, 2],
        [[], 1, None, {}], [{"a": 1}, {"a": "1"}, None, {}],
    ]
    for group in groups:
        for collision in (False, True):
            for expression in expressions():
                yield {
                    "expression": expression, "columns": BASE, "dynamic": True,
                    "meta": [{"x": x, "y": group[-i - 1]} for i, x in enumerate(group)],
                    "collision": collision,
                }
    for expression in expressions():
        yield {"expression": expression, "columns": BASE, "dynamic": False, "meta": [{}] * 4, "collision": False}
    strings = [
        '{"x":NaN}', '{"x":Infinity}', '{"x":-Infinity}', '{"x":1e999}',
        '{"x":"\\ud800"}', '{"x":"\\ud83d\\ude00"}', '{"x":1,"x":2}',
        '{"x":"{\\"a\\":1}"}', '[NaN]', '{"x":[NaN]}', '{"x":}',
        '{"x":01}', '{"x":"\\q"}', '{"x":' + "1" * 4301 + '}',
        '{"x":[NaN],"y":[1]}', '{"x":[[NaN]],"y":[[1]]}',
        '{"x":[2],"y":[NaN]}', '{"x":[NaN],"y":[NaN]}',
        '{"x":[1e999],"y":[Infinity]}', '{"x":[1,"a"],"y":[1,2]}',
        '{"x":[null],"y":[1]}', '{"x":[1],"y":[2]}',
        '{"x":{"a":1,"b":2,"a":3},"y":{"b":2,"a":3}}',
        '{"x":{"\\ud800":1,"\\ud800":2},"y":{"\\ud800":2}}',
    ]
    for raw in strings:
        columns = {**BASE, "s": [raw] * 4}
        for expression in [
            "s['x'] is null", "s['x'] == x['x']", "not (s['x'] == 1)",
            "s['x']['a'] is null", "s['x'] like '_'", "s['x'] == s['x']",
            "json_contains(s, x)", "json_contains_all(s, x)", "array_length(s) == 1",
            "json_contains(s['x'], x)", "true or s['x'] is null",
            "s['x'] == s['y']", "s['x'] != s['y']",
            *[
                expression
                for left, right in [("s['x']", "s['y']"), ("s['x']", "x['y']"), ("s['x'][0]", "s['y'][0]")]
                for operator in ("<", "<=", ">", ">=")
                for expression in (f"{left} {operator} {right}", f"not ({left} {operator} {right})")
            ],
        ]:
            yield {"expression": expression, "columns": columns, "dynamic": True, "meta": [{"x": raw}] * 4, "collision": False}
    # 深层原文存储为普通字符串；输入本身的解码与 Rust 析构均不得递归爆栈。
    for depth in (128, 129, 9997, 9998):
        for opening, closing in [("[", "]"), ('{"a":', "}")]:
            raw = opening * depth + "0" + closing * depth
            for expression in ("s['x'] is null", "array_length(s) == 1", "true or s['x'] is null"):
                yield {"expression": expression, "columns": {**BASE, "s": [raw] * 4}, "dynamic": True, "meta": [{}] * 4, "collision": False}


def run_case(case):
    fields = [FieldSchema(name=name, dtype=getattr(DataType, dtype), **({"is_primary": True} if name == "id" else {"dim": 2} if dtype == "FLOAT_VECTOR" else {})) for name, dtype in FIELDS]
    if case["collision"]:
        fields.append(FieldSchema(name="__meta__x", dtype=DataType.VARCHAR))
    schema = CollectionSchema(fields=fields, enable_dynamic_field=case["dynamic"])
    types = {"INT64": pa.int64(), "INT8": pa.int8(), "INT16": pa.int16(), "INT32": pa.int32(), "FLOAT": pa.float32(), "DOUBLE": pa.float64(), "BOOL": pa.bool_(), "VARCHAR": pa.string(), "JSON": pa.string(), "FLOAT_VECTOR": pa.list_(pa.float32(), 2)}
    columns = {name: pa.array(case["columns"][name], type=types[dtype]) for name, dtype in FIELDS}
    columns["_seq"] = pa.array([1, 2, 3, 4], type=pa.int64())
    if case["dynamic"]:
        columns["$meta"] = pa.array([json.dumps(row, ensure_ascii=True) for row in case["meta"]])
    if case["collision"]:
        columns["__meta__x"] = pa.array(["sentinel"] * 4)
    phase = "parse"
    try:
        ast = parse_expr(case["expression"])
        phase = "compile"
        compiled = compile_expr(ast, schema, case["expression"])
        case["backend"] = compiled.backend
        phase = "evaluate"
        case["mask"] = evaluate(compiled, pa.table(columns)).to_pylist()
    except Exception as error:
        case.update(phase=phase, error_type=type(error).__name__, error=str(error))
    return case


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--verify", action="store_true")
    parser.add_argument("--arrow15-python")
    parser.add_argument("--dump-results", action="store_true")
    args = parser.parse_args()
    assert milvus_lite.__version__ == "3.2.1"
    assert Path(milvus_lite.__file__).resolve().is_relative_to(ROOT / "references/milvus-lite")
    assert subprocess.check_output(["git", "-C", str(ROOT / "references/milvus-lite"), "rev-parse", "--short=7", "HEAD"], text=True).strip() == "43d1257"
    payload = {"reference": "3.2.1 / 43d1257", "fields": [list(field) for field in FIELDS], "cases": [run_case(case) for case in cases()]}
    if args.dump_results:
        print(json.dumps(payload, ensure_ascii=True, allow_nan=False, separators=(",", ":")))
        return
    if args.verify:
        expected = json.loads(OUTPUT.read_text())
        assert pa.__version__ in ("15.0.0", "25.0.1"), "unverified Arrow version"
        for case in expected["cases"]:
            alternate = case.pop("arrow15_error", None)
            if pa.__version__ == "15.0.0" and alternate is not None:
                case["error"] = alternate
        assert payload == expected, "filter tree truth differs; --verify does not overwrite"
    else:
        assert pa.__version__ == "25.0.1", "Rust error text follows the fixed Arrow 25.0.1 reference"
        assert args.arrow15_python, "regeneration must verify both reference Arrow versions"
        raw = subprocess.check_output([args.arrow15_python, str(Path(__file__).resolve()), "--dump-results"], text=True)
        alternate = json.loads(raw)
        assert alternate["reference"] == payload["reference"] and alternate["fields"] == payload["fields"]
        assert len(alternate["cases"]) == len(payload["cases"])
        for case, old in zip(payload["cases"], alternate["cases"]):
            if old != case:
                error = old["error"]
                match = re.fullmatch(r"Float value (-?[0-9.]+) was truncated converting to int64", error)
                assert match and old["error_type"] == "ArrowInvalid", "unregistered Arrow version difference"
                upgraded = f"Float value {float(match[1]):.6f} was truncated converting to int64"
                assert upgraded == case["error"], "unregistered Arrow diagnostic difference"
                restored = {**old, "error": upgraded}
                assert restored == case, "Arrow result, backend, phase or input differs"
                case["arrow15_error"] = error
        header = json.dumps({"reference": payload["reference"], "fields": payload["fields"]}, ensure_ascii=True, separators=(",", ":"))[:-1]
        content = header + ',"cases":[\n' + ",\n".join(json.dumps(case, ensure_ascii=True, allow_nan=False, separators=(",", ":")) for case in payload["cases"]) + "\n]}\n"
        OUTPUT.write_text(content)
    print(f"{len(payload['cases'])} filter tree cases verified with Arrow {pa.__version__}")


if __name__ == "__main__":
    main()
