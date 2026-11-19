"""0038：nullable 稠密向量的空值写为 Arrow null 后，与 3.x 的互通形态。

pyarrow 低于 26 时 3.x 读不了含空行的定长列表 Parquet：含空行的集合在 Arrow 15 / 25 上按登记的失败形态断言，
这些失败就是 0002 中登记风险的证据。Arrow 26 上 3.x 能读，其输出错位、WAL 中的空行按零向量参与检索、
partial upsert 把空值改写成零向量，同样按 3.x 的实际结果断言。没写过空值的 nullable 集合与非 nullable 时
相同，各版本都能读。3.x 写下的目录里空值已是零向量，本实现读为零向量。
"""

import json
import sys

import pytest
from test_cross_implementation import ARROW26, PYTHONS, interpreters, run  # noqa: F401

ARROWS = {"15": PYTHONS[1], "25": PYTHONS[2], "26": ARROW26}
OLD_ARROW_FLUSH = {
    "15": "Lists with non-zero length null components are not supported",
    "25": "Expected all lists to be of size=2",
}
UNREADABLE = "Expected all lists to be of size=2"

# id 1、2 为空；其后的有效行 3、4 用来暴露 3.x 的输出错位。
RUST_WRITE = r'''
import os,sys
from pymilvus import MilvusClient,DataType
from milvus_lite.server_manager import server_manager_instance as manager
path,state,nulls=sys.argv[1:];nulls=nulls=='nulls'
c=MilvusClient(path,dedicated=True)
s=c.create_schema(auto_id=False,enable_dynamic_field=False)
s.add_field('id',DataType.INT64,is_primary=True);s.add_field('v',DataType.FLOAT_VECTOR,dim=2,nullable=True)
s.add_field('g',DataType.INT64)
p=c.prepare_index_params();p.add_index('v',index_type='FLAT',metric_type='L2')
c.create_collection('c',schema=s,index_params=p)
rows=[{'id':0,'v':[1.,0.]},{'id':1,'v':None},{'id':2,'v':None},{'id':3,'v':[0.,1.]},{'id':4,'v':[2.,2.]}]
if not nulls:rows=[r for r in rows if r['v'] is not None]
# 合并要 4 个数据文件（与 3.x 相同），正常关闭时触发。
for part in range(4 if state=='compacted' else 1):
 c.insert('c',[dict(r,id=r['id']+10*part,g=0) for r in rows])
 if state!='wal':c.flush('c')
if state=='wal':os._exit(0)
c.close();manager.release_server(path)
'''

# 每一步单独记录成败：3.x 在较早的一步失败时，后面的步骤仍要观察。
OBSERVE = r'''
import json,sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as manager
path,mode=sys.argv[1:]
c=MilvusClient(path,dedicated=True)
out={}
def step(name,fn):
 try:out[name]=['ok',fn()]
 except Exception as e:out[name]=['err',str(getattr(e,'message',None) or e)]
step('load',lambda:c.load_collection('c'))
step('count',lambda:c.query('c',filter='',output_fields=['count(*)'])[0]['count(*)'])
step('query',lambda:sorted([r['id'],r['v']] for r in c.query('c',filter='id < 10',output_fields=['v'])))
step('get',lambda:[[r['id'],r['v']] for id in (1,3,4) for r in c.get('c',ids=[id],output_fields=['v'])])
step('search',lambda:sorted(h['id'] for h in c.search('c',[[1.,0.]],limit=20)[0] if h['id']<10))
if mode in ('flush','upsert'):
 if mode=='upsert':step('upsert',lambda:c.upsert('c',[{'id':1,'g':9}],partial_update=True) and None)
 step('flush',lambda:c.flush('c'))
c.close();manager.release_server(path)
print(json.dumps(out))
'''

# 本实现读回：每行的向量与检索命中的主键。
RUST_READ = r'''
import json,sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as manager
path,mode=sys.argv[1:];c=MilvusClient(path,dedicated=True)
c.load_collection('c')
rows=sorted([r['id'],r['v']] for r in c.query('c',filter='id < 10',output_fields=['v']))
hits=sorted(h['id'] for h in c.search('c',[[1.,0.]],limit=20)[0] if h['id']<10)
c.close();manager.release_server(path)
print(json.dumps([rows,hits]))
'''

INSPECT = r'''
import json,sys
from pathlib import Path
import pyarrow.parquet as pq
root=Path(sys.argv[1])/'collections/c'
m=json.loads((root/'manifest.json').read_text())
latest={}
for name in m['partitions']['_default']['data_files']:
 t=pq.read_table(root/'partitions/_default'/name)
 t.validate(full=True)
 for i,seq,v in zip(t.column('id').to_pylist(),t.column('_seq').to_pylist(),t.column('v').to_pylist()):
  if i<10 and seq>=latest.get(i,(-1,None))[0]:latest[i]=(seq,v)
print(json.dumps(sorted([i,v] for i,(_,v) in latest.items())))
'''

STORED = [[0, [1.0, 0.0]], [1, None], [2, None], [3, [0.0, 1.0]], [4, [2.0, 2.0]]]
SINGLE_GETS = [[1, None], [3, [0.0, 1.0]], [4, [2.0, 2.0]]]


def rust_write(tmp_path, state, nulls=True):
    path = tmp_path / f"{state}.db"
    run([sys.executable, "-c", RUST_WRITE, str(path), state, "nulls" if nulls else "plain"], cwd=tmp_path)
    if state == "compacted":
        manifest = json.loads((path / "collections/c/manifest.json").read_text())
        assert len(manifest["partitions"]["_default"]["data_files"]) == 1, manifest
    return path


def observe(python, path, mode="read"):
    return json.loads(run([python, "-c", OBSERVE, str(path), mode], cwd=path.parent))


def rust_read(path):
    return json.loads(run([sys.executable, "-c", RUST_READ, str(path), "read"], cwd=path.parent))


@pytest.mark.parametrize("arrow", ["15", "25"])
@pytest.mark.parametrize("state", ["wal", "flushed", "compacted"])
def test_old_pyarrow_fails_as_registered_and_keeps_the_data(tmp_path, interpreters, state, arrow):
    path = rust_write(tmp_path, state)
    got = observe(interpreters[ARROWS[arrow]], path, "flush")
    if state == "wal":
        # 回放成功，空行按零向量参与检索；flush 写 Parquet 时失败。
        assert got["load"] == ["ok", None] and got["count"] == ["ok", 5], got
        assert got["get"] == ["ok", SINGLE_GETS], got
        assert got["search"] == ["ok", [0, 1, 2, 3, 4]], got
        assert got["flush"][0] == "err" and OLD_ARROW_FLUSH[arrow] in got["flush"][1], got
    else:
        # 含空行的 Parquet 读不了，整个集合不可用。
        for name in ("load", "count", "query", "get", "search", "flush"):
            assert got[name][0] == "err" and UNREADABLE in got[name][1], (name, got)
    # 数据仍在盘上，本实现照常读出。
    assert rust_read(path) == [STORED, [0, 3, 4]]


@pytest.mark.parametrize("state", ["wal", "flushed", "compacted"])
def test_pyarrow_26_reads_with_the_registered_3x_defects(tmp_path, interpreters, state):
    path = rust_write(tmp_path, state)
    got = observe(interpreters[ARROW26], path, "upsert")
    assert got["load"] == ["ok", None], got
    assert got["count"] == ["ok", 20 if state == "compacted" else 5], got
    # 单行 Get 正确；多行输出中空行之后的有效行错位（3.x 的完整占位输出，0030 的同一缺陷）。
    assert got["get"] == ["ok", SINGLE_GETS], got
    rows = dict(got["query"][1])
    assert rows[1] is None and rows[2] is None and rows[0] == [1.0, 0.0], got
    assert rows[3] != [0.0, 1.0] or rows[4] != [2.0, 2.0], got
    # WAL 回放进 MemTable 的空行按零向量参与检索；Parquet 中的空行被排除。
    assert got["search"] == ["ok", [0, 1, 2, 3, 4] if state == "wal" else [0, 3, 4]], got
    assert got["upsert"] == ["ok", None] and got["flush"] == ["ok", None], got
    # partial upsert 只给 g：3.x 把 id 1 的空值永久改写成零向量；没动过的 id 2 仍为空。
    stored = dict(json.loads(run([interpreters[ARROW26], "-c", INSPECT, str(path)])))
    assert stored[1] == [0.0, 0.0] and stored[2] is None and stored[4] == [2.0, 2.0], stored
    rows, hits = rust_read(path)
    assert rows == [[0, [1.0, 0.0]], [1, [0.0, 0.0]], [2, None], [3, [0.0, 1.0]], [4, [2.0, 2.0]]]
    assert hits == [0, 1, 3, 4]


def test_rust_reads_parquet_that_3x_wrote_from_its_null_wal(tmp_path, interpreters):
    path = rust_write(tmp_path, "wal")
    assert observe(interpreters[ARROW26], path, "flush")["flush"] == ["ok", None]
    assert dict(json.loads(run([interpreters[ARROW26], "-c", INSPECT, str(path)])))[1] is None
    assert rust_read(path) == [STORED, [0, 3, 4]]


@pytest.mark.parametrize("arrow", ["15", "25"])
@pytest.mark.parametrize("state", ["wal", "flushed"])
def test_nullable_collection_without_nulls_stays_readable_by_old_pyarrow(tmp_path, interpreters, state, arrow):
    path = rust_write(tmp_path, state, nulls=False)
    got = observe(interpreters[ARROWS[arrow]], path, "flush")
    assert got["count"] == ["ok", 3], got
    assert got["query"] == ["ok", [[0, [1.0, 0.0]], [3, [0.0, 1.0]], [4, [2.0, 2.0]]]], got
    assert got["search"] == ["ok", [0, 3, 4]] and got["flush"] == ["ok", None], got


REFERENCE_WRITE = r'''
import sys
from pymilvus import MilvusClient,DataType
path=sys.argv[1];c=MilvusClient(path)
s=c.create_schema(auto_id=False,enable_dynamic_field=False)
s.add_field('id',DataType.INT64,is_primary=True);s.add_field('v',DataType.FLOAT_VECTOR,dim=2,nullable=True)
s.add_field('g',DataType.INT64)
p=c.prepare_index_params();p.add_index('v',index_type='FLAT',metric_type='L2')
c.create_collection('c',schema=s,index_params=p)
c.insert('c',[{'id':0,'v':[1.,0.],'g':0},{'id':1,'v':None,'g':0}]);c.flush('c');c.close()
'''

RUST_EXTEND = r'''
import json,sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as manager
path=sys.argv[1];c=MilvusClient(path,dedicated=True);c.load_collection('c')
c.insert('c',[{'id':2,'v':None,'g':0},{'id':3,'v':[0.,1.],'g':0}]);c.flush('c')
rows=sorted([r['id'],r['v']] for r in c.query('c',filter='id >= 0',output_fields=['v']))
hits=sorted(h['id'] for h in c.search('c',[[1.,0.]],limit=20)[0])
c.close();manager.release_server(path)
print(json.dumps([rows,hits]))
'''


def test_3x_written_nulls_read_as_zero_vectors(tmp_path, interpreters):
    path = tmp_path / "ref.db"
    run([interpreters[PYTHONS[0]], "-c", REFERENCE_WRITE, str(path)], cwd=tmp_path)
    rows, hits = json.loads(run([sys.executable, "-c", RUST_EXTEND, str(path)], cwd=tmp_path))
    # 3.x 写入时已把空值换成零向量，本实现无法还原；之后本实现写入的空值读回 None。
    assert rows == [[0, [1.0, 0.0]], [1, [0.0, 0.0]], [2, None], [3, [0.0, 1.0]]]
    assert hits == [0, 1, 3]
