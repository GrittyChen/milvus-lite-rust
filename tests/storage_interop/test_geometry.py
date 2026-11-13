"""GEOMETRY 的数据文件互通：schema.json（dtype、默认值原文）、WAL 与 segment 里的 WKT 原文，Arrow 15/25 双向（0034）。"""
import json
import sys

import pytest
from test_cross_implementation import PYTHONS, interpreters, run  # noqa: F401

COMMON = r'''
import json,os,sys
from pymilvus import DataType,MilvusClient
from pymilvus.grpc_gen import milvus_pb2
from milvus_lite.server_manager import ServerManager
path=sys.argv[1];manager=ServerManager()
c=MilvusClient(manager.start_and_get_uri(path),dedicated=True)
stub=c._get_connection()._stub
'''

# 原文不规范化：大小写、空白、Unicode 数字与下划线都按写入的文本保存。
WRITE = COMMON + r'''
state=sys.argv[2]
s=MilvusClient.create_schema(auto_id=False)
s.add_field("id",DataType.INT64,is_primary=True)
s.add_field("v",DataType.FLOAT_VECTOR,dim=2)
s.add_field("geo",DataType.GEOMETRY,nullable=True)
s.add_field("g2",DataType.GEOMETRY,default_value="  polygon((0 0,1 0,1 1,0 0)) ")
c.create_collection("c",schema=s)
p=c.prepare_index_params();p.add_index("v",index_type="FLAT",metric_type="L2")
c.create_index("c",p);c.load_collection("c")
texts=["POINT(1 1)","  point ( 1e1   -2.5 )  ","PO\u0130NT(\u0661 1_0)\n","POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))",None]
c.insert("c",[{"id":i,"v":[1.0,float(i)],"geo":t} for i,t in enumerate(texts)])
c.insert("c",[{"id":10,"v":[0.0,0.0],"geo":"POINT(3 3)","g2":"POINT(4\u30004)"}])
if state!="wal":c.flush("c")
if state=="wal":os._exit(0)
c.close();manager.release_all()
'''

READ = COMMON + r'''
mode=sys.argv[2]
c.load_collection("c")
if mode=="change":
    c.insert("c",[{"id":20,"v":[1.0,0.5],"geo":"polygon((0 0,1 0,1 1,0 0))"}])
    c.upsert("c",[{"id":0,"v":[1.0,0.0],"geo":None,"g2":"POINT(7 7)"}])
    c.delete("c",ids=[1])
    c.flush("c")
field=[f for f in stub.DescribeCollection(milvus_pb2.DescribeCollectionRequest(collection_name="c")).schema.fields if f.name=="g2"][0]
rows=sorted((r["id"],r["geo"],r["g2"]) for r in c.query("c",filter="id >= 0",output_fields=["geo","g2"],limit=100))
hits=[[h["id"],h["entity"]["geo"]] for h in c.search("c",data=[[0.0,0.0]],limit=2,output_fields=["geo"])[0]]
print(json.dumps({"type":field.data_type,"default":[field.default_value.WhichOneof("data"),field.default_value.string_data],"rows":rows,"hits":hits},sort_keys=True))
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
def test_geometry_directories_open_and_change_across_implementations(tmp_path, interpreters, writer, state):  # noqa: F811
    path = tmp_path / "g.db"
    run([python_of(interpreters, writer), "-c", WRITE, str(path), state])
    default = "  polygon((0 0,1 0,1 1,0 0)) "
    first = read(interpreters, path)
    assert first["type"] == 24
    assert first["default"] == ["string_data", default]
    assert first["rows"] == [
        [0, "POINT(1 1)", default],
        [1, "  point ( 1e1   -2.5 )  ", default],
        [2, "PO\u0130NT(\u0661 1_0)\n", default],
        [3, "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))", default],
        [4, None, default],
        [10, "POINT(3 3)", "POINT(4\u30004)"],
    ]
    schema = json.loads((path / "collections" / "c" / "schema.json").read_text())
    fields = {f["name"]: f for f in schema["fields"]}
    assert fields["geo"]["dtype"] == fields["g2"]["dtype"] == "geometry"
    assert fields["g2"]["default_value"] == default

    other = "reference25" if writer == "rust" else "rust"
    changed = json.loads(run([python_of(interpreters, other), "-c", READ, str(path), "change"]))
    assert changed["rows"] == [
        [0, None, "POINT(7 7)"],
        [2, "PO\u0130NT(\u0661 1_0)\n", default],
        [3, "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))", default],
        [4, None, default],
        [10, "POINT(3 3)", "POINT(4\u30004)"],
        [20, "polygon((0 0,1 0,1 1,0 0))", default],
    ]
    assert read(interpreters, path) == changed
