"""jieba 分词器的数据目录互通（0036）。

参照环境须装 jieba 0.42.1（0036 决策 F、G）。三个实现轮流写读只有 WAL、已 flush 与经过合并的 jieba 集合：
没有用户词与带用户词（都是不在词典里的新造词）的两个集合，`analyzer_params` 的回显、生成的稀疏值要在对方打开后
原样读出；由对方写入与 partial upsert 重新生成之后三方再读一次。BM25 检索结果只在本实现与 Arrow 25 的 3.x 之间
逐位比较（Arrow 15 的 3.x 用 NumPy 1，见 0031「与 3.x 的差异」）。

3.x 每次构造分词器都把用户词再加进进程全局的词典（0036 决策 C）。读者每次都是新进程，所以对新造词，各方读出的
结果相同；受污染的 3.x 进程写出的目录单独由 `test_polluted_3x_directory` 覆盖（决策 D）。
"""
import json
import sys

import pytest
from test_bm25 import WRITERS, python_of
from test_cross_implementation import PYTHONS, interpreters, run  # noqa: F401

COMMON = r'''
import json,os,sys
from pymilvus import DataType,Function,FunctionType,MilvusClient
from milvus_lite.server_manager import ServerManager
path=sys.argv[1];manager=ServerManager()
c=MilvusClient(manager.start_and_get_uri(path),dedicated=True)
TEXTS=["我爱米尔维斯","深度学习是人工智能的子领域","价格1.5元，折扣0.5%",None,"米尔维斯向量数据库很快","ΣΑΣ Σ T.S.Eliot","","今天的天气是很好的"]
PARAMS={"plain":{"tokenizer":"jieba","filter":[{"type":"stop","stop_words":["的",1.5]}]},
        "user":{"tokenizer":{"type":"jieba","mode":"exact","dict":["米尔维斯","向量数据库"]}}}
def row(i):
    r={"id":i}
    if i%9!=3: r["text"]=TEXTS[i%len(TEXTS)]
    return r
'''

WRITE = COMMON + r'''
state=sys.argv[2]
for name,params in PARAMS.items():
    s=MilvusClient.create_schema(auto_id=False)
    s.add_field("id",DataType.INT64,is_primary=True)
    s.add_field("text",DataType.VARCHAR,max_length=100,enable_analyzer=True,nullable=True,analyzer_params=params)
    s.add_field("sp",DataType.SPARSE_FLOAT_VECTOR)
    s.add_function(Function(name="bm",function_type=FunctionType.BM25,input_field_names=["text"],output_field_names=["sp"]))
    c.create_collection(name,schema=s)
    for part in range(4 if state=="compacted" else 1):
        c.insert(name,[row(part*4+i) for i in range(4)])
        if state!="wal":c.flush(name)
if state=="wal":os._exit(0)
c.close();manager.release_all()
'''

READ = COMMON + r'''
mode=sys.argv[2]
out={}
for name in PARAMS:
    if mode=="change":
        c.upsert(name,[{"id":0,"text":"米尔维斯和深度学习"}],partial_update=True)
        c.insert(name,[row(99)])
        c.flush(name)
    d=c.describe_collection(name)
    out[name]={"rows":sorted(c.query(name,filter="id >= 0",output_fields=["*"]),key=lambda r:r["id"]),
               "params":[f.get("params") for f in d["fields"] if f["name"]=="text"]}
    if mode=="read":
        res=c.search(name,data=["米尔维斯","深度学习 天气","1.5 0.5%","σ"],anns_field="sp",limit=20)
        out[name]["search"]=[[(h["id"],h["distance"]) for h in q] for q in res]
print(json.dumps(out,sort_keys=True,ensure_ascii=False,default=repr))
c.close();manager.release_all()
'''


def read(interpreters, path, mode="read"):  # noqa: F811
    results = [json.loads(run([python_of(interpreters, name), "-c", READ, str(path), mode])) for name in WRITERS]
    rust, ref15, ref25 = results
    if mode == "read":
        for name in rust:
            assert rust[name]["search"] == ref25[name]["search"]
            assert all(rust[name]["search"][:3]), name
            for r in results:
                del r[name]["search"]
    assert rust == ref15 == ref25
    return rust


def term_id(term):
    h = 0x811C9DC5
    for b in term.encode("utf-8"):
        h = ((h ^ b) * 0x01000193) & 0xFFFFFFFF
    return str(h)


@pytest.mark.parametrize("writer", list(WRITERS))
@pytest.mark.parametrize("state", ["wal", "flushed", "compacted"])
def test_jieba_directories_open_in_every_implementation(tmp_path, interpreters, writer, state):  # noqa: F811
    path = tmp_path / "j.db"
    run([python_of(interpreters, writer), "-c", WRITE, str(path), state])
    if state == "compacted":
        # 每个集合 4 次 flush 达到合并阈值，正常关闭排空维护后各剩一个数据文件（0010）。
        for name in ("plain", "user"):
            assert len(list((path / "collections" / name).rglob("data_*.parquet"))) == 1
    first = read(interpreters, path)
    n = 16 if state == "compacted" else 4
    for name in ("plain", "user"):
        assert [r["id"] for r in first[name]["rows"]] == list(range(n))
    # search 模式与停用词：没有用户词时“米尔维斯”切成三段；“的”与浮点停用词 1.5 都不出现。
    plain = {r["id"]: r["sp"] for r in first["plain"]["rows"]}
    assert set(plain[0]) == {term_id(t) for t in ("我", "爱", "米尔", "米尔维", "斯")}
    assert term_id("1.5") not in plain[2] and term_id("0.5%") in plain[2]
    # exact 模式加用户词：用户词整体出现。
    user = {r["id"]: r["sp"] for r in first["user"]["rows"]}
    assert set(user[0]) == {term_id(t) for t in ("我", "爱", "米尔维斯")}
    # `analyzer_params` 按原值回显（0029），三方相同由 read() 断言。
    assert json.loads(first["user"]["params"][0]["analyzer_params"]) == {
        "tokenizer": {"type": "jieba", "mode": "exact", "dict": ["米尔维斯", "向量数据库"]}}
    changed = read(interpreters, path, "change")
    for name in ("plain", "user"):
        rows = {r["id"]: r for r in changed[name]["rows"]}
        assert rows[0]["sp"] != first[name]["rows"][0]["sp"] and 99 in rows


POLLUTE = COMMON + r'''
mode=sys.argv[2]
def create(name,params):
    s=MilvusClient.create_schema(auto_id=False)
    s.add_field("id",DataType.INT64,is_primary=True)
    s.add_field("text",DataType.VARCHAR,max_length=100,enable_analyzer=True,analyzer_params=params)
    s.add_field("sp",DataType.SPARSE_FLOAT_VECTOR)
    s.add_function(Function(name="bm",function_type=FunctionType.BM25,input_field_names=["text"],output_field_names=["sp"]))
    c.create_collection(name,schema=s)
if mode=="write":
    create("user",PARAMS["user"]);create("plain",{"tokenizer":"jieba"})
    c.insert("plain",[{"id":0,"text":"我爱米尔维斯"}])
else:
    c.insert("plain",[{"id":1,"text":"我爱米尔维斯"}])
print(json.dumps({r["id"]:r["sp"] for r in c.query("plain",filter="id >= 0",output_fields=["sp"])},sort_keys=True))
c.close();manager.release_all()
'''


def test_polluted_3x_directory(tmp_path, interpreters):  # noqa: F811
    """0036 决策 D：3.x 进程里先建了带用户词的集合，之后没有用户词的集合按全局词典切分。本实现打开后原样读出
    已存的稀疏值，新写入的行按本实现的切法生成，同一集合里两种切法并存。"""
    path = tmp_path / "p.db"
    written = json.loads(run([interpreters[PYTHONS[2]], "-c", POLLUTE, str(path), "write"]))
    polluted = {term_id(t) for t in ("我", "爱", "米尔", "维斯", "米尔维", "米尔维斯")}
    assert set(written["0"]) == polluted
    rows = json.loads(run([sys.executable, "-c", POLLUTE, str(path), "append"]))
    assert set(rows["0"]) == polluted
    assert set(rows["1"]) == {term_id(t) for t in ("我", "爱", "米尔", "米尔维", "斯")}
