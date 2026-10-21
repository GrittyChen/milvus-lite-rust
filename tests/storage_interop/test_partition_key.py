"""0017 B：隐含桶 WAL、Arrow 15/25、恢复后追加和旧自动 ID 前缀保护。"""
import json
import shutil
import sys

import pytest
from test_cross_implementation import PYTHONS, interpreters, run

WRITE=r'''
import os,sys
from pymilvus import MilvusClient,DataType
from milvus_lite.server_manager import server_manager_instance as m
path,state=sys.argv[1:];c=MilvusClient(path,dedicated=True)
s=c.create_schema(enable_dynamic_field=True);s.add_field('id',DataType.INT64,is_primary=True)
s.add_field('k',DataType.INT64,is_partition_key=True,nullable=True)
s.add_field('vector',DataType.FLOAT_VECTOR,dim=2);s.add_field('a',DataType.ARRAY,element_type=DataType.INT8,max_capacity=8)
s.add_field('j',DataType.JSON,nullable=True);s.add_field('n',DataType.INT64,nullable=True,default_value=0)
p=c.prepare_index_params();p.add_index('vector',index_type='FLAT',metric_type='L2');c.create_collection('c',schema=s,index_params=p)
for i,k in [(1,0),(2,1),(3,None)]:
 c.insert('c',[{'id':i,'k':k,'vector':[float(i),0.],'a':[i],'j':{'i':i},'extra':i}])
 if state in ('flushed','compacted') or state=='mixed' and i==1:c.flush('c')
 # 3.x 在后台线程非原子地写索引文件，flush 后立即 os._exit 可能留下截断的文件；
 # 正常关闭会等后台写完，随后崩溃时仍是已 flush 的段加未 flush 的 WAL（见 0019 关于参照实现崩溃时索引文件的「试点发现」）。
 if state=='mixed' and i==1:c.close();m.release_server(path);c=MilvusClient(path,dedicated=True)
if state=='compacted':
 for i in range(10,18):
  c.insert('c',[{'id':i,'k':0,'vector':[float(i),0.],'a':[i],'j':{'i':i},'extra':i}]);c.flush('c')
if state in ('wal','mixed'):os._exit(0)
c.close();m.release_server(path)
'''
READ_APPEND=r'''
import json,sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as m
c=MilvusClient(sys.argv[1],dedicated=True);c.load_collection('c')
rows=sorted(c.query('c',filter='',output_fields=['id','k','a','j','n','extra']),key=lambda r:r['id'])
assert len(c.list_partitions('c'))==17
c.upsert('c',[{'id':1,'extra':100}],partial_update=True)
c.insert('c',[{'id':100,'k':1,'vector':[1.,0.],'a':[],'j':None,'extra':100}]);c.flush('c')
assert len(c.get('c',ids=[1,100],output_fields=['id']))==2
c.close();m.release_server(sys.argv[1]);print(json.dumps(rows,sort_keys=True))
'''
INSPECT=r'''
import json,sys,pyarrow as pa,pyarrow.parquet as pq
from pathlib import Path
root=Path(sys.argv[1])/'collections/c';state=sys.argv[2]
s=json.loads((root/'schema.json').read_text());assert [f['name'] for f in s['fields'] if f.get('is_partition_key')]==['k']
if (root/'manifest.json').exists():
 m=json.loads((root/'manifest.json').read_text());assert set(m['partitions'])=={'_default',*['_pk_'+str(i) for i in range(16)]}
 if state=='compacted':assert len(m['partitions']['_pk_10']['data_files'])<9,m
 for part,files in m['partitions'].items():
  for file in files['data_files']:
   t=pq.read_table(root/'partitions'/part/file);t.validate(full=True);assert '_partition' not in t.column_names
labels=[]
for file in sorted((root/'wal').glob('wal_data_*.arrow')):
 for b in pa.ipc.open_stream(file):
  b.validate(full=True);labels.extend(b.column(b.schema.get_field_index('_partition')).to_pylist())
assert all(p in {'_pk_10','_pk_11','_pk_4'} for p in labels),labels
print('ok')
'''


@pytest.mark.parametrize('writer',['rust','reference15','reference25'])
@pytest.mark.parametrize('state',['wal','flushed','mixed','compacted'])
def test_key_partition_bidirectional_read_append_and_real_encodings(tmp_path,interpreters,writer,state):
    python=sys.executable if writer=='rust' else interpreters[PYTHONS[1 if writer=='reference15' else 2]]
    path=tmp_path/'source.db';run([python,'-c',WRITE,str(path),state])
    expected=[{'id':i,'k':(0 if i in [1,*range(10,18)] else 1 if i==2 else None),'a':[i],'j':{'i':i},'n':0,'extra':i}
              for i in [1,2,3]+(list(range(10,18)) if state=='compacted' else [])]
    for env in PYTHONS[1:]:assert run([interpreters[env],'-c',INSPECT,str(path),state]).strip()=='ok'
    for i,reader in enumerate([sys.executable,interpreters[PYTHONS[1]],interpreters[PYTHONS[2]]]):
        copy=tmp_path/f'read{i}.db';shutil.copytree(path,copy)
        assert json.loads(run([reader,'-c',READ_APPEND,str(copy)]))==expected
        # 追加后再由另一个实现打开，确认 manifest/seq/桶归属仍可继续使用。
        check=r'''
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as m
c=MilvusClient(sys.argv[1],dedicated=True);c.load_collection('c');assert list(c.get('c',ids=[1],output_fields=['extra']))==[{'id':1,'extra':100}];assert list(c.get('c',ids=[100],output_fields=['k']))==[{'id':100,'k':1}];c.close();m.release_server(sys.argv[1]);print('ok')
'''
        other=interpreters[PYTHONS[2]] if i==0 else sys.executable
        assert run([other,'-c',check,str(copy)]).strip()=='ok'


@pytest.mark.parametrize('arrow',PYTHONS[1:])
def test_3x_unconfirmed_bad_auto_id_prefix_remains_readable_and_generated_collision_is_rejected(tmp_path,interpreters,arrow):
    path=tmp_path/'bad-prefix.db'
    write=r'''
import os,sys
from pymilvus import MilvusClient,DataType
c=MilvusClient(sys.argv[1],dedicated=True)
s=c.create_schema(auto_id=True);s.add_field('id',DataType.INT64,is_primary=True,auto_id=True);s.add_field('k',DataType.INT64,is_partition_key=True);s.add_field('vector',DataType.FLOAT_VECTOR,dim=2);s.add_field('a',DataType.ARRAY,element_type=DataType.INT8,max_capacity=8)
p=c.prepare_index_params();p.add_index('vector',index_type='FLAT',metric_type='L2');c.create_collection('c',schema=s,index_params=p)
try:c.insert('c',[{'k':k,'vector':[0.,0.],'a':a} for k,a in [(0,[1]),(1,[128]),(0,[3])]])
except Exception as e:assert e.code==1 and e.message=='Value 128 too large to fit in C integer type'
else:raise AssertionError('reference must reject later bucket')
os._exit(0)
'''
    run([interpreters[arrow],'-c',write,str(path)])
    read=r'''
import hashlib,json,sys
from pathlib import Path
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as m
path=Path(sys.argv[1]);c=MilvusClient(str(path),dedicated=True);c.load_collection('c')
assert sorted(c.query('c',filter='',output_fields=['id','a']),key=lambda r:r['id'])==[{'id':1,'a':[1]},{'id':3,'a':[3]}]
def files():return {str(p.relative_to(path)):hashlib.sha256(p.read_bytes()).hexdigest() for p in path.rglob('*') if p.is_file() and p.name!='LOCK'}
before=files()
try:c.insert('c',[{'k':1,'vector':[0.,0.],'a':[]}])
except Exception as e:assert e.code==1 and e.message=='storage error: auto-generated primary key 3 would overwrite an existing row'
else:raise AssertionError('must protect existing id 3')
assert files()==before
c.flush('c');c.close();m.release_server(str(path))
c=MilvusClient(str(path),dedicated=True);c.load_collection('c');assert len(c.get('c',ids=[1,3],output_fields=['id']))==2
try:c.insert('c',[{'k':1,'vector':[0.,0.],'a':[]}])
except Exception as e:assert e.code==1 and 'primary key 3 would overwrite' in e.message
else:raise AssertionError('collision persists after reopen')
c.close();m.release_server(str(path));print('ok')
'''
    assert run([sys.executable,'-c',read,str(path)]).strip()=='ok'
