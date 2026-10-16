"""标量空值与零默认值经真实 SDK 走完整生命周期。"""

from contextlib import contextmanager

import pytest
from pymilvus import DataType, MilvusClient
from milvus_lite.server_manager import ServerManager


@contextmanager
def client_at(path):
    manager = ServerManager()
    client = MilvusClient(manager.start_and_get_uri(str(path)), dedicated=True)
    try:
        yield client
    finally:
        client.close()
        manager.release_all()


SCALARS = [
    (DataType.BOOL, True, False), (DataType.INT8, 7, 0), (DataType.INT16, 7, 0),
    (DataType.INT32, 7, 0), (DataType.INT64, 7, 0), (DataType.FLOAT, 1.5, 0.0),
    (DataType.DOUBLE, 1.5, 0.0), (DataType.VARCHAR, "abc", ""), (DataType.JSON, {"k": 1}, {}),
]


def create(client, dtype, *, nullable, default=None):
    schema = client.create_schema(auto_id=False, enable_dynamic_field=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    kwargs = {"nullable": nullable}
    if dtype == DataType.VARCHAR:
        kwargs["max_length"] = 16
    if default is not None:
        kwargs["default_value"] = default
    schema.add_field("x", dtype, **kwargs)
    indexes = client.prepare_index_params()
    indexes.add_index("vector", index_type="FLAT", metric_type="L2")
    client.create_collection("c", schema=schema, index_params=indexes)


@pytest.mark.parametrize("dtype,good,zero", SCALARS)
def test_nullable_scalars_preserve_masks_across_query_search_flush_reopen_and_delete(tmp_path, dtype, good, zero):
    path = tmp_path / "nullable.db"
    expected = [{"id": 1, "x": None}, {"id": 2, "x": good}, {"id": 3, "x": zero}, {"id": 4, "x": None}]

    def check(client):
        assert list(client.query("c", filter="id >= 0", output_fields=["id", "x"])) == expected
        assert list(client.get("c", [1, 4], output_fields=["x"])) == [expected[0], expected[3]]
        assert list(client.query("c", filter="x is null", output_fields=["id"])) == [{"id": 1}, {"id": 4}]
        hits = client.search("c", [[0.0, 0.0], [0.0, 0.0]], limit=4, output_fields=["x"], search_params={"metric_type": "L2"})
        for query in hits:
            assert [hit["id"] for hit in query] == [1, 2, 3, 4]
            assert [hit["entity"]["x"] for hit in query] == [None, good, zero, None]
        assert list(client.query("c", filter="id >= 0", output_fields=["x"], offset=1, limit=2)) == expected[1:3]

    with client_at(path) as client:
        create(client, dtype, nullable=True)
        client.insert("c", [{"id": i, "vector": [float(i), 0.0], **({"x": x} if i != 4 else {})}
                            for i, x in [(1, None), (2, good), (3, zero), (4, None)]])
        check(client)
        client.flush("c")
        check(client)
    with client_at(path) as client:
        client.load_collection("c")
        check(client)
        assert client.delete("c", filter="x is null") == [1, 4]
        assert list(client.query("c", filter="id >= 0", output_fields=["id", "x"])) == expected[1:3]


@pytest.mark.parametrize("dtype,good,zero", SCALARS[:-1])
@pytest.mark.parametrize("nullable", [False, True])
def test_falsy_defaults_distinguish_partial_missing_explicit_null_and_full_upsert(tmp_path, dtype, good, zero, nullable):
    path = tmp_path / "defaults.db"
    with client_at(path) as client:
        create(client, dtype, nullable=nullable, default=zero)
        client.insert("c", [{"id": 1, "vector": [1.0, 0.0], "x": None},
                            {"id": 2, "vector": [2.0, 0.0]}, {"id": 3, "vector": [3.0, 0.0], "x": good}])
        assert list(client.query("c", filter="id >= 0", output_fields=["id", "x"])) == [
            {"id": 1, "x": zero}, {"id": 2, "x": zero}, {"id": 3, "x": good}]
        client.upsert("c", [{"id": 3}], partial_update=True)
        assert list(client.get("c", [3], output_fields=["x"])) == [{"id": 3, "x": good}]
        client.upsert("c", [{"id": 3, "x": None}], partial_update=True)
        assert list(client.get("c", [3], output_fields=["x"])) == [{"id": 3, "x": zero}]
        client.upsert("c", [{"id": 3, "vector": [3.0, 0.0], "x": good}])
        client.upsert("c", [{"id": 3, "vector": [3.0, 0.0]}])
        assert list(client.get("c", [3], output_fields=["x"])) == [{"id": 3, "x": zero}]
        client.flush("c")
    with client_at(path) as client:
        client.load_collection("c")
        assert list(client.query("c", filter="id >= 0", output_fields=["id", "x"])) == [
            {"id": 1, "x": zero}, {"id": 2, "x": zero}, {"id": 3, "x": zero}]


@pytest.mark.parametrize("dtype,kind,value,expected", [
    (DataType.INT64, "string_data", "9223372036854775808", "Value out of range: 9223372036854775808"),
    (DataType.INT64, "double_data", 9223372036854775808.0, "Value out of range: 9223372036854775808"),
    (DataType.INT64, "double_data", 1e20, "Value out of range: 100000000000000000000"),
    (DataType.INT32, "string_data", "2147483648", "Value out of range: 2147483648"),
])
def test_describe_never_saturates_invalid_integer_defaults(tmp_path, dtype, kind, value, expected):
    from pymilvus.grpc_gen import common_pb2, milvus_pb2, schema_pb2

    with client_at(tmp_path / "wide-default.db") as client:
        stub = client._get_connection()._stub
        x = schema_pb2.FieldSchema(name="x", data_type=dtype)
        setattr(x.default_value, kind, value)
        vector = schema_pb2.FieldSchema(name="vector", data_type=DataType.FLOAT_VECTOR,
                                        type_params=[common_pb2.KeyValuePair(key="dim", value="2")])
        raw = schema_pb2.CollectionSchema(name="c", fields=[
            schema_pb2.FieldSchema(name="id", data_type=DataType.INT64, is_primary_key=True), vector, x])
        result = stub.CreateCollection(milvus_pb2.CreateCollectionRequest(collection_name="c", schema=raw.SerializeToString()))
        assert result.code == 0
        result = stub.DescribeCollection(milvus_pb2.DescribeCollectionRequest(collection_name="c"))
        assert result.status.code == 1 and result.status.error_code == 0
        assert result.status.reason == expected


@pytest.mark.parametrize("dtype,text,slot,expected", [
    (DataType.INT64, "١_٢", "long_data", 12),
    (DataType.DOUBLE, "١.٥", "double_data", 1.5),
    (DataType.INT64, "0" * 4301, None, "Exceeds the limit (4300 digits) for integer string conversion: value has 4301 digits; use sys.set_int_max_str_digits() to increase the limit"),
])
def test_describe_default_unicode_and_digit_limits(tmp_path, dtype, text, slot, expected):
    from pymilvus.grpc_gen import common_pb2, milvus_pb2, schema_pb2

    with client_at(tmp_path / "unicode-default.db") as client:
        stub=client._get_connection()._stub
        x=schema_pb2.FieldSchema(name="x",data_type=dtype);x.default_value.string_data=text
        raw=schema_pb2.CollectionSchema(name="c",fields=[schema_pb2.FieldSchema(name="id",data_type=DataType.INT64,is_primary_key=True),
            schema_pb2.FieldSchema(name="vector",data_type=DataType.FLOAT_VECTOR,type_params=[common_pb2.KeyValuePair(key="dim",value="2")]),x])
        result=stub.CreateCollection(milvus_pb2.CreateCollectionRequest(collection_name="c",schema=raw.SerializeToString()))
        assert result.code==0
        result=stub.DescribeCollection(milvus_pb2.DescribeCollectionRequest(collection_name="c"))
        if slot is None:
            assert (result.status.code,result.status.error_code,result.status.reason)==(1,0,expected)
        else:
            assert result.status.code==0
            assert result.schema.fields[-1].default_value.WhichOneof("data")==slot
            assert getattr(result.schema.fields[-1].default_value,slot)==expected
