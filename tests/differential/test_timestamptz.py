"""0025：TIMESTAMPTZ 类型（写入形态、输出格式、默认值、nullable、schema 规则）对照 3.x（43d1257）。

本文件覆盖第 1 步（带偏移的时间字符串）；过滤与时区在后续步骤追加。已登记的差异单独断言：
越界整数微秒（本实现拒绝，3.x 写入后毒化读取）、使用越界整数默认值的写入（同理；创建与描述两边一致）与
`ARRAY<TIMESTAMPTZ>`（本实现 UNIMPLEMENTED）。
"""

from pathlib import Path

import pytest
from pymilvus import DataType, MilvusClient
from pymilvus.grpc_gen import milvus_pb2, schema_pb2

from compare import run
from test_reopen import pair  # noqa: F401  （fixture）
from test_scenarios import index

INSERT_FORMS = [
    "2025-01-01T00:00:00Z", "2025-01-01T00:00:00+08:00", "2025-01-01T00:00:00-05:30", "2025-01-01T00:00:00+0800",
    "2025-01-01T00:00:00+08", "2025-01-01T00:00:00+08:00:30", "2025-01-01T00:00:00.123456+08:00",
    "2025-01-01T00:00:00.123456789Z", "2025-01-01T00:00:00.5Z", "2025-01-01T00:00:00.1234567Z",
    "2025-01-01", "2025-01-01T00:00:00", "2025-01-01 00:00:00Z", "2025-01-01 00:00:00+00:00", "20250101T000000Z",
    "2025-W01-1T00:00:00Z", "2025-001T00:00:00Z", "2025-01-01T24:00:00Z", "2025-01-01T23:59:60Z",
    "0001-01-01T00:00:00Z", "0001-01-01T00:00:00+01:00", "9999-12-31T23:59:59.999999Z",
    "9999-12-31T23:59:59.999999-01:00", "0000-01-01T00:00:00Z", "10000-01-01T00:00:00Z", " 2025-01-01T00:00:00Z ",
    "2025-01-01t00:00:00z", "2025-01-01T00:00:00 Z", "2024-02-29T12:00:00Z", "2023-02-29T12:00:00Z", "", "abc",
    "2025-13-01T00:00:00Z", "2025-01-01T00:00:00+25:00", "2025-01-01T00:00:00-00:00", "2025-01-01T00:00:00.000000Z",
    "1970-01-01T00:00:00Z", "1969-12-31T23:59:59.999999Z", 0, 1735689600000000, 1.5, True, None,
]


def collection(client, name, *, nullable=True, default=None, extra=False):
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    kwargs = {"nullable": nullable}
    if default is not None:
        kwargs["default_value"] = default
    schema.add_field("ts", DataType.TIMESTAMPTZ, **kwargs)
    if extra:
        schema.add_field("ts2", DataType.TIMESTAMPTZ, nullable=True)
    client.create_collection(name, schema=schema, index_params=index("L2"))
    client.load_collection(name)


def query(client, name, expr="id >= 0", fields=("id", "ts")):
    return sorted(
        (tuple(row[f] for f in fields)) for row in client.query(name, filter=expr, output_fields=list(fields))
    )


def test_insert_forms_and_output_match_3x(pair):
    d = pair.open()
    d.step("create", lambda c: collection(c, "t"))
    for i, value in enumerate(INSERT_FORMS):
        def write(c, i=i, value=value):
            c.insert("t", [{"id": i, "vector": [0.0, 0.0], "ts": value}])
            return query(c, "t", f"id == {i}")
        a = run(write, d.rust)
        b = run(write, d.ref)
        assert a == b and repr(a) == repr(b), f"{value!r}\n  rust: {a!r}\n  3.x:  {b!r}"
    d.step("all", lambda c: query(c, "t"))
    pair.close()
    d = pair.open()
    d.step("load", lambda c: c.load_collection("t"))
    d.step("reopened", lambda c: query(c, "t"))
    d.step("flush", lambda c: c.flush("t"))
    d.step("flushed", lambda c: query(c, "t"))


def test_nullable_default_upsert_delete_and_describe_match_3x(pair):
    d = pair.open()
    d.step("create nullable", lambda c: collection(c, "n", nullable=True, extra=True))
    d.step("create not nullable", lambda c: collection(c, "nn", nullable=False))
    d.step("create default aware", lambda c: collection(c, "da", default="2025-01-01T08:00:00+08:00"))
    d.step("create default int", lambda c: collection(c, "di", default=1735689600000000))
    d.step("create default naive", lambda c: collection(c, "dn", default="2025-01-01T08:00:00"), error=True)
    d.step("create default bad", lambda c: collection(c, "db", default="xx"), error=True)
    d.step("create default out of range offset", lambda c: collection(c, "dr", default="0001-01-01T00:00:00+01:00"), error=True)
    d.step("insert nullable", lambda c: c.insert("n", [
        {"id": 1, "vector": [0.0, 0.0], "ts": "2025-01-02T12:30:00.5Z", "ts2": "2000-01-01T00:00:00+01:00"},
        {"id": 2, "vector": [0.0, 0.0]}, {"id": 3, "vector": [0.0, 0.0], "ts": None, "ts2": None}]))
    d.step("not nullable missing", lambda c: c.insert("nn", [{"id": 1, "vector": [0.0, 0.0]}]), error=True)
    d.step("not nullable none", lambda c: c.insert("nn", [{"id": 1, "vector": [0.0, 0.0], "ts": None}]), error=True)
    d.step("default aware insert", lambda c: (c.insert("da", [{"id": 1, "vector": [0.0, 0.0]}, {"id": 2, "vector": [0.0, 0.0], "ts": None}]), query(c, "da"))[1])
    d.step("default int insert", lambda c: (c.insert("di", [{"id": 1, "vector": [0.0, 0.0]}]), query(c, "di"))[1])
    d.step("nullable rows", lambda c: query(c, "n", fields=("id", "ts", "ts2")))
    d.step("upsert", lambda c: (c.upsert("n", [{"id": 1, "vector": [0.0, 0.0], "ts": "2030-01-01T00:00:00Z"}]), query(c, "n", fields=("id", "ts", "ts2")))[1])
    d.step("delete", lambda c: (c.delete("n", ids=[2]), query(c, "n", fields=("id", "ts", "ts2")))[1])
    d.step("get", lambda c: [(r["id"], r.get("ts")) for r in c.get("n", ids=[1, 3], output_fields=["ts"])])
    d.step("search output", lambda c: [[(h["id"], h["entity"].get("ts")) for h in x] for x in c.search("n", data=[[0.0, 0.0]], limit=5, output_fields=["ts", "ts2"])])
    d.step("output *", lambda c: query(c, "n", fields=("id", "ts", "ts2")))
    # 落盘形态：schema.json（dtype、微秒默认值）两边逐字节相同。
    for name in ("n", "da", "di"):
        files = [(Path(root) / "collections" / name / "schema.json").read_bytes() for root in (pair.rust_path, pair.ref_path)]
        assert files[0] == files[1], f"{name}\n  rust: {files[0]!r}\n  3.x:  {files[1]!r}"
    d.step("describe types", lambda c: [(f["name"], str(f["type"])) for f in c.describe_collection("da")["fields"]])

    def default_echo(c):
        stub = c._get_connection()._stub
        reply = stub.DescribeCollection(milvus_pb2.DescribeCollectionRequest(collection_name="da"))
        field = next(f for f in reply.schema.fields if f.name == "ts")
        return field.default_value.WhichOneof("data"), field.default_value.timestamptz_data
    result = d.step("describe default", default_echo)
    assert result == ("ok", ["timestamptz_data", 1735689600000000])
    d.step("group by", lambda c: c.search("n", data=[[0.0, 0.0]], limit=2, group_by_field="ts"), error=True)


def test_schema_rules_and_registered_differences(pair):
    d = pair.open()
    for name in ("iso", "ISO", "interval", "INTERVAL"):
        def reserved(c, name=name):
            s = MilvusClient.create_schema(auto_id=False)
            s.add_field("id", DataType.INT64, is_primary=True)
            s.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
            s.add_field(name, DataType.INT64)
            c.create_collection("r_" + name, schema=s)
        d.step(f"reserved {name}", reserved, error=True)

    def array_of_timestamps(c):
        s = MilvusClient.create_schema(auto_id=False)
        s.add_field("id", DataType.INT64, is_primary=True)
        s.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
        s.add_field("a", DataType.ARRAY, element_type=DataType.TIMESTAMPTZ, max_capacity=2)
        c.create_collection("arr", schema=s)
    # 已登记差异：3.x 能建表，本实现 UNIMPLEMENTED。
    assert run(array_of_timestamps, d.ref)[0] == "ok"
    rust = run(array_of_timestamps, d.rust)
    assert rust[:2] == ("rpc", "UNIMPLEMENTED"), rust

    # 原始 gRPC 的整数微秒：范围内两边一致；越界 3.x 写入成功（之后读取毒化），本实现拒绝（已登记）。
    d.step("create raw", lambda c: collection(c, "raw"))

    def raw_insert(c, values):
        stub = c._get_connection()._stub
        ids = schema_pb2.FieldData(type=5, field_name="id")
        ids.scalars.long_data.data.extend(range(1000, 1000 + len(values)))
        vec = schema_pb2.FieldData(type=101, field_name="vector")
        vec.vectors.dim = 2
        vec.vectors.float_vector.data.extend([0.0] * (2 * len(values)))
        ts = schema_pb2.FieldData(type=26, field_name="ts")
        ts.scalars.timestamptz_data.data.extend(values)
        reply = stub.Insert(milvus_pb2.InsertRequest(collection_name="raw", num_rows=len(values), fields_data=[ids, vec, ts]))
        return reply.status.code, reply.status.reason
    in_range = [0, 253402300799999999, -62135596800000000, -1]
    a, b = raw_insert(d.rust, in_range), raw_insert(d.ref, in_range)
    assert a == b == (0, "")
    d.step("raw rows", lambda c: query(c, "raw", "id >= 1000"))
    for bad in (253402300800000000, -62135596800000001, 2**62, 2**63 - 1, -(2**63)):
        assert raw_insert(d.rust, [bad]) == (1, "date value out of range")
    assert d.rust.query("raw", filter="", output_fields=["count(*)"]) == [{"count(*)": 4}]
    # 3.x 写入成功，之后读到该行的查询整体失败。
    assert raw_insert(d.ref, [2**62]) == (0, "")
    with pytest.raises(Exception, match="date value out of range"):
        d.ref.query("raw", filter="id >= 1000", output_fields=["ts"])


def test_varchar_slot_strings_and_out_of_range_defaults(pair):
    d = pair.open()
    d.step("create", lambda c: collection(c, "s"))

    def raw_string_insert(c, values, row_id, valid=None):
        stub = c._get_connection()._stub
        n = len(valid) if valid else len(values)
        ids = schema_pb2.FieldData(type=5, field_name="id")
        ids.scalars.long_data.data.extend(range(row_id, row_id + n))
        vec = schema_pb2.FieldData(type=101, field_name="vector")
        vec.vectors.dim = 2
        vec.vectors.float_vector.data.extend([0.0] * (2 * n))
        ts = schema_pb2.FieldData(type=21, field_name="ts")  # VARCHAR 槽：3.x 由引擎按 schema 解析
        ts.scalars.string_data.data.extend(values)
        ts.valid_data.extend(valid or [])
        reply = stub.Insert(milvus_pb2.InsertRequest(collection_name="s", num_rows=n, fields_data=[ids, vec, ts]))
        return reply.status.code, reply.status.reason
    cases = [["2025-01-01T00:00:00Z"], ["2025-01-01T08:00:00.1234567+08:00"], ["2025-01-01T00:00:00"], ["not a time"], [""],
             ["0001-01-01T00:00:00+08:00"], ["9999-12-31T23:59:59-08:00"], ["2025-01-01T00:00:00Z", "bad"]]
    for i, values in enumerate(cases):
        a, b = raw_string_insert(d.rust, values, 100 * (i + 1)), raw_string_insert(d.ref, values, 100 * (i + 1))
        assert a == b, f"{values!r}\n  rust: {a!r}\n  3.x:  {b!r}"
    a = raw_string_insert(d.rust, ["", "2030-01-01T00:00:00Z"], 900, valid=[False, True])
    assert a == raw_string_insert(d.ref, ["", "2030-01-01T00:00:00Z"], 900, valid=[False, True]) == (0, "")
    d.step("rows", lambda c: query(c, "s"))

    # 整数默认值的范围 3.x 创建时不查：创建、描述、显式给值的写入两边一致；
    # 使用默认值的写入 3.x 接受（之后读取毒化），本实现报 1 且不落盘（已登记）。
    for name, default in (("dmax", 253402300800000000), ("dmin", -62135596800000001)):
        d.step(f"create {name}", lambda c, name=name, default=default: collection(c, name, default=default))
        d.step(f"describe {name}", lambda c, name=name: str(next(f for f in c.describe_collection(name)["fields"] if f["name"] == "ts")["default_value"]))
        d.step(f"explicit {name}", lambda c, name=name: (c.insert(name, [{"id": 2, "vector": [0.0, 0.0], "ts": "2025-01-01T00:00:00Z"}]), query(c, name))[1])
        with pytest.raises(Exception, match="date value out of range"):
            d.rust.insert(name, [{"id": 1, "vector": [0.0, 0.0]}])
        assert query(d.rust, name) == [(2, "2025-01-01T00:00:00Z")]
        d.ref.insert(name, [{"id": 1, "vector": [0.0, 0.0]}])
        with pytest.raises(Exception, match="date value out of range"):
            d.ref.query(name, filter="id >= 0", output_fields=["ts"])
    pair.close()
    d = pair.open()
    d.step("list after reopen", lambda c: sorted(c.list_collections()))
    d.step("describe after reopen", lambda c: str(next(f for f in c.describe_collection("dmax")["fields"] if f["name"] == "ts")["default_value"]))
