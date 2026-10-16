"""固定参照 43d1257 的 ARRAY 元素转换与完整过滤路线真值；离线显式执行。"""

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
REF = "43d1257774e629bc9f66873977ab7c320d5bf5a7"
TYPES = {"bool": True, "int8": 7, "int16": 7, "int32": 7, "int64": 7,
         "float": 1.5, "double": 1.5, "varchar": "abc"}


def conversion_cases(pa):
    candidates = [None, True, False, 1, -129, 128, 32768, 2 ** 63, 1.0, 1.5,
                  -128.9, 127.9, 128.0, 2 ** 24 + 1, 2 ** 53 + 1, "1", "abc", {}, [1]]
    cases = []
    for name in TYPES:
        arrow_type = {"bool": pa.bool_(), "int8": pa.int8(), "int16": pa.int16(),
                      "int32": pa.int32(), "int64": pa.int64(), "float": pa.float32(),
                      "double": pa.float64(), "varchar": pa.string()}[name]
        for value in candidates:
            case = {"type": name, "value": value}
            try:
                case["result"] = pa.array([[value]], type=pa.list_(arrow_type)).to_pylist()
            except Exception as error:
                case.update(error_type=type(error).__name__, error=str(error))
            cases.append(case)
    return cases


def filter_cases(pa):
    from milvus_lite.schema.types import CollectionSchema, DataType, FieldSchema, TYPE_MAP
    from milvus_lite.search.filter.eval import evaluate
    from milvus_lite.search.filter.parser import parse_expr
    from milvus_lite.search.filter.semantic import compile_expr

    cases = []
    for name, good in TYPES.items():
        dtype = getattr(DataType, name.upper())
        for shape in ("mixed", "all_null", "empty"):
            xs = [None, [], [good], [None, good]] if shape == "mixed" else [None] * 4 if shape == "all_null" else [[], [], [], []]
            literal = json.dumps(good)
            schema = CollectionSchema([
                FieldSchema("id", DataType.INT64, is_primary=True),
                FieldSchema("vector", DataType.FLOAT_VECTOR, dim=2),
                FieldSchema("x", DataType.ARRAY, element_type=dtype, nullable=True),
                FieldSchema("j", DataType.JSON)], enable_dynamic_field=True)
            table = pa.table({"id": pa.array([1, 2, 3, 4], type=pa.int64()),
                              "vector": pa.array([[1., 0.]] * 4, type=pa.list_(pa.float32(), 2)),
                              "x": pa.array(xs, type=pa.list_(TYPE_MAP[dtype])),
                              "j": pa.array(['{"k":1}', '{"k":null}', '{}', 'null']),
                              "$meta": pa.array(['{"d":1}', '{"d":null}', '{}', '{"d":false}'])})
            expressions = ["x is null", "x is not null", "exists x", "not (x is null)",
                           "array_length(x) == 0", "array_length(x) > 0", f"array_contains(x,{literal})",
                           f"not array_contains(x,{literal})", f"array_contains_all(x,[{literal}])",
                           f"array_contains_any(x,[{literal}])", "array_contains_all(x,[])", "array_contains_any(x,[])",
                           f"x[0] == {literal}", "x[0] is null", f"x[1] == {literal}", "x[99] is null",
                           "x[-1] is null", f"x == {literal}", "x == x", "x in [1]", "x not in [1]",
                           "x in []", "x not in []", "x == []", "not (x == 1)",
                           "x is null or d == 1", "x is null and d == 1", "x == 1 or d == 1",
                           "x is null or j['k'] == 1", "false and array_contains(x,missing)",
                           "true or array_contains(x,missing)", "array_contains(x,[1])"]
            for expression in expressions:
                case = {"element_type": name, "shape": shape, "values": xs, "expression": expression}
                phase = "parse"
                try:
                    parsed = parse_expr(expression)
                    phase = "compile"
                    compiled = compile_expr(parsed, schema, expression)
                    case["backend"] = compiled.backend
                    phase = "evaluate"
                    case["mask"] = evaluate(compiled, table).to_pylist()
                except Exception as error:
                    case.update(phase=phase, error_type=type(error).__name__, error=str(error))
                cases.append(case)
    return cases


def business_cases():
    sys.path.insert(0, str(ROOT / "tests/compatibility_audit"))
    import schema_truth
    cases = [case for case in schema_truth.values() if case["dtype"] == "ARRAY"]
    for case in cases:
        good = schema_truth.SCALARS[case["element_type"]][0]
        case["default_value"] = None if case["default"] == "absent" else [] if case["default"] == "empty" else [good]
    return cases


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verify", action="store_true")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    import pyarrow as pa
    import milvus_lite
    reference = ROOT / "references/milvus-lite"
    assert subprocess.check_output(["git", "-C", str(reference), "rev-parse", "HEAD"], text=True).strip() == REF
    assert Path(milvus_lite.__file__).resolve().is_relative_to(reference)
    assert pa.__version__.startswith(("15.", "25."))
    result = {"reference": REF, "generator_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "conversions": conversion_cases(pa), "filters": filter_cases(pa), "business": business_cases(),
              "business_generator_sha256": hashlib.sha256((ROOT / "tests/compatibility_audit/schema_truth.py").read_bytes()).hexdigest()}
    destination = ROOT / "crates/core/tests/fixtures/array_v321.json"
    if args.verify:
        assert json.loads(destination.read_text()) == result, "ARRAY fixture differs from reference"
    else:
        destination.write_text(json.dumps(result, ensure_ascii=True, indent=2, allow_nan=False) + "\n")
    if args.output:
        args.output.write_text(json.dumps({"arrow": pa.__version__, "conversions": len(result['conversions']),
                                          "filters": len(result['filters']), "verified": args.verify,
                                          "fixture_sha256": hashlib.sha256(destination.read_bytes()).hexdigest()}, indent=2) + "\n")


if __name__ == "__main__":
    main()
