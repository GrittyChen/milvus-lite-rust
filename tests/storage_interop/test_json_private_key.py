"""serde_json 的数字私有键 `$serde_json::private::Number`（T）在各共享文件里互通：JSON 字段与 `$meta`（WAL 与 segment）、
manifest 的分区名 / 字段名 / 索引参数、schema 的属性与 analyzer_params、别名与数据库属性文件，Arrow 15/25 双向。"""
import json
import sys

import pytest
from test_cross_implementation import PYTHONS, interpreters, run  # noqa: F401

COMMON = r'''
import json,os,sys
from pymilvus import DataType,MilvusClient
from pymilvus.grpc_gen import common_pb2,milvus_pb2,schema_pb2
from milvus_lite.server_manager import ServerManager
T="$serde_json::private::Number"
path=sys.argv[1];manager=ServerManager()
c=MilvusClient(manager.start_and_get_uri(path),dedicated=True)
stub=c._get_connection()._stub
def kv(**p): return [common_pb2.KeyValuePair(key=k,value=v) for k,v in p.items()]
'''

WRITE = COMMON + r'''
state=sys.argv[2]
values=[{T:"1"},{T:"abc"},{T:1},{T:"1","b":2},{"b":2,T:"1"},{"a":{T:"1"}},[{T:"1.5e3"}],{T:{T:"2"}}]
s=MilvusClient.create_schema(auto_id=False,enable_dynamic_field=True)
s.add_field("id",DataType.INT64,is_primary=True);s.add_field("v",DataType.FLOAT_VECTOR,dim=2);s.add_field("j",DataType.JSON)
s.add_field("t",DataType.VARCHAR,max_length=8,enable_analyzer=True,analyzer_params={"y":1,T:"abc"})
c.create_collection("c",schema=s,properties={T:"1"})
c.create_partition("c",T)
blob=stub.CreateIndex(milvus_pb2.CreateIndexRequest(collection_name="c",field_name="v",
    extra_params=kv(index_type="HNSW",metric_type="L2",params=json.dumps({T:"1","x":{T:"2"}}))))
assert blob.code==0,blob.reason
c.load_collection("c")
c.insert("c",[{"id":i,"v":[1.0,float(i)],"j":v,"t":"x","x":v} for i,v in enumerate(values)])
c.insert("c",[{"id":100,"v":[1.0,0.0],"j":{},"t":"x",T:"1","y":3}],partition_name=T)
f=MilvusClient.create_schema(auto_id=False);f.add_field("id",DataType.INT64,is_primary=True);f.add_field(T,DataType.FLOAT_VECTOR,dim=2)
p=c.prepare_index_params();p.add_index(T,index_type="FLAT",metric_type="L2")
c.create_collection("field",schema=f,index_params=p)
c.insert("field",[{"id":1,T:[0.0,1.0]}])
c.create_alias("c",T)
c.create_database("num");c.using_database("num")
n=MilvusClient.create_schema(auto_id=False);n.add_field("id",DataType.INT64,is_primary=True);n.add_field("v",DataType.FLOAT_VECTOR,dim=2)
c.create_collection("1",schema=n);c.create_alias("1",T);c.using_database("default")
c.create_database("only",properties={T:"1"})
c.rename_collection("c","c2")
if state!="wal":c.flush("c2");c.flush("field")
if state=="wal":os._exit(0)
c.close();manager.release_all()
'''

READ = COMMON + r'''
mode=sys.argv[2]
c.load_collection("c2");c.load_collection("field")
if mode=="change":
    c.insert("c2",[{"id":50,"v":[1.0,5.0],"j":{"b":1,T:"9"},"t":"x","x":{T:"9"}}],partition_name=T)
    c.flush("c2")
    c.alter_database_properties("only",{"b":"y"})
    c.create_alias("field","b2")
def raw(name):
    fields=stub.DescribeCollection(milvus_pb2.DescribeCollectionRequest(collection_name=name)).schema.fields
    index=stub.DescribeIndex(milvus_pb2.DescribeIndexRequest(collection_name=name,field_name="v")).index_descriptions
    return {"fields":{f.name:[[p.key,p.value] for p in f.type_params] for f in fields},"index":[[[p.key,p.value] for p in i.params] for i in index]}
out={
 "rows":sorted(c.query("c2",filter="id >= 0",output_fields=["j","x",T,"y"]),key=lambda r:r["id"]),
 "partition":sorted(r["id"] for r in c.query("c2",filter="id >= 0",partition_names=[T])),
 "filters":{e:sorted(r["id"] for r in c.query("c2",filter=e)) for e in [f'j["{T}"] == "1"',f'x["{T}"] == "1"',f'$meta["{T}"] == "1"']},
 "partitions":c.list_partitions("c2"),
 "properties":c.describe_collection("c2")["properties"],
 "raw":raw("c2"),
 "field":c.query("field",filter="id >= 0",output_fields=[T]),
 "alias":c.describe_alias(T),
 "only":c.describe_database("only"),
}
c.using_database("num");out["num"]=c.list_aliases("1");c.using_database("default")
print(json.dumps(out,sort_keys=True,default=repr))
c.close();manager.release_all()
'''

WRITERS = {"rust": None, "reference15": PYTHONS[1], "reference25": PYTHONS[2]}


def python_of(interpreters, name):  # noqa: F811
    env = WRITERS[name]
    return sys.executable if env is None else interpreters[env]


def read(interpreters, path, mode="read"):  # noqa: F811
    results = [json.loads(run([python_of(interpreters, name), "-c", READ, str(path), mode])) for name in WRITERS]
    assert results[0] == results[1] == results[2]
    return results[0]


@pytest.mark.parametrize("writer", list(WRITERS))
@pytest.mark.parametrize("state", ["wal", "flushed"])
def test_private_number_key_files_open_in_every_implementation(tmp_path, interpreters, writer, state):  # noqa: F811
    T = "$serde_json::private::Number"
    path = tmp_path / "t.db"
    run([python_of(interpreters, writer), "-c", WRITE, str(path), state])
    first = read(interpreters, path)
    rows = {r["id"]: r for r in first["rows"]}
    assert rows[0]["j"] == {T: "1"} and rows[0]["x"] == {T: "1"}
    assert rows[4]["x"] == {"b": 2, T: "1"} and rows[6]["j"] == [{T: "1.5e3"}]
    assert rows[100][T] == "1" and rows[100]["y"] == 3
    assert first["partition"] == [100] and T in first["partitions"]
    assert first["properties"] == {T: "1"}
    assert first["raw"]["index"][0][2] == ["params", json.dumps({T: "1", "x": {T: "2"}}, sort_keys=True)]
    assert first["field"] == [{"id": 1, T: [0.0, 1.0]}]
    assert first["alias"]["collection_name"] == "c2"
    assert first["num"]["aliases"] == [T]
    assert first["only"][T] == "1"

    other = "reference25" if writer == "rust" else "rust"
    changed = json.loads(run([python_of(interpreters, other), "-c", READ, str(path), "change"]))
    assert {r["id"]: r for r in changed["rows"]}[50]["x"] == {T: "9"}
    assert changed["only"][T] == "1" and changed["only"]["b"] == "y"
    assert read(interpreters, path) == changed
