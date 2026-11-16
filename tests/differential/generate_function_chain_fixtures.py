"""导出公共 function chain（0035）的组件真值：真实 3.x（`1320d26`）的解码、校验与执行，以及线格式编码的核对。

在参照环境（pymilvus 3.0、可编辑安装的参照实现、CPython 3.12）运行，写两个文件，格式都是
{"schemas": {名字: [[字段名, 3.x 类型名, 是否主键], ...]}, "cases": [...]}：

- crates/grpc/tests/fixtures/function_chain_validation.json：每一项是一条链的线格式字节（十六进制）、schema 与 nq，
  以及 3.x `function_chain_to_repr` + `validate_function_chain` 的结果（错误文案，或链读到的 schema 字段与是否用了
  `rerank_model`）。grpc 的单元测试把字节解码后走同一条路径。
- crates/core/tests/fixtures/function_chain_execution.json：每一项是一条链（3.x 解码出的表示）、各查询的输入行，
  以及 3.x 编译、执行后的输出行（`$id`、`$score` 与临时列的 Python `repr`），或运行期错误的文案。
  core 的单元测试用同样的输入执行本实现的链。

链用 [tests/python/chainwire.py](../python/chainwire.py) 编码，与端到端测试发出的字节相同。生成时逐条核对编码：
把字节解析成 pymilvus 3.0 的 `schema_pb2.FunctionChain` 后逐字段还原，必须与构造它的描述相同；SDK 能构造的链
（没有未设置的值、空名字与空键）另用 `FunctionChainOp` / `FunctionChainExpr` 的 `to_proto()` 构造，两者必须相等。
复制自参照的用例与 SDK 冒烟用 chainwire 里与 SDK 同名的 `FunctionChain` / `col` / `fn` 构造链：它们的每一种写法
（`copied_chains`）同时交给 `pymilvus.function_chain`，两边的结果必须相等。这些核对不写进文件。

值的写法：整数与字符串、布尔照原样；浮点数写成 {"f": 位模式十六进制}；TIMESTAMPTZ 写成 {"t": UTC 微秒}；
参数里的数组、对象与 bytes 写成 {"a": [...]}、{"o": {...}}、{"y": 十六进制}。

--verify 只比较，不改写。
"""
from __future__ import annotations

import json
import math
import random
import struct
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tests/python"))

import chainwire as w  # noqa: E402
from chainwire import Col  # noqa: E402
from milvus_lite.adapter.grpc.translators.function_chain import function_chain_to_repr  # noqa: E402
from milvus_lite.exceptions import SchemaValidationError  # noqa: E402
from milvus_lite.function.compiler import compile_function_chain  # noqa: E402
from milvus_lite.function.dataframe import DataFrame  # noqa: E402
from milvus_lite.function.repr import ColumnArg  # noqa: E402
from milvus_lite.function.validator import validate_function_chain  # noqa: E402
from milvus_lite.schema.types import CollectionSchema, DataType, FieldSchema  # noqa: E402
from pymilvus.exceptions import ParamError  # noqa: E402
from pymilvus.function_chain.chain import FunctionChainExpr, FunctionChainOp, col  # noqa: E402
from pymilvus.grpc_gen import schema_pb2  # noqa: E402

assert sys.version_info[:2] == (3, 12), sys.version
VALIDATION = ROOT / "crates/grpc/tests/fixtures/function_chain_validation.json"
EXECUTION = ROOT / "crates/core/tests/fixtures/function_chain_execution.json"


# ---- 链的描述与编码 ----

class Expr:
    def __init__(self, name, *args, **params):
        self.name, self.args, self.params = name, args, params


class Op:
    def __init__(self, name, *, expr=None, inputs=(), outputs=(), **params):
        self.name, self.expr, self.inputs, self.outputs, self.params = name, expr, tuple(inputs), tuple(outputs), params


class Chain:
    def __init__(self, *ops, stage=w.L2_RERANK, name=""):
        self.ops, self.stage, self.name = ops, stage, name


def expr(name, *args, **params):
    return Expr(name, *args, **params)


def op(name, **kwargs):
    return Op(name, **kwargs)


def chain(*ops, **kwargs):
    return Chain(*ops, **kwargs)


def nc(*args, **params):
    return expr("num_combine", *args, **params)


def decay(*args, **params):
    return expr("decay", *args, **params)


def rnd(arg, decimal):
    return expr("round_decimal", arg, decimal=decimal)


def rerank(*args, **params):
    return expr("rerank_model", *args, **params)


def mapped(output, e):
    return op("map", expr=e, outputs=[output])


def sort(column, **params):
    inputs = [column] + [params[k] for k in ("tie_break_col",) if k in params]
    return op("sort", inputs=inputs, column=column, **params)


def limit(n, **params):
    return op("limit", limit=n, **params)


def wire(c):
    def encode_expr(e):
        return w.expr(e.name, *e.args, **e.params)

    ops = [
        w.op(o.name, expr=None if o.expr is None else encode_expr(o.expr), inputs=o.inputs, outputs=o.outputs, **o.params)
        for o in c.ops
    ]
    return w.chain(*ops, stage=c.stage, name=c.name)


def norm(v):
    """可比较的形式：区分 bool / int、浮点按位模式、`Col` 与字符串字面量、数组与对象。"""
    if v is None:
        return None
    if isinstance(v, Col):
        return ("col", str(v))
    if isinstance(v, bool):
        return ("b", v)
    if isinstance(v, int):
        return ("i", v)
    if isinstance(v, float):
        return ("f", struct.pack("<d", v).hex())
    if isinstance(v, str):
        return ("s", v)
    if isinstance(v, bytes):
        return ("y", v.hex())
    if isinstance(v, (list, tuple)):
        return ("a", [norm(x) for x in v])
    if isinstance(v, dict):
        return ("o", sorted((k, norm(x)) for k, x in v.items()))
    raise TypeError(type(v))


def described(c):
    return (c.name, c.stage, [
        (o.name, None if o.expr is None else (o.expr.name, [norm(a) for a in o.expr.args], norm(o.expr.params)),
         list(o.inputs), list(o.outputs), norm(o.params))
        for o in c.ops
    ])


def from_pb_value(v):
    kind = v.WhichOneof("value")
    if kind is None:
        return None
    if kind == "array_value":
        return [from_pb_value(x) for x in v.array_value.values]
    if kind == "object_value":
        return {k: from_pb_value(x) for k, x in v.object_value.fields.items()}
    return getattr(v, kind)


def from_pb(pb):
    def arg(a):
        assert a.WhichOneof("arg") in ("column", "literal")
        return Col(a.column.name) if a.WhichOneof("arg") == "column" else from_pb_value(a.literal)

    ops = []
    for o in pb.ops:
        e = None
        if o.HasField("expr"):
            e = Expr(o.expr.name, *[arg(a) for a in o.expr.args], **{k: from_pb_value(v) for k, v in o.expr.params.items()})
        ops.append(Op(o.op, expr=e, inputs=list(o.inputs), outputs=list(o.outputs),
                      **{k: from_pb_value(v) for k, v in o.params.items()}))
    return Chain(*ops, stage=pb.stage, name=pb.name)


def sdk(c):
    """pymilvus 3.0 构造同一条链；构造不出（未设置的值、空名字、空键）时返回 None。"""
    try:
        ops = []
        for o in c.ops:
            e = None
            if o.expr is not None:
                args = tuple(col(str(a)) if isinstance(a, Col) else a for a in o.expr.args)
                e = FunctionChainExpr(o.expr.name, args=args, params=dict(o.expr.params))
            ops.append(FunctionChainOp(op=o.name, expr=e, inputs=o.inputs, outputs=o.outputs, params=dict(o.params)).to_proto())
    except ParamError:
        return None
    return schema_pb2.FunctionChain(name=c.name, stage=c.stage, ops=ops)


SDK_CHECKED = 0


def parsed(c):
    """链的字节，以及 3.x 一侧解析出的消息；逐字段核对编码。"""
    global SDK_CHECKED
    data = wire(c)
    pb = schema_pb2.FunctionChain.FromString(data)
    assert described(from_pb(pb)) == described(c), described(c)
    expected = sdk(c)
    if expected is not None:
        assert expected == pb, described(c)
        SDK_CHECKED += 1
    return data, pb


# ---- schema ----

FIELD_TYPES = {
    "id": DataType.INT64, "i8": DataType.INT8, "i16": DataType.INT16, "i32": DataType.INT32, "i64": DataType.INT64,
    "f": DataType.FLOAT, "d": DataType.DOUBLE, "b": DataType.BOOL, "s": DataType.VARCHAR, "tz": DataType.TIMESTAMPTZ,
    "j": DataType.JSON, "arr": DataType.ARRAY, "geo": DataType.GEOMETRY, "vector": DataType.FLOAT_VECTOR,
    "sv": DataType.SPARSE_FLOAT_VECTOR,
}


def make_schema(pk, pk_type, names):
    fields = [(pk, pk_type, True)] + [(n, FIELD_TYPES[n], False) for n in names]
    return fields, CollectionSchema(
        fields=[FieldSchema(name=n, dtype=t, is_primary=p) for n, t, p in fields], enable_dynamic_field=True)


ALL = ["i8", "i16", "i32", "i64", "f", "d", "b", "s", "tz", "j", "arr", "geo", "vector", "sv"]
SCALARS = ["i8", "i16", "i32", "i64", "f", "d", "b", "s", "tz"]
SCHEMAS = {
    "main": make_schema("id", DataType.INT64, ALL),
    "varchar_pk": make_schema("pk", DataType.VARCHAR, ALL),
    "score_pk": make_schema("$score", DataType.INT64, ["i64"]),
    "id_pk": make_schema("$id", DataType.VARCHAR, ["i64"]),
}


def schema_json(key):
    return [[n, t.name, p] for n, t, p in SCHEMAS[key][0]]


# ---- 值的写法 ----

def tagged(v):
    if v is None or isinstance(v, (bool, str)):
        return v
    if isinstance(v, int):
        assert -(1 << 63) <= v < 1 << 63
        return v
    if isinstance(v, float):
        return {"f": struct.pack(">d", v).hex()}
    if isinstance(v, datetime):
        delta = v - EPOCH
        return {"t": (delta.days * 86400 + delta.seconds) * 10**6 + delta.microseconds}
    if isinstance(v, bytes):
        return {"y": v.hex()}
    if isinstance(v, (list, tuple)):
        return {"a": [tagged(x) for x in v]}
    if isinstance(v, dict) or hasattr(v, "items"):
        return {"o": {k: tagged(v[k]) for k in sorted(v)}}
    raise TypeError(type(v))


EPOCH = datetime(1970, 1, 1, tzinfo=timezone.utc)


def repr_json(rep):
    """3.x 解码出的链表示；参数按键排序（本实现的解码同样排序）。"""
    def params(p):
        return {k: tagged(p[k]) for k in sorted(p)}

    return {
        "stage": rep.stage,
        "ops": [
            {
                "op": o.op,
                "expr": None if o.expr is None else {
                    "name": o.expr.name,
                    "args": [{"column": a.name} if isinstance(a, ColumnArg) else {"literal": tagged(a.value)}
                             for a in o.expr.args],
                    "params": params(o.expr.params),
                },
                "inputs": list(o.inputs),
                "outputs": list(o.outputs),
                "params": params(o.params),
            }
            for o in rep.ops
        ],
    }


# ---- 校验 ----

def validation_case(c, schema="main", nq=1):
    data, pb = parsed(c)
    try:
        rep = function_chain_to_repr(pb)
        validated = validate_function_chain(rep, SCHEMAS[schema][1], nq)
    except SchemaValidationError as error:
        result = {"error": str(error)}
    else:
        result = {"required": list(validated.required_schema_fields),
                  "rerank_model": any(o.expr is not None and o.expr.name == "rerank_model" for o in rep.ops)}
    return {"schema": schema, "nq": nq, "wire": data.hex(), **result}


S = Col("$score")
ID = Col("$id")


def validation_cases():
    ok = mapped("$score", nc(S, Col("i64")))
    cases = []

    def add(c, **kwargs):
        cases.append(validation_case(c, **kwargs))

    # 解码：空名字、未设置的值、对象的空键；所有算子解码完才查阶段，后面算子的解码错误先于前面算子的校验错误。
    add(chain(op("")))
    add(chain(mapped("x", expr("", Col("i64")))))
    add(chain(mapped("x", nc(Col("i64"), Col("")))))
    add(chain(mapped("x", nc(Col("i64"), None))))
    add(chain(mapped("x", nc(Col("i64"), [1, None]))))
    add(chain(mapped("x", nc(Col("i64"), {"k": None}))))
    add(chain(mapped("x", nc(Col("i64"), {"": 1}))))
    add(chain(mapped("x", nc(Col("i64"), Col("i64"), mode=None))))
    add(chain(mapped("x", nc(Col("i64"), Col("i64"), weights=[1.0, {"a": [None]}]))))
    add(chain(mapped("x", nc(Col("i64"), Col("i64"), weights={"": 1}))))
    add(chain(op("limit", limit=None)))
    add(chain(op("limit", limit=[[None]])))
    add(chain(op("sort", inputs=["i64", ""], column="i64")))
    add(chain(op("map", expr=nc(S, Col("i64")), outputs=[""])))
    add(chain(op("map", expr=nc(S, Col("i64")), outputs=["a", ""])))
    add(chain(ok, op(""), stage=99))
    add(chain(mapped("$id", nc(S)), op("limit", limit=None)))
    add(chain(mapped("$id", nc(S)), mapped("y", expr(""))))
    for stage in (99, -1, 7, -(1 << 31)):
        add(chain(ok, stage=stage))
    add(chain(stage=99))

    # 链：阶段、空链、主键名、算子名（大小写敏感）
    for stage in (0, 1, 2, 3, 4, 6):
        add(chain(ok, stage=stage))
    add(chain(stage=3))
    add(chain())
    add(chain(ok, name="named"))
    for name in ("filter", "Map", "MAP", "merge", "select", "group_by", " map"):
        add(chain(ok, op(name)))
    add(chain(limit(1)), schema="score_pk")
    add(chain(limit(1)), schema="id_pk")
    add(chain(), schema="id_pk")
    add(chain(ok, stage=3), schema="score_pk")

    # map 的形状与输出名
    add(chain(op("map", outputs=["$score"])))
    add(chain(op("map", expr=nc(S, Col("i64")), inputs=["i64"], outputs=["$score"])))
    add(chain(op("map", expr=nc(S, Col("i64")))))
    add(chain(op("map", expr=nc(S, Col("i64")), outputs=["a", "b"])))
    add(chain(op("map", expr=nc(S, Col("i64")), outputs=["a"], extra=1)))
    for out in ("$id", "$other", "$", "id", "i64", "vector", "dyn", "tmp", "$score", "score", "pk"):
        add(chain(mapped(out, nc(S, Col("i64")))))
    add(chain(mapped("pk", nc(S, Col("i64")))), schema="varchar_pk")
    add(chain(mapped("id", nc(S, Col("i64")))), schema="varchar_pk")
    add(chain(mapped("t", nc(S, Col("i64"))), mapped("t", nc(Col("t"), Col("d"))), mapped("$score", nc(Col("t"), S))))
    add(chain(mapped("t", nc(S, Col("t")))))
    add(chain(sort("t"), mapped("t", nc(S, Col("i64")))))
    add(chain(mapped("t", nc(S, Col("i64"))), sort("t")))
    for name in ("xgboost", "Num_combine", "NUM_COMBINE", "bm25", "score_combine", "embedding", "filter"):
        add(chain(mapped("$score", expr(name, S))))

    # 输入名
    for name in ("$seg_offset", "$pk", "$", "dyn", "nope", "j", "arr", "geo", "vector", "sv", "$score", "$id", "id",
                 "i8", "i16", "i32", "f", "d", "tz"):
        add(chain(mapped("$score", nc(S, Col(name)))))
    for name in ("b", "s"):
        add(chain(mapped("$score", nc(S, Col(name)))))
    add(chain(mapped("$score", nc(Col("$id"), Col("pk")))), schema="varchar_pk")
    add(chain(mapped("$score", nc(S, Col("id")))), schema="varchar_pk")
    add(chain(sort("$id"), sort("pk"), mapped("$score", nc(S, Col("i64")))), schema="varchar_pk")

    # 字面量的种类
    for literal in ([1], {"k": 1}, b"x", [], {}, "s", True, 1, -1, 1.5, float("nan"), float("inf"), -(1 << 63)):
        add(chain(mapped("$score", nc(S, literal))))
    add(chain(mapped("$score", nc(1, 2))))
    add(chain(mapped("$score", nc(1, 2.5, mode="max"))))
    add(chain(mapped("$score", decay(1.5, function="gauss", origin=0, scale=1))))
    add(chain(mapped("$score", decay("2024", function="gauss", origin=0, scale=1))))
    add(chain(mapped("$score", rnd(2.345, 1))))
    add(chain(mapped("$score", rerank("doc", provider="cohere", queries=["q"]))))

    # num_combine
    for args, params in [
        ((S,), {}), ((), {}), ((S, Col("s")), {}), ((S, Col("b")), {}), ((S, Col("tz")), {}), ((S, True), {}),
        ((S, Col("i64")), {"mode": "sum"}), ((S, Col("i64")), {"mode": "Sum"}), ((S, Col("i64")), {"mode": "mean"}),
        ((S, Col("i64")), {"mode": 1}), ((S, Col("i64")), {"mode": ""}), ((S, Col("i64")), {"mode": ["sum"]}),
        ((S, Col("i64")), {"mode": "weighted"}), ((S, Col("i64")), {"mode": "weighted", "weights": [1]}),
        ((S, Col("i64")), {"mode": "weighted", "weights": [1, 2, 3]}),
        ((S, Col("i64")), {"mode": "weighted", "weights": [1, float("nan")]}),
        ((S, Col("i64")), {"mode": "weighted", "weights": [1, float("inf")]}),
        ((S, Col("i64")), {"mode": "weighted", "weights": [1, True]}),
        ((S, Col("i64")), {"mode": "weighted", "weights": [1, "2"]}),
        ((S, Col("i64")), {"mode": "weighted", "weights": "12"}),
        ((S, Col("i64")), {"mode": "weighted", "weights": {"a": 1, "b": 2}}),
        ((S, Col("i64")), {"mode": "weighted", "weights": []}),
        ((S, Col("i64")), {"mode": "weighted", "weights": 1}),
        ((S, Col("i64")), {"mode": "weighted", "weights": [-1, 0.5]}),
        ((S, Col("i64")), {"mode": "weighted", "weights": [[1], 2]}),
        ((S, Col("i64")), {"mode": "sum", "weights": [1, 2]}),
        ((S, Col("i64")), {"weights": [1, 2]}),
        ((S, Col("i64")), {"mode": "max", "weights": []}),
        ((S, Col("i64")), {"mode": "avg", "extra": "ignored", "Mode": 3}),
        ((S, Col("i64"), Col("f"), Col("d"), 2), {"mode": "multiply"}),
        ((S, float("nan")), {"mode": "min"}),
        ((Col("s"), Col("b"), Col("i64")), {}),
    ]:
        add(chain(mapped("$score", nc(*args, **params))))

    # decay
    base = {"function": "gauss", "origin": 0, "scale": 1}
    for args, params in [
        ((Col("i64"),), base), ((), base), ((Col("i64"), Col("d")), base), ((Col("s"),), base), ((Col("b"),), base),
        ((Col("tz"),), base), ((S,), base), ((Col("i64"),), {**base, "function": "exp"}),
        ((Col("i64"),), {**base, "function": "linear"}), ((Col("i64"),), {**base, "function": "Gauss"}),
        ((Col("i64"),), {**base, "function": "sigmoid"}), ((Col("i64"),), {**base, "function": 1}),
        ((Col("i64"),), {"origin": 0, "scale": 1}), ((Col("i64"),), {"function": "gauss", "scale": 1}),
        ((Col("i64"),), {"function": "gauss", "origin": 0}), ((Col("i64"),), {**base, "origin": True}),
        ((Col("i64"),), {**base, "origin": "0"}), ((Col("i64"),), {**base, "origin": float("nan")}),
        ((Col("i64"),), {**base, "origin": float("-inf")}), ((Col("i64"),), {**base, "scale": 0}),
        ((Col("i64"),), {**base, "scale": -1.5}), ((Col("i64"),), {**base, "scale": float("inf")}),
        ((Col("i64"),), {**base, "offset": -1}), ((Col("i64"),), {**base, "offset": 0}),
        ((Col("i64"),), {**base, "offset": float("nan")}), ((Col("i64"),), {**base, "decay": 0}),
        ((Col("i64"),), {**base, "decay": 1}), ((Col("i64"),), {**base, "decay": 1.5}),
        ((Col("i64"),), {**base, "decay": "0.5"}), ((Col("i64"),), {**base, "decay": False}),
        ((Col("i64"),), {**base, "decay": 0.999, "offset": 3, "scale": 2.5, "origin": -7, "extra": [1]}),
        ((Col("i64"),), {**base, "scale": [1]}), ((Col("i64"),), {**base, "origin": 1 << 62}),
    ]:
        add(chain(mapped("$score", decay(*args, **params))))

    # round_decimal
    for args, params in [
        ((S,), {"decimal": 2}), ((), {"decimal": 2}), ((S, S), {"decimal": 2}), ((Col("s"),), {"decimal": 2}),
        ((Col("tz"),), {"decimal": 2}), ((Col("b"),), {"decimal": 2}), ((S,), {}), ((S,), {"decimal": 7}),
        ((S,), {"decimal": -1}), ((S,), {"decimal": 0}), ((S,), {"decimal": 6}), ((S,), {"decimal": True}),
        ((S,), {"decimal": 2.0}), ((S,), {"decimal": "2"}), ((S,), {"decimal": 2, "extra": {"k": []}}),
    ]:
        add(chain(mapped("$score", expr("round_decimal", *args, **params))))

    # rerank_model
    good = {"provider": "cohere", "queries": ["q"]}
    for args, params, nq in [
        ((Col("s"),), good, 1), ((Col("s"),), {**good, "queries": ["q", "r"]}, 2), ((), good, 1),
        ((Col("s"), Col("s")), good, 1), ((Col("i64"),), good, 1), ((Col("tz"),), good, 1), ((Col("b"),), good, 1),
        ((Col("s"),), {**good, "base_url": "x"}, 1), ((Col("s"),), {**good, "API_KEY": "x"}, 1),
        ((Col("s"),), {**good, "Token": 1}, 1), ((Col("s"),), {**good, "secret": "x"}, 1),
        ((Col("s"),), {"provider": "cohere"}, 1), ((Col("s"),), {**good, "queries": []}, 1),
        ((Col("s"),), {**good, "queries": "q"}, 1), ((Col("s"),), {**good, "queries": ["q", ""]}, 2),
        ((Col("s"),), {**good, "queries": ["q", 1]}, 2), ((Col("s"),), {**good, "queries": {"q": "r"}}, 1),
        ((Col("s"),), {**good, "queries": ["q"]}, 2), ((Col("s"),), {**good, "queries": ["q", "r"]}, 1),
        ((Col("s"),), {"queries": ["q"]}, 1), ((Col("s"),), {**good, "provider": "voyageai"}, 1),
        ((Col("s"),), {**good, "provider": "COHERE"}, 1), ((Col("s"),), {**good, "provider": 1}, 1),
        ((Col("s"),), {**good, "provider": ""}, 1), ((Col("s"),), {**good, "model_name": "m"}, 1),
        ((Col("s"),), {**good, "model_name": 1}, 1), ((Col("s"),), {**good, "zz": 1, "aa": 2}, 1),
        ((Col("s"),), {**good, "Zz": 1, "aa": 2, "_x": 0}, 1), ((Col("s"),), {**good, "top_k": 3}, 1),
        ((Col("s"),), {**good, "max_tokens_per_doc": 3}, 1),
        ((Col("s"),), {**good, "zz": 1, "base_url": "x"}, 1), ((Col("s"),), {**good, "zz": 1, "provider": "x"}, 1),
        ((Col("s"),), {**good, "zz": 1, "queries": []}, 1), ((Col("s"),), {**good, "zz": 1, "model_name": 2}, 1),
        ((Col("pk"),), good, 1), ((Col("$id"),), good, 1),
    ]:
        add(chain(mapped("$score", rerank(*args, **params))), nq=nq,
            schema="varchar_pk" if args and args[0] in ("pk", "$id") else "main")

    # sort
    for o in [
        op("sort", expr=nc(S, S), inputs=["$score"], column="$score"),
        op("sort", inputs=["$score"], outputs=["x"], column="$score"),
        op("sort", inputs=["$score"]), op("sort", inputs=[""], column=""), op("sort", inputs=["$score"], column=1),
        op("sort", inputs=["$score"], column="$score", desc=1), op("sort", inputs=["$score"], column="$score", desc="no"),
        op("sort", inputs=["$score", ""], column="$score", tie_break_col=""),
        op("sort", inputs=["$score", "1"], column="$score", tie_break_col=1),
        op("sort", inputs=[], column="$score"), op("sort", inputs=["$score", "$id"], column="$score"),
        op("sort", inputs=["$id", "$score"], column="$score", tie_break_col="$id"),
        op("sort", inputs=["$score"], column="$score", tie_break_col="$id"),
        op("sort", inputs=["$score", "$score"], column="$score", tie_break_col="$score"),
        op("sort", inputs=["$score", "$id", "x"], column="$score", tie_break_col="$id"),
        op("sort", inputs=["$score"], column="$score", desc=False, extra=1),
        sort("j"), sort("vector"), sort("nope"), sort("$seg_offset"), sort("$score", tie_break_col="j"),
        sort("$score", tie_break_col="dyn"), sort("b"), sort("s", desc=False), sort("tz", tie_break_col="i8"),
        sort("id", tie_break_col="f"), sort("$id", tie_break_col="d"),
    ]:
        add(chain(o))

    # limit
    for params in [{}, {"limit": 0}, {"limit": -1}, {"limit": True}, {"limit": 1.0}, {"limit": "1"}, {"limit": 1},
                   {"limit": 1, "offset": -1}, {"limit": 1, "offset": True}, {"limit": 1, "offset": 1.0},
                   {"limit": 1, "offset": 0}, {"limit": 1 << 62, "offset": 1 << 62}, {"limit": 1, "extra": "x"},
                   {"offset": 1}, {"limit": [1]}]:
        add(chain(op("limit", **params)))
    add(chain(op("limit", expr=nc(S, S), limit=1)))
    add(chain(op("limit", inputs=["$score"], limit=1)))
    add(chain(op("limit", outputs=["x"], limit=1)))

    # 通过校验的链：读到的 schema 字段按首次出现的顺序，去重，不含 `$score`、`$id`、主键名与临时列。
    add(chain(mapped("t", nc(Col("d"), Col("i8"), S)), mapped("u", decay(Col("tz"), function="exp", origin=0, scale=1)),
              sort("s", tie_break_col="b"), mapped("$score", nc(Col("t"), Col("u"), Col("i8"), Col("f"), Col("id"))),
              mapped("$score", rnd(S, 3)), sort("$score", desc=False, tie_break_col="$id"), limit(2, offset=1)))
    add(chain(sort("i16"), sort("i16", tie_break_col="i32"), limit(5)))
    add(chain(sort("pk", tie_break_col="s"), sort("$id")), schema="varchar_pk")
    add(chain(mapped("$score", rerank(Col("s"), provider="cohere", queries=["a", "b"])), sort("i64")), nq=2)
    return cases


# ---- 执行 ----

INT_RANGES = {"i8": 8, "i16": 16, "i32": 32, "i64": 64}


def f32(x):
    return struct.unpack("<f", struct.pack("<f", x))[0]


def random_value(rng, name, pool=None, specials=False, none=0.15):
    if rng.random() < none:
        return None
    if pool is not None:
        return rng.choice(pool)
    if name in INT_RANGES:
        bits = INT_RANGES[name]
        lo, hi = -(1 << (bits - 1)), (1 << (bits - 1)) - 1
        return rng.choice([lo, hi, 0, -1, 1, rng.randint(lo, hi), rng.randint(-1000, 1000)])
    if name in ("f", "d", "$score"):
        choices = [rng.uniform(-10, 10), rng.uniform(-1e6, 1e6), rng.random(), -0.0, 0.0,
                   math.ldexp(rng.uniform(1, 2), rng.randint(-60, 60))]
        if name == "d":
            choices += [1e300, -1e300, 5e-324]
        if specials:
            choices += [math.nan, math.inf, -math.inf]
        value = rng.choice(choices)
        return f32(value) if name == "f" else value
    if name == "b":
        return rng.random() < 0.5
    if name == "s":
        return rng.choice(["", "a", "b", "ab", "B", "é", "z" * 3, "10", "9"])
    if name == "tz":
        micros = rng.choice([0, -1, 1, 1_700_000_000_123_456, -62_135_596_800_000_000, 253_402_300_799_999_999,
                             rng.randint(-62_135_596_800_000_000, 253_402_300_799_999_999),
                             rng.randint(1_600_000_000, 1_800_000_000) * 10**6 + rng.randint(0, 999_999)])
        return EPOCH + timedelta(microseconds=micros)
    raise KeyError(name)


def execution_case(c, chunks, schema="exec", label=""):
    """`chunks` 是每个查询的行：(id, 分数, {字段: 值})。"""
    data, pb = parsed(c)
    fields, collection = SCHEMAS[schema]
    pk = fields[0][0]
    rep = function_chain_to_repr(pb)
    validated = validate_function_chain(rep, collection, len(chunks))
    required = list(validated.required_schema_fields)
    rows = []
    frames = []
    for chunk in chunks:
        frame = []
        encoded = []
        for hit_id, score, values in chunk:
            row = {name: values.get(name) for name in required}
            row["$id"] = hit_id
            row[pk] = hit_id
            row["$score"] = score
            frame.append(row)
            encoded.append([tagged(hit_id), tagged(score), {name: tagged(values.get(name)) for name in required}])
        frames.append(frame)
        rows.append(encoded)
    temps = list(dict.fromkeys(o.outputs[0] for o in rep.ops if o.op == "map" and o.outputs[0] != "$score"))
    try:
        output = compile_function_chain(validated).execute(DataFrame(frames))
    except (ValueError, ArithmeticError) as error:
        result = {"error": str(error)}
    else:
        result = {"output": [[[repr(r["$id"]), repr(r["$score"])] + [repr(r[t]) for t in temps]
                              for r in output.chunk(i)] for i in range(output.num_chunks)]}
    return {"label": label, "schema": schema, "chain": repr_json(rep), "temps": temps, "rows": rows, **result}


SCHEMAS["exec"] = make_schema("id", DataType.INT64, SCALARS)
SCHEMAS["exec_varchar"] = make_schema("pk", DataType.VARCHAR, SCALARS)
NUMERIC = ["i8", "i16", "i32", "i64", "f", "d"]


def random_rows(rng, count, names, *, start=0, specials=False, none=0.15, pools=None, score_pool=None, ids=None):
    pools = pools or {}
    rows = []
    for k in range(count):
        hit_id = ids[k] if ids is not None else start + k
        if score_pool is not None:
            score = rng.choice(score_pool)
        else:
            score = random_value(rng, "$score", specials=specials, none=0)
        rows.append((hit_id, score, {n: random_value(rng, n, pools.get(n), specials, none) for n in names}))
    return rows


def execution_cases():
    rng = random.Random(35)
    cases = []

    def add(c, chunks, **kwargs):
        cases.append(execution_case(c, chunks, **kwargs))

    def clean(count=12, chunks=3, names=SCALARS, **kwargs):
        return [random_rows(rng, count, names, start=100 * q, **kwargs) for q in range(chunks)]

    modes = [{"mode": m} for m in ("sum", "multiply", "max", "min", "avg")]
    arg_sets = [
        (S, Col("i64")), (Col("i8"), Col("i16"), Col("i32")), (Col("f"), Col("d"), S), (Col("i64"), Col("i64")),
        (ID, Col("i64"), 7), (Col("d"), -2.5), (Col("i64"), Col("i64"), Col("i64"), Col("d")), (Col("f"), 1e308),
        (Col("i32"), Col("i32"), 0.1, -(1 << 63)),
    ]
    # num_combine：六种模式，随机输入（含空值），再逐个代入边界值。
    for params in modes + [{"mode": "weighted"}]:
        for args in arg_sets:
            p = dict(params)
            if p["mode"] == "weighted":
                p["weights"] = [rng.choice([0.5, -1, 2, 1e-3, 3.25, 1e308]) for _ in args]
            c = chain(mapped("$score", nc(*args, **p)), mapped("t", nc(Col("$score"), ID, mode=p["mode"],
                                                                         **({"weights": [1, 1]} if "weights" in p else {}))))
            add(c, clean(), label=f"num_combine {p['mode']}")
    for params in modes + [{"mode": "weighted", "weights": [1.5, -2]}]:
        label = f"num_combine {params['mode']} edge"
        for value in (math.nan, math.inf, -math.inf, 1e308, -1e308, 5e-324, -0.0, None):
            add(chain(mapped("$score", nc(Col("d"), Col("d"), **params))), [[(1, 0.5, {"d": value})]], label=label)
            if value is not None:
                add(chain(mapped("$score", nc(S, Col("d"), **params))), [[(1, value, {"d": 1e308})]], label=label)
        for value in ((1 << 63) - 1, -(1 << 63), None):
            add(chain(mapped("$score", nc(Col("i64"), Col("i64"), **params))), [[(1, 0.5, {"i64": value})]],
                label=label)

    # 超过 int64 的整数：每一步把同一个临时列加 3 次，到 3^605 * 2^64（约 8.4e307），再加两次仍能转成浮点，加三次就不能。
    grow = [mapped("t", nc(Col("i64"), Col("i64")))]
    grow += [mapped("t", nc(Col("t"), Col("t"), Col("t"))) for _ in range(605)]
    big_rows = [[(1, 0.5, {"i64": (1 << 63) - 1, "d": -1.5}), (2, 0.25, {"i64": -(1 << 63), "d": 3.0}),
                 (3, 0.75, {"i64": 1, "d": 0.0}), (4, 0.5, {"i64": None, "d": 1.0})]]
    add(chain(*grow, mapped("u", nc(Col("t"), Col("d"), mode="avg")), mapped("v", nc(Col("t"), Col("d"), mode="max")),
              mapped("w", nc(Col("t"), Col("d"), mode="min")), mapped("x", nc(Col("t"), Col("d"), mode="weighted",
                                                                                 weights=[1e-300, 1])),
              mapped("y", decay(Col("t"), function="exp", origin=0, scale=1)), mapped("$score", rnd(Col("w"), 2)),
              sort("t", tie_break_col="$id")), big_rows, label="big integers")
    add(chain(*grow, mapped("t", nc(Col("t"), Col("t")))), big_rows, label="big integer doubled")
    add(chain(*grow, mapped("t", nc(Col("t"), Col("t"), Col("t")))), big_rows, label="big integer overflow")
    add(chain(*grow, mapped("u", nc(Col("t"), Col("d"), mode="multiply"))), big_rows, label="big integer multiply")
    # fsum 的抵消与溢出
    for values in ([1e308, 1e308, -1e308], [1e16, 1.0, -1e16], [0.1, 0.2, 0.3], [1.7976931348623157e308] * 3,
                   [5e-324, 5e-324, -5e-324], [2.0**53, 1.0, 1.0]):
        args = [Col("d")] + list(values)
        add(chain(mapped("$score", nc(*args, mode="avg"))), [[(1, 0.0, {"d": values[0]})]], label="avg fsum")
        add(chain(mapped("$score", nc(*args, mode="sum"))), [[(1, 0.0, {"d": values[0]})]], label="sum")

    # decay：三种函数、各种参数（含整数 scale 的精确平方）与输入列。
    params_sets = [
        {"origin": 0, "scale": 1}, {"origin": 5, "scale": 2.5, "offset": 1, "decay": 0.25},
        {"origin": -1e6, "scale": 3037000500, "offset": 0.5, "decay": 0.9}, {"origin": 1.7e9, "scale": 86400, "decay": 0.1},
        {"origin": 0.0, "scale": 1e-300}, {"origin": 1e300, "scale": 1e300, "offset": 1e300, "decay": 1e-300},
        {"origin": 100, "scale": 94906267, "offset": 3}, {"origin": 0, "scale": 1 << 62},
    ]
    for function in ("gauss", "exp", "linear"):
        for params in params_sets:
            for column in ("i64", "d", "f", "$score", "tz", "i8", "id"):
                c = chain(mapped("t", decay(Col(column), function=function, **params)), sort("t", tie_break_col="$id"))
                add(c, clean(8, 2, specials=column in ("d", "f", "$score")), label=f"decay {function} {column}")

    # round_decimal：0–6 位，浮点、整数、空值与非有限值。
    for decimal in range(7):
        for column in ("$score", "d", "f", "i64", "i8"):
            c = chain(mapped("r", rnd(Col(column), decimal)), mapped("$score", rnd(S, decimal)))
            add(c, clean(14, 2, specials=True), label=f"round_decimal {decimal} {column}")
    halves = [0.5, 1.5, 2.5, -0.5, 0.125, 0.375, 2.675, 1.005, 1e22, 123456.7890125, 5e-324, 1e-7, 0.45, 1e300]
    for decimal in range(7):
        add(chain(mapped("$score", rnd(S, decimal))), [[(k, v, {}) for k, v in enumerate(halves)]],
            label=f"round_decimal halves {decimal}")

    # sort：各种列种类，空值与 NaN，平局（从小集合取值）、升降序、平局列与稳定性。
    pools = {
        "i8": [-1, 0, 1, 127], "i16": [3, 3, 4], "i32": [0, 1 << 30], "i64": [5, -5, (1 << 63) - 1],
        "f": [f32(0.1), 0.5, -0.0, 0.0, math.nan, math.inf], "d": [0.1, 0.5, -0.0, 0.0, math.nan, -math.inf, 1e300],
        "b": [True, False], "s": ["", "a", "B", "ab", "é"],
        "tz": [EPOCH, EPOCH + timedelta(microseconds=1), EPOCH - timedelta(days=1)],
    }
    score_pool = [0.5, 0.25, 0.5, math.nan, -0.0, 0.0, math.inf]
    columns = SCALARS + ["$score", "$id", "id"]
    for column in columns:
        for desc in (True, False):
            for tie in (None, "$id", "i8", "d", "s", "tz", "b"):
                if tie == column:
                    continue
                params = {"desc": desc}
                if tie is not None:
                    params["tie_break_col"] = tie
                ids = rng.sample(range(12), 12)
                chunks = [random_rows(rng, 12, SCALARS, pools=pools, score_pool=score_pool, ids=ids), [],
                          random_rows(rng, 5, SCALARS, pools=pools, score_pool=score_pool)]
                add(chain(sort(column, **params)), chunks, label=f"sort {column} {desc} {tie}")
    # 字符串主键：`$id`、主键名与 VARCHAR 列。
    for column, tie in (("$id", None), ("pk", "$score"), ("$score", "pk"), ("s", "$id")):
        params = {} if tie is None else {"tie_break_col": tie}
        ids = ["b", "a", "", "é", "ab", "B", "10", "9"]
        add(chain(sort(column, desc=False, **params), limit(5, offset=1)),
            [random_rows(rng, 8, SCALARS, pools=pools, score_pool=score_pool, ids=ids)], schema="exec_varchar",
            label=f"sort varchar {column}")
    # 混合整数与浮点的列：max 返回元素本身，排序按 Python 的精确比较。
    mixed = chain(mapped("m", nc(Col("i64"), Col("d"), mode="max")), sort("m", tie_break_col="i64"))
    mixed_pools = {"i64": [(1 << 53) + 1, 1 << 53, 3, -1], "d": [float(1 << 53), 3.0, 2.5, -1.0, math.nan]}
    for _ in range(4):
        add(mixed, [random_rows(rng, 16, ["i64", "d"], pools=mixed_pools, none=0.1)], label="sort mixed")

    # limit：各种 offset / limit 与 chunk 大小（含空 chunk）。
    for lim, offset in ((1, 0), (2, 1), (3, 5), (100, 0), (1, 100), (4, 3)):
        chunks = [random_rows(rng, n, ["i64"], start=10 * n) for n in (0, 1, 3, 5, 8)]
        add(chain(sort("i64", desc=False, tie_break_col="$id"), limit(lim, offset=offset)), chunks,
            label=f"limit {lim} {offset}")
        add(chain(limit(lim, offset=offset), mapped("$score", nc(S, Col("i64")))), chunks, label=f"limit first {lim}")

    # 运行期错误的先后：一个算子做完全部查询才进入下一个。
    order = chain(mapped("t", nc(S, 1)), mapped("$score", nc(Col("t"), 1e308, mode="multiply")))
    add(order, [[(1, 1e10, {})], [(2, math.nan, {})]], label="error order")
    add(order, [[(1, 1e10, {})], [(2, 1.0, {})]], label="error order")
    add(order, [[(1, 1.0, {})], [(2, 1e10, {})]], label="error order")
    add(chain(mapped("t", nc(S, Col("d"))), sort("t"), limit(1)),
        [[(1, 1.0, {"d": 1.0})], [(2, 1.0, {"d": math.inf})]], label="error later chunk")

    # 随机链：固定种子的组合，3.x 校验不通过的跳过。
    count = 0
    while count < 120:
        ops = []
        temps = []
        for _ in range(rng.randint(1, 5)):
            readable = NUMERIC + ["$score", "$id"] + temps
            kind = rng.choice(["nc", "nc", "decay", "round", "sort", "limit"])
            out = rng.choice(["$score", "$score", f"t{len(temps)}"])
            if kind == "nc":
                args = [Col(rng.choice(readable)) for _ in range(rng.randint(2, 4))]
                if rng.random() < 0.3:
                    args.append(rng.choice([1, -3, 0.5, 2.0, 1e-3]))
                mode = rng.choice(["sum", "multiply", "max", "min", "avg", "weighted"])
                extra = {"weights": [rng.choice([1, 0.5, -2, 0.25]) for _ in args]} if mode == "weighted" else {}
                ops.append(mapped(out, nc(*args, mode=mode, **extra)))
            elif kind == "decay":
                ops.append(mapped(out, decay(Col(rng.choice(readable + ["tz"])), function=rng.choice(["gauss", "exp", "linear"]),
                                             origin=rng.choice([0, 1.5, -10]), scale=rng.choice([1, 0.5, 100]),
                                             offset=rng.choice([0, 0.25]), decay=rng.choice([0.5, 0.1]))))
            elif kind == "round":
                ops.append(mapped(out, rnd(Col(rng.choice(readable)), rng.randint(0, 6))))
            elif kind == "sort":
                column = rng.choice(readable + ["s", "b", "tz"])
                params = {"desc": rng.random() < 0.5}
                if rng.random() < 0.5:
                    params["tie_break_col"] = rng.choice(readable + ["s"])
                ops.append(sort(column, **params))
            else:
                ops.append(limit(rng.randint(1, 6), offset=rng.randint(0, 3)))
            if ops[-1].name == "map" and out != "$score" and out not in temps:
                temps.append(out)
        c = chain(*ops)
        try:
            validate_function_chain(function_chain_to_repr(schema_pb2.FunctionChain.FromString(wire(c))),
                                    SCHEMAS["exec"][1], 2)
        except SchemaValidationError:
            continue
        count += 1
        add(c, [random_rows(rng, rng.randint(0, 9), SCALARS, start=50 * q, specials=rng.random() < 0.2, none=0.1)
                for q in range(2)], label="random")
    return cases


def render(cases):
    """schema 一行一个，用例一行一个，便于看 diff。"""
    used = list(dict.fromkeys(case["schema"] for case in cases))
    schemas = ",\n".join(f"  {json.dumps(key)}: {json.dumps(schema_json(key))}" for key in used)
    lines = ",\n".join("  " + json.dumps(case, ensure_ascii=True) for case in cases)
    return f'{{\n"schemas": {{\n{schemas}\n}},\n"cases": [\n{lines}\n]\n}}\n'


def copied_chains():
    """复制自参照的用例（tests/python/adapter/test_function_chain.py、tests/python/compatibility/
    test_function_chain_l2.py）与 SDK 冒烟里的每一种链：同一段构造分别交给 chainwire 里与 SDK 同名的写法和
    pymilvus 3.0 的 `pymilvus.function_chain`，字节解析后必须与 `to_proto()` 相等。"""
    from pymilvus import function_chain as sdk

    def popularity(api, sort=True):
        chain = (api.FunctionChain(api.FunctionChainStage.L2_RERANK)
                 .map("tmp_score", api.fn.num_combine(api.col("$score"), api.col("popularity"), mode="sum"))
                 .map("$score", api.fn.round_decimal(api.col("tmp_score"), decimal=2)))
        if sort:
            chain.sort(api.col("$score"), desc=True, tie_break_col=api.col("$id"))
        return chain

    def score_plus_ts(api, name="score_plus_ts"):
        return (api.FunctionChain(api.FunctionChainStage.L2_RERANK, name=name)
                .map("$score", api.fn.num_combine(api.col("$score"), api.col("ts"), mode="sum"))
                .sort(api.col("$score"), desc=True, tie_break_col=api.col("$id")))

    def mapped_only(api, name, output, *args):
        return (api.FunctionChain(api.FunctionChainStage.L2_RERANK, name=name)
                .map(output, api.fn.num_combine(*[api.col(a) for a in args], mode="sum")))

    def rerank_chain(api, **params):
        return api.FunctionChain(api.FunctionChainStage.L2_RERANK).map(
            "$score", api.fn.rerank_model(api.col("title"), queries=["query"], **params))

    builds = [
        popularity, lambda api: popularity(api, sort=False), lambda api: popularity(api).limit(2),
        lambda api: (api.FunctionChain(api.FunctionChainStage.L2_RERANK)
                     .map("dynamic_tag", api.fn.num_combine(api.col("$score"), api.col("popularity"), mode="sum"))
                     .map("$score", api.fn.round_decimal(api.col("dynamic_tag"), decimal=2))
                     .sort(api.col("$score"), desc=True, tie_break_col=api.col("$id"))),
        lambda api: (api.FunctionChain(api.FunctionChainStage.L2_RERANK)
                     .map("$score", api.fn.num_combine(api.col("$score"), api.col("id"), mode="sum"))
                     .sort(api.col("$score"), desc=True, tie_break_col=api.col("$id"))),
        lambda api: (api.FunctionChain(api.FunctionChainStage.L2_RERANK)
                     .map("$score", api.fn.round_decimal(api.col("$score"), decimal=2))
                     .sort(api.col("$score"), desc=True).limit(2)),
        lambda api: (api.FunctionChain(api.FunctionChainStage.L2_RERANK)
                     .map("$score", api.fn.round_decimal(api.col("$score"), decimal=0))
                     .sort(api.col("$score"), desc=True, tie_break_col=api.col("$id"))),
        lambda api: api.FunctionChain(api.FunctionChainStage.L2_RERANK).map(
            "$score", api.fn.num_combine(api.col("$score"), api.col("popularity"), mode="weighted",
                                         weights=[1.0, 0.1])),
        lambda api: api.FunctionChain(api.FunctionChainStage.L2_RERANK).limit(2),
        lambda api: api.FunctionChain(api.FunctionChainStage.L2_RERANK).limit(1),
        lambda api: rerank_chain(api, provider="unknown-provider"),
        lambda api: rerank_chain(api, provider="cohere", **{"Base_Url": "https://attacker.example"}),
        lambda api: rerank_chain(api, provider="cohere", typo="value"),
        score_plus_ts,
        lambda api: (api.FunctionChain(api.FunctionChainStage.L2_RERANK, name="l2_temp_score")
                     .map("tmp_score", api.fn.num_combine(api.col("$score"), api.col("ts"), mode="sum"))
                     .map("$score", api.fn.num_combine(api.col("tmp_score"), api.col("$score"), mode="sum"))
                     .sort(api.col("$score"), desc=True, tie_break_col=api.col("$id"))),
        lambda api: api.FunctionChain(api.FunctionChainStage.L2_RERANK, name="l2_limit").limit(2),
        lambda api: mapped_only(api, "bad_l2_write_id", "$id", "$score", "ts"),
        lambda api: mapped_only(api, "bad_l2_reserved_temp_output", "$tmp_score", "$score", "ts"),
        lambda api: mapped_only(api, "bad_l2_seg_offset_input", "$score", "$seg_offset", "$score"),
        lambda api: mapped_only(api, "bad_l2_unknown_system_input", "$score", "$tmp_score", "$score"),
        lambda api: score_plus_ts(api, name="").limit(3),
        # 构造参数的其余写法：decay 的缺省值与整数参数、显式 offset、`col` 之外的字符串列名。
        lambda api: api.FunctionChain(api.FunctionChainStage.L2_RERANK).map(
            "$score", api.fn.decay(api.col("ts"), function="gauss", origin=0, scale=8)),
        lambda api: api.FunctionChain(api.FunctionChainStage.L2_RERANK).map(
            "t", api.fn.decay(api.col("ts"), function="exp", origin=1.5, scale=2, offset=1, decay=0.25)).limit(4, 1),
        lambda api: api.FunctionChain(api.FunctionChainStage.L2_RERANK).sort("ts", desc=False, tie_break_col="$id"),
    ]
    for build in builds:
        expected = build(sdk).to_proto()
        assert schema_pb2.FunctionChain.FromString(build(w).to_bytes()) == expected, expected
    return len(builds)


COPIED = copied_chains()
validation = render(validation_cases())
execution = render(execution_cases())
assert SDK_CHECKED > 900, SDK_CHECKED
for target, text in ((VALIDATION, validation), (EXECUTION, execution)):
    if "--verify" in sys.argv:
        assert target.read_text(encoding="ascii") == text, target
    else:
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="ascii")
print(f"ok: {SDK_CHECKED} chains also built with the SDK; {COPIED} SDK-style constructions match")
