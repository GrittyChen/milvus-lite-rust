"""公共 function chain（0035）的线格式编码。

pymilvus 2.6 的 `schema_pb2` 里没有 FunctionChain 的任何消息，`SearchRequest` 也没有 24 号字段，所以不经
protobuf 生成类，直接拼字节；两条 SDK 线用同一段代码得到同一份请求。
"""

from __future__ import annotations

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


def attach(request, *chains):
    """把链写进 SearchRequest 的 24 号字段；2.6 的消息把它当未知字段原样带出。"""
    request.MergeFromString(b"".join(_field(24, c) for c in chains))
    return request
