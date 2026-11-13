"""过滤算术的直接模糊对照（0032）：真实 3.x 的 parse / compile / evaluate 与 Rust 内存集合的 Query。

运行前构建 cargo build -p milvus-lite-core --example filter_arith_probe。
脚本在 3.x 的可编辑安装环境运行（Arrow 15 / 25 均可），不经服务、不依赖固定语料。
每张随机表（类型化列、JSON 列与 $meta）上生成一组表达式，覆盖 Arrow、hybrid 与 Python 三条路线；
本实现命中的主键须与 3.x 的掩码为 True 的行相同，错误须文案相同。

两类结果取决于本实现之外的条件，生成时排除，不算作一致：
- 3.x 中字符串 / 列表重复或拼接的结果长度超过 10^6、又不至于让分配大小本身越界时，成败取决于机器内存，
  本实现则可能先触及 0032 的资源上限。脚本在本进程内包装 3.x 的 _ARITH_OPS，遇到这种结果放弃该表达式
  （计入 discarded），不调用原运算；分配大小超过 2^60 字节的仍交给 3.x 自己报错（OverflowError 或
  MemoryError，后者对应本实现的资源错误）。
- 0032 登记的 hybrid 推断差异（pyarrow 按元素顺序推断：超出 int64 的整数先于其他类型、float 之后的 bool）。
  $meta 的「混排」取值不含超出 int64 的整数，含 float 时不含 bool；这些差异由组件真值两边分别断言。
"""
import argparse
from collections import Counter
import json
import math
from pathlib import Path
import random
import subprocess
import tempfile

import milvus_lite
import pyarrow as pa
from milvus_lite.schema.types import CollectionSchema, DataType, FieldSchema
from milvus_lite.search.filter.eval import evaluate
from milvus_lite.search.filter.eval import python_backend
from milvus_lite.search.filter.parser import parse_expr
from milvus_lite.search.filter.semantic import compile_expr

ROOT = Path(__file__).resolve().parents[2]
SMALL_RESULT = 10**6
HUGE_BYTES = 2**60


class Discard(BaseException):
    """不是 Exception 的子类：hybrid 的回退与 Python 路线的吞错都不会拦下它。"""


def guarded(op, name):
    def run(left, right):
        sequences = (str, list)
        if name == "*":
            if isinstance(left, sequences) and isinstance(right, int):
                base, count = left, right
            elif isinstance(right, sequences) and isinstance(left, int):
                base, count = right, left
            else:
                return op(left, right)
            length = len(base) * count
        elif name == "+" and isinstance(left, sequences) and type(left) is type(right):
            length = len(left) + len(right)
        else:
            return op(left, right)
        # 字符串每个码位至多 4 字节，列表每个元素一个 8 字节指针。
        if length > SMALL_RESULT and length * (4 if isinstance(base if name == "*" else left, str) else 8) <= HUGE_BYTES:
            raise Discard()
        return op(left, right)
    return run


for _name, _op in list(python_backend._ARITH_OPS.items()):
    python_backend._ARITH_OPS[_name] = guarded(_op, _name)

# (名称, 类型, 可空, ARRAY 元素类型, 取值池)
INT8 = [-128, -127, -1, 0, 1, 2, 3, 127]
INT16 = INT8 + [-32768, 255, 32767]
INT32 = INT16 + [2**24, 2**24 + 1, -(2**24) - 1, 2**31 - 1, -(2**31)]
INT64 = INT32 + [2**53, 2**53 + 1, -(2**53) - 1, 2**62, 2**63 - 1, -(2**63)]
FLOAT = ["0.0", "-0.0", "1.5", "-2.5", "0.1", "2.0", "3e+38", "-3e+38", "1e-45", "16777216.0", "nan", "inf", "-inf"]
DOUBLE = FLOAT + ["1e+308", "-1e+308", "5e-324", "9007199254740992.0"]
FIELDS = [
    ("i8", "INT8", True, None, INT8), ("i16", "INT16", True, None, INT16),
    ("i32", "INT32", True, None, INT32), ("n", "INT64", True, None, INT64),
    ("f", "FLOAT", True, None, FLOAT), ("d", "DOUBLE", True, None, DOUBLE),
    ("flag", "BOOL", False, None, [True, False]), ("s", "VARCHAR", False, None, ["", "a", "ab"]),
    ("a", "ARRAY", True, "INT64", [[1], [2, 3], []]), ("j", "JSON", True, None, None),
]
NUMERIC = ["i8", "i16", "i32", "n", "f", "d"]
INVALID = ["flag", "s", "a"]
SMALL_INTS = [0, 1, 2, 3, 7, -1, -2]
BIG_INTS = [2**53 + 1, -(2**63), 2**63 - 1, 2**63, 2**64, 2**70, -(2**70), 10**30]
FLOATS = [0.0, -0.0, 1.5, -2.5, 0.1, 1e308, -1e308, 5e-324]
OTHERS = ["", "a", "ab", "é", [], [1], [1, "a"], [0.5, [2]], [None], {"k": 1}]
INT_LITERALS = ["0", "1", "2", "3", "7", "16777217", "9007199254740993", "4611686018427387904",
                "9223372036854775807", "9223372036854775808", "1180591620717411303424",
                "100000000000000000000000000000"]
FLOAT_LITERALS = ["0.0", "1.5", "2.5", "0.1", "1e308", "1e-300", "3.4e38"]
COMPARE = ["==", "!=", "<", "<=", ">", ">="]


def json_value(rng):
    pool = rng.choice([SMALL_INTS, BIG_INTS, FLOATS, [True, False], OTHERS, [None]])
    return rng.choice(pool)


def meta_values(rng, rows, coverage):
    """一个动态键在各行的取值；缺失用 Ellipsis 表示。"""
    profile = rng.choice(["int", "float", "numeric", "other", "mixed", "absent"])
    coverage["meta_" + profile] += 1
    if profile == "absent":
        return [...] * rows
    pools = {
        "int": SMALL_INTS + BIG_INTS[:4],
        "float": FLOATS,
        "numeric": SMALL_INTS + BIG_INTS + FLOATS,
        "other": OTHERS + [True, False],
    }
    if profile == "mixed":
        floats = rng.random() < 0.5
        pool = SMALL_INTS + [2**53 + 1, -(2**63), 2**63 - 1] + OTHERS
        pool += FLOATS if floats else [True, False]
    else:
        pool = pools[profile]
    return [rng.choice(pool + [None, ...]) for _ in range(rows)]


def make_table(rng, coverage):
    rows = rng.randint(3, 6)
    columns = {}
    for name, dtype, nullable, _, pool in FIELDS:
        if dtype == "JSON":
            values = []
            for _ in range(rows):
                if rng.random() < 0.15:
                    values.append(None)
                    continue
                obj = {key: json_value(rng) for key in ("x", "y") if rng.random() < 0.85}
                values.append(json.dumps(obj))
        else:
            values = [rng.choice(pool) for _ in range(rows)]
            if nullable and rng.random() < 0.6:
                values = [None if rng.random() < 0.3 else v for v in values]
        columns[name] = values
    keys = {key: meta_values(rng, rows, coverage) for key in ("x", "y")}
    meta = [json.dumps({k: v[row] for k, v in keys.items() if v[row] is not ...}) for row in range(rows)]
    return {"rows": rows, "fields": [list(f[:4]) for f in FIELDS], "columns": columns, "meta": meta}


def leaf(rng, mode):
    roll = rng.random()
    if roll < 0.3:
        # 超出 int64 的字面量在 Arrow / hybrid 中直接报 OverflowError，少量即可覆盖。
        pool = INT_LITERALS[:5] if rng.random() < 0.8 else INT_LITERALS[5:]
        return rng.choice(pool if rng.random() < 0.6 else FLOAT_LITERALS)
    if roll < 0.31:
        return rng.choice(INVALID)
    if mode == "arrow" or roll < 0.55:
        return rng.choice(NUMERIC)
    if mode == "hybrid":
        return rng.choice(["x", "y"])
    return rng.choice(["j['x']", "j['y']", "$meta['x']", "$meta['y']", "x"])


def arith(rng, mode, depth):
    if depth == 0 or rng.random() < 0.25:
        return leaf(rng, mode)
    if rng.random() < 0.1:
        return "-" + arith(rng, mode, depth - 1)
    left, right = arith(rng, mode, depth - 1), arith(rng, mode, depth - 1)
    return f"({left} {rng.choice('+-*/')} {right})"


def predicate(rng, mode):
    left = arith(rng, mode, rng.randint(1, 3))
    if mode == "python" and rng.random() < 0.1:
        return f"{left} like 'a%'"
    if rng.random() < 0.5:
        right = rng.choice(INT_LITERALS[:5] + FLOAT_LITERALS[:4] + ["'a'", "'ab'"] if mode == "python" else INT_LITERALS[:5] + FLOAT_LITERALS[:4])
    else:
        right = arith(rng, mode, rng.randint(0, 2))
    return f"{left} {rng.choice(COMPARE)} {right}"


def expression(rng, coverage):
    mode = rng.choice(["arrow", "hybrid", "python"])
    coverage["mode_" + mode] += 1
    text = predicate(rng, mode)
    roll = rng.random()
    if roll < 0.3:
        text = f"{text} {rng.choice(['and', 'or'])} {predicate(rng, mode)}"
    elif roll < 0.4:
        text = f"not ({text})"
    return text


def schema():
    fields = [FieldSchema(name="id", dtype=DataType.INT64, is_primary=True),
              FieldSchema(name="vector", dtype=DataType.FLOAT_VECTOR, dim=2)]
    for name, dtype, nullable, element, _ in FIELDS:
        kwargs = {"nullable": nullable}
        if element:
            kwargs["element_type"] = getattr(DataType, element)
        fields.append(FieldSchema(name=name, dtype=getattr(DataType, dtype), **kwargs))
    return CollectionSchema(fields=fields, enable_dynamic_field=True)


def arrow_table(spec):
    types = {"INT8": pa.int8(), "INT16": pa.int16(), "INT32": pa.int32(), "INT64": pa.int64(),
             "FLOAT": pa.float32(), "DOUBLE": pa.float64(), "BOOL": pa.bool_(), "VARCHAR": pa.string(),
             "JSON": pa.string()}
    rows = spec["rows"]
    columns = {"id": pa.array(range(rows), type=pa.int64()),
               "vector": pa.array([[1.0, 1.0]] * rows, type=pa.list_(pa.float32(), 2))}
    for name, dtype, _, element in spec["fields"]:
        values = spec["columns"][name]
        if dtype in ("FLOAT", "DOUBLE"):
            values = [None if v is None else float(v) for v in values]
        arrow = pa.list_(types[element]) if dtype == "ARRAY" else types[dtype]
        columns[name] = pa.array(values, type=arrow)
    columns["_seq"] = pa.array(range(1, rows + 1), type=pa.int64())
    columns["$meta"] = pa.array(spec["meta"], type=pa.string())
    return pa.table(columns)


def reference(source, compiled_schema, table, coverage):
    try:
        compiled = compile_expr(parse_expr(source), compiled_schema, source)
        coverage["backend_" + compiled.backend] += 1
        mask = evaluate(compiled, table).to_pylist()
    except Discard:
        return None
    except MemoryError:
        coverage["memory_error"] += 1
        return {"error": None, "resource": True}
    except Exception as error:
        coverage["error"] += 1
        coverage["error_" + type(error).__name__] += 1
        return {"error": str(error)}
    hits = [row for row, hit in enumerate(mask) if hit]
    coverage["hits" if hits else "no_hits"] += 1
    return {"ids": hits}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--probe", type=Path, default=ROOT / "target/debug/examples/filter_arith_probe")
    parser.add_argument("--tables", type=int, default=2000)
    parser.add_argument("--per-table", type=int, default=25)
    parser.add_argument("--seed", type=int, default=32)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    assert args.tables > 0 and args.per_table > 0
    assert milvus_lite.__version__ == "3.2.2"
    assert Path(milvus_lite.__file__).resolve().is_relative_to(ROOT / "references/milvus-lite")
    assert subprocess.check_output(["git", "-C", str(ROOT / "references/milvus-lite"), "rev-parse", "--short=7", "HEAD"], text=True).strip() == "1320d26"
    rng = random.Random(args.seed)
    coverage = Counter()
    compiled_schema = schema()
    with tempfile.TemporaryDirectory(prefix="mlr_arith_fuzz_") as scratch:
        path = Path(scratch)
        inputs, expected = path / "input.jsonl", path / "reference.jsonl"
        with inputs.open("w") as requests, expected.open("w") as answers:
            for _ in range(args.tables):
                spec = make_table(rng, coverage)
                table = arrow_table(spec)
                requests.write(json.dumps({"table": spec}, ensure_ascii=False) + "\n")
                for _ in range(args.per_table):
                    source = expression(rng, coverage)
                    result = reference(source, compiled_schema, table, coverage)
                    if result is None:
                        coverage["discarded"] += 1
                        continue
                    requests.write(json.dumps({"expression": source}, ensure_ascii=False) + "\n")
                    answers.write(json.dumps({"expression": source, "table": coverage["tables"], "result": result}, ensure_ascii=False) + "\n")
                coverage["tables"] += 1
        with inputs.open("rb") as source:
            rust = subprocess.run([str(args.probe.resolve())], stdin=source, capture_output=True, check=True).stdout.decode().splitlines()
        mismatches = []
        checked = 0
        with expected.open() as answers:
            for line, actual in zip(answers, rust, strict=True):
                checked += 1
                case, actual = json.loads(line), json.loads(actual)
                if case["result"] != actual:
                    mismatches.append({**case, "rust": actual})
    assert checked == args.tables * args.per_table - coverage["discarded"], (checked, coverage)
    assert checked > 0.95 * args.tables * args.per_table, "too many discarded cases"
    required = ["mode_arrow", "mode_hybrid", "mode_python", "backend_arrow", "backend_hybrid", "backend_python",
                "hits", "no_hits", "error", "memory_error"]
    missing = [key for key in required if not coverage[key]]
    report = {"reference": "3.2.2+3 / 1320d26", "arrow": pa.__version__, "seed": args.seed, "checked": checked,
              "coverage": dict(sorted(coverage.items())), "missing_coverage": missing, "mismatches": mismatches}
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({k: v for k, v in report.items() if k != "mismatches"}), flush=True)
    print(f"{len(mismatches)} mismatches; report: {args.output}", flush=True)
    if mismatches or missing:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
