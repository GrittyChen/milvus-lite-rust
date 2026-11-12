"""真实 3.x / 1320d26 的过滤算术组件真值（0032）；--verify 不改写语料。

与 generate_filter_tree_fixtures.py 结构相同，但使用独立的 schema（含可空列与 ARRAY），
不改动既有语料。参照的 tests/search/filter/test_e2e.py、test_semantic.py 与
tests/engine/test_collection_filter.py 直接导入 3.x 内部模块，只作真值来源：
其中的算术表达式在 SOURCE_EXPRESSIONS 中转成本语料的用例。
"""

import argparse
import itertools
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
OUTPUT = ROOT / "crates/core/tests/fixtures/filter_arith.json"
# (名称, 类型, 可空, ARRAY 元素类型)
FIELDS = [
    ("id", "INT64", False, None), ("i8", "INT8", True, None), ("i16", "INT16", False, None),
    ("i32", "INT32", True, None), ("n", "INT64", True, None), ("age", "INT64", False, None),
    ("f", "FLOAT", True, None), ("d", "DOUBLE", True, None), ("flag", "BOOL", False, None),
    ("s", "VARCHAR", False, None), ("a", "ARRAY", True, "INT64"), ("af", "ARRAY", False, "DOUBLE"),
    ("j", "JSON", True, None), ("vector", "FLOAT_VECTOR", False, None),
]
COLUMNS = {
    "id": [1, 2, 3, 4],
    "i8": [127, -128, None, 1],
    "i16": [32767, 2, 3, -32768],
    "i32": [2**24 + 1, None, 5, -(2**24) - 1],
    "n": [2**53 + 1, None, 5, None],
    "age": [0, 1, 2**62, -(2**63)],
    "f": [None, 1.0, 1.1, 1.5],
    "d": [0.0, -0.0, None, 1.5],
    "flag": [True, False, True, False],
    "s": ["a", "b", "c", "d"],
    "a": [[1], [2], None, []],
    "af": [[1.0], [], [2.5], []],
    "j": [json.dumps(x) for x in [{"k": 1}, {"k": 2.5}, {"k": "a"}, {"k": None}]],
    "vector": [[1.0, 0.0]] * 4,
}
# 动态字段的几组取值：hybrid 的推断、回退与 Python 路线（0032 第 2 步启用）。
META = [
    [{"x": 1, "y": 2}, {"x": 2, "y": 0}, {"x": None}, {}],
    [{"x": 1.5, "y": 0.0}, {"x": -0.0, "y": 2}, {"x": 2**62}, {"x": 3}],
    [{"x": True, "y": 1}, {"x": "a", "y": 2}, {"x": [1], "y": 2}, {"x": None}],
    [{}, {}, {}, {}],
]
NUMERIC = ["i8", "i16", "i32", "n", "age", "f", "d"]
LITERALS = ["1", "1.5", "-1", "16777217", "9007199254740993", "9223372036854775807"]
SOURCE_EXPRESSIONS = [
    # test_e2e.py / test_semantic.py / test_collection_filter.py 中的算术形状。
    "age + 1 > 2", "age * 2 == 4", "age - 1 >= 0", "d / 2 > 0.4", "age / 2 == 0.5",
    "(age + 1) * 2 > 3", "-age < 0", "age * 1.5 > 1", "f * 2 > d",
]


def expressions():
    result = []
    operands = NUMERIC + ["1", "1.5"]
    for left, right in itertools.product(operands, repeat=2):
        if left in ("1", "1.5") and right in ("1", "1.5"):
            continue
        for op in "+-*/":
            e = f"{left} {op} {right}"
            result += [f"{e} > 0", f"{e} < 0", f"{e} == {e}"]
    for column in NUMERIC:
        for literal in LITERALS:
            for op in "+-*/":
                result += [f"{column} {op} {literal} > 0", f"{literal} {op} {column} > 0"]
        result += [f"-{column} > 0", f"-{column} == 0", f"- -{column} > 0", f"0 - {column} == -{column}"]
    result += [
        # 回绕与结果宽度
        "i8 + i8 == -2", "i8 * i8 == 1", "i8 + 1 > 127", "i8 - 1 < -128", "i16 + i16 == -2",
        "i16 * i8 == -127", "(i8 + i8) + age == -2", "(i8 + i8) == -2", "(i8 * i8) / 2 > 0",
        "age * age == 0", "age + age < 0", "age * 3 == 0", "-age == age", "age + 9223372036854775807 > 0",
        "9223372036854775807 + 1 < 0", "1 + 1 == 2", "(1 + 2) * 3 == 9", "1 / 0 > 0", "0 / 0 == 0 / 0",
        "-(-9223372036854775808) > 0", "-(9223372036854775807) - 1 < 0", "2 * 3 > id",
        # 转换检查与 null 错位
        "n == f", "f == n", "n < f", "n + 1 == f", "f == n + 1", "n + f > 0", "f + n > 0",
        "n / n > 0", "n / f > 0", "f / n > 0", "i32 + f > 0", "i32 * 1.0 > 0", "i32 / 2 > 0",
        "f + 16777216 > 0", "f + 16777217 > 0", "d + 9007199254740992 > 0", "d + 9007199254740993 > 0",
        "9007199254740993 + 1.0 > 0", "9007199254740993 / 1 > 0", "n < 5 and n + f > 0",
        "n + f > 0 and n < 0", "n + f > 0 or true", "true or n + f > 0",
        "age + 9223372036854775808 > 0", "age > 0 or age + 9223372036854775808 > 0",
        "n + f > 0 and age + 9223372036854775808 > 0", "age + 9223372036854775808 > 0 and n + f > 0",
        # 浮点精度与除零
        "f * f == 1.21", "f * f > 1.2099", "f + 0.1 == 1.2", "d + 0.1 == 1.1", "f / f == 1", "f / 0 > 1",
        "d / 0 > 1", "d / 0 < 0", "d / 0 == d / 0", "id / 0 > 1", "i8 / 0 < 0", "-d == 0", "1 / -d < 0",
        "1 / (0 - d) > 0", "f * 1e38 * 10 > 0", "id / 2 == 0.5", "id / 3 * 3 == id",
        # ARRAY 的内核缺失（类型取转换之后的类型）
        "a + 1 > 0", "1 + a > 0", "a - 1 > 0", "a * 1.5 > 0", "a / 1 > 0", "f / a > 0", "i8 / a > 0",
        "a + a > 0", "af + 1 > 0", "-a > 0", "-af > 0", "a + af > 0", "i8 + a > 0", "n / a > 0",
        "a + 1 > 0 and n + f > 0", "n + f > 0 and a + 1 > 0",
        # 类型检查
        "flag + 1 > 0", "s + 1 > 0", "j + 1 > 0", "-flag > 0", "-s > 0", "-vector > 0", "vector + 1 > 0",
        "id + 'a' > 0", "id % 2 == 0", "id ** 2 == 4", "id + 1 in [2]", "(id + 1) is null", "id + 1",
        # 动态字段与路径（第 2 步才放开；第 1 步 Rust 一侧应为 UNIMPLEMENTED）
        "x + 1 > 1", "x * 2 == 2", "x / y > 0", "-x < 0", "x + age > 0", "x * x > 0",
        "x + 1 == 'a' and age == 9223372036854775808", "x == 'a' and age == 9223372036854775808",
        "x + 1 == 2 and age == 9223372036854775808", "j['k'] + 1 > 1", "j['k'] * 2 == 'aa'",
        "text_match(s, 'a') and id + 1 > 0",
    ]
    result += SOURCE_EXPRESSIONS
    return list(dict.fromkeys(result))


def cases():
    # 动态键只在引用 x / y 的表达式中出现；其余表达式与 $meta 无关，只取一组。
    for expression in expressions():
        dynamic = re.search(r"\b[xy]\b", expression) is not None
        for group in range(len(META)) if dynamic else [0]:
            yield {"expression": expression, "meta": group}


def schema():
    fields = []
    for name, dtype, nullable, element in FIELDS:
        kwargs = {"nullable": nullable}
        if name == "id":
            kwargs = {"is_primary": True}
        if dtype == "FLOAT_VECTOR":
            kwargs["dim"] = 2
        if element:
            kwargs["element_type"] = getattr(DataType, element)
        if name == "s":
            kwargs["enable_analyzer"] = True
            kwargs["enable_match"] = True
        fields.append(FieldSchema(name=name, dtype=getattr(DataType, dtype), **kwargs))
    return CollectionSchema(fields=fields, enable_dynamic_field=True)


def table(group):
    types = {
        "INT64": pa.int64(), "INT8": pa.int8(), "INT16": pa.int16(), "INT32": pa.int32(),
        "FLOAT": pa.float32(), "DOUBLE": pa.float64(), "BOOL": pa.bool_(), "VARCHAR": pa.string(),
        "JSON": pa.string(), "FLOAT_VECTOR": pa.list_(pa.float32(), 2),
    }
    columns = {}
    for name, dtype, _, element in FIELDS:
        arrow = pa.list_(types[element]) if dtype == "ARRAY" else types[dtype]
        columns[name] = pa.array(COLUMNS[name], type=arrow)
    columns["_seq"] = pa.array([1, 2, 3, 4], type=pa.int64())
    columns["$meta"] = pa.array([json.dumps(row) for row in META[group]])
    return pa.table(columns)


def run_case(case, compiled_schema):
    phase = "parse"
    try:
        ast = parse_expr(case["expression"])
        phase = "compile"
        compiled = compile_expr(ast, compiled_schema, case["expression"])
        case["backend"] = compiled.backend
        phase = "evaluate"
        case["mask"] = evaluate(compiled, table(case["meta"])).to_pylist()
    except Exception as error:
        case.update(phase=phase, error_type=type(error).__name__, error=str(error))
    return case


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--verify", action="store_true")
    parser.add_argument("--arrow15-python")
    parser.add_argument("--dump-results", action="store_true")
    args = parser.parse_args()
    assert milvus_lite.__version__ == "3.2.2"
    assert Path(milvus_lite.__file__).resolve().is_relative_to(ROOT / "references/milvus-lite")
    assert subprocess.check_output(["git", "-C", str(ROOT / "references/milvus-lite"), "rev-parse", "--short=7", "HEAD"], text=True).strip() == "1320d26"
    compiled_schema = schema()
    payload = {
        "reference": "3.2.2+3 / 1320d26",
        "fields": [list(field) for field in FIELDS],
        "columns": COLUMNS,
        "meta": META,
        "cases": [run_case(case, compiled_schema) for case in cases()],
    }
    if args.dump_results:
        print(json.dumps(payload, ensure_ascii=True, separators=(",", ":")))
        return
    if args.verify:
        expected = json.loads(OUTPUT.read_text())
        assert pa.__version__ in ("15.0.0", "25.0.1"), "unverified Arrow version"
        assert payload == expected, "filter arithmetic truth differs; --verify does not overwrite"
    else:
        assert pa.__version__ == "25.0.1", "Rust error text follows the fixed Arrow 25.0.1 reference"
        assert args.arrow15_python, "regeneration must verify both reference Arrow versions"
        raw = subprocess.check_output([args.arrow15_python, str(Path(__file__).resolve()), "--dump-results"], text=True)
        # 两个 Arrow 版本的算术结果与文案实测完全相同；出现差异即停下，不登记例外。
        assert json.loads(raw) == payload, "Arrow 15 and 25 disagree on filter arithmetic"
        header = json.dumps({key: payload[key] for key in ("reference", "fields", "columns", "meta")}, ensure_ascii=True, separators=(",", ":"))[:-1]
        content = header + ',"cases":[\n' + ",\n".join(json.dumps(case, ensure_ascii=True, separators=(",", ":")) for case in payload["cases"]) + "\n]}\n"
        OUTPUT.write_text(content)
    print(f"{len(payload['cases'])} filter arithmetic cases verified with Arrow {pa.__version__}")


if __name__ == "__main__":
    main()
