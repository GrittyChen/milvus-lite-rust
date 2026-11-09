"""analyzer 属性的 schema 文件互通：任意真值的 analyzer_params、键序、WAL 与 segment，Arrow 15/25 双向（0029）。"""
import json
import sys

import pytest
from test_cross_implementation import PYTHONS, interpreters, run  # noqa: F401

COMMON = r'''
import json,os,sys
from pymilvus import DataType,MilvusClient
from pymilvus.grpc_gen import common_pb2,milvus_pb2,schema_pb2
from milvus_lite.server_manager import ServerManager
path=sys.argv[1];manager=ServerManager()
c=MilvusClient(manager.start_and_get_uri(path),dedicated=True)
stub=c._get_connection()._stub
def kv(**p): return [common_pb2.KeyValuePair(key=k,value=v) for k,v in p.items()]
'''

CREATE = r'''
def field(name,dtype,**p): return schema_pb2.FieldSchema(name=name,data_type=dtype,type_params=kv(**p))
def create(name,extra):
    fields=[schema_pb2.FieldSchema(name="id",data_type=DataType.INT64,is_primary_key=True),field("v",DataType.FLOAT_VECTOR,dim="2"),*extra]
    blob=schema_pb2.CollectionSchema(name=name,fields=fields).SerializeToString()
    status=stub.CreateCollection(milvus_pb2.CreateCollectionRequest(collection_name=name,schema=blob))
    assert status.code==0,status.reason
'''

WRITE = COMMON + CREATE + r'''
state=sys.argv[2]
create("c",[
    field("doc",DataType.VARCHAR,max_length="64",enable_analyzer="true",enable_match="true",analyzer_params='{"z":1.0,"a":{"y":"中","b":[1e5,null]}}'),
    field("list",DataType.VARCHAR,max_length="64",analyzer_params="[1]"),
    field("text",DataType.VARCHAR,max_length="64",analyzer_params='"x"'),
    field("num",DataType.INT64,enable_match="true",analyzer_params="5"),
    field("wide",DataType.VARCHAR,max_length="64",analyzer_params="1"*400),
    field("flag",DataType.VARCHAR,max_length="64",analyzer_params="true"),
    schema_pb2.FieldSchema(name="j",data_type=DataType.JSON),
])
p=c.prepare_index_params();p.add_index("v",index_type="FLAT",metric_type="L2")
c.create_index("c",p);c.load_collection("c")
rows=[{"id":i,"v":[1.0,float(i)],"doc":w,"list":"l","text":"t","num":i,"wide":"w","flag":"f","j":{"K":w,"中":"文"}}
      for i,w in enumerate(["Hello World","ΣΑΣ foo_bar","İstanbul x²y"])]
c.insert("c",rows)
if state!="wal":c.flush("c")
if state=="wal":os._exit(0)
c.close();manager.release_all()
'''

READ = COMMON + r'''
mode=sys.argv[2]
c.load_collection("c")
if mode=="change":
    c.insert("c",[{"id":9,"v":[1.0,0.5],"doc":"HELLO again","list":"l","text":"t","num":9,"wide":"w","flag":"f","j":{"k":1}}])
    c.flush("c")
def params():
    fields=stub.DescribeCollection(milvus_pb2.DescribeCollectionRequest(collection_name="c")).schema.fields
    return {f.name:[[p.key,p.value] for p in f.type_params] for f in fields}
exprs=["text_match(doc,'hello')","text_match(doc,'σας')","text_match(doc,'i')","text_match(doc,'x²y')","not text_match(doc,'hello')","text_match(j,'k')","text_match(j,'中')","text_match(num,'1')"]
out={"params":params(),"hits":{e:sorted(r["id"] for r in c.query("c",filter=e,output_fields=["id"],limit=20)) for e in exprs}}
print(json.dumps(out,sort_keys=True))
c.close();manager.release_all()
'''

BAD_WRITE = COMMON + CREATE + r'''
create("c",[field("t",DataType.VARCHAR,max_length="8",analyzer_params=sys.argv[2])])
create("other",[])
c.close();manager.release_all()
'''

BAD_READ = COMMON + r'''
from pymilvus.exceptions import MilvusException
try:
    c.describe_collection("c")
except MilvusException as e:
    code=e.code
else:
    raise SystemExit("collection opened")
other=[f["name"] for f in c.describe_collection("other")["fields"]]
c.drop_collection("c")
print(json.dumps([code,other,sorted(c.list_collections())]))
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
def test_analyzer_attributes_and_text_match_across_implementations(tmp_path, interpreters, writer, state):  # noqa: F811
    path = tmp_path / "t.db"
    run([python_of(interpreters, writer), "-c", WRITE, str(path), state])
    first = read(interpreters, path)
    # 新进程重开后，analyzer_params 的键序来自 sort_keys 写出的文件，不再是建表时的输入顺序。
    assert first["params"]["doc"] == [
        ["max_length", "64"], ["enable_analyzer", "true"], ["enable_match", "true"],
        ["analyzer_params", '{"a": {"b": [100000.0, null], "y": "\\u4e2d"}, "z": 1.0}'],
    ]
    assert first["params"]["list"] == [["max_length", "64"], ["analyzer_params", "[1]"]]
    assert first["params"]["text"] == [["max_length", "64"], ["analyzer_params", '"x"']]
    assert first["params"]["num"] == [["enable_match", "true"], ["analyzer_params", "5"]]
    assert first["params"]["wide"] == [["max_length", "64"], ["analyzer_params", "1" * 400]]
    assert first["hits"]["text_match(doc,'hello')"] == [0]
    assert first["hits"]["text_match(doc,'σας')"] == [1]
    assert first["hits"]["text_match(doc,'i')"] == [2]
    assert first["hits"]["text_match(num,'1')"] == []
    assert first["hits"]["text_match(j,'中')"] == [0, 1, 2]

    other = "reference25" if writer == "rust" else "rust"
    changed = json.loads(run([python_of(interpreters, other), "-c", READ, str(path), "change"]))
    assert changed["hits"]["text_match(doc,'hello')"] == [0, 9]
    assert read(interpreters, path) == changed


@pytest.mark.parametrize("arrow", [PYTHONS[1], PYTHONS[2]])
@pytest.mark.parametrize("text", ['{"x": NaN}', "Infinity", "1e999", "[" * 200 + "]" * 200],
                         ids=["nan", "infinity", "overflow", "depth-200"])
def test_reference_schema_rust_cannot_represent_fails_clearly_and_can_be_dropped(tmp_path, interpreters, arrow, text):  # noqa: F811
    """登记的差异（0029）：打开这个集合报错，同库其他集合不受影响，drop 可用。"""
    path = tmp_path / "bad.db"
    run([interpreters[arrow], "-c", BAD_WRITE, str(path), text])
    code, other, remaining = json.loads(run([sys.executable, "-c", BAD_READ, str(path)]))
    assert code == 1
    assert other == ["id", "v"]
    assert remaining == ["other"]
