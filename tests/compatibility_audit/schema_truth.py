"""固定参照版本的 schema、空值过滤与向量生命周期探针；输出只写仓库外。"""

import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
from types import SimpleNamespace

REF = "43d1257774e629bc9f66873977ab7c320d5bf5a7"
SCALARS = {"BOOL": (True, False), "INT8": (7, 0), "INT16": (7, 0), "INT32": (7, 0),
           "INT64": (7, 0), "FLOAT": (1.5, 0.0), "DOUBLE": (1.5, 0.0),
           "VARCHAR": ("abc", ""), "JSON": ({"k": 1}, {})}


def attempt(fn):
    try:
        return {"ok": True, "value": fn()}
    except Exception as error:
        return {"ok": False, "error_type": type(error).__name__, "error": str(error)}


def values():
    from milvus_lite.engine.collection import Collection
    from milvus_lite.schema.arrow_builder import build_wal_data_schema
    from milvus_lite.schema.types import CollectionSchema, DataType, FieldSchema
    from milvus_lite.schema.validation import validate_record, validate_schema

    cases = []
    for dtype, (good, zero) in SCALARS.items():
        for nullable in (False, True):
            for default_name, default in (("absent", None), ("value", good), ("falsy", zero), ("list_candidate", ["bad"])):
                for input_name, value in (("missing", None), ("null", None), ("value", good), ("falsy", zero), ("list_candidate", ["bad"])):
                    field = FieldSchema("x", getattr(DataType, dtype), nullable=nullable,
                                        default_value=copy.deepcopy(default), **({"max_length": 8} if dtype == "VARCHAR" else {}))
                    schema = CollectionSchema([FieldSchema("id", DataType.INT64, is_primary=True),
                                               FieldSchema("vector", DataType.FLOAT_VECTOR, dim=2), field])
                    record = {"id": 1, "vector": [1.0, 0.0]}
                    if input_name != "missing":
                        record["x"] = copy.deepcopy(value)
                    case = {"dtype": dtype, "nullable": nullable, "default": default_name, "default_value": default,
                            "input": input_name, "input_value": value}
                    case["schema"] = attempt(lambda: validate_schema(schema))
                    case["validate"] = attempt(lambda: validate_record(record, schema))
                    case["normalized_record"] = record
                    if case["validate"]["ok"]:
                        def encode():
                            host = SimpleNamespace(_schema=schema, _wal_data_schema=build_wal_data_schema(schema))
                            batch = Collection._build_wal_data_batch(host, [record], "_default", [1])
                            column = batch.column("x")
                            return {"arrow_type": str(column.type), "null_count": column.null_count, "values": column.to_pylist()}
                        case["batch"] = attempt(encode)
                    cases.append(case)
    for dtype, (good, _) in SCALARS.items():
        if dtype == "JSON":
            continue
        for nullable in (False, True):
            for default_name, default in (("absent", None), ("empty", []), ("value", [good])):
                for input_name, value in (("missing", None), ("null", None), ("empty", []), ("value", [good]),
                                          ("null_element", [None, good]), ("wrong_element", [{}]), ("oversize", [good] * 3)):
                    field = FieldSchema("x", DataType.ARRAY, element_type=getattr(DataType, dtype), max_capacity=2,
                                        nullable=nullable, default_value=copy.deepcopy(default))
                    schema = CollectionSchema([FieldSchema("id", DataType.INT64, is_primary=True),
                                               FieldSchema("vector", DataType.FLOAT_VECTOR, dim=2), field])
                    record = {"id": 1, "vector": [1.0, 0.0]}
                    if input_name != "missing":
                        record["x"] = copy.deepcopy(value)
                    case = {"dtype": "ARRAY", "element_type": dtype, "nullable": nullable,
                            "default": default_name, "input": input_name, "input_value": value}
                    case["schema"] = attempt(lambda: validate_schema(schema))
                    case["validate"] = attempt(lambda: validate_record(record, schema))
                    case["normalized_record"] = record
                    if case["validate"]["ok"]:
                        def encode_array():
                            host = SimpleNamespace(_schema=schema, _wal_data_schema=build_wal_data_schema(schema))
                            col = Collection._build_wal_data_batch(host, [record], "_default", [1]).column("x")
                            return {"arrow_type": str(col.type), "null_count": col.null_count, "values": col.to_pylist()}
                        case["batch"] = attempt(encode_array)
                    cases.append(case)
    return cases


def filters():
    import pyarrow as pa
    from milvus_lite.schema.types import CollectionSchema, DataType, FieldSchema, TYPE_MAP
    from milvus_lite.search.filter.eval import evaluate
    from milvus_lite.search.filter.parser import parse_expr
    from milvus_lite.search.filter.semantic import compile_expr

    cases = []
    for dtype, (good, zero) in SCALARS.items():
        literal = json.dumps(good, ensure_ascii=True)
        if dtype == "JSON":
            literal = "'null'"
        expressions = ["x is null", "x is not null", "exists x", f"x == {literal}", f"x != {literal}",
                       f"x in [{literal}]", f"x not in [{literal}]", "x in []", "x not in []", "x in [null]",
                       "not (x is null)", f"not (x == {literal})", f"x > {literal}", "x like 'a%'",
                       f"x == {literal} or id > 2", f"x == {literal} and id > 2",
                       f"x == {literal} or j['k'] == 1", f"x == {literal} and j['k'] == 1",
                       f"x == {literal} or json_contains(j['a'],1)",
                       f"false and x == {literal}", f"true or x == {literal}",
                       "x is null or d == 1", "x is null or d['k'] == 1"]
        for all_null in (False, True):
            xs = [None] * 4 if all_null else [None, good, zero, None]
            if dtype == "JSON" and not all_null:
                xs = [None, "null", '{"k": 1}', '{}']
            fields = [FieldSchema("id", DataType.INT64, is_primary=True),
                      FieldSchema("vector", DataType.FLOAT_VECTOR, dim=2),
                      FieldSchema("x", getattr(DataType, dtype), nullable=True), FieldSchema("j", DataType.JSON)]
            schema = CollectionSchema(fields, enable_dynamic_field=True)
            columns = {"id": pa.array([1, 2, 3, 4], type=pa.int64()), "vector": pa.array([[1.0, 0.0]] * 4, type=pa.list_(pa.float32(), 2)),
                       "x": pa.array(xs, type=TYPE_MAP[getattr(DataType, dtype)]),
                       "j": pa.array(['{"k":1,"a":[1]}', '{"k":null,"a":[]}', '{}', 'null']),
                       "$meta": pa.array(['{"d":1}', '{"d":null}', '{}', '{"d":{"k":1}}'])}
            table = pa.table(columns)
            for expr in expressions:
                case = {"dtype": dtype, "all_null": all_null, "expression": expr, "values": xs}
                phase = "parse"
                try:
                    ast = parse_expr(expr)
                    phase = "compile"
                    compiled = compile_expr(ast, schema, expr)
                    case["backend"] = compiled.backend
                    phase = "evaluate"
                    case["mask"] = evaluate(compiled, table).to_pylist()
                except Exception as error:
                    case.update(phase=phase, error_type=type(error).__name__, error=str(error))
                cases.append(case)
    for dtype, (good, _) in SCALARS.items():
        if dtype == "JSON":
            continue
        literal = json.dumps(good)
        fields = [FieldSchema("id", DataType.INT64, is_primary=True),
                  FieldSchema("vector", DataType.FLOAT_VECTOR, dim=2),
                  FieldSchema("x", DataType.ARRAY, element_type=getattr(DataType, dtype), nullable=True)]
        schema = CollectionSchema(fields)
        xs = [None, [], [good], [None, good]]
        table = pa.table({"id": pa.array([1, 2, 3, 4], type=pa.int64()),
                          "x": pa.array(xs, type=pa.list_(TYPE_MAP[getattr(DataType, dtype)])),
                          "vector": pa.array([[1.0, 0.0]] * 4, type=pa.list_(pa.float32(), 2))})
        for expr in ("x is null", "x is not null", "array_length(x) == 0", "array_length(x) > 0",
                     f"array_contains(x,{literal})", f"not array_contains(x,{literal})",
                     f"array_contains_all(x,[{literal}])", f"array_contains_any(x,[{literal}])",
                     f"x[0] == {literal}", "x[0] is null", f"x[1] == {literal}", "x[99] is null"):
            case = {"dtype": "ARRAY", "element_type": dtype, "expression": expr, "values": xs}
            phase = "parse"
            try:
                ast = parse_expr(expr)
                phase = "compile"
                compiled = compile_expr(ast, schema, expr)
                case["backend"] = compiled.backend
                phase = "evaluate"
                case["mask"] = evaluate(compiled, table).to_pylist()
            except Exception as error:
                case.update(phase=phase, error_type=type(error).__name__, error=str(error))
            cases.append(case)
    return cases


def snapshot(col):
    return {"get": attempt(lambda: col.get([1, 2, 3])),
            "query": attempt(lambda: col.query(output_fields=["id", "vector"], order_by_pk=True)),
            "search": attempt(lambda: col.search([[0.0, 0.0]], top_k=20, metric_type="L2", output_fields=["vector"])),
            "memtable": col._memtable.to_arrow_table().to_pylist(),
            "segments": [{"rows": seg.table.to_pylist(), "null_mask": None if seg.vector_null_mask is None else seg.vector_null_mask.tolist()}
                         for seg in col._segment_cache.values()],
            "data_files": list(col._manifest.get_data_files("_default"))}


def vector_child(directory, output):
    from milvus_lite.db import MilvusLite
    from milvus_lite.schema.types import CollectionSchema, DataType, FieldSchema

    db = MilvusLite(str(directory))
    schema = CollectionSchema([FieldSchema("id", DataType.INT64, is_primary=True),
                               FieldSchema("vector", DataType.FLOAT_VECTOR, dim=2, nullable=True)])
    col = db.create_collection("c", schema)
    col.create_index("vector", {"index_type": "FLAT", "metric_type": "L2"})
    col.load()
    col.insert([{"id": 1, "vector": None}, {"id": 2, "vector": [0.0, 0.0]}, {"id": 3, "vector": [1.0, 0.0]}])
    Path(output).write_text(json.dumps(snapshot(col), indent=2) + "\n")
    os._exit(0)


def vectors(args):
    from milvus_lite.constants import COMPACTION_MIN_FILES_PER_BUCKET
    from milvus_lite.db import MilvusLite

    directory = args.output.parent / (args.output.stem + "-vector.db")
    if directory.exists():
        raise ValueError("向量目录已存在")
    child_out = args.output.parent / (args.output.stem + "-active.json")
    subprocess.run([sys.executable, __file__, "--reference", str(args.reference), "--kind", "vector-child",
                    "--output", str(child_out), "--data-dir", str(directory)], check=True, timeout=60)
    result = {"memtable": json.loads(child_out.read_text())}
    result["wal_files_before_replay"] = [str(p.relative_to(directory)) for p in directory.rglob("*.arrow")]
    db = MilvusLite(str(directory))
    try:
        col = db.get_collection("c")
        col.load()
        result["wal_replay"] = snapshot(col)
        col.flush()
        col._wait_for_bg(timeout=60)
        result["segment"] = snapshot(col)
        initial = set(col._manifest.get_data_files("_default"))
        for number in range(COMPACTION_MIN_FILES_PER_BUCKET - 1):
            col.insert([{"id": 10 + number, "vector": None}])
            col.flush()
            col._wait_for_bg(timeout=60)
        result["compaction"] = snapshot(col)
        result["compaction_replaced_initial_file"] = not bool(initial & set(col._manifest.get_data_files("_default")))
        if not result["compaction_replaced_initial_file"]:
            raise AssertionError("没有实际发生 compaction")
    finally:
        db.close()
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--kind", required=True, choices=("values", "filters", "vectors", "vector-child", "rpc"))
    parser.add_argument("--data-dir", type=Path)
    parser.add_argument("--verify-nullable-filter-fixture", action="store_true")
    args = parser.parse_args()
    args.reference, args.output = args.reference.resolve(), args.output.resolve()
    repo = Path(__file__).resolve().parents[2]
    if repo == args.output or repo in args.output.parents:
        raise ValueError("输出必须位于仓库外")
    if args.data_dir is not None and (args.data_dir.resolve() == repo or repo in args.data_dir.resolve().parents):
        raise ValueError("数据目录必须位于仓库外")
    if args.output.exists():
        raise ValueError("不覆盖已有证据")
    actual = subprocess.check_output(["git", "-C", str(args.reference), "rev-parse", "HEAD"], text=True).strip()
    if actual != REF:
        raise ValueError(f"参照版本不符：{actual}")
    subprocess.run(["git", "-C", str(args.reference), "diff", "--exit-code", "HEAD", "--", "milvus_lite"], check=True, stdout=subprocess.DEVNULL)
    sys.path.insert(0, str(args.reference))
    sys.dont_write_bytecode = True
    import milvus_lite
    import pyarrow as pa
    if Path(milvus_lite.__file__).resolve().parent != args.reference / "milvus_lite":
        raise ValueError("实际加载的参照路径不符")
    if args.kind == "vector-child":
        vector_child(args.data_dir, args.output)
    result = {"reference": REF, "arrow": pa.__version__, "kind": args.kind,
              "generator_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    if args.kind == "rpc":
        from rpc_truth import run
        result["rpc_generator_sha256"] = hashlib.sha256(Path(__file__).with_name("rpc_truth.py").read_bytes()).hexdigest()
        result["cases"] = run(args.output.parent / (args.output.stem + ".db"))
    else:
        result["cases"] = values() if args.kind == "values" else filters() if args.kind == "filters" else vectors(args)
    if args.verify_nullable_filter_fixture:
        if args.kind!="filters":
            parser.error("fixture 核验只适用于 filters")
        expected=json.loads((repo/"crates/core/tests/fixtures/nullable_filter_v321.json").read_text())
        actual=[case for case in result["cases"] if case["dtype"]!="ARRAY"]
        if expected["reference"]!=REF or expected["cases"]!=actual:
            raise AssertionError("nullable 过滤组件真值发生变化")
    with args.output.open("x") as out:
        json.dump(result, out, ensure_ascii=True, indent=2)
        out.write("\n")
    print(json.dumps({k: v for k, v in result.items() if k != "cases"}), "cases", len(result["cases"]))


if __name__ == "__main__":
    main()
