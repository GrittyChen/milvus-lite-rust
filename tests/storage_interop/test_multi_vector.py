"""多向量字段的数据目录互通：不同维度、按字段的索引定义，WAL 与 segment，Arrow 15/25 双向（0028）。"""
import hashlib
import json
import sys
from pathlib import Path

import pytest
from test_cross_implementation import PYTHONS, interpreters, run  # noqa: F401

COMMON = r'''
import json,os,sys
from pymilvus import DataType,MilvusClient
from milvus_lite.server_manager import ServerManager
path=sys.argv[1];manager=ServerManager()
c=MilvusClient(manager.start_and_get_uri(path),dedicated=True)
def row(i): return {"id":i,"s":f"s{i}","v1":[1.0+i,0.0],"v2":[3.0*(i+1),0.0,4.0*(i+1)]}
'''

WRITE = COMMON + r'''
state=sys.argv[2]
s=MilvusClient.create_schema(auto_id=False,enable_dynamic_field=False)
s.add_field("id",DataType.INT64,is_primary=True);s.add_field("v1",DataType.FLOAT_VECTOR,dim=2)
s.add_field("s",DataType.VARCHAR,max_length=16);s.add_field("v2",DataType.FLOAT_VECTOR,dim=3)
p=c.prepare_index_params();p.add_index("v1",index_type="FLAT",metric_type="L2");p.add_index("v2",index_type="FLAT",metric_type="COSINE")
c.create_collection("c",schema=s,index_params=p)
for part in range(4 if state=="compacted" else 1):
    c.insert("c",[row(part*2+i) for i in range(2)])
    if state!="wal":c.flush("c")
if state=="wal":os._exit(0)
c.close();manager.release_all()
'''

READ = COMMON + r'''
mode=sys.argv[2]
c.load_collection("c")
if mode=="change":
    c.upsert("c",[{"id":0,"v2":[0.0,5.0,0.0]}],partial_update=True)
    c.delete("c",ids=[1])
    c.insert("c",[row(99)])
    c.flush("c")
rows=sorted(c.query("c",filter="id >= 0",output_fields=["*"]),key=lambda r:r["id"])
v1=c.search("c",[[1.0,0.0]],limit=20,output_fields=["s"])[0]
v2=c.search("c",[[0.0,0.0,1.0]],anns_field="v2",limit=20,output_fields=["s"])[0]
out={"rows":rows,"indexes":sorted(c.list_indexes("c")),
     "metrics":[c.describe_index("c",n)["metric_type"] for n in ("v1","v2")],
     "v1":sorted([h["id"],round(h["distance"],5)] for h in v1),
     "v2":sorted([h["id"],round(h["distance"],5)] for h in v2)}
print(json.dumps(out,sort_keys=True))
c.close();manager.release_all()
'''

NULL_WRITE = COMMON + r'''
state=sys.argv[2]
s=MilvusClient.create_schema(auto_id=False,enable_dynamic_field=False)
s.add_field("id",DataType.INT64,is_primary=True);s.add_field("v1",DataType.FLOAT_VECTOR,dim=2)
s.add_field("v2",DataType.FLOAT_VECTOR,dim=3)
c.create_collection("c",schema=s)
c.insert("c",[{"id":0,"v1":[1.0,0.0],"v2":[0.0,0.0,1.0]}])
c.upsert("c",[{"id":50,"v1":[1.0,0.0]}],partial_update=True)
if state=="flushed":c.flush("c")
os._exit(0) if state=="wal" else (c.close(),manager.release_all())
'''

RUST_OPEN_FAILS = COMMON + r'''
from pymilvus.exceptions import MilvusException
try:
    c.query("c",filter="id >= 0",output_fields=["id"])
except MilvusException as e:
    print(json.dumps([e.code,e.message]))
else:
    raise SystemExit("collection with a null vector opened")
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
@pytest.mark.parametrize("state", ["wal", "flushed", "compacted"])
def test_two_vector_directories_open_in_every_implementation(tmp_path, interpreters, writer, state):  # noqa: F811
    path = tmp_path / "m.db"
    run([python_of(interpreters, writer), "-c", WRITE, str(path), state])
    first = read(interpreters, path)
    n = 8 if state == "compacted" else 2
    assert [r["id"] for r in first["rows"]] == list(range(n))
    assert first["indexes"] == ["v1", "v2"]
    assert first["metrics"] == ["L2", "COSINE"]
    # 不共线才看得出范数：相似度被截断到 1 以内，共线时错误的范数也得 1。
    assert all(score == 0.8 for _, score in first["v2"])
    assert dict(map(tuple, first["v1"]))[0] == 0.0

    # 对方改写后再由所有实现读取。
    other = "reference25" if writer == "rust" else "rust"
    changed = json.loads(run([python_of(interpreters, other), "-c", READ, str(path), "change"]))
    assert [r["id"] for r in changed["rows"]] == [0, *range(2, n), 99]
    assert changed["rows"][0]["v2"] == [0.0, 5.0, 0.0]
    assert changed["rows"][0]["v1"] == [1.0, 0.0]
    assert read(interpreters, path) == changed


def digests(root):
    return {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in sorted(Path(root).rglob("*")) if p.is_file() and p.name != "LOCK"}


@pytest.mark.parametrize("state", ["wal", "flushed"])
def test_a_null_later_vector_from_3x_fails_explicitly_and_changes_nothing(tmp_path, interpreters, state):  # noqa: F811
    path = tmp_path / "null.db"
    run([interpreters[PYTHONS[2]], "-c", NULL_WRITE, str(path), state])
    before = digests(path)
    out = json.loads(run([sys.executable, "-c", RUST_OPEN_FAILS, str(path)]))
    assert out[0] != 0 and "v2" in out[1], out
    assert digests(path) == before
