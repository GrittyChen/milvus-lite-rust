"""BM25 function 的数据目录互通（0031 第 1 步）。

三个实现轮流写读 WAL、segment、compaction 三种状态：函数定义、输出字段标记、停用词、默认值与 null 文本生成的
稀疏值都要在对方打开后原样读出；由对方改写（partial upsert 重新生成、写入）之后三方再读一次。BM25 检索结果
（id 与距离）只在本实现与 Arrow 25 的 3.x 之间逐位比较：Arrow 15 的 3.x 用 NumPy 1，分数按 float64 计算，
已在 0031「与 3.x 的差异」登记，只比数据。
"""
import json
import sys

import pytest
from test_cross_implementation import PYTHONS, interpreters, run  # noqa: F401

COMMON = r'''
import json,os,sys
from pymilvus import DataType,Function,FunctionType,MilvusClient
from milvus_lite.server_manager import ServerManager
path=sys.argv[1];manager=ServerManager()
c=MilvusClient(manager.start_and_get_uri(path),dedicated=True)
TEXTS=["The quick brown fox 95h9wb8","fox FOX the dog",None,"naïve café 重复 重复","and the 1 cat",""]
def row(i):
    r={"id":i,"tag":f"t{i}"}
    if i%7!=5: r["text"]=TEXTS[i%len(TEXTS)]
    return r
'''

WRITE = COMMON + r'''
state=sys.argv[2]
s=MilvusClient.create_schema(auto_id=False,enable_dynamic_field=True)
s.add_field("id",DataType.INT64,is_primary=True);s.add_field("tag",DataType.VARCHAR,max_length=16)
s.add_field("text",DataType.VARCHAR,max_length=100,enable_analyzer=True,nullable=True,
            analyzer_params={"tokenizer":"standard","filter":[{"type":"stop","stop_words":["the","1"]}]})
s.add_field("sp",DataType.SPARSE_FLOAT_VECTOR)
s.add_function(Function(name="bm",function_type=FunctionType.BM25,input_field_names=["text"],output_field_names=["sp"],
                        params={"note":"x","n":3}))
c.create_collection("c",schema=s)
for part in range(4 if state=="compacted" else 1):
    c.insert("c",[row(part*3+i) for i in range(3)])
    if state!="wal":c.flush("c")
if state=="wal":os._exit(0)
c.close();manager.release_all()
'''

READ = COMMON + r'''
mode=sys.argv[2]
if mode=="change":
    c.upsert("c",[{"id":0,"text":"brand new words"}],partial_update=True)
    c.upsert("c",[{"id":1,"tag":"z"}],partial_update=True)
    c.insert("c",[row(99)])
    c.flush("c")
d=c.describe_collection("c")
out={"rows":sorted(c.query("c",filter="id >= 0",output_fields=["*"]),key=lambda r:r["id"]),
     "fields":[(f["name"],f.get("params"),f.get("is_function_output",False)) for f in d["fields"]],
     "functions":[(f["name"],f["type"],list(f["input_field_names"]),list(f["output_field_names"]),f["params"])
                  for f in d["functions"]]}
if mode=="read":
    res=c.search("c",data=["fox dog","quick FOX the","重复 naïve café","95h9wb8"],anns_field="sp",limit=20)
    out["search"]=[[(h["id"],h["distance"]) for h in q] for q in res]
print(json.dumps(out,sort_keys=True,default=repr))
c.close();manager.release_all()
'''

WRITERS = {"rust": None, "reference15": PYTHONS[1], "reference25": PYTHONS[2]}


def python_of(interpreters, name):  # noqa: F811
    env = WRITERS[name]
    return sys.executable if env is None else interpreters[env]


def read(interpreters, path, mode="read"):  # noqa: F811
    results = [json.loads(run([python_of(interpreters, name), "-c", READ, str(path), mode])) for name in WRITERS]
    rust, ref15, ref25 = results
    # 「change」由三方依次改写同一目录，每次都多出旧版本行（计入 segment 的 N），分数只在只读时比较。
    if mode == "read":
        assert rust["search"] == ref25["search"]
        assert rust["search"][0] and rust["search"][1] and rust["search"][3]
        for r in results:
            del r["search"]
    assert rust == ref15 == ref25
    return rust


@pytest.mark.parametrize("writer", list(WRITERS))
@pytest.mark.parametrize("state", ["wal", "flushed", "compacted"])
def test_bm25_directories_open_in_every_implementation(tmp_path, interpreters, writer, state):  # noqa: F811
    path = tmp_path / "b.db"
    run([python_of(interpreters, writer), "-c", WRITE, str(path), state])
    if state == "compacted":
        # 4 次 flush 达到合并阈值，正常关闭排空维护后只剩一个合并后的数据文件（0010）。
        assert len(list(path.rglob("data_*.parquet"))) == 1
    first = read(interpreters, path)
    n = 12 if state == "compacted" else 3
    assert [r["id"] for r in first["rows"]] == list(range(n))
    # 停用词去掉 the；大小写合并；文本缺失时生成空向量。
    assert first["rows"][1]["sp"] == {"3069416270": 2.0, "3865623817": 1.0}
    assert first["rows"][2]["sp"] == {}
    # FNV-1a("95h9wb8") == 0xFFFFFFFF：生成的维度可以是普通稀疏值不允许的 2^32-1（0031「写入」）。
    assert first["rows"][0]["sp"]["4294967295"] == 1.0
    assert first["functions"] == [["bm", 1, ["text"], ["sp"], {"note": "x", "n": "3"}]]
    assert ["sp", {}, True] in first["fields"]
    changed = read(interpreters, path, "change")
    rows = {r["id"]: r for r in changed["rows"]}
    assert rows[0]["sp"] != first["rows"][0]["sp"]
    assert rows[1]["sp"] == first["rows"][1]["sp"] and rows[1]["tag"] == "z"
    assert 99 in rows
