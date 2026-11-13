"""真实 3.x / 1320d26 的 hybrid 动态列推断与转换组件真值（0033）；--verify 不改写语料。

每条语料是一个动态键在一个数据源各行的取值（客户端原始键序）。3.x 一侧经写入函数
separate_dynamic_fields（sort_keys=True）生成 $meta，再由 hybrid 的 _augment_table 物化，
与真实路径相同。结果只保存可观察的部分：回退（错误原文被 hybrid 吞掉，不保存）、
OverflowError 原文，或成功时的顶层形状与标量转换后的值。

Arrow 25 与 Arrow 15 只在 0033 登记的对象字段顺序情形下结果不同，这些条目另存 arrow15。
"""

import argparse
import json
import random
import subprocess
from pathlib import Path

import milvus_lite
import pyarrow as pa
from milvus_lite.schema.types import CollectionSchema, DataType, FieldSchema
from milvus_lite.schema.validation import separate_dynamic_fields
from milvus_lite.search.filter.eval.hybrid_backend import _augment_table

ROOT = Path(__file__).resolve().parents[2]
OUTPUT = ROOT / "crates/core/tests/fixtures/filter_dynamic_infer.json"
# 与 hybrid_backend.evaluate_hybrid 的回退清单相同。
FALLBACK = (pa.ArrowInvalid, pa.ArrowTypeError, pa.ArrowNotImplementedError,
            TypeError, NotImplementedError, KeyError)
B63 = 2**63
ATOMS = [
    None, True, False, 0, 1, -1, 2**53, 2**53 + 1, -(2**53) - 1, B63 - 1, B63, -B63, -B63 - 1,
    2**64, 2**70, 1.5, 0.0, -0.0, 1e308, "a", "",
    [], [1], [1.5], ["a"], [True], [B63], [None], [1, "a"], ["a", B63], [B63, "a"],
    [1.5, True], [True, 1.5], [[1]], [{"k": 1}],
    {}, {"k": 1}, {"k": "a"}, {"k": B63}, {"k": True}, {"k": 1.5}, {"j": 1},
    {"b": 1, "a": 1}, {"b": B63, "a": "s"}, {"a": "s", "b": B63}, {"k": [1]}, {"k": {"j": B63}},
]
# 0033「字段顺序」表与「与现状的对比」表中的情形。
EXPLICIT = [
    [{"b": 1}, {"a": 1}, {"a": "s", "b": B63}],
    [{"b": 1, "a": 1}, {"b": B63, "a": "s"}],
    [{"b": 1}, {"a": "s", "b": B63}],
    [{"b": 1}, {"a": "x", "b": 2**64}],
    [{"a": 1}, {"a": "x", "b": 2**64}],
    [1.5, True], [1.5, 1, False], [1.5, 0.1, False, 0.1], [0.0, 2, 1.5, True],
    [2**64, "a"], [1, 2**64, "a"], ["a", 2**64], [1.5, 2**64],
    [1, True], [True, 1.5], [[2**64, "a"]], [[1.5], [True]], [{"k": True}, {"k": 1.5}],
    [None, None], [None, 1, None, 2**63 - 1],
    # 版本分歧的变体：列表里的对象、嵌套对象、第三个键与反向（先回退的字段排在后面）。
    [[{"b": 1}], [{"a": 1}], [{"a": "s", "b": B63}]],
    [{"k": {"b": 1}}, {"k": {"a": 1}}, {"k": {"b": B63, "a": "s"}}],
    [{"c": 1}, {"b": 1}, {"a": 1}, {"a": "s", "c": B63}],
    [{"b": 1}, {"a": "s"}, {"a": B63, "b": "t"}],
]


def random_value(rng, depth):
    c = rng.random()
    if depth < 3 and c < 0.15:
        return [random_value(rng, depth + 1) for _ in range(rng.randint(0, 3))]
    if depth < 3 and c < 0.3:
        keys = rng.sample(["a", "b", "c"], rng.randint(0, 3))  # 键序随机，不预先排序
        return {k: random_value(rng, depth + 1) for k in keys}
    return rng.choice([None, None, True, False, 0, 7, -3, 2**53, 2**53 + 1, B63 - 1, B63,
                       -B63, -B63 - 1, 2**64, 1.5, -2.0, "x", ""])


def cases():
    out = [[a] for a in ATOMS]
    out += [[a, b] for a in ATOMS for b in ATOMS]
    rng = random.Random(33)
    out += [[rng.choice(ATOMS) for _ in range(3)] for _ in range(2500)]
    out += [[random_value(rng, 0) for _ in range(rng.randint(1, 5))] for _ in range(2500)]
    out += EXPLICIT
    return out


SCHEMA = CollectionSchema(fields=[
    FieldSchema(name="id", dtype=DataType.INT64, is_primary=True),
    FieldSchema(name="vector", dtype=DataType.FLOAT_VECTOR, dim=2),
], enable_dynamic_field=True)


def outcome(values):
    metas = [separate_dynamic_fields({"id": i, "vector": [0.0, 0.0], "x": v}, SCHEMA)[1]
             for i, v in enumerate(values)]
    table = pa.table({"id": pa.array(range(len(values)), pa.int64()), "$meta": pa.array(metas, pa.string())})
    try:
        column = _augment_table(table, {"x"}).column("__meta__x").combine_chunks()
    except FALLBACK:
        return {"kind": "fallback"}
    except OverflowError as error:
        return {"kind": "error", "message": str(error)}
    t = column.type
    shape = ("null" if pa.types.is_null(t) else "bool" if pa.types.is_boolean(t) else
             "int" if t == pa.int64() else "float" if t == pa.float64() else
             "string" if pa.types.is_string(t) else
             "complex" if pa.types.is_list(t) or pa.types.is_struct(t) else None)
    assert shape, t
    result = {"kind": "ok", "shape": shape}
    if shape != "complex":
        result["values"] = column.to_pylist()
    return result


def results():
    return [outcome(values) for values in cases()]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--verify", action="store_true")
    parser.add_argument("--arrow15-python")
    parser.add_argument("--dump-results", action="store_true")
    args = parser.parse_args()
    assert milvus_lite.__version__ == "3.2.2"
    assert Path(milvus_lite.__file__).resolve().is_relative_to(ROOT / "references/milvus-lite")
    assert subprocess.check_output(["git", "-C", str(ROOT / "references/milvus-lite"), "rev-parse", "--short=7", "HEAD"], text=True).strip() == "1320d26"
    mine = results()
    if args.dump_results:
        print(json.dumps(mine, ensure_ascii=True, separators=(",", ":")))
        return
    if args.verify:
        payload = json.loads(OUTPUT.read_text())
        assert [case["values"] for case in payload["cases"]] == cases(), "corpus differs"
        if pa.__version__ == "25.0.1":
            expected = [case["arrow25"] for case in payload["cases"]]
        else:
            assert pa.__version__ == "15.0.0", "unverified Arrow version"
            expected = [case.get("arrow15", case["arrow25"]) for case in payload["cases"]]
        assert mine == expected, "dynamic inference truth differs; --verify does not overwrite"
    else:
        assert pa.__version__ == "25.0.1", "Rust follows the fixed Arrow 25.0.1 reference"
        assert args.arrow15_python, "regeneration must run both reference Arrow versions"
        arrow15 = json.loads(subprocess.check_output(
            [args.arrow15_python, str(Path(__file__).resolve()), "--dump-results"], text=True))
        lines = []
        for values, a25, a15 in zip(cases(), mine, arrow15):
            case = {"values": values, "arrow25": a25}
            if a15 != a25:
                case["arrow15"] = a15
            lines.append(json.dumps(case, ensure_ascii=True, separators=(",", ":")))
        OUTPUT.write_text('{"reference":"3.2.2+3 / 1320d26","cases":[\n' + ",\n".join(lines) + "\n]}\n")
        print(f"{sum('arrow15' in line for line in lines)} cases differ under Arrow 15")
    print(f"{len(mine)} dynamic inference cases verified with Arrow {pa.__version__}")


if __name__ == "__main__":
    main()
