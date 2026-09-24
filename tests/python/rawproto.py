"""手工拼出的 proto 字节，用于构造大消息：避免在 Python 中生成上亿个 float 对象。"""

from __future__ import annotations


def _varint(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)


def _field(tag, payload):
    return _varint(tag << 3 | 2) + _varint(len(payload)) + payload


INT64 = 5
FLOAT_VECTOR = 101


def raw_insert(name, ids, dim):
    """InsertRequest 的字节：INT64 主键 id 与全为 0 的 FLOAT_VECTOR 列 v。"""
    long_data = _field(1, b"".join(_varint(i) for i in ids))
    id_fd = _varint(1 << 3) + _varint(INT64) + _field(2, b"id") + _field(3, _field(3, long_data))
    vectors = _varint(1 << 3) + _varint(dim) + _field(2, _field(1, bytes(4 * dim * len(ids))))
    v_fd = _varint(1 << 3) + _varint(FLOAT_VECTOR) + _field(2, b"v") + _field(4, vectors)
    return (_field(3, name.encode()) + _field(5, id_fd) + _field(5, v_fd)
            + _varint(7 << 3) + _varint(len(ids)))
