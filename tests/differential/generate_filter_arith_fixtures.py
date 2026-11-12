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
    ("j", "JSON", True, None), ("vector", "FLOAT_VECTOR", False, None), ("p", "JSON", True, None),
]
# Python 路线（经 JSON 路径）的取值：大整数、真除法的被除数 q 与除数 r、bool、字符串、列表与 JSON NaN。
# h 只在第 0 行，专供会报错的表达式使用。
P_ROWS = [
    {"i": 3, "b": 2**70, "f": 1.5, "s": "ab", "l": [1, "x"], "t": True, "z": 0, "e": "", "el": [],
     "q": 2**70 + 1, "r": 3, "u": "\u00e9", "h": 10**400},
    {"i": -2, "b": -(2**70), "f": -0.0, "s": "", "l": [], "t": False, "z": 0.0, "e": "", "el": [],
     "q": -(2**60 + 1), "r": 7, "u": "x"},
    {"i": 0, "b": 2**64, "f": float("nan"), "s": "x", "l": [float("nan"), [1]], "t": True, "z": 0,
     "e": "", "el": [], "q": 1, "r": 2**1075, "u": "y"},
    None,
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
    "p": [None if row is None else json.dumps(row) for row in P_ROWS],
}
# 动态字段的几组取值：hybrid 的推断、回退与 Python 路线。
META = [
    [{"x": 1, "y": 2}, {"x": 2, "y": 0}, {"x": None}, {}],
    [{"x": 1.5, "y": 0.0}, {"x": -0.0, "y": 2}, {"x": 2**62}, {"x": 3}],
    [{"x": True, "y": 1}, {"x": "a", "y": 2}, {"x": [1], "y": 2}, {"x": None}],
    [{}, {}, {}, {}],
    [{"x": 2**64, "y": 3}, {"x": 1.5, "y": 2}, {"x": None, "y": 0}, {"x": -(2**70), "y": 4}],
    [{"x": 2**64, "y": 3}, {"x": 1, "y": 2}, {}, {"x": -3, "y": 4}],
    [{"x": "ab", "y": True}, {"x": [1, "a"], "y": 2}, {"x": "", "y": 0}, {"x": [], "y": 3}],
    # 以下四组是登记的 hybrid 推断差异（0032「与 3.x 的差异」）：pyarrow 按元素顺序推断与报错，
    # 本实现对数字与其他类型的混排一律整源回退 Python 路线。
    [{"x": 2**64, "y": 3}, {"x": "s", "y": 2}, {"x": [1, 2], "y": 0}, {"x": 1.5, "y": 4}],
    [{"x": "s", "y": 3}, {"x": 2**64, "y": 2}, {}, {"x": 1, "y": 4}],
    [{"x": 1.5, "y": 1}, {"x": 0.1, "y": 2}, {"x": False, "y": 3}, {"x": 0.1, "y": 4}],
    [{"x": 0.0, "y": 1}, {"x": 2, "y": 2}, {"x": 1.5, "y": 3}, {"x": True, "y": 4}],
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
        # 动态字段与路径
        "x + 1 > 1", "x * 2 == 2", "x / y > 0", "-x < 0", "x + age > 0", "x * x > 0",
        "x + 1 == 'a' and age == 9223372036854775808", "x == 'a' and age == 9223372036854775808",
        "x + 1 == 2 and age == 9223372036854775808", "j['k'] + 1 > 1", "j['k'] * 2 == 'aa'",
        "text_match(s, 'a') and id + 1 > 0",
    ]
    result += SOURCE_EXPRESSIONS
    result += python_expressions()
    return list(dict.fromkeys(result))


def python_expressions():
    """Python 路线（JSON 路径、array_length）与 hybrid 回退中的 Python 值规则。"""
    big = 9223372036854775808
    result = [
        # 精确大整数与 bool
        "p['i'] + p['i'] == 6", "p['i'] - p['b'] < 0", "p['i'] * p['b'] > 0", "p['b'] * p['b'] > p['b']",
        f"p['b'] + 1 == {2**70 + 1}", f"p['b'] * p['b'] == {2**140}", f"p['b'] - 1 < {2**70}",
        "p['b'] * 1.0 > 0", "p['b'] * 1.0 == p['b']", "p['b'] > 1.5", "p['b'] + 0.5 > p['b']",
        "-p['i'] < 0", "-p['b'] < 0", "-p['s'] == ''", "-p['t'] == -1", "- -p['i'] == p['i']",
        "p['t'] + 1 == 2", "p['t'] / 2 == 0.5", "p['t'] - p['t'] == 0", "p['t'] * p['b'] == p['b']",
        # 真除法、int 与 float、除零
        "p['b'] / p['i'] > 0", "p['i'] / 2 == 1.5", "p['b'] / 0 > 0", "p['f'] / 0 > 0", "p['i'] / p['z'] > 0",
        "p['f'] / p['z'] > 0", "p['f'] + 0 == p['f'] + 0", "p['f'] * 2 > 0", "p['f'] - p['f'] == 0",
        "p['i'] * 1e308 * 10 > 0", "p['i'] / 3 * 3 == p['i']",
        # 字符串
        "p['s'] + p['s'] == 'abab'", "p['s'] * 2 == 'abab'", "2 * p['s'] == p['s'] + p['s']",
        "p['s'] * -1 == ''", "p['s'] * 0 == p['e']", "p['s'] + p['u'] > 'ab'", "p['s'] + 1 > 0",
        "p['s'] * 1.5 == 'ab'", "p['s'] * p['s'] == ''", "p['s'] * p['t'] == p['s']", "p['s'] + p['l'] == p['s']",
        "p['s'] + 'c' like 'ab%'", "p['s'] * 2 like 'abab'",
        # 列表（JSON NaN 是同一个 Python 对象，列表相等按身份判等）
        "p['l'] + p['l'] == p['l'] * 2", "p['l'] * 2 == p['l'] * 2", "p['l'] + p['l'] > p['l']",
        "p['l'] * 0 == p['el']", "p['l'] * p['t'] == p['l']", "p['l'] * 1.5 == p['l']",
        "p['l'] + p['l'] == p['l']", "p['l'] + p['el'] == p['l']", "p['l'][0] + 1 == 2", "p['n'] + 1 > 0",
        # 重复的判定顺序：次数放不进 Py_ssize_t → 空结果 → 长度乘积 → 分配
        f"p['s'] * {big} == ''", f"p['e'] * {big} == ''", f"p['s'] * -{big + 1} == ''", f"p['s'] * -{big} == ''",
        f"p['l'] * {big} == p['l']", f"p['el'] * {big} == p['l']", f"{big} * p['s'] == ''",
        f"p['s'] * {2**62} == ''", f"p['e'] * {2**62} == ''", f"p['s'] * {2**62 - 1} == ''",
        f"p['l'] * {2**62} == p['l']", f"p['el'] * {2**62} == p['el']", f"p['l'] * {2**61} == p['l']",
        # 会报错的大整数与 Python 的短路
        "p['h'] + 1 > 0", "p['h'] + 1.5 > 0", "1.5 * p['h'] > 0", "p['h'] / 1 > 0", "p['h'] / p['h'] == 1",
        "p['h'] * 0 == 0", "p['h'] > 1.5", "p['h'] - p['h'] == 0", "p['h'] / 0 > 0",
        "p['i'] > 100 and p['h'] + 1.5 > 0", "p['h'] + 1.5 > 0 and p['i'] > 100",
        "p['i'] < 100 or p['h'] + 1.5 > 0", "p['h'] * 1.5 > 0 or p['i'] < 100",
        # ARRAY 字段经 Python 路线：列表拼接与重复
        "array_length(a) >= 0 and a + a == a * 2", "array_length(a) >= 0 and a * 2 > a",
        "array_length(af) >= 0 and af + af == af * 2", "array_length(a) >= 0 and a + af > a",
        "array_length(a) >= 0 and a + 1 > 0", f"array_length(a) >= 0 and a * {big} == a",
        f"array_length(a) >= 0 and a * {2**62} == a", "array_length(a) >= 0 and i8 + 1 > 127",
        "array_length(a) >= 0 and age * age > 0", "array_length(a) >= 0 and f * f == 1.21",
        "array_length(a) >= 0 and n + f > 0", "array_length(a) >= 0 and id / 0 > 1",
        # hybrid 回退后的 Python 值规则
        "x * 2 == x + x", "x + 'b' == 'ab'", "x * y == x + x", "x + 0.5 > 1", "x - x == 0",
        "x == 'a'", "x > 0", "4611686018427387904 / x > 0", "x * 1180591620717411303424 > 0",
        "(x * 1180591620717411303424) - (x * x) == 0",
    ]
    # 正确舍入的整数真除法：期望值由 Python 自己算出，字面量按 repr 往返。
    for row in P_ROWS:
        if row is not None:
            result.append(f"p['q'] / p['r'] == {row['q'] / row['r']!r}")
    return result


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
