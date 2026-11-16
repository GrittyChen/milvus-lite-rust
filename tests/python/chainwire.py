"""公共 function chain（0035）的线格式编码。

pymilvus 2.6 的 `schema_pb2` 里没有 FunctionChain 的任何消息，`SearchRequest` 也没有 24 号字段，所以不经
protobuf 生成类，直接拼字节；两条 SDK 线用同一段代码得到同一份请求。
"""

from __future__ import annotations

import contextlib
import struct

L2_RERANK = 5

def _varint(n):
    n &= (1 << 64) - 1
    out = bytearray()
    while n >= 0x80:
        out.append(n & 0x7F | 0x80)
        n >>= 7
    out.append(n)
    return bytes(out)


def _field(number, payload):  # 长度前缀字段
    return _varint(number << 3 | 2) + _varint(len(payload)) + payload


def value(v):
    if v is None:  # oneof 未设置
        return b""
    if isinstance(v, bool):
        return _varint(1 << 3) + _varint(int(v))
    if isinstance(v, int):
        return _varint(2 << 3) + _varint(v)
    if isinstance(v, float):
        return _varint(3 << 3 | 1) + struct.pack("<d", v)
    if isinstance(v, str):
        return _field(4, v.encode())
    if isinstance(v, (list, tuple)):
        return _field(5, b"".join(_field(1, value(x)) for x in v))
    if isinstance(v, dict):
        return _field(6, b"".join(_field(1, _entry(k, x)) for k, x in v.items()))
    if isinstance(v, bytes):
        return _field(7, v)
    raise TypeError(type(v))


def _entry(key, v):
    return _field(1, key.encode()) + _field(2, value(v))


def _params(number, params):
    return b"".join(_field(number, _entry(k, v)) for k, v in params.items())


class Col(str):
    pass


def expr(name, *args, **params):
    body = _field(1, name.encode())
    for a in args:
        body += _field(2, _field(1, _field(1, a.encode())) if isinstance(a, Col) else _field(2, value(a)))
    return body + _params(3, params)


def op(name, *, expr=None, inputs=(), outputs=(), **params):
    body = _field(1, name.encode())
    if expr is not None:
        body += _field(2, expr)
    body += b"".join(_field(3, x.encode()) for x in inputs) + b"".join(_field(4, x.encode()) for x in outputs)
    return body + _params(5, params)


def chain(*ops, stage=L2_RERANK, name=""):
    return (_field(1, name.encode()) if name else b"") + _varint(2 << 3) + _varint(stage) + b"".join(_field(3, o) for o in ops)


def attach(request, *chains, field=24):
    """把链写进 SearchRequest 的 24 号字段（HybridSearchRequest 是 15 号）；2.6 的消息把它当未知字段原样带出。"""
    request.MergeFromString(b"".join(_field(field, c.to_bytes() if isinstance(c, FunctionChain) else c)
                                     for c in chains))
    return request


@contextlib.contextmanager
def sending(client, *chains):
    """经 `client` 发出的 Search 都带上这些链，代替 3.0 SDK 的 `search(function_chains=...)`；响应照常由 SDK 解析。"""
    stub = client._get_connection()._stub
    original = stub.Search

    def search(request, *args, **kwargs):
        attach(request, *chains)
        return original(request, *args, **kwargs)

    stub.Search = search
    try:
        yield
    finally:
        stub.Search = original


# ---- pymilvus 3.0 `pymilvus.function_chain` 的同名写法 ----
# 复制自参照实现的用例用它构造链，用例体因此与参照相同；参数的整理（`sort` 的 `desc` 缺省为真、`limit` 带
# `offset=0`、`num_combine` 的 `mode` 与浮点 `weights`、`decay` 的五个参数）与 SDK 的 `to_proto()` 一致，
# 由 tests/differential/generate_function_chain_fixtures.py 用真实 SDK 逐条核对。SDK 自己的参数检查不复刻。

class FunctionChainStage:
    L2_RERANK = L2_RERANK


def col(name):
    return Col(name)


class _Expr:
    def __init__(self, name, *args, **params):
        self.bytes = expr(name, *args, **params)


class fn:  # noqa: N801  （与 SDK 的模块名相同）
    @staticmethod
    def num_combine(*cols, mode="sum", weights=None):
        params = {"mode": mode}
        if weights is not None:
            params["weights"] = [float(weight) for weight in weights]
        return _Expr("num_combine", *cols, **params)

    @staticmethod
    def decay(value, *, function, origin, scale, offset=0, decay=0.5):
        return _Expr("decay", value, function=function, origin=origin, scale=scale, offset=offset, decay=decay)

    @staticmethod
    def round_decimal(value, *, decimal):
        return _Expr("round_decimal", value, decimal=decimal)

    @staticmethod
    def rerank_model(value, *, queries, **provider_params):
        return _Expr("rerank_model", value, queries=queries, **provider_params)


class FunctionChain:
    def __init__(self, stage, name=""):
        self._stage, self._name, self._ops = stage, name, []

    def map(self, output, e):
        self._ops.append(op("map", expr=e.bytes, outputs=[output]))
        return self

    def sort(self, by, desc=True, tie_break_col=None):
        params = {"column": str(by), "desc": desc}
        inputs = [str(by)]
        if tie_break_col is not None:
            params["tie_break_col"] = str(tie_break_col)
            inputs.append(str(tie_break_col))
        self._ops.append(op("sort", inputs=inputs, **params))
        return self

    def limit(self, limit, offset=0):
        self._ops.append(op("limit", limit=limit, offset=offset))
        return self

    def to_bytes(self):
        return chain(*self._ops, stage=self._stage, name=self._name)
