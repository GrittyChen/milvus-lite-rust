"""0034：GEOMETRY 类型（WKT 的接受范围、原文输出、默认值、nullable、schema 规则、协议槽位）对照 3.x（1320d26）。

本文件覆盖第 1 步。已登记的差异单独断言：`ARRAY<GEOMETRY>`（本实现 UNIMPLEMENTED）与原始 gRPC 的
`bytes_data`、非有限浮点默认值（本实现对任何类型都是 UNIMPLEMENTED，0015）。第 1 步里引用 GEOMETRY 字段的过滤一律
UNIMPLEMENTED（两次提交之间的中间状态），第 2 步改为与 3.x 对照。
"""

import shutil
from pathlib import Path

import pytest
from pymilvus import AnnSearchRequest, DataType, Function, FunctionType, MilvusClient, RRFRanker
from pymilvus.grpc_gen import milvus_pb2, schema_pb2

from compare import run
from test_reopen import pair  # noqa: F401  （fixture）
from test_scenarios import index

SQUARE = "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))"

# 3.x 用两个正则加 Python `float()` 解析；下面每一项都按 3.x 的判定写入或拒绝。
WKT_FORMS = [
    "POINT(1 1)", "POINT(20 20)", SQUARE, "  point ( 1e1   -2.5 )  ", "POINT(1 1)\n", "POINT(1 1)\n\n", "\tPoInT\t(\t1\t2\t)\t",
    "POİNT(1 1)", "POıNT(1 1)", "POINT(١ ٢)", "POINT(１２ 1)", "POINT(١_٢ 1)", "POINT(1_0 1)", "POINT(1e1_0 1)",
    "POINT(1\u30002)", "POINT(1\x1c2)", "POINT(1\x852)", "POINT(1\xa02)", "POINT(1\u20282)", "POINT(1\u200b2)",
    "POINT(+1 -.5)", "POINT(1. 1.)", "POINT(-0 -0.0)", "POINT(1e-400 1)", "POINT(1e308 -1e308)", "POINT(1E2 1)",
    "POINT(0x10 1)", "POINT(1__0 1)", "POINT(1_ 1)", "POINT(_1 1)", "POINT(.e1 1)", "POINT(1e 1)", "POINT(+-1 1)",
    "POINT(inf 1)", "POINT(-inf 1)", "POINT(nan 1)", "POINT(infinity 1)", "POINT(1e400 1)", "POINT(1)", "POINT(1 2 3)",
    "POINT(1,2)", "POINT((1 2))", "POINT(1 2", "POINT 1 2)", "POINT()", "POINT EMPTY", "POINT Z(1 1 1)", "xPOINT(1 1)",
    "POINT(1 1)x", "POINT(1 1)\x00", "POſNT(1 1)", "SRID=4326;POINT(1 1)", "MULTIPOINT(1 1)", "LINESTRING(0 0,1 1)",
    "", " ", "not-wkt",
    "polygon ( ( 0 0 ,1 0, 1 1 , 0 0 ) ) \n", "POLYGON((0 0,0 0,0 0,0 0))", "POLYGON((0\n0,1\xa00,1 1,0 0))",
    "POLYGON((0 0,1 0,1 1,-0 -0.0))", "POLYGON((0 0,4 0,4 4,2 1,0 4,0 0))", "POLYGON((0 0,1 0,1 1,0 0,))",
    "POLYGON(())", "POLYGON((0 0,1 0,0 0))", "POLYGON((0 0,1 0,1 1,0 1))", "POLYGON((0 0,2 2,2 0,0 2,0 0))",
    "POLYGON((0 0,1 0,1 1,0 0,0 0))", "POLYGON((0 0,4 0,4 4,0 4,0 0),(1 1,2 1,2 2,1 2,1 1))",
    "POLYGON((0 0,1 0,1 1,0 0)", "POLYGON(0 0,1 0,1 1,0 0)", "POLYGON((0 0 0,1 0 0,1 1 0,0 0 0))",
    "POLYGON((0 0,1 0,,1 1,0 0))", "POLYGON((0 0,1 0,1 inf,0 0))", "POLYGON((0 0,1 0,1 1,0 0)) x",
    "POLYGON((0 0,1e-10 0,1 1,0 0))", "POLYGON((0 0,2 0,1 0,1 1,0 0))", "POLYGON((0 0,2 0,2 2,1 0,0 2,0 0))",
]


def collection(client, name, *, nullable=False, default=None, extra=False, **geo):
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    kwargs = {"nullable": nullable, **geo}
    if default is not None:
        kwargs["default_value"] = default
    schema.add_field("geo", DataType.GEOMETRY, **kwargs)
    if extra:
        schema.add_field("g2", DataType.GEOMETRY, nullable=True, default_value="POINT(9 9)")
    client.create_collection(name, schema=schema, index_params=index("L2"))
    client.load_collection(name)


def query(client, name, expr="id >= 0", fields=("id", "geo")):
    return sorted(
        (tuple(row[f] for f in fields)) for row in client.query(name, filter=expr, output_fields=list(fields))
    )


def test_wkt_forms_and_original_text_match_3x(pair):
    d = pair.open()
    d.step("create", lambda c: collection(c, "t"))
    accepted = 0
    for i, value in enumerate(WKT_FORMS):
        def write(c, i=i, value=value):
            c.insert("t", [{"id": i, "vector": [0.0, 0.0], "geo": value}])
            return query(c, "t", f"id == {i}")
        a = run(write, d.rust)
        b = run(write, d.ref)
        assert a == b and repr(a) == repr(b), f"{value!r}\n  rust: {a!r}\n  3.x:  {b!r}"
        accepted += b[0] == "ok"
    # 场景写错时两边会一起拒绝全部输入。
    assert 25 < accepted < len(WKT_FORMS) - 25, accepted
    d.step("all", lambda c: query(c, "t"))
    pair.close()
    d = pair.open()
    d.step("load", lambda c: c.load_collection("t"))
    d.step("reopened", lambda c: query(c, "t"))
    d.step("flush", lambda c: c.flush("t"))
    d.step("flushed", lambda c: query(c, "t"))
    d.step("compact", lambda c: c.compact("t") is not None)
    d.step("compacted", lambda c: query(c, "t"))
    pair.close()
    d = pair.open()
    d.step("load again", lambda c: c.load_collection("t"))
    d.step("reopened after flush", lambda c: query(c, "t"))


def test_nullable_default_upsert_delete_and_describe_match_3x(pair):
    d = pair.open()
    d.step("create nullable", lambda c: collection(c, "n", nullable=True, extra=True))
    d.step("create not nullable", lambda c: collection(c, "nn"))
    d.step("create default", lambda c: collection(c, "dv", default="  polygon((0 0,1 0,1 1,0 0)) "))
    d.step("create default bad", lambda c: collection(c, "db", default="nope"), error=True)
    d.step("create default empty", lambda c: collection(c, "de", default=""), error=True)
    d.step("create max_length", lambda c: collection(c, "ml", max_length=5))
    d.step("insert nullable", lambda c: c.insert("n", [
        {"id": 1, "vector": [0.0, 0.0], "geo": "POINT(1 1)", "g2": "POINT(2 2)"},
        {"id": 2, "vector": [1.0, 0.0]},
        {"id": 3, "vector": [0.0, 1.0], "geo": None, "g2": None},
        {"id": 4, "vector": [1.0, 1.0], "geo": SQUARE}]))
    # nullable 加默认值：缺失与显式 None 都取默认值，这一列不会有 NULL。
    assert d.step("nullable rows", lambda c: query(c, "n", fields=("id", "geo", "g2"))) == ("ok", [
        [1, "POINT(1 1)", "POINT(2 2)"], [2, None, "POINT(9 9)"], [3, None, "POINT(9 9)"], [4, SQUARE, "POINT(9 9)"]])
    d.step("not nullable missing", lambda c: c.insert("nn", [{"id": 1, "vector": [0.0, 0.0]}]), error=True)
    d.step("not nullable none", lambda c: c.insert("nn", [{"id": 1, "vector": [0.0, 0.0], "geo": None}]), error=True)
    d.step("not a string", lambda c: c.insert("nn", [{"id": 1, "vector": [0.0, 0.0], "geo": 5}]), error=True)
    d.step("bad row rejects the batch", lambda c: c.insert("nn", [
        {"id": 1, "vector": [0.0, 0.0], "geo": "POINT(1 1)"}, {"id": 2, "vector": [0.0, 0.0], "geo": "x"}]), error=True)
    d.step("nothing written", lambda c: query(c, "nn"))
    d.step("default insert", lambda c: (c.insert("dv", [
        {"id": 1, "vector": [0.0, 0.0]}, {"id": 2, "vector": [0.0, 0.0], "geo": None},
        {"id": 3, "vector": [0.0, 0.0], "geo": "POINT(2 2)"}]), query(c, "dv"))[1])
    d.step("max_length is ignored", lambda c: (c.insert("ml", [{"id": 1, "vector": [0.0, 0.0], "geo": "POINT(123456 1)"}]), query(c, "ml"))[1])
    d.step("upsert", lambda c: (c.upsert("n", [{"id": 1, "vector": [0.0, 0.0], "geo": "POINT(7 7)"}]), query(c, "n", fields=("id", "geo", "g2")))[1])
    d.step("partial upsert", lambda c: (c.upsert("n", [{"id": 4, "geo": "POINT(8 8)"}], partial_update=True), query(c, "n", fields=("id", "geo", "g2")))[1])
    d.step("partial upsert bad", lambda c: c.upsert("n", [{"id": 4, "geo": "zzz"}], partial_update=True), error=True)
    d.step("delete", lambda c: (c.delete("n", ids=[2]), query(c, "n", fields=("id", "geo", "g2")))[1])
    d.step("get", lambda c: [(r["id"], r.get("geo")) for r in c.get("n", ids=[1, 3], output_fields=["geo"])])
    d.step("search output", lambda c: [[(h["id"], h["entity"]) for h in x] for x in c.search("n", data=[[0.0, 0.0]], limit=5, output_fields=["geo", "g2"])])
    d.step("search *", lambda c: [[(h["id"], h["entity"]) for h in x] for x in c.search("n", data=[[0.0, 0.0]], limit=5, output_fields=["*"])])
    d.step("hybrid output", lambda c: [[(h["id"], h["entity"]) for h in x] for x in c.hybrid_search("n", [
        AnnSearchRequest([[0.0, 0.0]], "vector", {"metric_type": "L2"}, limit=5),
        AnnSearchRequest([[1.0, 1.0]], "vector", {"metric_type": "L2"}, limit=5)], RRFRanker(), limit=5, output_fields=["geo", "g2"])])

    def iterate(c):
        iterator = c.query_iterator("n", batch_size=1, filter="id >= 0", output_fields=["geo", "g2"])
        rows = []
        while batch := iterator.next():
            rows += [(r["id"], r["geo"], r["g2"]) for r in batch]
        iterator.close()
        return sorted(rows)
    d.step("query iterator output", iterate)
    # 落盘形态：schema.json（dtype、默认值原文、不保存 max_length）两边逐字节相同。
    for name in ("n", "dv", "ml"):
        files = [(Path(root) / "collections" / name / "schema.json").read_bytes() for root in (pair.rust_path, pair.ref_path)]
        assert files[0] == files[1], f"{name}\n  rust: {files[0]!r}\n  3.x:  {files[1]!r}"
        assert b'"dtype": "geometry"' in files[0]
    d.step("describe", lambda c: [(f["name"], str(f["type"]), f["params"]) for f in c.describe_collection("ml")["fields"]])

    def default_echo(c):
        stub = c._get_connection()._stub
        reply = stub.DescribeCollection(milvus_pb2.DescribeCollectionRequest(collection_name="dv"))
        field = next(f for f in reply.schema.fields if f.name == "geo")
        return field.data_type, field.default_value.WhichOneof("data"), field.default_value.string_data
    assert d.step("describe default", default_echo) == ("ok", [24, "string_data", "  polygon((0 0,1 0,1 1,0 0)) "])
    d.step("group by", lambda c: c.search("n", data=[[0.0, 0.0]], limit=2, group_by_field="geo"), error=True)
    for index_type in ("INVERTED", "RTREE"):
        def scalar_index(c, index_type=index_type):
            params = c.prepare_index_params()
            params.add_index(field_name="geo", index_type=index_type)
            c.create_index("n", params)
        d.step(f"{index_type} index", scalar_index, error=True)
    pair.close()
    d = pair.open()
    d.step("load", lambda c: c.load_collection("n"))
    d.step("reopened", lambda c: query(c, "n", fields=("id", "geo", "g2")))
    d.step("default after reopen", lambda c: (c.load_collection("dv"), c.insert("dv", [{"id": 9, "vector": [0.0, 0.0]}]), query(c, "dv"))[2])


def fields(n, row_id=1):
    ids = schema_pb2.FieldData(type=5, field_name="id")
    ids.scalars.long_data.data.extend(range(row_id, row_id + n))
    vec = schema_pb2.FieldData(type=101, field_name="vector")
    vec.vectors.dim = 2
    vec.vectors.float_vector.data.extend([0.0] * (2 * n))
    return [ids, vec]


def test_raw_grpc_schema_rules_and_slots_match_3x(pair):
    d = pair.open()

    def create(c, name, edit, vector=True):
        schema = schema_pb2.CollectionSchema(name=name)
        if vector:
            schema.fields.add(name="id", data_type=5, is_primary_key=True)
        schema.fields.add(name="vector", data_type=101).type_params.add(key="dim", value="2")
        edit(schema.fields.add(name="geo", data_type=24))
        status = c._get_connection()._stub.CreateCollection(
            milvus_pb2.CreateCollectionRequest(collection_name=name, schema=schema.SerializeToString()))
        return status.code, status.reason

    def default(slot, value):
        return lambda f: setattr(f.default_value, slot, value)
    # 主键与 partition key：pymilvus 在客户端先拦截，服务端的文案只有原始 gRPC 看得到。
    assert d.step("primary key", lambda c: create(c, "pk", lambda f: setattr(f, "is_primary_key", True), vector=False)) == (
        "ok", [6, "primary key 'geo' must be VARCHAR or INT64, got DataType.GEOMETRY"])
    assert d.step("partition key", lambda c: create(c, "pkey", lambda f: setattr(f, "is_partition_key", True))) == (
        "ok", [6, "partition key 'geo' must be INT64 or VARCHAR, got GEOMETRY"])
    for name, slot, value in (("dl", "long_data", 5), ("dbool", "bool_data", True), ("dd", "double_data", 1.5),
                              ("ds", "string_data", "bad")):
        assert d.step(f"default {slot}", lambda c, name=name, slot=slot, value=value: create(c, name, default(slot, value))) == (
            "ok", [6, "GEOMETRY field 'geo' default_value is invalid"])
    # 已登记差异：bytes_data 默认值 3.x 按上面的文案拒绝，本实现对任何类型的二进制默认值都是 UNIMPLEMENTED。
    assert run(lambda c: create(c, "dby", default("bytes_data", b"POINT(1 1)")), d.ref) == (
        "ok", [6, "GEOMETRY field 'geo' default_value is invalid"])
    assert run(lambda c: create(c, "dby", default("bytes_data", b"POINT(1 1)")), d.rust)[:2] == ("rpc", "UNIMPLEMENTED")
    # 同一类登记差异（0015）：非有限的浮点默认值先于按类型的检查被拒绝。
    for slot in ("float_data", "double_data"):
        assert run(lambda c: create(c, "dinf", default(slot, float("inf"))), d.ref) == (
            "ok", [6, "GEOMETRY field 'geo' default_value is invalid"])
        assert run(lambda c: create(c, "dinf", default(slot, float("inf"))), d.rust)[:2] == ("rpc", "UNIMPLEMENTED")
    assert d.step("create", lambda c: create(c, "r", lambda f: setattr(f, "nullable", True))) == ("ok", [0, ""])
    d.step("index", lambda c: c.create_index("r", index("L2")))
    d.step("load", lambda c: c.load_collection("r"))

    def insert(c, row_id, n, fill, field_type=24, valid=None):
        geo = schema_pb2.FieldData(type=field_type, field_name="geo")
        fill(geo.scalars)
        geo.valid_data.extend(valid or [])
        reply = c._get_connection()._stub.Insert(
            milvus_pb2.InsertRequest(collection_name="r", num_rows=n, fields_data=[*fields(n, row_id), geo]))
        return reply.status.code, reply.status.reason
    cases = [
        ("wkt slot", 2, lambda s: s.geometry_wkt_data.data.extend(["POINT(1 1)", "POINT(2 2)"]), 24, None),
        ("string slot", 1, lambda s: s.string_data.data.extend(["POINT(3 3)"]), 24, None),
        ("varchar type", 1, lambda s: s.string_data.data.extend(["POINT(4 4)"]), 21, None),
        ("varchar type wkt slot", 1, lambda s: s.geometry_wkt_data.data.extend(["POINT(4 4)"]), 21, None),
        ("wkb slot", 1, lambda s: s.geometry_data.data.extend([b"\x01\x01\x00\x00\x00" + b"\x00" * 16]), 24, None),
        ("long slot", 1, lambda s: s.long_data.data.extend([1]), 24, None),
        ("no slot", 1, lambda s: None, 24, None),
        ("unknown type", 1, lambda s: s.geometry_wkt_data.data.extend(["POINT(5 5)"]), 0, None),
        ("bad second row", 2, lambda s: s.geometry_wkt_data.data.extend(["POINT(6 6)", "x"]), 24, None),
        ("valid_data", 2, lambda s: s.geometry_wkt_data.data.extend(["POINT(7 7)"]), 24, [True, False]),
        ("valid_data full", 2, lambda s: s.geometry_wkt_data.data.extend(["POINT(8 8)", "ignored"]), 24, [True, False]),
        ("invalid text under null", 2, lambda s: s.geometry_wkt_data.data.extend(["bad", "POINT(9 9)"]), 24, [False, True]),
    ]
    failed = 0
    for i, (name, n, fill, field_type, valid) in enumerate(cases):
        a = run(lambda c: insert(c, 100 * (i + 1), n, fill, field_type, valid), d.rust)
        b = run(lambda c: insert(c, 100 * (i + 1), n, fill, field_type, valid), d.ref)
        assert a == b, f"{name}\n  rust: {a!r}\n  3.x:  {b!r}"
        failed += b != ("ok", [0, ""])
    assert 3 <= failed <= len(cases) - 5, failed
    d.step("rows", lambda c: query(c, "r"))

    def output(c):
        reply = c._get_connection()._stub.Query(
            milvus_pb2.QueryRequest(collection_name="r", expr="id >= 0", output_fields=["geo"]))
        geo = next(f for f in reply.fields_data if f.field_name == "geo")
        ids = next(f for f in reply.fields_data if f.field_name == "id")
        rows = sorted(zip(ids.scalars.long_data.data, geo.scalars.geometry_wkt_data.data, geo.valid_data))
        return geo.type, geo.scalars.WhichOneof("data"), rows
    result = d.step("output slot", output)
    assert result[1][:2] == [24, "geometry_wkt_data"]
    # 空值位置是空串加 valid_data。
    assert [1001, "", False] in [list(r) for r in result[1][2]]


def test_array_of_geometry_is_a_registered_difference(pair, tmp_path):
    d = pair.open()

    def array_of_geometry(c):
        s = MilvusClient.create_schema(auto_id=False)
        s.add_field("id", DataType.INT64, is_primary=True)
        s.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
        s.add_field("a", DataType.ARRAY, element_type=DataType.GEOMETRY, max_capacity=2)
        c.create_collection("arr", schema=s)
    # 已登记差异：3.x 能建表（原始 gRPC 还能写入，读出为空数组），本实现 UNIMPLEMENTED。
    assert run(array_of_geometry, d.ref)[0] == "ok"
    rust = run(array_of_geometry, d.rust)
    assert rust[:2] == ("rpc", "UNIMPLEMENTED"), rust
    assert "ARRAY element type" in rust[2]

    def raw_create(c):
        schema = schema_pb2.CollectionSchema(name="arr_raw")
        schema.fields.add(name="id", data_type=5, is_primary_key=True)
        schema.fields.add(name="vector", data_type=101).type_params.add(key="dim", value="2")
        schema.fields.add(name="a", data_type=22, element_type=24).type_params.add(key="max_capacity", value="2")
        status = c._get_connection()._stub.CreateCollection(
            milvus_pb2.CreateCollectionRequest(collection_name="arr_raw", schema=schema.SerializeToString()))
        return status.code, status.reason
    assert run(raw_create, d.ref) == ("ok", [0, ""])
    rust = run(raw_create, d.rust)
    assert rust[:2] == ("rpc", "UNIMPLEMENTED") and "ARRAY element type" in rust[2], rust

    def raw_write(c):
        array = schema_pb2.FieldData(type=22, field_name="a")
        array.scalars.array_data.element_type = 24
        array.scalars.array_data.data.add().string_data.data.extend(["POINT(1 1)", "zzz"])
        reply = c._get_connection()._stub.Insert(
            milvus_pb2.InsertRequest(collection_name="arr", num_rows=1, fields_data=[*fields(1), array]))
        return reply.status.code, reply.status.reason
    assert raw_write(d.ref) == (0, "")
    d.ref.flush("arr")
    d.step("create plain", lambda c: collection(c, "plain"))
    d.step("insert plain", lambda c: c.insert("plain", [{"id": 1, "vector": [0.0, 0.0], "geo": "POINT(1 1)"}]))
    pair.close()
    # 3.x 写下的这种集合由本实现打开：明确拒绝、数据不变、可以 drop，同目录的 GEOMETRY 集合不受影响。
    copied = tmp_path / "from3x.db"
    shutil.copytree(pair.ref_path, copied)
    source = copied / "collections" / "arr"
    before = sorted((p.relative_to(source).as_posix(), p.read_bytes()) for p in source.rglob("*") if p.is_file())
    assert any(name.endswith(".parquet") for name, _ in before)
    client = MilvusClient(str(copied))
    try:
        assert sorted(client.list_collections()) == ["arr", "arr_raw", "plain"]
        for call in (lambda: client.describe_collection("arr"), lambda: client.load_collection("arr"),
                     lambda: client.query("arr", filter="id >= 0")):
            with pytest.raises(Exception, match="ARRAY element type"):
                call()
        after = sorted((p.relative_to(source).as_posix(), p.read_bytes()) for p in source.rglob("*") if p.is_file())
        assert after == before
        client.load_collection("plain")
        assert query(client, "plain") == [(1, "POINT(1 1)")]
        client.drop_collection("arr")
        assert sorted(client.list_collections()) == ["arr_raw", "plain"]
        assert not source.exists()
    finally:
        client.close()


def test_geometry_fields_named_like_rerank_virtual_columns_match_3x(pair):
    """名字与 rerank 虚拟列相同的 GEOMETRY 字段：链算出的值按 3.x 的 `_coerce_for` 以 `str(v)` 写进这一列。"""
    d = pair.open()

    def create(c):
        schema = MilvusClient.create_schema(auto_id=False)
        schema.add_field("id", DataType.INT64, is_primary=True)
        schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
        schema.add_field("age", DataType.INT64)
        schema.add_field("_decay_score", DataType.GEOMETRY, nullable=True)
        c.create_collection("v", schema=schema, index_params=index("L2"))
        c.load_collection("v")
        c.insert("v", [{"id": i, "vector": [1.0, 0.25 * i], "age": i, "_decay_score": f"POINT({i} {i})"}
                       for i in range(4)])
    d.step("create", create)
    decay = Function("d", FunctionType.RERANK, ["age"],
                     params={"reranker": "decay", "function": "linear", "origin": 0, "scale": 4})
    for output in (["_decay_score"], ["_decay_score", "age"], ["*"]):
        d.step(f"plain {output}", lambda c, output=output: [
            [(h["id"], h["entity"]) for h in x] for x in c.search("v", data=[[1.0, 0.0]], limit=4, output_fields=output)])
        d.step(f"decay {output}", lambda c, output=output: [
            [(h["id"], h["entity"]) for h in x]
            for x in c.search("v", data=[[1.0, 0.0]], limit=4, output_fields=output, ranker=decay)])
    # 存储的 WKT 被衰减因子的文本盖掉：这一列此时不是合法 WKT，与 3.x 相同。
    result = d.step("factor text", lambda c: [
        h["entity"]["_decay_score"] for h in c.search("v", data=[[1.0, 0.0]], limit=4, output_fields=["_decay_score"], ranker=decay)[0]])
    assert result[0] == "ok" and all(not text.startswith("POINT") for text in result[1]), result


INTERMEDIATE = ["geo is null", "geo == geo", "geo == 1", "text_match(geo, 'x')", "ST_ISVALID(geo)"]


@pytest.mark.parametrize("expr", INTERMEDIATE)
def test_filters_on_geometry_fields_are_unimplemented_until_step_two(pair, expr):
    d = pair.open()
    c = d.rust
    collection(c, "f", nullable=True)
    c.insert("f", [{"id": 1, "vector": [0.0, 0.0], "geo": "POINT(1 1)"}, {"id": 2, "vector": [1.0, 0.0], "geo": None}])
    boost = Function(name="b", input_field_names=[], function_type=FunctionType.RERANK,
                     params={"reranker": "boost", "filter": expr, "weight": 0.5})

    def query_iterator():
        iterator = c.query_iterator("f", batch_size=1, filter=expr)
        try:
            return iterator.next()
        finally:
            iterator.close()

    def search_iterator():
        iterator = c.search_iterator("f", data=[[0.0, 0.0]], batch_size=1, filter=expr)
        try:
            return iterator.next()
        finally:
            iterator.close()
    calls = {
        "query": lambda: c.query("f", filter=expr),
        "count": lambda: c.query("f", filter=expr, output_fields=["count(*)"]),
        "delete": lambda: c.delete("f", filter=expr),
        "search": lambda: c.search("f", data=[[0.0, 0.0]], limit=2, filter=expr),
        "boost": lambda: c.search("f", data=[[0.0, 0.0]], limit=2, ranker=boost),
        "hybrid": lambda: c.hybrid_search("f", [AnnSearchRequest([[0.0, 0.0]], "vector", {"metric_type": "L2"}, limit=2, expr=expr)], RRFRanker(), limit=2),
        "query iterator": query_iterator,
        "search iterator": search_iterator,
    }
    for name, call in calls.items():
        result = run(lambda _: call(), c)
        assert result == (
            "rpc", "UNIMPLEMENTED", "MilvusLite does not support filter expressions: geometry predicates are not supported"), f"{name}: {result!r}"
    # 没有一条被执行：两行都还在。
    assert query(c, "f") == [(1, "POINT(1 1)"), (2, None)]
