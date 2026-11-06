"""gRPC 翻译层的端到端用例：pymilvus 的常用路径，以及用手写请求构造的边界输入。

期望值取自 3.x（v3.2.1）在 pymilvus 3.0.2 / 2.6.17 下的实测；与 3.x 不同之处对应
docs/decisions/0004-m1-scope.md「决策」，在用例中注明。
"""

from __future__ import annotations

import json
import struct

import grpc
import pytest
from pymilvus import DataType, MilvusClient
from pymilvus.exceptions import MilvusException
from pymilvus.grpc_gen import common_pb2, milvus_pb2, milvus_pb2_grpc, schema_pb2

from milvus_lite.server_manager import ServerManager
from rawproto import raw_insert

MAX = 256 * 1024 * 1024


@pytest.fixture
def env(tmp_path):
    manager = ServerManager()
    path = str(tmp_path / "t.db")
    uri = manager.start_and_get_uri(path)
    channel = grpc.insecure_channel(
        uri.removeprefix("http://"),
        options=[("grpc.max_receive_message_length", -1), ("grpc.max_send_message_length", -1)],
    )
    stub = milvus_pb2_grpc.MilvusServiceStub(channel)
    client = MilvusClient(uri=uri)
    yield client, stub, channel
    client.close()
    channel.close()
    manager.release_all()


def status_of(resp):
    s = resp if isinstance(resp, common_pb2.Status) else resp.status
    return s.code, s.error_code, s.reason


def unimplemented(call, *args, **kwargs):
    with pytest.raises(grpc.RpcError) as info:
        call(*args, **kwargs)
    assert info.value.code() == grpc.StatusCode.UNIMPLEMENTED, info.value.details()
    assert info.value.details().startswith("MilvusLite does not support ")
    return info.value.details()


def schema_bytes(*fields, auto_id=False, dynamic=False, functions=()):
    s = schema_pb2.CollectionSchema(name="ignored", autoID=auto_id, enable_dynamic_field=dynamic)
    s.fields.extend(fields)
    s.functions.extend(functions)
    return s.SerializeToString()


def field(name, dtype, primary=False, **params):
    f = schema_pb2.FieldSchema(name=name, data_type=dtype, is_primary_key=primary)
    for k, v in params.items():
        f.type_params.add(key=k, value=str(v))
    return f


def create(stub, name, *fields, **kw):
    return stub.CreateCollection(
        milvus_pb2.CreateCollectionRequest(collection_name=name, schema=schema_bytes(*fields, **kw))
    )


def basic(stub, name="c", dynamic=True):
    """id INT64 主键、v 二维向量、t VARCHAR。"""
    resp = create(
        stub,
        name,
        field("id", DataType.INT64, primary=True),
        field("v", DataType.FLOAT_VECTOR, dim=2),
        field("t", DataType.VARCHAR, max_length=16),
        dynamic=dynamic,
    )
    assert status_of(resp) == (0, 0, "")


def long_fd(name, values):
    fd = schema_pb2.FieldData(type=DataType.INT64, field_name=name)
    fd.scalars.long_data.data.extend(values)
    return fd


def str_fd(name, values):
    fd = schema_pb2.FieldData(type=DataType.VARCHAR, field_name=name)
    fd.scalars.string_data.data.extend(values)
    return fd


def vec_fd(name, rows, dim=2):
    fd = schema_pb2.FieldData(type=DataType.FLOAT_VECTOR, field_name=name)
    fd.vectors.dim = dim
    fd.vectors.float_vector.data.extend([x for r in rows for x in r])
    return fd


def json_fd(name, values, dynamic=False):
    fd = schema_pb2.FieldData(type=DataType.JSON, field_name=name, is_dynamic=dynamic)
    fd.scalars.json_data.data.extend(values)
    return fd


def insert(stub, name, *fds, num_rows=None, **kw):
    n = num_rows if num_rows is not None else max(
        (len(fd.scalars.long_data.data) for fd in fds if fd.HasField("scalars")), default=0
    )
    return stub.Insert(milvus_pb2.InsertRequest(collection_name=name, fields_data=fds, num_rows=n, **kw))


def seed(stub, name="c"):
    resp = insert(
        stub,
        name,
        long_fd("id", [1, 2, 3, 4, 5]),
        vec_fd("v", [[i, i / 2] for i in range(1, 6)]),
        str_fd("t", [f"t{i}" for i in range(1, 6)]),
        json_fd("$meta", [json.dumps({"zz": i, "aa": f"a{i}"}).encode() for i in range(1, 6)], dynamic=True),
    )
    assert status_of(resp)[0] == 0, resp


def l2_index(stub, name="c"):
    r = stub.CreateIndex(milvus_pb2.CreateIndexRequest(
        collection_name=name, field_name="v",
        extra_params=[common_pb2.KeyValuePair(key="index_type", value="FLAT"),
                      common_pb2.KeyValuePair(key="metric_type", value="L2")]))
    assert status_of(r) == (0, 0, "")


def search_req(name="c", vectors=([0.0, 0.0],), params=None, **kw):
    pg = common_pb2.PlaceholderGroup()
    pv = pg.placeholders.add(tag="$0", type=101)
    for v in vectors:
        pv.values.append(struct.pack(f"<{len(v)}f", *v))
    sp = {"topk": "3", "round_decimal": "-1", "params": "{}"}
    sp.update(params or {})
    return milvus_pb2.SearchRequest(
        collection_name=name,
        placeholder_group=pg.SerializeToString(),
        search_params=[common_pb2.KeyValuePair(key=k, value=v) for k, v in sp.items()],
        **kw,
    )


def query_req(name="c", expr="", output_fields=(), **kw):
    return milvus_pb2.QueryRequest(collection_name=name, expr=expr, output_fields=list(output_fields), **kw)


def columns(fields_data):
    return [fd.field_name for fd in fields_data]


def meta_rows(fields_data):
    fd = next(fd for fd in fields_data if fd.field_name == "$meta")
    return [bytes(b) for b in fd.scalars.json_data.data]


# ── collection 生命周期与元数据 ─────────────────────────────────


def test_quick_start_through_pymilvus(env):
    client, _, _ = env
    client.create_collection("q", dimension=4)
    assert client.list_collections() == ["q"]
    desc = client.describe_collection("q")
    assert desc["auto_id"] is False and desc["enable_dynamic_field"] is True
    assert [f["name"] for f in desc["fields"]] == ["id", "vector"]
    rows = [{"id": i, "vector": [i * 0.1, 1 - i * 0.1, 0.5, 0.2], "text": f"t{i}"} for i in range(5)]
    assert client.insert("q", rows)["ids"] == [0, 1, 2, 3, 4]
    assert client.query("q", filter="", output_fields=["count(*)"])[0]["count(*)"] == 5
    assert client.get_collection_stats("q") == {"row_count": 5}
    hits = client.search("q", data=[[0.1, 0.9, 0.5, 0.2]], limit=2)[0]
    assert [h["id"] for h in hits] == [1, 0]
    # 不指定 output_fields 时 3.x 返回主键以外的全部字段，包括向量与动态字段。
    assert set(hits[0]["entity"]) == {"id", "vector", "text"}
    got = client.get("q", ids=[3, 1, 3, 99])
    assert [r["id"] for r in got] == [3, 1, 3]
    assert client.has_collection("q") and not client.has_collection("nope")
    idx = client.describe_index("q", "vector")
    assert idx["index_type"] == "AUTOINDEX" and idx["metric_type"] == "COSINE"
    assert str(client.get_load_state("q")["state"]) == "Loaded"
    client.release_collection("q")
    assert str(client.get_load_state("q")["state"]) == "NotLoad"
    client.load_collection("q")
    client.drop_collection("q")
    client.drop_collection("q")
    assert client.list_collections() == []


def test_describe_collection_fields_and_properties(env):
    _, stub, _ = env
    props = [("a", "1"), ("b", "2"), ("", "x"), ("a", "3")]
    resp = stub.CreateCollection(
        milvus_pb2.CreateCollectionRequest(
            collection_name="c",
            schema=schema_bytes(
                field("id", DataType.INT64, primary=True),
                field("v", DataType.FLOAT_VECTOR, dim=" 4"),
                field("t", DataType.VARCHAR),
                auto_id=True,
            ),
            properties=[common_pb2.KeyValuePair(key=k, value=v) for k, v in props],
            shards_num=3,
        )
    )
    assert status_of(resp) == (0, 0, "")
    d = stub.DescribeCollection(milvus_pb2.DescribeCollectionRequest(collection_name="c"))
    assert d.collection_name == "c" and d.schema.name == "c"
    assert d.shards_num == 1 and d.num_partitions == 1
    assert d.schema.autoID and not d.schema.enable_dynamic_field
    # Python dict 语义：空 key 丢弃，同名 key 保留首次出现的位置、取最后的值。
    assert [(p.key, p.value) for p in d.properties] == [("a", "3"), ("b", "2")]
    fields = {f.name: f for f in d.schema.fields}
    assert fields["id"].autoID and fields["id"].is_primary_key
    assert [(p.key, p.value) for p in fields["v"].type_params] == [("dim", "4")]
    assert list(fields["t"].type_params) == []
    assert all(f.fieldID == 0 for f in d.schema.fields)


def test_show_collections_and_statistics(env):
    _, stub, _ = env
    basic(stub, "b")
    basic(stub, "a")
    l2_index(stub, "b")
    stub.ReleaseCollection(milvus_pb2.ReleaseCollectionRequest(collection_name="b"))
    r = stub.ShowCollections(milvus_pb2.ShowCollectionsRequest())
    assert list(r.collection_names) == ["a", "b"]
    assert list(r.inMemory_percentages) == [100, 0]
    assert list(r.query_service_available) == [True, False]
    r = stub.ShowCollections(milvus_pb2.ShowCollectionsRequest(collection_names=["b", "zz"]))
    assert list(r.collection_names) == ["b"]
    seed(stub, "a")
    s = stub.GetCollectionStatistics(milvus_pb2.GetCollectionStatisticsRequest(collection_name="a"))
    assert [(kv.key, kv.value) for kv in s.stats] == [("row_count", "5")]


def test_database_names(env):
    _, stub, _ = env
    basic(stub)
    foo = [("dbname", "foo")]
    # ShowCollections 只有通用异常分支，其余 RPC 按 DatabaseNotFound 映射。
    assert status_of(stub.ShowCollections(milvus_pb2.ShowCollectionsRequest(), metadata=foo)) == (
        1, 0, "database 'foo' does not exist")
    for resp in [
        stub.DescribeCollection(milvus_pb2.DescribeCollectionRequest(collection_name="c"), metadata=foo),
        stub.GetLoadState(milvus_pb2.GetLoadStateRequest(collection_name="c"), metadata=foo),
        stub.DropCollection(milvus_pb2.DropCollectionRequest(collection_name="c", db_name="foo")),
    ]:
        assert status_of(resp) == (6, 6, "database 'foo' does not exist")
    r = stub.DescribeCollection(milvus_pb2.DescribeCollectionRequest(collection_name="c", db_name="a/b"))
    assert status_of(r) == (1, 0, "invalid database name: 'a/b'")
    # 请求中的 db_name 优先于 metadata。
    r = stub.DescribeCollection(milvus_pb2.DescribeCollectionRequest(collection_name="c", db_name="default"),
                                metadata=foo)
    assert status_of(r)[0] == 0
    # CreateCollection 先翻译 schema 再查库。
    r = stub.CreateCollection(milvus_pb2.CreateCollectionRequest(
        collection_name="x", db_name="foo", schema=schema_bytes(field("h", DataType.FLOAT16_VECTOR, dim=2))))
    assert status_of(r)[:2] == (6, 6) and "Float16Vector" in status_of(r)[2]


def test_missing_collection_codes(env):
    _, stub, _ = env
    missing = (100, 4, "collection 'x' does not exist")
    assert status_of(stub.DescribeCollection(milvus_pb2.DescribeCollectionRequest(collection_name="x"))) == missing
    assert status_of(stub.LoadCollection(milvus_pb2.LoadCollectionRequest(collection_name="x"))) == missing
    assert status_of(stub.GetLoadingProgress(milvus_pb2.GetLoadingProgressRequest(collection_name="x"))) == missing
    r = stub.GetLoadState(milvus_pb2.GetLoadStateRequest(collection_name="x"))
    assert status_of(r)[0] == 0 and r.state == common_pb2.LoadStateNotExist
    assert status_of(stub.DropCollection(milvus_pb2.DropCollectionRequest(collection_name="x")))[0] == 0
    basic(stub)
    r = create(stub, "c", field("id", DataType.INT64, primary=True), field("v", DataType.FLOAT_VECTOR, dim=2))
    assert status_of(r) == (1, 1, "collection 'c' already exists")
    r = create(stub, "", field("id", DataType.INT64, primary=True), field("v", DataType.FLOAT_VECTOR, dim=2))
    assert status_of(r) == (1, 0, "collection name must not be empty")


def test_create_collection_translation_errors(env):
    _, stub, _ = env
    pk = field("id", DataType.INT64, primary=True)
    r = create(stub, "c", pk, field("h", DataType.FLOAT16_VECTOR, dim=2))
    assert status_of(r)[:2] == (6, 6)
    assert status_of(r)[2].startswith(
        "field 'h' uses Milvus type Float16Vector which MilvusLite does not support. Phase 10.2 supports: ")
    r = create(stub, "c", pk, field("v", DataType.FLOAT_VECTOR))
    assert status_of(r) == (6, 6, "FLOAT_VECTOR field 'v' is missing required 'dim' type_param")
    r = create(stub, "c", pk, field("v", DataType.FLOAT_VECTOR, dim="abc"))
    assert status_of(r) == (6, 6, "FLOAT_VECTOR field 'v' has non-integer dim 'abc'")
    r = create(stub, "c", pk, field("v", DataType.FLOAT_VECTOR, dim=2), field("t", DataType.VARCHAR, max_length="x"))
    assert status_of(r) == (6, 6, "VARCHAR field 't' has non-integer max_length 'x'")
    r = create(stub, "c", pk, field("v", DataType.FLOAT_VECTOR, dim=0))
    assert status_of(r) == (6, 6, "vector field 'v' requires dim > 0, got 0")
    r = stub.CreateCollection(milvus_pb2.CreateCollectionRequest(collection_name="c", schema=b"\xff\xff"))
    assert status_of(r) == (
        1, 0, "Error parsing message with type 'milvus.proto.schema.CollectionSchema': Wire format was corrupt")


def test_create_collection_values_3x_treats_as_unset(env):
    _, stub, _ = env
    pk = field("id", DataType.INT64, primary=True)
    v = field("v", DataType.FLOAT_VECTOR, dim=2)
    nullable = field("n", DataType.INT64)
    nullable.nullable = True
    # 3.x 翻译阶段的错误先于 M1 不支持的功能报告。
    r = create(stub, "c", pk, v, nullable, field("h", DataType.FLOAT16_VECTOR, dim=2))
    assert status_of(r)[:2] == (6, 6)
    # 这些取值在 3.x 中视同未设置，不算使用了对应的功能。
    r = create(stub, "c", pk, v, field("t", DataType.VARCHAR, max_length=8, enable_analyzer="no",
                                       analyzer_params="{}"))
    assert status_of(r) == (0, 0, "")


def test_create_collection_remaining_unsupported(env):
    _, stub, _ = env
    pk = field("id", DataType.INT64, primary=True)
    v = field("v", DataType.FLOAT_VECTOR, dim=2)
    arr = field("a", DataType.ARRAY, max_capacity=4)
    arr.element_type = DataType.INT32
    assert status_of(create(stub, "c", pk, v, arr)) == (0, 0, "")
    assert status_of(stub.DropCollection(milvus_pb2.DropCollectionRequest(collection_name="c"))) == (0, 0, "")
    # 0015 B 已接入八种标量元素；JSON 元素与嵌套 ARRAY 继续明确拒绝。
    arr.element_type = DataType.JSON
    unimplemented(create, stub, "c", pk, v, arr)
    arr.element_type = DataType.ARRAY
    unimplemented(create, stub, "c", pk, v, arr)
    nullable = field("v", DataType.FLOAT_VECTOR, dim=2)
    nullable.nullable = True
    unimplemented(create, stub, "c", pk, nullable)
    unimplemented(create, stub, "c", pk, v, field("t", DataType.VARCHAR, max_length=8, enable_analyzer="True"))
    unimplemented(create, stub, "c", pk, v, field("t", DataType.VARCHAR, max_length=8,
                                                  analyzer_params='{"type": "english", "x": NaN}'))
    # 集合的 timezone 属性随 TIMESTAMPTZ 一起接入（0025）：合法值保存，坏值 6 且不创建。
    def create_with_timezone(name, value):
        return status_of(stub.CreateCollection(milvus_pb2.CreateCollectionRequest(
            collection_name=name, schema=schema_bytes(pk, v),
            properties=[common_pb2.KeyValuePair(key="timezone", value=value)])))
    assert create_with_timezone("tz", "UTC") == (0, 0, "")
    assert create_with_timezone("tz_bad", "Mars/Base") == (6, 6, "unknown timezone 'Mars/Base'")
    assert create_with_timezone("tz_bad", "") == (6, 6, "timezone property must be a non-empty string")
    assert [name for name in stub.ShowCollections(milvus_pb2.ShowCollectionsRequest()).collection_names
            if name.startswith("tz")] == ["tz"]


# ── 写入 ─────────────────────────────────────────────────────


def test_insert_decodes_by_field_data_type(env):
    _, stub, _ = env
    basic(stub)
    wrong_slot = schema_pb2.FieldData(type=DataType.INT64, field_name="id")
    wrong_slot.scalars.float_data.data.append(1.0)
    r = insert(stub, "c", wrong_slot, vec_fd("v", [[1, 0]]), str_fd("t", ["a"]), num_rows=1)
    assert status_of(r) == (6, 6, "FieldData 'id' has 0 rows, expected 1")
    bad = schema_pb2.FieldData(type=999, field_name="x")
    bad.scalars.long_data.data.append(1)
    r = insert(stub, "c", long_fd("id", [1]), bad, num_rows=1)
    assert status_of(r) == (6, 6, "FieldData 'x' uses scalar type Unknown(999) which MilvusLite does not support")
    r = insert(stub, "c", long_fd("id", [1]), schema_pb2.FieldData(type=5, field_name="y"), num_rows=1)
    assert status_of(r) == (6, 6, "FieldData 'y' has neither scalars nor vectors")
    v0 = vec_fd("v", [[1, 0]])
    v0.vectors.dim = 0
    r = insert(stub, "c", long_fd("id", [1]), v0, num_rows=1)
    assert status_of(r) == (6, 6, "FieldData 'v' has invalid dim 0")
    # M1 不支持的列（空值、ARRAY）不影响其后各列上 3.x 的解码错误先报出。
    nulls = str_fd("t", [])
    nulls.valid_data.append(False)
    r = insert(stub, "c", long_fd("id", [1]), nulls, bad, num_rows=1)
    assert status_of(r) == (6, 6, "FieldData 'x' uses scalar type Unknown(999) which MilvusLite does not support")
    arr = schema_pb2.FieldData(type=DataType.ARRAY, field_name="a")
    arr.scalars.array_data.data.add().int_data.data.append(1)
    r = insert(stub, "c", arr, long_fd("id", [1, 2]), num_rows=1)
    assert status_of(r) == (6, 6, "FieldData 'id' has 2 rows, expected 1")
    f16 = schema_pb2.FieldData(type=DataType.FLOAT16_VECTOR, field_name="v")
    f16.vectors.dim = 2
    f16.vectors.float16_vector = b"\x00" * 4
    r = insert(stub, "c", long_fd("id", [1]), f16, num_rows=1)
    assert status_of(r)[2] == ("FieldData 'v' uses vector type Float16Vector which MilvusLite does not support "
                               "(supported: FloatVector, SparseFloatVector)")


def test_insert_results_and_dynamic_columns(env):
    _, stub, _ = env
    basic(stub)
    r = insert(
        stub, "c", long_fd("id", [7, 7]), vec_fd("v", [[1, 0], [0, 1]]), str_fd("t", ["a", "b"]),
        json_fd("extra", [b'{"k": 1}', b'{"k": 2}'], dynamic=True),
    )
    assert status_of(r)[0] == 0
    assert list(r.IDs.int_id.data) == [7, 7] and r.insert_cnt == 2 and list(r.succ_index) == [0, 1]
    assert r.timestamp == 0
    # is_dynamic 的列无论叫什么名字都按 `$meta` 展开。
    q = stub.Query(query_req(expr="id in [7]", output_fields=["k"]))
    assert meta_rows(q.fields_data) == [b'{"k": 2}']
    r = insert(stub, "c", num_rows=0)
    assert status_of(r)[0] == 0 and r.IDs.WhichOneof("id_field") is None
    assert r.insert_cnt == 0 and list(r.succ_index) == []


def test_insert_default_partition_and_all_valid_data(env):
    _, stub, _ = env
    basic(stub)
    r = insert(stub, "c", long_fd("id", [1]), vec_fd("v", [[1, 0]]), str_fd("t", ["a"]), partition_name="_default")
    assert status_of(r)[0] == 0
    t = str_fd("t", ["a"])
    t.valid_data.append(True)
    assert status_of(insert(stub, "c", long_fd("id", [2]), vec_fd("v", [[1, 0]]), t))[0] == 0
    # 向量列按完整长度发送时 3.x 忽略 valid_data，哪怕它的长度不对。
    v = vec_fd("v", [[1, 0], [0, 1]])
    v.valid_data.append(True)
    r = insert(stub, "c", long_fd("id", [3, 4]), v, str_fd("t", ["a", "b"]))
    assert status_of(r)[0] == 0


def test_insert_m1_unsupported(env):
    _, stub, _ = env
    basic(stub)
    r = insert(stub, "c", long_fd("id", [1]), vec_fd("v", [[1, 0]]), str_fd("t", ["a"]), partition_name="p")
    assert status_of(r) == (200, 200, "p")
    t = str_fd("t", [])
    t.valid_data.append(False)
    r = insert(stub, "c", long_fd("id", [3]), vec_fd("v", [[1, 0]]), t)
    assert status_of(r) == (6, 6, "field 't' is None but not nullable")


@pytest.mark.parametrize("valid", [[], [False], [True]])
@pytest.mark.parametrize("mutation", ["Insert", "Upsert"])
def test_bad_json_is_decoded_before_null_masks_and_later_duplicate_fields(env, valid, mutation):
    client, stub, _ = env
    basic(stub)
    bad = schema_pb2.FieldData(type=DataType.JSON, field_name="t", valid_data=valid)
    bad.scalars.json_data.data.append(b"{bad")
    request = getattr(milvus_pb2, mutation + "Request")(
        collection_name="c", num_rows=1,
        fields_data=[long_fd("id", [1]), vec_fd("v", [[1, 0]]), bad, str_fd("t", ["ok"])],
    )
    r = getattr(stub, mutation)(request)
    assert status_of(r) == (6, 6, "field 't' value is not valid JSON: key must be a string at line 1 column 2")
    assert list(client.query("c", filter="id >= 0", output_fields=["id"])) == []


@pytest.mark.parametrize("mutation", ["Insert", "Upsert"])
@pytest.mark.parametrize("json_name", ["j", "$meta"])
@pytest.mark.parametrize("earlier_error", ["missing_pk", "vector_length", "vector_dim", "mask_length"])
def test_bad_json_preserves_protocol_and_business_error_priority(env, mutation, json_name, earlier_error):
    client, stub, _ = env
    assert status_of(create(
        stub, "c", field("id", DataType.INT64, primary=True),
        field("v", DataType.FLOAT_VECTOR, dim=2), field("j", DataType.JSON), dynamic=True,
    )) == (0, 0, "")
    bad = json_fd(json_name, [b"{bad"], dynamic=json_name == "$meta")
    fds = [long_fd("id", [1]), vec_fd("v", [[1, 0]]), bad]
    if json_name == "$meta":
        fds.append(json_fd("j", [b"{} "]))
    if earlier_error == "missing_pk":
        fds.pop(0)
        expected = "primary key 'id' missing or None"
    elif earlier_error == "vector_length":
        fds[1] = vec_fd("v", [[1, 0], [2, 0]])
        expected = "FieldData 'v' float_vector data has 4 elements, expected num_rows(1) * dim(2) = 2"
    elif earlier_error == "vector_dim":
        fds[1] = vec_fd("v", [[1, 0, 0]], dim=3)
        expected = "vector field 'v' expected dim 2, got 3"
    else:
        bad.valid_data.extend([False, True])
        expected = f"FieldData '{json_name}' valid_data length 2 != num_rows 1"
    r = getattr(stub, mutation)(getattr(milvus_pb2, mutation + "Request")(
        collection_name="c", num_rows=1, fields_data=fds,
    ))
    assert status_of(r) == (6, 6, expected)
    assert client.query("c", output_fields=["count(*)"]) == [{"count(*)": 0}]


@pytest.mark.parametrize("mutation", ["Insert", "Upsert"])
@pytest.mark.parametrize("json_name", ["t", "$meta"])
def test_later_row_bad_json_does_not_hide_first_row_business_error(env, mutation, json_name):
    client, stub, _ = env
    basic(stub)
    t = str_fd("t", ["unused", "ok"])
    t.valid_data.extend([False, True])
    bad = json_fd(json_name, [b"{}", b"{bad"], dynamic=json_name == "$meta")
    # typed 的坏 JSON 即使被后列覆盖，也只在其原行的字段检查阶段报告。
    fds = [long_fd("id", [1, 2]), vec_fd("v", [[1, 0], [0, 1]]), bad, t]
    r = getattr(stub, mutation)(getattr(milvus_pb2, mutation + "Request")(
        collection_name="c", num_rows=2, fields_data=fds,
    ))
    assert status_of(r) == (6, 6, "field 't' is None but not nullable")
    assert client.query("c", output_fields=["count(*)"]) == [{"count(*)": 0}]


@pytest.mark.parametrize("mutation", ["Insert", "Upsert"])
@pytest.mark.parametrize("json_name", ["t", "v"])
def test_second_row_shadowed_bad_json_keeps_existing_rows_unchanged(env, mutation, json_name):
    client, stub, _ = env
    basic(stub)
    assert status_of(insert(stub, "c", long_fd("id", [99]), vec_fd("v", [[1, 0]]), str_fd("t", ["seed"]))) == (0, 0, "")
    fds = [long_fd("id", [1, 2]), json_fd(json_name, [b"123", b"{bad"]),
           vec_fd("v", [[1, 0], [0, 1]]), str_fd("t", ["ok", "ok"])]
    r = getattr(stub, mutation)(getattr(milvus_pb2, mutation + "Request")(
        collection_name="c", num_rows=2, fields_data=fds,
    ))
    assert status_of(r) == (6, 6, f"field '{json_name}' value is not valid JSON: key must be a string at line 1 column 2")
    assert client.query("c", output_fields=["count(*)"]) == [{"count(*)": 1}]
    assert list(client.get("c", [1, 2, 99], output_fields=["t"])) == [{"id": 99, "t": "seed"}]


@pytest.mark.parametrize("mutation", ["Insert", "Upsert"])
@pytest.mark.parametrize("vector_problem", ["missing", "dim", "valid"])
def test_json_in_primary_slot_keeps_vector_checks_and_scalar_type_error(env, mutation, vector_problem):
    client, stub, _ = env
    basic(stub)
    fds = [json_fd("id", [b"{bad"]), str_fd("t", ["ok"])]
    if vector_problem != "missing":
        fds.append(vec_fd("v", [[1, 0, 0]] if vector_problem == "dim" else [[1, 0]],
                          dim=3 if vector_problem == "dim" else 2))
    expected = {
        "missing": "vector field 'v' missing or None",
        "dim": "vector field 'v' expected dim 2, got 3",
        "valid": "field 'id' value '{bad' does not match dtype DataType.INT64",
    }[vector_problem]
    r = getattr(stub, mutation)(getattr(milvus_pb2, mutation + "Request")(
        collection_name="c", num_rows=1, fields_data=fds,
    ))
    assert status_of(r) == (6, 6, expected)
    assert client.query("c", output_fields=["count(*)"]) == [{"count(*)": 0}]


@pytest.mark.parametrize("mutation", ["Insert", "Upsert"])
def test_json_in_vector_slot_keeps_vector_type_error(env, mutation):
    client, stub, _ = env
    basic(stub)
    r = getattr(stub, mutation)(getattr(milvus_pb2, mutation + "Request")(
        collection_name="c", num_rows=1,
        fields_data=[long_fd("id", [1]), json_fd("v", [b"{bad"]), str_fd("t", ["ok"])],
    ))
    assert status_of(r) == (6, 6, "vector field 'v' must be list/tuple, got str")
    assert client.query("c", output_fields=["count(*)"]) == [{"count(*)": 0}]


@pytest.mark.parametrize("mutation", ["Insert", "Upsert"])
@pytest.mark.parametrize("auto_id", [False, True])
def test_masked_bad_json_is_not_hidden_by_defaults_or_generated_primary(env, mutation, auto_id):
    client, stub, _ = env
    primary = field("id", DataType.INT64, primary=True)
    primary.autoID = auto_id
    title = field("t", DataType.VARCHAR, max_length=16)
    title.nullable = True
    title.default_value.string_data = "fallback"
    assert status_of(create(stub, "c", primary, field("v", DataType.FLOAT_VECTOR, dim=2), title)) == (0, 0, "")
    name = "id" if auto_id else "t"
    bad = json_fd(name, [b"{bad"])
    bad.valid_data.append(False)
    fds = [vec_fd("v", [[1, 0]]), bad]
    fds.append(str_fd("t", ["ok"]) if auto_id else long_fd("id", [1]))
    r = getattr(stub, mutation)(getattr(milvus_pb2, mutation + "Request")(
        collection_name="c", num_rows=1, fields_data=fds,
    ))
    assert status_of(r) == (6, 6, f"field '{name}' value is not valid JSON: key must be a string at line 1 column 2")
    assert client.query("c", output_fields=["count(*)"]) == [{"count(*)": 0}]


@pytest.mark.parametrize("mutation", ["Insert", "Upsert"])
@pytest.mark.parametrize("raw,expected,valid", [
    (b" \t\r\nnull\r\n\t ", None, True),
    (b'"null"', "null", True),
    (b'{"v": null}', {"v": None}, True),
    (b'[null, 1]', [None, 1], True),
    (b'false', False, True),
    (b'\x0cnull', None, False),
    (b'null\x0b', None, False),
    (b'n ull', None, False),
    (b'null null', None, False),
    (b'\xef\xbb\xbfnull', None, False),
    (b'\xffnull', None, False),
    (b' \r\t\n', None, False),
])
def test_json_null_text_preserves_whitespace_rules_validation_and_stored_values(env, mutation, raw, expected, valid):
    client, stub, _ = env
    column = field("j", DataType.JSON)
    column.nullable = True
    assert status_of(create(stub, "c", field("id", DataType.INT64, primary=True),
                            field("v", DataType.FLOAT_VECTOR, dim=2), column)) == (0, 0, "")
    r = getattr(stub, mutation)(getattr(milvus_pb2, mutation + "Request")(
        collection_name="c", num_rows=1,
        fields_data=[long_fd("id", [1]), vec_fd("v", [[1, 0]]), json_fd("j", [raw])],
    ))
    if valid:
        assert status_of(r) == (0, 0, "")
        assert list(client.get("c", [1], output_fields=["j"])) == [{"id": 1, "j": expected}]
    else:
        code, error_code, reason = status_of(r)
        assert (code, error_code) == (6, 6)
        assert reason.startswith("field 'j' value is not valid JSON:")
        assert client.query("c", output_fields=["count(*)"]) == [{"count(*)": 0}]
        assert status_of(insert(stub, "c", long_fd("id", [2]), vec_fd("v", [[1, 0]]), json_fd("j", [b"{}"]))) == (0, 0, "")
        assert list(client.get("c", [1, 2], output_fields=["j"])) == [{"id": 2, "j": {}}]


@pytest.mark.parametrize("mutation", ["Insert", "Upsert"])
def test_valid_json_in_shadowed_column_keeps_last_column_wins(env, mutation):
    client, stub, _ = env
    basic(stub)
    r = getattr(stub, mutation)(getattr(milvus_pb2, mutation + "Request")(
        collection_name="c", num_rows=1,
        fields_data=[long_fd("id", [1]), vec_fd("v", [[1, 0]]),
                     json_fd("t", [b"123"]), str_fd("t", ["ok"])],
    ))
    assert status_of(r) == (0, 0, "")
    assert list(client.get("c", [1], output_fields=["t"])) == [{"id": 1, "t": "ok"}]


def test_json_field_values_round_trip_m1_differs(env):
    # 0004「决策」：3.x 对顶层数字与布尔报 pyarrow 的错，JSON 字符串值读回时丢了引号。
    client, _, _ = env
    s = MilvusClient.create_schema()
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("j", DataType.JSON)
    client.create_collection("j", schema=s)
    values = [{"a": [1, 2]}, [1, "x", None], 5, True, '"abc"']
    client.insert("j", [{"id": i, "v": [1, 0], "j": v} for i, v in enumerate(values)])
    got = client.get("j", ids=list(range(len(values))), output_fields=["j"])
    assert [r["j"] for r in got] == [{"a": [1, 2]}, [1, "x", None], 5, True, "abc"]


# ── 检索 ─────────────────────────────────────────────────────


def test_search_parameter_parsing(env):
    _, stub, _ = env
    basic(stub)
    seed(stub)
    l2_index(stub)
    r = stub.Search(search_req(params={"topk": "null"}))
    assert status_of(r) == (6, 6, "SearchRequest.search_params is missing required 'topk'")
    r = stub.Search(search_req(params={"topk": "abc"}))
    assert status_of(r) == (6, 6, "SearchRequest.search_params['topk'] must be int, got 'abc'")
    r = stub.Search(search_req(params={"offset": "x"}))
    assert status_of(r) == (1, 0, "invalid literal for int() with base 10: 'x'")
    r = stub.Search(search_req(params={"metric_type": "l2"}))
    assert status_of(r) == (1, 0, "unsupported metric_type 'l2'; expected one of ('COSINE', 'L2', 'IP')")
    assert list(stub.Search(search_req(params={"topk": "2.5"})).results.ids.int_id.data) == [1, 2]
    r = stub.Search(search_req(params={"topk": "0"}))
    assert status_of(r)[0] == 0 and list(r.results.topks) == [0] and r.results.top_k == 0
    # 负 offset 与 3.x 一样使结果变少：取前 topk + offset 个。
    r = stub.Search(search_req(params={"topk": "3", "offset": "-1"}))
    assert list(r.results.ids.int_id.data) == [1, 2]
    r = stub.Search(search_req(params={"topk": "2", "offset": "2"}))
    assert list(r.results.ids.int_id.data) == [3, 4]
    r = stub.Search(search_req(params={"anns_field": "t"}))
    assert status_of(r) == (6, 6, "anns_field 't' is not a vector field (dtype=VARCHAR)")


def test_search_round_decimal_uses_3x_float64_distance(env):
    _, stub, _ = env
    basic(stub)
    seed(stub)
    l2_index(stub)
    r = stub.Search(search_req(params={"topk": "2", "round_decimal": "1"}))
    # [1, 0.5] 到原点的 L2 距离在 3.x 的 float64 中是 1.250000036705515，舍入为 1.3。
    assert list(r.results.scores) == [struct.unpack("f", struct.pack("f", x))[0] for x in (1.3, 5.0)]


@pytest.mark.parametrize("metric,expected", [
    # 3.x 实测：查询向量 [0.6, 0.8]，三行 [0.3, 0.7]、[1.0, 0.1]、[-0.4, 0.9]。
    ("COSINE", {"4": [0.9717000126838684, 0.6765999794006348, 0.48739999532699585],
                "2": [0.9700000286102295, 0.6800000071525574, 0.49000000953674316],
                "0": [1.0, 1.0, 0.0]}),
    ("IP", {"4": [0.7400000095367432, 0.6800000071525574, 0.47999998927116394],
            "2": [0.7400000095367432, 0.6800000071525574, 0.47999998927116394],
            "0": [1.0, 1.0, 0.0]}),
])
def test_search_round_decimal_cosine_and_ip(env, metric, expected):
    _, stub, _ = env
    create(stub, "r", field("id", DataType.INT64, primary=True), field("v", DataType.FLOAT_VECTOR, dim=2))
    stub.CreateIndex(milvus_pb2.CreateIndexRequest(
        collection_name="r", field_name="v",
        extra_params=[common_pb2.KeyValuePair(key="index_type", value="FLAT"),
                      common_pb2.KeyValuePair(key="metric_type", value=metric)]))
    insert(stub, "r", long_fd("id", [1, 2, 3]), vec_fd("v", [[0.3, 0.7], [1.0, 0.1], [-0.4, 0.9]]))
    for rd, scores in expected.items():
        r = stub.Search(search_req("r", vectors=([0.6, 0.8],), params={"round_decimal": rd}))
        assert list(r.results.ids.int_id.data) == [1, 2, 3]
        assert list(r.results.scores) == scores, rd


def test_search_placeholder_errors(env):
    _, stub, _ = env
    basic(stub)
    req = search_req()
    req.placeholder_group = b""
    assert status_of(stub.Search(req)) == (6, 6, "SearchRequest is missing placeholder_group")
    # 能解析、但 placeholders 为空（只有一个未知字段）。
    req.placeholder_group = b"\x10\x01"
    assert status_of(stub.Search(req)) == (6, 6, "PlaceholderGroup is empty")
    req.placeholder_group = b"\xff\xff"
    assert status_of(stub.Search(req)) == (
        1, 0, "Error parsing message with type 'milvus.proto.common.PlaceholderGroup': Wire format was corrupt")

    def with_type(t, values=(b"\x00\x00\x80\x3f",)):
        pg = common_pb2.PlaceholderGroup()
        pg.placeholders.add(tag="$0", type=t, values=list(values))
        req = search_req()
        req.placeholder_group = pg.SerializeToString()
        return stub.Search(req)

    assert status_of(with_type(100)) == (
        6, 6, "PlaceholderValue type 100 not supported (supported: FloatVector=101, SparseFloatVector=104, VarChar=21)")
    assert status_of(with_type(101, [])) == (6, 6, "PlaceholderGroup has no query vectors")
    assert status_of(with_type(101, [b"\x00" * 7])) == (6, 6, "PlaceholderValue blob is not a multiple of 4 bytes (7)")
    assert status_of(with_type(104)) == (1, 0, "sparse vector bytes length 4 is not a multiple of 8")
    assert status_of(with_type(104, [b"\x00" * 8])) == (
        1, 0, "float() argument must be a string or a real number, not 'dict'")
    assert status_of(with_type(21, [b"hello"])) == (
        6, 6, "Text query on field 'v' requires a TEXT_EMBEDDING function targeting that field")


def test_search_parameters_3x_treats_as_absent(env):
    _, stub, _ = env
    basic(stub)
    seed(stub)
    assert status_of(stub.Search(search_req(partition_names=["_default"])))[0] == 0
    assert status_of(stub.Search(search_req(params={"group_by_field": '""', "anns_field": "5"})))[0] == 0
    r = stub.Search(search_req(params={"params": '{"radius": "x"}'}))
    assert status_of(r) == (1, 0, "could not convert string to float: 'x'")
    # load 状态先于 M1 不支持的参数检查。
    l2_index(stub)
    stub.ReleaseCollection(milvus_pb2.ReleaseCollectionRequest(collection_name="c"))
    assert status_of(stub.Search(search_req(dsl="id > 0")))[:2] == (101, 101)


def test_search_m1_unsupported(env):
    _, stub, _ = env
    basic(stub)
    seed(stub)
    unimplemented(stub.Search, search_req(dsl="id + 1 > 0"))
    response = stub.Search(search_req(params={"metric_type": '"L2"', "params": '{"radius": 6, "range_filter": 1}'}))
    assert status_of(response)[0] == 0
    assert list(response.results.ids.int_id.data) == [1, 2]
    empty = stub.Search(search_req(partition_names=["p"]))
    assert status_of(empty)[0] == 0 and not empty.results.ids.int_id.data
    # 请求的 timezone 已接入（0025）：合法时区不影响结果，坏时区在响应编码处报 6（空表达式也一样）。
    assert status_of(stub.Search(search_req(params={"timezone": "UTC"})))[0] == 0
    assert status_of(stub.Search(search_req(params={"timezone": "Mars/Base"}))) == (
        6, 6, "unknown timezone 'Mars/Base'")
    # Python 的 json.loads 能解析、serde_json 不能解析的 params：其中可能有 range search 的参数，
    # 不能像解析失败那样忽略（0004「决策」）。
    for extra in ["NaN", "Infinity", "-Infinity", '"\\ud800"']:
        unimplemented(stub.Search, search_req(params={"params": '{"radius": 2.5, "x": ' + extra + '}'}))
    assert status_of(stub.Search(search_req(params={"params": '{"radius": 2.5, "x": 1e999}'})))[0] == 0


def test_search_group_by_translation(env):
    """分组检索的参数解析与响应编码；窗口与分组算法的真值见 core 的 group_by 测试和差分。"""
    _, stub, _ = env
    basic(stub, "g", dynamic=False)
    insert(
        stub, "g",
        long_fd("id", [1, 2, 3, 4, 5, 6]),
        vec_fd("v", [[i, 0] for i in range(1, 7)]),
        str_fd("t", ["a", "a", "b", "b", "c", "c"]),
    )

    def run(**params):
        return stub.Search(search_req("g", params={"topk": "2", "metric_type": '"L2"', **params},
                                      output_fields=["id"]))

    r = run(group_by_field="t", group_size="1")
    assert status_of(r)[0] == 0
    assert list(r.results.ids.int_id.data) == [1, 3] and list(r.results.topks) == [2]
    assert r.results.top_k == 2
    group = r.results.group_by_field_value
    assert (group.field_name, group.type) == ("t", DataType.VARCHAR)
    assert list(group.scalars.string_data.data) == ["a", "b"] and not group.valid_data
    # 分组字段不在 output_fields 中，命中里不出现，但分组值仍然返回。
    assert columns(r.results.fields_data) == ["id"]

    r = run(group_by_field="t", group_size="2")
    assert list(r.results.ids.int_id.data) == [1, 2, 3, 4]
    assert list(r.results.group_by_field_value.scalars.string_data.data) == ["a", "a", "b", "b"]
    # group_size 为 0 等同于 1；负数使每组都留不下命中，此时不返回分组值。
    assert list(run(group_by_field="t", group_size="0").results.ids.int_id.data) == [1, 3]
    r = run(group_by_field="t", group_size="-1")
    assert status_of(r)[0] == 0 and not r.results.ids.int_id.data and not r.results.topks[0]
    assert not r.results.HasField("group_by_field_value")
    # group_size 先按 Python int() 解析；没有分组字段时同样报错。
    bad = (1, 0, "invalid literal for int() with base 10: 'abc'")
    assert status_of(run(group_by_field="t", group_size='"abc"')) == bad
    assert status_of(run(group_size='"abc"')) == bad
    # 没有 group_by_field（或空字符串）时不分组，也没有分组值。
    plain = run(group_size="2")
    assert list(plain.results.ids.int_id.data) == [1, 2]
    assert not plain.results.HasField("group_by_field_value")

    # strict_group_size 取 JSON 解析结果的 Python 真值；解析失败的文本按非空字符串为真。
    for value, strict in [("true", True), ("1", True), ("2", True), ("False", True), ('"x"', True),
                          ("false", False), ("0", False), ('""', False), ("null", False)]:
        r = run(group_by_field="t", group_size="3", strict_group_size=value, topk="3")
        assert status_of(r)[0] == 0
        assert bool(list(r.results.ids.int_id.data)) is not strict, value

    # 字段校验：不存在、类型不支持。
    assert status_of(run(group_by_field="nosuch"))[2] == "group_by_field 'nosuch' not found in schema"
    assert status_of(run(group_by_field="v"))[2] == (
        "group_by_field 'v' has type FLOAT_VECTOR which is not supported for group_by")
    # 动态字段的 key 不是 schema 字段。
    basic(stub, "d", dynamic=True)
    assert status_of(stub.Search(search_req("d", params={"group_by_field": "zz"})))[2] == (
        "group_by_field 'zz' not found in schema")


def test_timezone_translation(env):
    """0025 第 3 步：请求、集合、数据库三层 timezone 的优先级、两个校验点与 time_fields 的输出形态；
    与 3.x 的逐项对照见 tests/differential。"""
    client, stub, _ = env

    def make(name, **properties):
        schema = MilvusClient.create_schema(auto_id=False)
        schema.add_field("id", DataType.INT64, is_primary=True)
        schema.add_field("v", DataType.FLOAT_VECTOR, dim=2)
        schema.add_field("ts", DataType.TIMESTAMPTZ, nullable=True)
        index = client.prepare_index_params()
        index.add_index("v", index_type="FLAT", metric_type="L2")
        client.create_collection(name, schema=schema, index_params=index,
                                 **({"properties": properties} if properties else {}))
        client.load_collection(name)

    def ids(name, expr, **kwargs):
        return sorted(r["id"] for r in client.query(name, filter=expr, output_fields=["id"], **kwargs))

    def error(call, *args, **kwargs):
        with pytest.raises(Exception) as caught:
            call(*args, **kwargs)
        return str(caught.value)

    naive = "TIMESTAMPTZ value must include a timezone offset or use a configured timezone"
    make("plain")
    assert naive in error(client.insert, "plain", [{"id": 1, "v": [0.0, 0.0], "ts": "2025-01-01T08:00:00"}])
    # 请求 timezone 只影响过滤与 time_fields，写入没有请求时区。
    client.insert("plain", [{"id": 1, "v": [0.0, 0.0], "ts": "2025-01-01T00:00:00Z"}])
    assert ids("plain", "ts == ISO '2025-01-01T08:00:00'", timezone="Asia/Shanghai") == [1]
    assert ids("plain", "ts == ISO '2025-01-01T08:00:00'", timezone="utc") == []
    assert naive in error(client.query, "plain", filter="ts == ISO '2025-01-01T08:00:00'")

    # 集合 timezone：写入与过滤按它解释，请求值覆盖它，改属性不改已存数据。
    make("shanghai", timezone="Asia/Shanghai")
    client.insert("shanghai", [{"id": 1, "v": [0.0, 0.0], "ts": "2025-01-01T08:00:00"}])
    assert ids("shanghai", "ts == ISO '2025-01-01T00:00:00Z'") == [1]
    assert ids("shanghai", "ts == ISO '2025-01-01T08:00:00'") == [1]
    assert ids("shanghai", "ts == ISO '2025-01-01T08:00:00'", timezone="UTC") == []
    client.alter_collection_properties("shanghai", {"timezone": "America/New_York"})
    assert ids("shanghai", "ts == ISO '2025-01-01T00:00:00Z'") == [1]
    assert ids("shanghai", "ts == ISO '2024-12-31T19:00:00'") == [1]
    client.drop_collection_properties("shanghai", ["timezone"])
    assert naive in error(client.query, "shanghai", filter="ts == ISO '2024-12-31T19:00:00'")

    # 数据库 timezone：没有集合值时生效，改动立即影响已打开的集合；写入的 naive 默认值按创建时的值换算。
    client.create_database("tzdb", properties={"timezone": "Asia/Tokyo"})
    client.use_database("tzdb")
    make("dbc")
    client.insert("dbc", [{"id": 1, "v": [0.0, 0.0], "ts": "2025-01-01T09:00:00"}])
    assert ids("dbc", "ts == ISO '2025-01-01T00:00:00Z'") == [1]
    client.alter_database_properties("tzdb", {"timezone": "UTC"})
    assert ids("dbc", "ts == ISO '2025-01-01T09:00:00'") == []
    assert ids("dbc", "ts == ISO '2025-01-01T00:00:00'") == [1]
    client.drop_database_properties("tzdb", ["timezone"])
    assert naive in error(client.insert, "dbc", [{"id": 2, "v": [0.0, 0.0], "ts": "2025-01-01T00:00:00"}])
    client.use_database("default")
    client.drop_collection("dbc", db_name="tzdb")

    # 校验点：非空表达式在解析之前、响应编码处再一次；count(*) 加空表达式不经过任何一处。
    assert "unknown timezone 'bad'" in error(client.query, "plain", filter="id >", timezone="bad")
    assert "unknown timezone 'bad'" in error(client.query, "plain", filter="", output_fields=["id"], timezone="bad")
    assert client.query("plain", filter="", output_fields=["count(*)"], timezone="bad") == [{"count(*)": 1}]
    assert "unknown timezone 'bad'" in error(
        client.query, "plain", filter="id >= 0", output_fields=["count(*)"], timezone="bad")
    assert "unknown timezone 'bad'" in error(
        client.search, "plain", data=[[0.0, 0.0]], limit=1, timezone="bad")

    # time_fields：TIMESTAMPTZ 输出改为 Array<Int64>，空值整行无效，坏组件只在编码非空值时报错。
    client.insert("plain", [{"id": 2, "v": [0.0, 0.0], "ts": None},
                            {"id": 3, "v": [0.0, 0.0], "ts": "2025-03-09T07:30:15.25Z"}])
    rows = {r["id"]: r["ts"] for r in client.query(
        "plain", filter="id in [2, 3]", output_fields=["ts"],
        time_fields="Year, month,DAY\thour minute,second,microsecond", timezone="America/New_York")}
    assert rows == {2: None, 3: [2025, 3, 9, 3, 30, 15, 250000]}
    reply = stub.Query(milvus_pb2.QueryRequest(collection_name="plain", expr="id in [2, 3]", output_fields=["ts"],
                                               query_params=[common_pb2.KeyValuePair(key="time_fields", value="hour")]))
    column = next(f for f in reply.fields_data if f.field_name == "ts")
    assert column.type == 22 and column.scalars.array_data.element_type == 5
    assert list(column.valid_data) == [False, True]
    assert [list(a.long_data.data) for a in column.scalars.array_data.data] == [[], [7]]
    assert client.query("plain", filter="id == 2", output_fields=["ts"], time_fields="weekday")[0]["ts"] is None
    assert "unsupported time_fields component 'weekday'" in error(
        client.query, "plain", filter="id == 3", output_fields=["ts"], time_fields="weekday")
    hits = client.search("plain", data=[[0.0, 0.0]], limit=5, filter="id == 3", output_fields=["ts"],
                         time_fields="year,hour", timezone="Asia/Shanghai")
    assert hits[0][0]["entity"]["ts"] == [2025, 15]


def test_timestamptz_translation(env):
    """0025 第 1 步：写入形态、输出格式、默认值回显与范围校验；对照 3.x 的取值见 tests/differential。"""
    client, stub, _ = env
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    schema.add_field("ts", DataType.TIMESTAMPTZ, nullable=True)
    schema.add_field("ts2", DataType.TIMESTAMPTZ, default_value="2025-01-01T08:00:00+08:00")
    index = client.prepare_index_params()
    index.add_index("v", index_type="FLAT", metric_type="L2")
    client.create_collection("t", schema=schema, index_params=index)
    client.load_collection("t")

    def raw(rows, ts, ts_slot="string_data", ts2=None, valid=None, ts_type=26):
        ids = long_fd("id", list(range(100, 100 + rows)))
        vector = vec_fd("v", [[0.0, 0.0]] * rows)
        column = schema_pb2.FieldData(type=ts_type, field_name="ts")
        getattr(column.scalars, ts_slot).data.extend(ts)
        column.valid_data.extend(valid or [])
        fields = [ids, vector, column]
        if ts2 is not None:
            second = schema_pb2.FieldData(type=26, field_name="ts2")
            second.scalars.string_data.data.extend(ts2)
            fields.append(second)
        return stub.Insert(milvus_pb2.InsertRequest(collection_name="t", num_rows=rows, fields_data=fields))

    client.insert("t", [
        {"id": 1, "v": [0.0, 0.0], "ts": "2025-01-02T12:30:00.5+08:00", "ts2": "2024-02-29T00:00:00+03:00"},
        {"id": 2, "v": [0.0, 0.0], "ts": None},
        {"id": 3, "v": [0.0, 0.0], "ts": "0001-01-01T00:00:00Z", "ts2": "9999-12-31T23:59:59.999999Z"},
    ])
    rows = {r["id"]: r for r in client.query("t", filter="id >= 0", output_fields=["ts", "ts2"])}
    assert rows[1] == {"id": 1, "ts": "2025-01-02T04:30:00.500000Z", "ts2": "2024-02-28T21:00:00Z"}
    assert rows[2] == {"id": 2, "ts": None, "ts2": "2025-01-01T00:00:00Z"}
    assert rows[3]["ts"] == "0001-01-01T00:00:00Z" and rows[3]["ts2"] == "9999-12-31T23:59:59.999999Z"
    # 响应：类型 26，值放 string_data，空值走 valid_data。
    reply = stub.Query(milvus_pb2.QueryRequest(collection_name="t", expr="id in [1, 2]", output_fields=["ts"]))
    ts = next(f for f in reply.fields_data if f.field_name == "ts")
    assert ts.type == 26 and list(ts.scalars.string_data.data) == ["2025-01-02T04:30:00.500000Z", ""]
    assert list(ts.valid_data) == [True, False]
    # 描述集合回显默认值（微秒）。
    described = stub.DescribeCollection(milvus_pb2.DescribeCollectionRequest(collection_name="t"))
    default = next(f for f in described.schema.fields if f.name == "ts2").default_value
    assert default.WhichOneof("data") == "timestamptz_data" and default.timestamptz_data == 1_735_689_600_000_000

    # 原始 gRPC：整数微秒在范围内可写，越界整批拒绝、零写入；long_data 槽被忽略。
    assert status_of(raw(2, [0, 253_402_300_799_999_999], "timestamptz_data"))[0] == 0
    before = client.query("t", filter="", output_fields=["count(*)"])
    for bad in (253_402_300_800_000_000, -62_135_596_800_000_001, 2**63 - 1, -(2**63)):
        assert status_of(raw(2, [5, bad], "timestamptz_data")) == (1, 0, "date value out of range")
    assert status_of(raw(1, [5], "long_data")) == (6, 6, "FieldData 'ts' has 0 rows, expected 1")
    assert client.query("t", filter="", output_fields=["count(*)"]) == before
    # 字符串：无偏移与格式错误按 3.x 的文案；带偏移的越界是 1。
    naive = "TIMESTAMPTZ value must include a timezone offset or use a configured timezone"
    assert status_of(raw(1, ["2025-01-01T00:00:00"]))[0::2] == (6, naive)
    assert status_of(raw(1, ["nope"]))[0::2] == (6, "TIMESTAMPTZ value 'nope' is not a valid ISO 8601 timestamp")
    assert status_of(raw(1, ["0001-01-01T00:00:00+01:00"])) == (1, 0, "date value out of range")
    # 空值行不解析（按完整长度发送时占位串不是合法时间）。
    assert status_of(raw(2, ["", "2030-01-01T00:00:00Z"], valid=[False, True]))[0] == 0
    assert status_of(raw(1, ["2030-01-01T00:00:00Z"], valid=[True]))[0] == 0

    # 字符串放进 VARCHAR 槽（type 21）：3.x 由引擎按 schema 解析，越界报 1，其余失败按类型不符报 6。
    assert status_of(raw(1, ["2025-01-01T08:00:00.1234567+08:00"], ts_type=21))[0] == 0
    assert status_of(raw(2, ["", "2030-01-01T00:00:00Z"], valid=[False, True], ts_type=21))[0] == 0
    for text in ("2025-01-01T00:00:00", "nope", ""):
        assert status_of(raw(1, [text], ts_type=21)) == (
            6, 6, f"field 'ts' value '{text}' does not match dtype DataType.TIMESTAMPTZ")
    for text in ("0001-01-01T00:00:00+01:00", "9999-12-31T23:59:59-08:00"):
        assert status_of(raw(1, [text], ts_type=21)) == (1, 0, "date value out of range")
    # 过滤：比较、`in`、空值、时间戳 ± 区间；类型错误与无偏移字面量沿用 3.x 的错误码与文案。
    def ids(expr):
        return sorted(r["id"] for r in client.query("t", filter=expr, output_fields=["id"]))
    assert ids("id < 4 and ts > ISO '2025-01-01T00:00:00Z'") == [1]
    assert ids("id < 4 and ts == ISO '2025-01-02T12:30:00.5+08:00'") == [1]
    assert ids("id < 4 and ts in [ISO '0001-01-01T00:00:00Z']") == [3]
    assert ids("id < 4 and ts is null") == [2]
    assert ids("id < 4 and ts + INTERVAL 'P1D' > ISO '2025-01-03T00:00:00Z'") == [1]
    assert ids("id < 4 and ts - INTERVAL 'P1D' < ts2") == [3]
    reply = stub.Query(milvus_pb2.QueryRequest(collection_name="t", expr="ts > 1"))
    assert status_of(reply)[0] == 6 and "comparison '>' between incompatible types" in status_of(reply)[2]
    reply = stub.Query(milvus_pb2.QueryRequest(collection_name="t", expr="ts > ISO '2025-01-01T00:00:00'"))
    assert status_of(reply)[0] == 6 and naive in status_of(reply)[2]
    reply = stub.Query(milvus_pb2.QueryRequest(collection_name="t", expr="ts + INTERVAL 'P106751992D' > ts2"))
    assert status_of(reply)[0::2] == (1, "Python int too large to convert to C long")
    # 不涉及时间值的算术仍是范围外构造。
    unimplemented(stub.Query, milvus_pb2.QueryRequest(collection_name="t", expr="id + 1 > 2"))
    # schema 规则。
    for name, build in {
        "pk": lambda s: s.add_field("p", DataType.TIMESTAMPTZ, is_primary=True),
        "iso": lambda s: s.add_field("iso", DataType.TIMESTAMPTZ),
    }.items():
        bad = MilvusClient.create_schema(auto_id=False)
        bad.add_field("id", DataType.INT64, is_primary=True) if name != "pk" else None
        bad.add_field("v", DataType.FLOAT_VECTOR, dim=2)
        build(bad)
        with pytest.raises(Exception) as info:
            client.create_collection("bad_" + name, schema=bad)
        assert "iso" in str(info.value) or "Primary key" in str(info.value), info.value
    arr = MilvusClient.create_schema(auto_id=False)
    arr.add_field("id", DataType.INT64, is_primary=True)
    arr.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    arr.add_field("a", DataType.ARRAY, element_type=DataType.TIMESTAMPTZ, max_capacity=2)
    with pytest.raises(grpc.RpcError) as info:
        client.create_collection("bad_arr", schema=arr)
    assert info.value.code() == grpc.StatusCode.UNIMPLEMENTED
    assert info.value.details().startswith("MilvusLite does not support ARRAY element type")


def test_search_result_encoding(env):
    _, stub, _ = env
    basic(stub)
    seed(stub)
    l2_index(stub)
    r = stub.Search(search_req(vectors=([0.0, 0.0], [5.0, 2.5]), params={"topk": "2"}))
    res = r.results
    assert res.num_queries == 2 and res.top_k == 2 and list(res.topks) == [2, 2]
    assert list(res.ids.int_id.data) == [1, 2, 5, 4]
    assert res.primary_field_name == "id" and r.collection_name == "c"
    # 不指定 output_fields：主键列总在，其余字段按 schema 顺序，最后是完整的 `$meta`。
    assert columns(res.fields_data) == ["id", "v", "t", "$meta"]
    assert list(res.output_fields) == ["v", "t", "$meta"]
    assert meta_rows(res.fields_data)[0] == b'{"aa": "a1", "zz": 1}'
    # 显式请求的动态 key 按请求顺序输出。
    r = stub.Search(search_req(params={"topk": "1"}, output_fields=["zz", "t", "aa"]))
    assert columns(r.results.fields_data) == ["id", "t", "$meta"]
    assert list(r.results.output_fields) == ["t", "zz", "aa"]
    assert meta_rows(r.results.fields_data) == [b'{"zz": 1, "aa": "a1"}']
    # 空结果仍带各列，向量列保留 dim。
    basic(stub, "e")
    r = stub.Search(search_req("e"))
    assert list(r.results.topks) == [0] and r.results.ids.WhichOneof("id_field") == "int_id"
    v = next(fd for fd in r.results.fields_data if fd.field_name == "v")
    assert v.vectors.dim == 2 and v.vectors.WhichOneof("data") == "float_vector"


def test_search_on_non_dynamic_collection_drops_unknown_fields(env):
    _, stub, _ = env
    basic(stub, dynamic=False)
    insert(stub, "c", long_fd("id", [1]), vec_fd("v", [[1, 0]]), str_fd("t", ["a"]))
    r = stub.Search(search_req(output_fields=["nope", "t"]))
    assert columns(r.results.fields_data) == ["id", "t"] and list(r.results.output_fields) == ["t"]


# ── 查询 ─────────────────────────────────────────────────────


def test_query_count_and_get_paths(env):
    _, stub, _ = env
    basic(stub)
    seed(stub)
    r = stub.Query(query_req(output_fields=["count(*)", "t"]))
    assert columns(r.fields_data) == ["count(*)"] and list(r.fields_data[0].scalars.long_data.data) == [5]
    assert list(r.output_fields) == ["count(*)"] and r.primary_field_name == ""
    params = [common_pb2.KeyValuePair(key="limit", value="1"), common_pb2.KeyValuePair(key="offset", value="1")]
    r = stub.Query(query_req(expr="id in [3, 1, 3, 99]", output_fields=["t"], query_params=params))
    assert list(r.fields_data[0].scalars.long_data.data) == [3, 1, 3]
    assert columns(r.fields_data) == ["id", "t", "$meta"] and list(r.output_fields) == ["t"]
    assert r.primary_field_name == "id" and r.collection_name == "c"
    r = stub.Query(query_req(expr="id in [1]"))
    assert list(r.output_fields) == [] and columns(r.fields_data) == ["id", "v", "t", "$meta"]
    r = stub.Query(query_req(expr="(id) IN [2]", output_fields=["*"]))
    assert list(r.output_fields) == ["id", "v", "t", "$meta"]
    assert status_of(stub.Query(query_req(expr="id in [1]", partition_names=["_default"])))[0] == 0


def test_query_m1_unsupported(env):
    _, stub, _ = env
    basic(stub)
    seed(stub)
    unimplemented(stub.Query, query_req(expr="id + 1 > 0", output_fields=["count(*)"]))
    iterator = [common_pb2.KeyValuePair(key="iterator", value="False")]
    response = stub.Query(query_req(expr="id in [3, 1]", query_params=iterator + [
        common_pb2.KeyValuePair(key="limit", value="1")]))
    assert status_of(response)[0] == 0
    assert list(response.fields_data[0].scalars.long_data.data) == [1]
    unimplemented(stub.Query, query_req(expr="id + 1 > 0"))
    # 时间字面量的比较已支持（0025）：恒真时命中全部行。
    constant = stub.Query(query_req(
        expr="ISO '2025-01-01T00:00:00Z' == ISO '2025-01-01T00:00:00Z'", output_fields=["count(*)"]))
    assert status_of(constant)[0] == 0 and list(constant.fields_data[0].scalars.long_data.data) == [5]
    empty = stub.Query(query_req(expr="id in [1]", partition_names=["p"]))
    assert status_of(empty)[0] == 0 and not empty.fields_data[0].scalars.long_data.data
    timezone = [common_pb2.KeyValuePair(key="timezone", value="UTC")]
    assert status_of(stub.Query(query_req(expr="id in [1]", query_params=timezone)))[0] == 0
    bad = [common_pb2.KeyValuePair(key="timezone", value="Mars/Base")]
    assert status_of(stub.Query(query_req(expr="id in [1]", query_params=bad))) == (
        6, 6, "unknown timezone 'Mars/Base'")


def test_query_load_state(env):
    _, stub, _ = env
    basic(stub)
    seed(stub)
    l2_index(stub)
    stub.ReleaseCollection(milvus_pb2.ReleaseCollectionRequest(collection_name="c"))
    # 空 expr 的 count(*) 不查 load 状态、不看分区。
    r = stub.Query(query_req(output_fields=["count(*)"], partition_names=["nope"]))
    assert list(r.fields_data[0].scalars.long_data.data) == [5]
    assert status_of(stub.Query(query_req(expr="id in [1]")))[:2] == (101, 101)
    assert status_of(stub.Query(query_req(expr="id > 1")))[:2] == (101, 101)
    # 带非空 expr 的 count(*) 走通用查询，先检查 load 状态。
    assert status_of(stub.Query(query_req(expr="id in [1]", output_fields=["count(*)"])))[:2] == (101, 101)


# ── 索引 ─────────────────────────────────────────────────────


def test_create_index_unsupported_types_and_json(env):
    # 0014：仍拒绝 3.x 自身无法构建的类型与无法保存的 JSON 参数。
    _, stub, _ = env
    basic(stub)
    kv = lambda **p: [common_pb2.KeyValuePair(key=k, value=v) for k, v in p.items()]
    unimplemented(stub.CreateIndex, milvus_pb2.CreateIndexRequest(
        collection_name="c", field_name="v", extra_params=kv(index_type="IVF_PQ", metric_type="L2")))
    # 3.x 能解析并保存的 params，serde_json 解析不了时不能当作空对象。
    unimplemented(stub.CreateIndex, milvus_pb2.CreateIndexRequest(
        collection_name="c", field_name="v", extra_params=kv(index_type="FLAT", metric_type="L2",
                                                             params='{"a": NaN}')))
    # 严格 JSON 中溢出为 inf 的数字与 NaN 字面量是同一类值：3.x 以 `Infinity` 写进 manifest。
    for extra in (dict(params='{"x": 1e999}'), dict(search_params='{"a": [-1e999]}')):
        unimplemented(stub.CreateIndex, milvus_pb2.CreateIndexRequest(
            collection_name="c", field_name="v",
            extra_params=kv(index_type="FLAT", metric_type="L2", **extra)))
    assert status_of(stub.DescribeIndex(milvus_pb2.DescribeIndexRequest(collection_name="c")))[:2] == (700, 11)


def test_create_and_describe_index(env):
    _, stub, _ = env
    basic(stub)
    kv = lambda **p: [common_pb2.KeyValuePair(key=k, value=v) for k, v in p.items()]
    r = stub.CreateIndex(milvus_pb2.CreateIndexRequest(collection_name="c", field_name="v",
                                                       extra_params=kv(metric_type="L2")))
    assert status_of(r) == (6, 6, "create_index missing required 'index_type' parameter")
    r = stub.DescribeIndex(milvus_pb2.DescribeIndexRequest(collection_name="c"))
    assert status_of(r) == (700, 11, "no index on collection 'c'")
    r = stub.CreateIndex(milvus_pb2.CreateIndexRequest(
        collection_name="c", field_name="v", index_name="whatever",
        extra_params=kv(index_type="flat", metric_type="IP", nlist="16",
                        params='{"b": 1, "a": 1e-05}', search_params='{"ef": "中"}')))
    assert status_of(r) == (0, 0, "")
    seed(stub)
    for req in [dict(field_name="v"), dict(index_name="v"), dict(index_name="v_idx"), dict(index_name="x"), {}]:
        d = stub.DescribeIndex(milvus_pb2.DescribeIndexRequest(collection_name="c", **req))
        assert len(d.index_descriptions) == 1, req
        idx = d.index_descriptions[0]
        assert (idx.index_name, idx.field_name, idx.indexed_rows, idx.total_rows) == ("v", "v", 5, 5)
        assert idx.state == common_pb2.Finished
        # pymilvus 平铺的 nlist 不保存；params 与 search_params 按 json.dumps(sort_keys=True) 输出。
        assert [(p.key, p.value) for p in idx.params] == [
            ("index_type", "FLAT"), ("metric_type", "IP"),
            ("params", '{"a": 1e-05, "b": 1}'), ("search_params", '{"ef": "\\u4e2d"}')]
    r = stub.DescribeIndex(milvus_pb2.DescribeIndexRequest(collection_name="c", field_name="t"))
    assert status_of(r) == (700, 11, "no index on field 't' of collection 'c'")
    r = stub.CreateIndex(milvus_pb2.CreateIndexRequest(
        collection_name="c", field_name="v", extra_params=kv(index_type="FLAT", metric_type="IP")))
    assert status_of(r) == (35, 35, "index already exists for field 'v'; call drop_index first")


# ── 消息大小上限 ─────────────────────────────────────────────


def test_messages_over_256mb_are_resource_exhausted(env):
    _, stub, channel = env
    raw = channel.unary_unary("/milvus.proto.milvus.MilvusService/Insert",
                              response_deserializer=milvus_pb2.MutationResult.FromString)
    with pytest.raises(grpc.RpcError) as info:
        raw(b"\x00" * (MAX + 1))
    assert info.value.code() == grpc.StatusCode.RESOURCE_EXHAUSTED
    assert info.value.details() == f"SERVER: Received message larger than max ({MAX + 1} vs. {MAX})"

    dim = 32768
    assert status_of(create(stub, "big", field("id", DataType.INT64, primary=True),
                            field("v", DataType.FLOAT_VECTOR, dim=dim)))[0] == 0
    for start in (0, 1100):
        assert status_of(raw(raw_insert("big", range(start, start + 1100), dim)))[0] == 0
    with pytest.raises(grpc.RpcError) as info:
        stub.Query(query_req("big", expr=f"id in [{', '.join(map(str, range(2200)))}]", output_fields=["v"]))
    assert info.value.code() == grpc.StatusCode.RESOURCE_EXHAUSTED
    assert info.value.details().startswith("SERVER: Sent message larger than max (")
    assert info.value.details().endswith(f" vs. {MAX})")
    # 连接不受影响。
    assert status_of(stub.Query(query_req("big", output_fields=["count(*)"])))[0] == 0


# ── M1 范围外的功能 ──────────────────────────────────────────


def test_upsert_field_ops_is_explicitly_unsupported(env):
    client, _, channel = env
    client.create_collection("q", dimension=2)
    request = milvus_pb2.UpsertRequest(collection_name="q", num_rows=1)
    # 2.6 的生成类没有 field_ops；按 3.0 proto 的第 11 字段拼线格式，两条客户端线均可发出。
    op = b"\x0a\x06vector\x10\x00"
    raw = request.SerializeToString() + bytes([0x5a, len(op)]) + op
    call = channel.unary_unary(
        "/milvus.proto.milvus.MilvusService/Upsert",
        request_serializer=lambda value: value,
        response_deserializer=milvus_pb2.MutationResult.FromString,
    )
    assert "field_ops" in unimplemented(call, raw)


def test_pymilvus_entry_points_remaining_unsupported(env):
    """当前范围外的功能各取 pymilvus 的一个入口：客户端不得吞掉 UNIMPLEMENTED。"""
    from pymilvus import AnnSearchRequest, Function, FunctionType, RRFRanker

    client, _, _ = env
    client.create_collection("q", dimension=2)
    client.insert("q", [{"id": i, "vector": [1.0, i / 50]} for i in range(30)])

    def with_field(*args, **kwargs):
        schema = MilvusClient.create_schema()
        schema.add_field("id", DataType.INT64, is_primary=True)
        schema.add_field("v", DataType.FLOAT_VECTOR, dim=2)
        schema.add_field(*args, **kwargs)
        return lambda: client.create_collection("n", schema=schema)

    rpc = "not implemented in this build of milvus-lite-rust yet"
    feature = "not supported in this build of milvus-lite-rust yet"
    # model 通过全部校验后才返回 UNIMPLEMENTED（0026 第 10 节），输入字段须是 VARCHAR。
    model_schema = MilvusClient.create_schema()
    model_schema.add_field("id", DataType.INT64, is_primary=True)
    model_schema.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    model_schema.add_field("s", DataType.VARCHAR, max_length=8)
    index = client.prepare_index_params()
    index.add_index("v", index_type="FLAT", metric_type="COSINE")
    client.create_collection("m", schema=model_schema, index_params=index)
    client.insert("m", [{"id": i, "v": [1.0, i / 50], "s": f"s{i}"} for i in range(3)])
    model = Function(name="r", function_type=FunctionType.RERANK, input_field_names=["s"],
                     params={"reranker": "model", "provider": "cohere", "queries": ["q"]})
    cases = [
        (with_field("n", DataType.FLOAT_VECTOR, dim=2, nullable=True), f"nullable vectors: {feature}"),
        (with_field("v2", DataType.FLOAT_VECTOR, dim=2), "multiple vector fields: "),
        (with_field("s", DataType.SPARSE_FLOAT_VECTOR), f"SPARSE_FLOAT_VECTOR fields: {feature}"),
        (with_field("g", DataType.GEOMETRY), f"GEOMETRY fields: {feature}"),
        # pymilvus 3.0 先发 AlterCollectionSchema，收到 UNIMPLEMENTED 后回退到 AddCollectionField。
        (lambda: client.add_collection_field("q", "x", DataType.INT64, nullable=True), f"AddCollectionField: {rpc}"),
        (lambda: client.hybrid_search("q", [AnnSearchRequest([[1, 0]], "vector", {}, 2)], RRFRanker(), limit=2),
         f"HybridSearch: {rpc}"),
        (lambda: client.search("q", [[1, 0]], filter="id + 1 > 0"), "filter expressions: arithmetic, text_match, and geometry predicates are not supported"),
        (lambda: client.search("m", [[1, 0]], ranker=model), f"rerank model providers: {feature}"),
        # 与分组同时出现时，model 的 UNIMPLEMENTED 不能被分组路径吞掉。
        (lambda: client.search("m", [[1, 0]], ranker=model, group_by_field="id"),
         f"rerank model providers: {feature}"),
    ]
    for call, details in cases:
        with pytest.raises(grpc.RpcError) as info:
            call()
        assert info.value.code() == grpc.StatusCode.UNIMPLEMENTED, details
        assert info.value.details().startswith(f"MilvusLite does not support {details}"), info.value.details()

    iterator = client.query_iterator("q", batch_size=2, limit=3, offset=1,
                                     filter="id in [9, 3, 7, 1, 5]", output_fields=["id"])
    try:
        assert list(iterator.next()) == [{"id": 3}, {"id": 5}]
        assert list(iterator.next()) == [{"id": 7}]
        assert list(iterator.next()) == []
    finally:
        iterator.close()

    # V1 后续批次经范围检索和主键排除续读；V2 仍按参照回退客户端 V1。
    it = client.search_iterator("q", data=[[1.0, 0.0]], batch_size=10, limit=3)
    assert [h["id"] for h in it.next()] == [0, 1, 2]
    assert len(it.next()) == 0
    it.close()
    it = client.search_iterator("q", data=[[1.0, 0.0]], batch_size=10, limit=25)
    assert [h["id"] for h in it.next()] == list(range(10))
    assert [h["id"] for h in it.next()] == list(range(10, 20))
    assert [h["id"] for h in it.next()] == list(range(20, 25))
    assert len(it.next()) == 0
    it.close()


def test_search_l2_rerank_functions(env):
    """Search 的 L2 rerank（0026 第 2 步）：rrf / weighted / decay 在引擎候选之后执行，分组交给 rerank 链。"""
    from pymilvus import Function, FunctionType

    client, _, _ = env
    schema = MilvusClient.create_schema()
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    schema.add_field("age", DataType.INT32)
    schema.add_field("cat", DataType.VARCHAR, max_length=4)
    index = client.prepare_index_params()
    index.add_index("v", index_type="FLAT", metric_type="COSINE")
    client.create_collection("l2", schema=schema, index_params=index)
    client.insert("l2", [{"id": i, "v": [1.0, i / 50], "age": 10 - i, "cat": "ab"[i % 2]} for i in range(10)])

    def ranker(reranker, inputs=(), **params):
        return Function(name="r", function_type=FunctionType.RERANK, input_field_names=list(inputs),
                        params={"reranker": reranker, **params})

    def hits(**kwargs):
        return [(h["id"], h["distance"]) for h in client.search("l2", [[1.0, 0.0]], limit=3, **kwargs)[0]]

    # rrf：只看名次，`1 / (k + rank + 1)`。
    assert hits(ranker=ranker("rrf", k=1)) == [(0, 0.5), (1, pytest.approx(1 / 3)), (2, 0.25)]
    # weighted 不归一化：COSINE 降序，分数是权重乘相似度。
    assert [i for i, _ in hits(ranker=ranker("weighted", weights=[0.5]))] == [0, 1, 2]
    # decay 把年龄越接近 origin 的排在前面；`_decay_score` 不在请求的输出字段里时不出现。
    result = client.search("l2", [[1.0, 0.0]], limit=3, output_fields=["age"],
                           ranker=ranker("decay", ["age"], function="linear", origin=1, scale=100, decay=0.5))[0]
    assert [h["id"] for h in result] == [9, 8, 7]
    assert all(set(h["entity"]) == {"id", "age"} for h in result)
    # 主键不在候选行里：decay 的因子恒为 0，全部同分，按 `$id` 升序（3.x 同）。
    assert hits(ranker=ranker("decay", ["id"], function="gauss", origin=0, scale=1)) == [(0, 0.0), (1, 0.0), (2, 0.0)]
    # 分组在 rerank 链里做：每组取前 group_size 个，组按组内最高分排序。
    grouped = client.search("l2", [[1.0, 0.0]], limit=2, group_by_field="cat", group_size=2, output_fields=["cat"],
                            ranker=ranker("rrf"))[0]
    assert [h["entity"]["cat"] for h in grouped] == ["a", "a", "b", "b"]
    # 链里的校验在检索之后、以 code 1 报出。
    with pytest.raises(MilvusException) as info:
        client.search("l2", [[1.0, 0.0]], ranker=ranker("rrf", k=0))
    assert info.value.code == 1 and "The rank params k should be in range (0, 16384)" in info.value.message

@pytest.mark.parametrize(
    "kind", ["AUTOINDEX", "HNSW", "HNSW_SQ", "IVF_FLAT", "IVF_SQ8"]
)
@pytest.mark.parametrize("metric", ["BM25", "NONE"])
def test_faiss_metric_rejection_is_unimplemented_and_does_not_create_index(
    env, kind, metric
):
    _, stub, _ = env
    basic(stub)
    request = milvus_pb2.CreateIndexRequest(
        collection_name="c",
        field_name="v",
        extra_params=[
            common_pb2.KeyValuePair(key="index_type", value=kind),
            common_pb2.KeyValuePair(key="metric_type", value=metric),
        ],
    )
    assert "requires COSINE / L2 / IP" in unimplemented(stub.CreateIndex, request)
    indexes = stub.DescribeIndex(milvus_pb2.DescribeIndexRequest(collection_name="c"))
    assert not indexes.index_descriptions
    # 原有缺少 metric / 非法枚举的错误优先于本实现的兼容性拒绝。
    request.extra_params[1].value = "bad"
    assert status_of(stub.CreateIndex(request)) == (
        1,
        0,
        "metric_type must be one of COSINE/L2/IP/BM25/NONE, got 'bad'",
    )
    del request.extra_params[1]
    assert status_of(stub.CreateIndex(request)) == (
        6,
        6,
        "create_index missing required 'metric_type' parameter",
    )


def test_drop_index_status_priority_names_and_loaded_transition(env):
    _, stub, _ = env
    basic(stub)
    request = lambda **p: milvus_pb2.DropIndexRequest(collection_name="c", **p)
    assert status_of(stub.DropIndex(request())) == (700, 11, "no index to drop")
    kv = lambda **p: [common_pb2.KeyValuePair(key=k, value=v) for k, v in p.items()]
    assert status_of(
        stub.CreateIndex(
            milvus_pb2.CreateIndexRequest(
                collection_name="c",
                field_name="v",
                extra_params=kv(index_type="HNSW", metric_type="L2"),
            )
        )
    ) == (0, 0, "")
    assert status_of(
        stub.CreateIndex(
            milvus_pb2.CreateIndexRequest(
                collection_name="c",
                field_name="id",
                extra_params=kv(index_type="INVERTED"),
            )
        )
    ) == (0, 0, "")
    assert status_of(
        stub.DropIndex(request(field_name="absent", index_name="v_idx"))
    ) == (700, 11, "no index on field 'absent'; indexed fields: ['v', 'id']")
    assert status_of(stub.DropIndex(request(index_name="v_idx"))) == (
        6,
        6,
        "index cannot be dropped on loaded collection; call release() first",
    )
    assert status_of(
        stub.ReleaseCollection(milvus_pb2.ReleaseCollectionRequest(collection_name="c"))
    ) == (0, 0, "")
    assert status_of(stub.DropIndex(request(index_name="id_idx"))) == (0, 0, "")
    assert [
        d.field_name
        for d in stub.DescribeIndex(
            milvus_pb2.DescribeIndexRequest(collection_name="c")
        ).index_descriptions
    ] == ["v"]
    assert status_of(stub.DropIndex(request(index_name="arbitrary"))) == (0, 0, "")
    assert (
        stub.GetLoadState(milvus_pb2.GetLoadStateRequest(collection_name="c")).state
        == common_pb2.LoadStateLoaded
    )
