"""serde_json 的数字私有键 `$serde_json::private::Number`（下称 T）在 3.x 里是普通键与普通名字：逐项对照 3.x，含重开。

开启 `arbitrary_precision` 后，serde_json 自带的 `Value` 反序列化把首键为 T 的对象当作数字；本实现的文本解析一律经
`json_text`。用例按触发路径分组：写入与 WAL / Parquet 读回、`$meta` 排序后 T 落到首位、manifest 的名字与索引参数、
schema 的改名与改属性、别名与数据库属性文件、请求参数。
"""

import grpc
import pytest
from pymilvus import DataType, MilvusClient
from pymilvus.grpc_gen import common_pb2, milvus_pb2, milvus_pb2_grpc, schema_pb2

from test_reopen import pair  # noqa: F401  （fixture）
from test_scenarios import index

T = "$serde_json::private::Number"

# JSON 字段与动态字段的嵌套值共用；首键为 T 的几种曾被改成数字或拒绝写入，非首键的在 `$meta` 排序后落到首位。
VALUES = {
    "only_number_text": {T: "1"},
    "only_text": {T: "abc"},
    "only_int": {T: 1},
    "first_with_more": {T: "1", "b": 2},
    "second": {"b": 2, T: "1"},
    "nested": {"a": {T: "1"}},
    "in_list": [{T: "1.5e3"}],
    "negative_zero": {T: "-0"},
    "token_of_token": {T: {T: "2"}},
    "float_text": {T: "1.0"},
}

FILTERS = [
    (f'j["{T}"] == "1"', False),
    (f'x["{T}"] == "1"', False),
    ("x == 1", False),
    (f'$meta["{T}"] == "1"', False),
    (f'exists j["{T}"]', False),
    (f'{T} == "1"', True),
]


def schema():
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=True)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("j", DataType.JSON)
    return s


def check_rows(d, tag):
    d.step(f"{tag} rows", lambda c: sorted(
        c.query("c", filter="id >= 0", output_fields=["j", "x", T, "y"]), key=lambda r: r["id"]))
    for expr, error in FILTERS:
        d.step(f"{tag} {expr}", lambda c, expr=expr: sorted(r["id"] for r in c.query("c", filter=expr)), error=error)


@pytest.mark.parametrize("flush", [False, True], ids=["wal", "flushed"])
def test_json_and_dynamic_values_round_trip(pair, flush):
    d = pair.open()
    d.step("create", lambda c: c.create_collection("c", schema=schema(), index_params=index("L2")))
    for n, (name, value) in enumerate(VALUES.items()):
        d.step(f"json {name}", lambda c, n=n, value=value: c.insert("c", [{"id": n, "vector": [0.0, 1.0], "j": value}]))
        d.step(f"dynamic {name}", lambda c, n=n, value=value: c.insert(
            "c", [{"id": 100 + n, "vector": [0.0, 1.0], "j": {}, "x": value}]))
    # 动态字段的顶层键名为 T：`$meta` 本身就是首键为 T 的对象。
    d.step("top key", lambda c: c.insert("c", [{"id": 200, "vector": [0.0, 1.0], "j": {}, T: "1"}]))
    d.step("top key with more", lambda c: c.insert("c", [{"id": 201, "vector": [0.0, 1.0], "j": {}, T: "1", "y": 3}]))
    if flush:
        d.step("flush", lambda c: c.flush("c"))
    check_rows(d, "before")
    pair.close()
    d = pair.open()
    d.step("load", lambda c: c.load_collection("c"))
    check_rows(d, "reopened")
    d.step("flush after reopen", lambda c: c.flush("c"))


def create(c, name, vector="vector", properties=None):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field(vector, DataType.FLOAT_VECTOR, dim=2)
    c.create_collection(name, schema=s, index_params=index("L2", field=vector), properties=properties or {})


def test_names_and_schema_rewrites_survive_reopen(pair):
    d = pair.open()
    # 分区名与建了索引的字段名是 manifest 里的对象键；3.x 按 sort_keys 写出，T 排到首位。
    d.step("part", lambda c: (create(c, "part"), c.create_partition("part", T)))
    d.step("part insert", lambda c: c.insert("part", [{"id": 1, "vector": [0.0, 1.0]}], partition_name=T))
    d.step("part flush", lambda c: c.flush("part"))
    d.step("field", lambda c: create(c, "field", vector=T))
    d.step("field insert", lambda c: c.insert("field", [{"id": 1, T: [0.0, 1.0]}]))
    d.step("field flush", lambda c: c.flush("field"))
    # 改名与改属性都要重写 schema.json；属性键 T 在改名后曾被写成 `"properties": 1`。
    d.step("rename", lambda c: (create(c, "rn", properties={T: "1"}), c.rename_collection("rn", "rn2")))
    d.step("alter", lambda c: (create(c, "alt", properties={T: "1"}), c.alter_collection_properties("alt", {"o": "x"})))
    pair.close()
    d = pair.open()
    d.step("part partitions", lambda c: c.list_partitions("part"))
    d.step("part rows", lambda c: (c.load_collection("part"), c.query("part", filter="id >= 0", partition_names=[T]))[1])
    d.step("field rows", lambda c: (c.load_collection("field"), c.query("field", filter="id >= 0", output_fields=[T]))[1])
    d.step("field search", lambda c: c.search("field", [[0.0, 1.0]], anns_field=T, limit=1))
    d.step("rename properties", lambda c: c.describe_collection("rn2")["properties"])
    d.step("rename again", lambda c: c.rename_collection("rn2", "rn3"))
    d.step("alter properties", lambda c: c.describe_collection("alt")["properties"])
    pair.close()
    d = pair.open()
    d.step("renamed twice", lambda c: c.describe_collection("rn3")["properties"])


def test_aliases_and_database_properties_survive_reopen(pair):
    d = pair.open()
    # 目标不是数字文本：解析失败曾让整个数据库的集合入口都报错。
    d.step("alias", lambda c: (create(c, "al"), create(c, "other"), c.create_alias("al", T)))
    # 别名与目标都是数字文本时整个文件曾被解析成数字，别名静默清空；单独放一个数据库。
    d.step("num db", lambda c: c.create_database("num"))
    d.step("num alias", lambda c: (c.using_database("num"), create(c, "1"), c.create_alias("1", T), c.using_database("default")))
    d.step("only db", lambda c: c.create_database("only", properties={T: "1"}))
    d.step("mixed db", lambda c: c.create_database("mixed", properties={T: "abc", "a": "x"}))
    pair.close()
    d = pair.open()
    d.step("other collection", lambda c: c.describe_collection("other")["collection_name"])
    d.step("aliases", lambda c: c.list_aliases("al"))
    d.step("describe alias", lambda c: c.describe_alias(T))
    d.step("query via alias", lambda c: (c.load_collection("al"), c.query(T, filter="id >= 0"))[1])
    d.step("num aliases", lambda c: (c.using_database("num"), c.list_aliases("1"))[1])
    d.step("back to default", lambda c: c.using_database("default"))
    d.step("only props", lambda c: c.describe_database("only"))
    d.step("mixed props", lambda c: c.describe_database("mixed"))
    # 唯一属性曾被读成空表；之后的修改会用空表重写文件，T 真正丢失。
    d.step("alter only", lambda c: c.alter_database_properties("only", {"b": "y"}))
    d.step("create alias after reopen", lambda c: c.create_alias("other", "b2"))
    pair.close()
    d = pair.open()
    d.step("only props after alter", lambda c: c.describe_database("only"))
    d.step("aliases after write", lambda c: c.list_aliases("other"))
    d.step("alias kept after write", lambda c: c.describe_alias(T))


def stubs(uris):
    channels = [grpc.insecure_channel(u.removeprefix("http://"), options=[("grpc.enable_http_proxy", 0)]) for u in uris]
    return channels, [milvus_pb2_grpc.MilvusServiceStub(c) for c in channels]


def pairs(params):
    return [common_pb2.KeyValuePair(key=k, value=v) for k, v in params.items()]


ANALYZER_PARAMS = [f'{{"{T}":"1"}}', f'{{"x":{{"{T}":"1e5"}}}}', f'{{"{T}":2,"y":1}}', f'{{"y":1,"{T}":"abc"}}']
INDEX_PARAMS = [f'{{"{T}":"1"}}', f'{{"{T}":"1","M":16}}', f'{{"x":{{"{T}":"2"}}}}']


def raw_state(stub):
    """analyzer_params 与索引参数只在原始 RPC 的 type_params / params 里可见。"""
    out = []
    for n in range(len(ANALYZER_PARAMS)):
        reply = stub.DescribeCollection(milvus_pb2.DescribeCollectionRequest(collection_name=f"a{n}"))
        field = next(f for f in reply.schema.fields if f.name == "t")
        out.append((reply.status.code, [(p.key, p.value) for p in field.type_params]))
    for n in range(len(INDEX_PARAMS)):
        reply = stub.DescribeIndex(milvus_pb2.DescribeIndexRequest(collection_name=f"i{n}", field_name="v"))
        out.append((reply.status.code, [[(p.key, p.value) for p in i.params] for i in reply.index_descriptions]))
    return out


def test_analyzer_and_index_params_survive_reopen(pair):
    pair.open()
    channels, both = stubs(pair.uris)
    try:
        for stub in both:
            for n, text in enumerate(ANALYZER_PARAMS):
                fields = [
                    schema_pb2.FieldSchema(name="id", data_type=DataType.INT64, is_primary_key=True),
                    schema_pb2.FieldSchema(name="t", data_type=DataType.VARCHAR,
                                           type_params=pairs({"max_length": "8", "analyzer_params": text})),
                    schema_pb2.FieldSchema(name="v", data_type=DataType.FLOAT_VECTOR, type_params=pairs({"dim": "2"})),
                ]
                blob = schema_pb2.CollectionSchema(name=f"a{n}", fields=fields).SerializeToString()
                status = stub.CreateCollection(milvus_pb2.CreateCollectionRequest(collection_name=f"a{n}", schema=blob))
                assert status.code == 0, status.reason
            for n, text in enumerate(INDEX_PARAMS):
                fields = [
                    schema_pb2.FieldSchema(name="id", data_type=DataType.INT64, is_primary_key=True),
                    schema_pb2.FieldSchema(name="v", data_type=DataType.FLOAT_VECTOR, type_params=pairs({"dim": "2"})),
                ]
                blob = schema_pb2.CollectionSchema(name=f"i{n}", fields=fields).SerializeToString()
                assert stub.CreateCollection(milvus_pb2.CreateCollectionRequest(collection_name=f"i{n}", schema=blob)).code == 0
                status = stub.CreateIndex(milvus_pb2.CreateIndexRequest(
                    collection_name=f"i{n}", field_name="v",
                    extra_params=pairs({"index_type": "HNSW", "metric_type": "L2", "params": text})))
                assert status.code == 0, status.reason
        before = [raw_state(stub) for stub in both]
        assert before[0] == before[1]
        # 参数原样保留（含只有 T 一个键的对象），不是被当作数字后丢成 `{}`。
        for code, descriptions in before[0][len(ANALYZER_PARAMS):]:
            assert code == 0 and T in dict(descriptions[0])["params"]
        for code, type_params in before[0][:len(ANALYZER_PARAMS)]:
            assert code == 0 and T in dict(type_params)["analyzer_params"]
    finally:
        for channel in channels:
            channel.close()
    pair.close()
    pair.open()
    channels, both = stubs(pair.uris)
    try:
        after = [raw_state(stub) for stub in both]
        # schema.json 按 sort_keys 写出，重开后 analyzer_params 的键序与建表时不同（0029），两边一致即可。
        assert after[0] == after[1]
        assert after[0][len(ANALYZER_PARAMS):] == before[0][len(ANALYZER_PARAMS):]
        for code, type_params in after[0][:len(ANALYZER_PARAMS)]:
            assert code == 0 and T in dict(type_params)["analyzer_params"]
    finally:
        for channel in channels:
            channel.close()


def test_search_params_keep_objects(pair):
    d = pair.open()
    d.step("create", lambda c: create(c, "c"))
    d.step("insert", lambda c: c.insert("c", [{"id": i, "vector": [float(i), 1.0]} for i in range(3)]))
    # 3.x 拿到的是 dict，`float(dict)` 报 TypeError；本实现曾把它当作 5 执行范围检索。
    d.step("radius object", lambda c: c.search(
        "c", [[0.0, 1.0]], limit=3, search_params={"params": {"radius": {T: "5"}, "range_filter": 0.0}}), error=True)
    d.step("unused object", lambda c: c.search("c", [[0.0, 1.0]], limit=3, search_params={"params": {T: "1"}}))
