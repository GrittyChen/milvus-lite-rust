"""稀疏向量字段的数据目录互通（0030）。

- 三个实现轮流写读：WAL、segment、compaction 三种状态，Arrow 15/25 与本实现双向，读后由对方改写（partial upsert
  继承稀疏值、删除、写入）再由三方读取。
- 3.x 引擎 API 写的目录：nullable 稀疏行（segment 与只在 WAL 中的行）、改写 parquet 得到的历史坏行、稀疏默认值。
  同一目录复制两份，本实现与 3.x 各执行同样的读写，除已登记的差异外逐项一致。
"""
import json
import shutil
import sys

import pytest
from test_cross_implementation import PYTHONS, interpreters, run  # noqa: F401

COMMON = r'''
import json,os,sys
from pymilvus import AnnSearchRequest,DataType,MilvusClient,RRFRanker
from milvus_lite.server_manager import ServerManager
path=sys.argv[1];manager=ServerManager()
c=MilvusClient(manager.start_and_get_uri(path),dedicated=True)
def row(i): return {"id":i,"s":f"s{i}","d":[1.0,float(i)],"sp":{(i*7)%11:1.0+i,i+20:0.5}}
'''

WRITE = COMMON + r'''
state=sys.argv[2]
s=MilvusClient.create_schema(auto_id=False,enable_dynamic_field=True)
s.add_field("id",DataType.INT64,is_primary=True);s.add_field("s",DataType.VARCHAR,max_length=16)
s.add_field("d",DataType.FLOAT_VECTOR,dim=2);s.add_field("sp",DataType.SPARSE_FLOAT_VECTOR)
p=c.prepare_index_params();p.add_index("d",index_type="FLAT",metric_type="L2")
p.add_index("sp",index_type="SPARSE_INVERTED_INDEX",metric_type="IP",params={"inverted_index_algo":"TAAT_NAIVE"})
c.create_collection("c",schema=s,index_params=p)
for part in range(4 if state=="compacted" else 1):
    c.insert("c",[dict(row(part*2+i),extra={"k":part}) for i in range(2)])
    if state!="wal":c.flush("c")
if state=="wal":os._exit(0)
c.close();manager.release_all()
'''

READ = COMMON + r'''
mode=sys.argv[2]
c.load_collection("c")
if mode=="change":
    c.upsert("c",[{"id":0,"d":[9.0,9.0]}],partial_update=True)
    c.delete("c",ids=[1])
    c.insert("c",[row(99)])
    c.flush("c")
q=[{0:1.0,7:1.0,20:2.0,21:1.0}]
out={"rows":sorted(c.query("c",filter="id >= 0",output_fields=["*"]),key=lambda r:r["id"]),
     "index":c.describe_index("c","sp"),
     "search":[[h["id"],h["distance"],h["entity"]] for h in c.search("c",q,anns_field="sp",limit=20,output_fields=["sp","s"])[0]],
     "hybrid":[[h["id"],h["distance"]] for h in c.hybrid_search("c",[AnnSearchRequest(q,"sp",{},20),
                                                                    AnnSearchRequest([[1.0,0.0]],"d",{},20)],RRFRanker(),limit=20)[0]]}
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
@pytest.mark.parametrize("state", ["wal", "flushed", "compacted"])
def test_sparse_directories_open_in_every_implementation(tmp_path, interpreters, writer, state):  # noqa: F811
    path = tmp_path / "s.db"
    run([python_of(interpreters, writer), "-c", WRITE, str(path), state])
    first = read(interpreters, path)
    n = 8 if state == "compacted" else 2
    assert [r["id"] for r in first["rows"]] == list(range(n))
    assert first["rows"][1]["sp"] == {"7": 2.0, "21": 0.5}
    assert first["index"]["metric_type"] == "IP"
    assert first["search"][0][0] == 1

    other = "reference25" if writer == "rust" else "rust"
    changed = json.loads(run([python_of(interpreters, other), "-c", READ, str(path), "change"]))
    assert [r["id"] for r in changed["rows"]] == [0, *range(2, n), 99]
    # partial upsert 继承旧行的稀疏值。
    assert changed["rows"][0]["sp"] == first["rows"][0]["sp"]
    assert changed["rows"][0]["d"] == [9.0, 9.0]
    assert read(interpreters, path) == changed


ENGINE = r'''
import os,sys,struct,glob
from milvus_lite.db import MilvusLite
from milvus_lite.schema.types import CollectionSchema,DataType,FieldSchema
path,stage=sys.argv[1],sys.argv[2]
def packed(pairs): return b"".join(struct.pack("<If",i,v) for i,v in pairs)
def fields(**sp): return CollectionSchema(fields=[FieldSchema("id",DataType.INT64,is_primary=True),
    FieldSchema("d",DataType.FLOAT_VECTOR,dim=2),FieldSchema("sp",DataType.SPARSE_FLOAT_VECTOR,**sp)])
if stage=="write":
    db=MilvusLite(path)
    for kind in ["neg","nan","inf","dim","len","empty","zero","unsorted_dup"]:
        col=db.create_collection(f"bad_{kind}",fields())
        col.insert([{"id":i,"d":[1,0],"sp":{1:float(i)}} for i in range(1,4)])
        col.flush()
    dflt=db.create_collection("dflt",fields(default_value={1:0.5}))
    dflt.insert([{"id":1,"d":[1,0]},{"id":2,"d":[1,0],"sp":{2:1.0}}])
    dflt.flush()
    nul=db.create_collection("nul",fields(nullable=True))
    nul.insert([{"id":1,"d":[1,0],"sp":{1:1.0}},{"id":2,"d":[1,0],"sp":None},{"id":3,"d":[1,0]}])
    nul.flush()
    nul.insert([{"id":4,"d":[1,0],"sp":None},{"id":5,"d":[1,0],"sp":{1:2.0}},{"id":6,"d":[1,0]}])
    part=db.create_collection("bad_part",fields())
    part.create_partition("p1")
    part.insert([{"id":i,"d":[1,0],"sp":{1:float(i)}} for i in (1,2)],partition_name="p1")
    part.insert([{"id":i,"d":[1,0],"sp":{1:float(i)}} for i in (11,12)])
    part.flush()
    wal=db.create_collection("bad_wal",fields())
    wal.insert([{"id":i,"d":[1,0],"sp":{1:float(i)}} for i in range(1,4)])
    os._exit(0)
import pyarrow as pa,pyarrow.ipc as ipc,pyarrow.parquet as pq
# 分区 p1 的 segment 里放坏行
[f]=glob.glob(os.path.join(path,"collections","bad_part","partitions","p1","data","*.parquet"))
t=pq.read_table(f);col=t.column("sp").to_pylist();col[0]=packed([(1,-1.0)])
pq.write_table(t.set_column(t.schema.get_field_index("sp"),t.schema.field("sp"),pa.array(col,type=t.schema.field("sp").type)),f)
# 只在 WAL 中的坏行：改写 Arrow IPC stream 里 id 2 的稀疏字节
[f]=glob.glob(os.path.join(path,"collections","bad_wal","wal","wal_data_*.arrow"))
with open(f,"rb") as stream: reader=ipc.open_stream(stream);schema=reader.schema;batches=list(reader)
out=[]
for b in batches:
    t=pa.Table.from_batches([b]);col=t.column("sp").to_pylist();ids=t.column("id").to_pylist()
    col=[packed([(1,-1.0)]) if pk==2 else v for pk,v in zip(ids,col)]
    out.append(t.set_column(t.schema.get_field_index("sp"),t.schema.field("sp"),pa.array(col,type=t.schema.field("sp").type)).to_batches()[0])
with open(f,"wb") as stream:
    w=ipc.new_stream(stream,schema)
    for b in out: w.write_batch(b)
    w.close()
bad={"neg":packed([(1,-1.0)]),"nan":packed([(1,float("nan"))]),"inf":packed([(1,float("inf"))]),
     "dim":packed([(2**32-1,1.0)]),"len":packed([(1,1.0)])[:7],"empty":b"","zero":packed([(1,0.0)]),
     "unsorted_dup":packed([(5,1.0),(1,2.0),(5,3.0)])}
for kind,blob in bad.items():
    [f]=glob.glob(os.path.join(path,"collections",f"bad_{kind}","partitions","_default","data","*.parquet"))
    t=pq.read_table(f);col=t.column("sp").to_pylist();col[1]=blob
    t=t.set_column(t.schema.get_field_index("sp"),t.schema.field("sp"),pa.array(col,type=t.schema.field("sp").type))
    pq.write_table(t,f)
'''

READ_ENGINE_DIR = r'''
import json,sys
import grpc
from pymilvus import AnnSearchRequest,MilvusClient,RRFRanker
from pymilvus.exceptions import MilvusException
from milvus_lite.server_manager import ServerManager
path=sys.argv[1];manager=ServerManager()
c=MilvusClient(manager.start_and_get_uri(path),dedicated=True)
out={}
def rec(name,fn):
    try: out[name]=["ok",fn()]
    except MilvusException as e: out[name]=["err",e.code,e.message]
    except grpc.RpcError as e: out[name]=["rpc",e.code().name,e.details()]
def hits(res): return [[[h["id"],h["distance"],h["entity"]] for h in q] for q in res]
for name in ["nul","bad_wal"]+[f"bad_{k}" for k in ["neg","nan","inf","dim","len","empty","zero","unsorted_dup"]]:
    rec(f"{name}/load",lambda:c.load_collection(name))
    rec(f"{name}/count",lambda:c.query(name,filter="",output_fields=["count(*)"]))
    rec(f"{name}/query",lambda:c.query(name,filter="id > 0",output_fields=["sp"]))
    rec(f"{name}/get2",lambda:c.get(name,ids=[2],output_fields=["sp"]))
    rec(f"{name}/search",lambda:hits(c.search(name,[{1:1.0}],anns_field="sp",limit=10,output_fields=["sp"])))
    rec(f"{name}/search_excl",lambda:hits(c.search(name,[{1:1.0}],anns_field="sp",limit=10,filter="id != 2")))
    rec(f"{name}/search_dense",lambda:hits(c.search(name,[[1.0,0.0]],anns_field="d",limit=10,output_fields=["sp"])))
    rec(f"{name}/hybrid",lambda:hits(c.hybrid_search(name,[AnnSearchRequest([{1:1.0}],"sp",{},10),
        AnnSearchRequest([[1.0,0.0]],"d",{},10)],RRFRanker(),limit=10)))
# 坏行被删除、整行覆盖、partial upsert 继承之后
rec("bad_neg/delete2",lambda:c.delete("bad_neg",ids=[2]))
rec("bad_neg/after_delete",lambda:hits(c.search("bad_neg",[{1:1.0}],anns_field="sp",limit=10)))
rec("bad_nan/upsert2",lambda:c.upsert("bad_nan",[{"id":2,"d":[1,0],"sp":{1:9.0}}])["upsert_count"])
rec("bad_nan/after_upsert",lambda:hits(c.search("bad_nan",[{1:1.0}],anns_field="sp",limit=10)))
for kind in ["inf","len","zero","unsorted_dup"]:
    rec(f"bad_{kind}/partial_inherit",lambda:c.upsert(f"bad_{kind}",[{"id":2,"d":[0,1]}],partial_update=True)["upsert_count"])
    rec(f"bad_{kind}/after_partial",lambda:c.query(f"bad_{kind}",filter="id == 2",output_fields=["sp","d"]))
rec("bad_wal/partial_inherit",lambda:c.upsert("bad_wal",[{"id":2,"d":[0,1]}],partial_update=True)["upsert_count"])
rec("bad_wal/delete2",lambda:c.delete("bad_wal",ids=[2]))
rec("bad_wal/after_delete",lambda:hits(c.search("bad_wal",[{1:1.0}],anns_field="sp",limit=10)))
# p1 的行全部删除后，单分区检索仍为该 segment 建倒排并报坏行；只选 _default 时不建
rec("bad_part/load",lambda:c.load_collection("bad_part"))
rec("bad_part/delete_p1",lambda:c.delete("bad_part",ids=[1,2]))
for parts in (["p1"],["_default"],["p1","_default"],None):
    rec(f"bad_part/search_{parts}",lambda parts=parts:hits(c.search("bad_part",[{1:1.0}],anns_field="sp",limit=10,partition_names=parts)))
rec("nul/partial_inherit",lambda:c.upsert("nul",[{"id":1,"d":[0,1]},{"id":2,"d":[0,1]}],partial_update=True)["upsert_count"])
rec("nul/flush",lambda:c.flush("nul"))
rec("nul/after_flush",lambda:c.query("nul",filter="id in [1, 2, 5]",output_fields=["sp","d"]))
# 稀疏默认值：本实现拒绝打开该集合（已登记），可以 drop，同库其他集合不受影响
rec("dflt/describe",lambda:c.describe_collection("dflt")["collection_name"])
rec("dflt/load",lambda:c.load_collection("dflt"))
rec("dflt/query",lambda:c.query("dflt",filter="id > 0",output_fields=["sp"]))
rec("dflt/drop",lambda:c.drop_collection("dflt"))
rec("dflt/list",lambda:sorted(c.list_collections()))
print(json.dumps(out,sort_keys=True,default=repr))
c.close();manager.release_all()
'''

DEFAULT_REFUSED = 'MilvusLite does not support collection content: default value of sparse vector field "sp"'


def registered(step, rust, ref):
    """0030 已登记的差异：返回 True 表示该步已按登记内容分别断言。"""
    collection, _, action = step.partition("/")
    if collection == "dflt" and action in ("describe", "load", "query"):
        assert rust == ["rpc", "UNIMPLEMENTED", DEFAULT_REFUSED], rust
        assert ref[0] == "ok", ref
        return True
    if collection == "nul" and action in ("query", "get2", "search", "search_dense", "after_flush"):
        # nullable 稀疏输出：本实现按紧凑格式编码，pymilvus 读出真实值；3.x 补齐 contents，有效行被读成 {}。
        # 只有有效行的 sp 取值不同：其余内容逐项相等，sp 的空值位置相同，本实现读出引擎 API 写入的值。
        assert rust[0] == ref[0] == "ok", (rust, ref)
        assert without_sp(rust) == without_sp(ref), (rust, ref)
        for (pk, got), (_, theirs) in zip(sparse_by_id(rust, action), sparse_by_id(ref, action), strict=True):
            assert got == NUL_VALUES.get(pk), (step, pk, got)
            # 3.x 按行补齐 contents，pymilvus 按有效行紧凑读取：第 k 个有效行读到第 k 行的内容，
            # 排在空值之后的有效行读成 {}。
            assert (theirs is None) == (got is None) and theirs in (got, {}), (step, pk, theirs)
        return True
    return False


# 3.x 引擎 API 写入 nul 集合的有效稀疏值；其余行为空值（partial upsert 继承各自的值或空值）。
NUL_VALUES = {1: {"1": 1.0}, 5: {"1": 2.0}}


def without_sp(value):
    if isinstance(value, dict):
        return {k: without_sp(v) for k, v in value.items() if k != "sp"}
    if isinstance(value, list):
        return [without_sp(v) for v in value]
    return value


def sparse_by_id(result, action):
    if action in ("query", "get2", "after_flush"):
        return [(row["id"], row["sp"]) for row in result[1]]
    return [(hit[0], hit[2]["sp"]) for hits in result[1] for hit in hits]


def test_3x_engine_directories_read_like_3x(tmp_path, interpreters):  # noqa: F811
    base = tmp_path / "engine.db"
    reference = interpreters[PYTHONS[2]]
    run([reference, "-c", ENGINE, str(base), "write"])
    run([reference, "-c", ENGINE, str(base), "poison"])
    results = {}
    for name, python in [("rust", sys.executable), ("ref", reference)]:
        copy = tmp_path / f"{name}.db"
        shutil.copytree(base, copy)
        results[name] = json.loads(run([python, "-c", READ_ENGINE_DIR, str(copy)]))
    rust, ref = results["rust"], results["ref"]
    assert rust.keys() == ref.keys()
    failures = [step for step in rust if not registered(step, rust[step], ref[step]) and rust[step] != ref[step]]
    assert not failures, "\n".join(f"{s}:\n  rust: {rust[s]}\n  3.x:  {ref[s]}" for s in failures)
    # 默认值集合 drop 之后，两边的集合列表相同。
    assert rust["dflt/list"] == ref["dflt/list"]
    # 登记的 3.x 缺陷确实出现：至少一个有效行被读成 {}。参照修复后这里失败，提示重新审视 0030 的登记。
    assert {} in [sp for _, sp in sparse_by_id(ref["nul/query"], "query")]
    # nullable 的真实值：只在 WAL 中的行与 segment 中的行都在，partial upsert 继承空值与非空值。
    assert rust["nul/after_flush"][1] == [
        {"id": 1, "sp": {"1": 1.0}, "d": [0.0, 1.0]},
        {"id": 2, "sp": None, "d": [0.0, 1.0]},
        {"id": 5, "sp": {"1": 2.0}, "d": [1.0, 0.0]},
    ]
