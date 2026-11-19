"""dump 用例的建数据场景：e2e 与差分共用，也可由 3.x 的 python 直接运行（只依赖 pymilvus）。

用法：<python> dump_cases.py <场景名> <数据目录.db>。每个场景都把集合命名为 ``c``。
"""

from __future__ import annotations

import sys

from pymilvus import DataType, Function, FunctionType, MilvusClient


def basic(c):
    s = MilvusClient.create_schema(auto_id=True, enable_dynamic_field=True)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("vec", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("text", DataType.VARCHAR, max_length=64)
    c.create_collection("c", schema=s)
    c.insert("c", [{"vec": [1.0, 0.5], "text": "a", "extra": 1, "nested": {"k": [1, 2]}},
                   {"vec": [0.25, 0.0], "text": "b"}])
    c.flush("c")
    # 未 flush 的行同样导出。
    c.insert("c", [{"vec": [0.0, 1.0], "text": "unflushed", "extra": "s"}])


def scalars(c):
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=True)
    s.add_field("pk", DataType.INT64, is_primary=True)
    s.add_field("vec", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("b", DataType.BOOL)
    s.add_field("i8", DataType.INT8)
    s.add_field("i64", DataType.INT64, nullable=True)
    s.add_field("i16", DataType.INT16, default_value=5)
    s.add_field("f", DataType.FLOAT)
    s.add_field("s", DataType.VARCHAR, max_length=32, nullable=True, default_value="dv")
    s.add_field("j", DataType.JSON, nullable=True)
    s.add_field("arr", DataType.ARRAY, element_type=DataType.INT64, max_capacity=4, nullable=True)
    s.add_field("barr", DataType.ARRAY, element_type=DataType.BOOL, max_capacity=4)
    s.add_field("farr", DataType.ARRAY, element_type=DataType.DOUBLE, max_capacity=4)
    c.create_collection("c", schema=s)
    c.insert("c", [
        {"pk": -1, "vec": [0.1, 0.2], "b": True, "i8": -3, "i64": -2**63, "f": 1e30, "s": "中文\n\"q\"",
         "j": {"x": 1, "y": [1, "z"], "big": 12345678901234567890, "f": 1.5, "u": "é"},
         "arr": [1, 2**62], "barr": [True, False], "farr": [0.1], "dyn": {"a": [None]}},
        {"pk": 2, "vec": [0.3, 0.4], "b": False, "i8": 7, "i64": None, "f": 0.5, "s": None, "j": [1, 2],
         "arr": None, "barr": [], "farr": []},
        {"pk": 3, "vec": [0.5, 0.6], "b": True, "i8": 0, "f": 2.0, "j": {"k": 3.25}, "barr": [False], "farr": [],
         "$x": 1},
        {"pk": 4, "vec": [0.0, 0.0], "b": True, "i8": 0, "f": 0.0, "barr": [], "farr": []},
    ])
    c.flush("c")
    c.delete("c", ids=[4])
    c.upsert("c", [{"pk": 3, "vec": [9, 9], "b": False, "i8": 1, "f": 2.5, "barr": [], "farr": [1.0], "dyn": "u"}])


def sparse(c):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("sp", DataType.SPARSE_FLOAT_VECTOR)
    c.create_collection("c", schema=s)
    p = c.prepare_index_params()
    p.add_index(field_name="sp", index_type="SPARSE_INVERTED_INDEX", metric_type="IP")
    c.create_index("c", index_params=p)
    c.insert("c", [{"id": 1, "sp": {3: 0.5, 10: 1.25}}, {"id": 2, "sp": {0: 1.0}}])


def bm25(c):
    s = MilvusClient.create_schema(auto_id=True)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("text", DataType.VARCHAR, max_length=256, enable_analyzer=True)
    s.add_field("sp", DataType.SPARSE_FLOAT_VECTOR)
    s.add_function(Function(name="bm", function_type=FunctionType.BM25,
                            input_field_names=["text"], output_field_names=["sp"]))
    c.create_collection("c", schema=s)
    p = c.prepare_index_params()
    p.add_index(field_name="sp", index_type="SPARSE_INVERTED_INDEX", metric_type="BM25")
    c.create_index("c", index_params=p)
    c.insert("c", [{"text": "hello world"}, {"text": "milvus lite dump"}])


def array_varchar(c):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("vec", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("tags", DataType.ARRAY, element_type=DataType.VARCHAR, max_capacity=4, max_length=8, nullable=True)
    c.create_collection("c", schema=s)
    c.insert("c", [{"id": 1, "vec": [1, 1], "tags": ["a", "bc"]}, {"id": 2, "vec": [1, 2], "tags": None}])


def partitions(c):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("vec", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("k", DataType.VARCHAR, max_length=8, is_partition_key=True)
    c.create_collection("c", schema=s, num_partitions=4)
    c.insert("c", [{"id": i, "vec": [i, 1], "k": f"k{i}"} for i in range(6)])


def explicit_partitions(c):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("vec", DataType.FLOAT_VECTOR, dim=2)
    c.create_collection("c", schema=s)
    c.create_partition("c", "p1")
    c.insert("c", [{"id": 1, "vec": [1, 1]}])
    c.insert("c", [{"id": 2, "vec": [2, 2]}], partition_name="p1")


def tz_geo(c):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("vec", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("ts", DataType.TIMESTAMPTZ, nullable=True)
    s.add_field("g", DataType.GEOMETRY, nullable=True)
    c.create_collection("c", schema=s)
    c.insert("c", [{"id": 1, "vec": [1, 1], "ts": "2024-01-02T03:04:05+08:00", "g": "POINT(1 2)"},
                   {"id": 2, "vec": [1, 1], "ts": None, "g": None}])


def multivec(c):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("v1", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("v2", DataType.FLOAT_VECTOR, dim=3)
    c.create_collection("c", schema=s)
    c.insert("c", [{"id": 1, "v1": [1, 2], "v2": [3, 4, 5]}])


def varchar_pk(c):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("pk", DataType.VARCHAR, is_primary=True, max_length=16)
    s.add_field("vec", DataType.FLOAT_VECTOR, dim=2)
    c.create_collection("c", schema=s)
    c.insert("c", [{"pk": k, "vec": [i, 0]} for i, k in enumerate(["b", "a", "ä", "日本", "z9", "0"])])


def empty(c):
    c.create_collection("c", dimension=2)


def released(c):
    c.create_collection("c", dimension=2)
    c.insert("c", [{"id": 1, "vector": [1, 2]}])
    c.release_collection("c")


def many(c):
    c.create_collection("c", dimension=4)
    for start in range(0, 2500, 500):
        c.insert("c", [{"id": i, "vector": [i, 0, 0, 1]} for i in range(start, start + 500)])
    c.flush("c")


def large(c):
    # 信号与写失败用例：行数多、单行较大，配合小 chunk 让导出持续若干秒。
    c.create_collection("c", dimension=64)
    for start in range(0, 4000, 1000):
        c.insert("c", [{"id": i, "vector": [float(i)] * 64} for i in range(start, start + 1000)])
    c.flush("c")


def non_finite(c):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("vec", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("d", DataType.DOUBLE)
    c.create_collection("c", schema=s)
    c.insert("c", [{"id": 1, "vec": [1, 1], "d": 0.5}, {"id": 2, "vec": [1, 1], "d": float("nan")}])


def non_finite_array(c):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("vec", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("da", DataType.ARRAY, element_type=DataType.DOUBLE, max_capacity=4)
    c.create_collection("c", schema=s)
    c.insert("c", [{"id": 1, "vec": [1, 1], "da": [1.0]}, {"id": 2, "vec": [1, 1], "da": [1.0, float("inf")]}])


CASES = {f.__name__: f for f in [basic, scalars, sparse, bm25, array_varchar, partitions, explicit_partitions,
                                   tz_geo, multivec, varchar_pk, empty, released, many, large, non_finite,
                                   non_finite_array]}


def build(name, db):
    client = MilvusClient(db)
    try:
        CASES[name](client)
    finally:
        client.close()


if __name__ == "__main__":
    build(sys.argv[1], sys.argv[2])
