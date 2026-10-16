"""ARRAY list 编码、父子 validity、默认值，Arrow 15/25 双向及交替写。"""
import json
import sys
import pytest
from test_cross_implementation import PYTHONS, interpreters, rust_binary, run

FIELDS={'b':True,'i8':7,'i16':7,'i32':7,'i64':7,'f':1.5,'d':1.5,'s':'abc'}
REF_WRITE=r'''
import os,sys
from milvus_lite.db import MilvusLite
from milvus_lite.schema.types import CollectionSchema,FieldSchema,DataType
path,state=sys.argv[1:];db=MilvusLite(path)
fields=[FieldSchema('id',DataType.INT64,is_primary=True),FieldSchema('vector',DataType.FLOAT_VECTOR,dim=2)]
types=[('b',DataType.BOOL,True),('i8',DataType.INT8,7),('i16',DataType.INT16,7),('i32',DataType.INT32,7),('i64',DataType.INT64,7),('f',DataType.FLOAT,1.5),('d',DataType.DOUBLE,1.5),('s',DataType.VARCHAR,'abc')]
fields += [FieldSchema(n,DataType.ARRAY,element_type=t,max_capacity=4,nullable=True) for n,t,v in types]
fields += [FieldSchema('default',DataType.ARRAY,element_type=DataType.INT64,default_value=[])]
c=db.create_collection('c',CollectionSchema(fields))
for part in range(4 if state=='compacted' else 1):
 c.insert([{'id':part*3+i+1,'vector':[1.,0.],**{n:None if i==0 else [] if i==1 else [None,v] for n,t,v in types}} for i in range(3)])
 if state!='wal':c.flush();c._wait_for_bg(timeout=30)
if state=='wal':os._exit(0)
db.close()
'''
INSPECT=r'''
import json,sys,pyarrow as pa,pyarrow.parquet as pq
from pathlib import Path
root=Path(sys.argv[1])/'collections/c';state=sys.argv[2]
if state=='wal':tables=[pa.Table.from_batches([b]) for p in (root/'wal').glob('wal_data_*.arrow') for b in pa.ipc.open_stream(p)]
else:
 files=json.loads((root/'manifest.json').read_text())['partitions']['_default']['data_files']
 if state=='compacted':assert len(files)==1,files
 tables=[pq.read_table(root/'partitions/_default'/p) for p in files]
rows=[]
for t in tables:
 t.validate(full=True)
 for name in ('b','i8','i16','i32','i64','f','d','s'):
  x=t.column(name);assert pa.types.is_list(x.type);assert x.null_count*3==len(x)
  assert sum(c.values.null_count for c in x.chunks)*3==len(x)
 rows.extend([{k:v for k,v in r.items() if k not in ('_seq','_partition','vector')} for r in t.to_pylist()])
print(json.dumps(rows))
'''
REF_READ_CHANGE=r'''
import json,sys
from milvus_lite.db import MilvusLite
path,mode=sys.argv[1:];db=MilvusLite(path);c=db.get_collection('c');c.load()
rows=c.query(output_fields=['id','b','i8','i16','i32','i64','f','d','s','default']);print(json.dumps(rows))
if mode=='change':
 c.upsert([{'id':2,'i8':None}])
 c.delete([3]);c.insert([{'id':99,'vector':[1.,0.]}]);c.flush()
db.close()
'''

@pytest.mark.parametrize('writer',['rust','reference15','reference25'])
@pytest.mark.parametrize('state',['wal','flushed','compacted'])
def test_array_files_masks_defaults_and_alternate_writes(tmp_path,interpreters,rust_binary,writer,state):
    path=tmp_path/'array.db';n=12 if state=='compacted' else 3
    if writer=='rust': run([str(rust_binary),'write-array-'+state,str(path)])
    else: run([interpreters[PYTHONS[1 if writer=='reference15' else 2]],'-c',REF_WRITE,str(path),state])
    expected=[{'id':i,**{k:None if (i-1)%3==0 else [] if (i-1)%3==1 else [None,v] for k,v in FIELDS.items()},'default':[]} for i in range(1,n+1)]
    for env in PYTHONS[1:]: assert json.loads(run([interpreters[env],'-c',INSPECT,str(path),state]))==expected
    assert json.loads(run([str(rust_binary),'inspect-array',str(path)]))==expected
    for env in PYTHONS[1:]: assert json.loads(run([interpreters[env],'-c',REF_READ_CHANGE,str(path),'read']))==expected
    # 参照修改后，Rust 读取；Rust 重开关闭再交回参照。
    run([interpreters[PYTHONS[0]],'-c',REF_READ_CHANGE,str(path),'change'])
    expected=[r for r in expected if r['id']!=3];expected[1]['i8']=None
    expected.append({'id':99,**dict.fromkeys(FIELDS),'default':[]})
    reference=json.loads(run([interpreters[PYTHONS[0]],'-c',REF_READ_CHANGE,str(path),'read']))
    assert sorted(reference,key=lambda r:r['id'])==expected
    assert json.loads(run([str(rust_binary),'inspect-array',str(path)]))==reference
    assert json.loads(run([interpreters[PYTHONS[0]],'-c',REF_READ_CHANGE,str(path),'read']))==reference

PROTOCOL_READ=r'''
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import ServerManager
m=ServerManager();c=MilvusClient(m.start_and_get_uri(sys.argv[1]),dedicated=True);c.load_collection('c')
try:
 assert list(c.get('c',[1,2],output_fields=['i8']))==[{'id':1,'i8':None},{'id':2,'i8':[]}]
 assert list(c.query('c',filter='array_contains(i8,7)',output_fields=['id']))==[{'id':3}]
 for name,reason in [('b',"'NoneType' object cannot be interpreted as an integer"),('i8',"'NoneType' object cannot be interpreted as an integer"),('i16',"'NoneType' object cannot be interpreted as an integer"),('i32',"'NoneType' object cannot be interpreted as an integer"),('i64',"'NoneType' object cannot be interpreted as an integer"),('f','must be real number, not NoneType'),('d','must be real number, not NoneType'),('s','bad argument type for built-in operation')]:
  try: c.get('c',[3],output_fields=[name])
  except Exception as e: assert (e.code,e.message)==(1,reason),(name,e)
  else: raise AssertionError(name+' child null silently accepted')
finally:c.close();m.release_all()
'''

@pytest.mark.parametrize('state',['wal','flushed'])
def test_child_null_gRPC_errors_match_reference_and_parent_null_empty_survive(tmp_path,interpreters,rust_binary,state):
    path=tmp_path/'protocol.db'
    run([str(rust_binary),'write-array-'+state,str(path)])
    for python in (sys.executable,interpreters[PYTHONS[0]]):
        run([python,'-c',PROTOCOL_READ,str(path)])

@pytest.mark.parametrize('arrow',PYTHONS[1:])
def test_array_snapshot_files_survive_rust_overwrite_delete_and_compaction(tmp_path,interpreters,rust_binary,arrow):
    import hashlib
    path=tmp_path/'pinned.db'
    run([interpreters[arrow],'-c',REF_WRITE,str(path),'flushed'])
    run([interpreters[arrow],'-c',"import sys;from milvus_lite.db import MilvusLite;d=MilvusLite(sys.argv[1]);d.create_snapshot('c','saved');d.close()",str(path)])
    collection=path/'collections/c'
    metadata=json.loads(next((collection/'snapshots/metadata').glob('*.json')).read_text())
    files=[collection/'partitions'/partition/f for partition,files in metadata['data_files'].items() for f in files]
    files+=list((collection/'snapshots').rglob('*.json'))
    before={str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in files}
    change=r'''
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import ServerManager
m=ServerManager();c=MilvusClient(m.start_and_get_uri(sys.argv[1]),dedicated=True)
try:
 c.delete('c',ids=[3])
 for i in range(4):
  c.insert('c',[{'id':100+i,'vector':[1.,0.],'default':[]}]);c.flush('c')
finally:c.close();m.release_all()
'''
    run([sys.executable,'-c',change,str(path)])
    manifest=json.loads((collection/'manifest.json').read_text());assert len(manifest['partitions']['_default']['data_files'])<5
    assert {str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in files}==before
    restore=r'''
import sys,json
from milvus_lite.db import MilvusLite
d=MilvusLite(sys.argv[1]);c=d.restore_snapshot('c','saved','restored');c.load();print(json.dumps(c.query(output_fields=['id','i8','default'])));d.close()
'''
    assert json.loads(run([interpreters[arrow],'-c',restore,str(path)]))==[{'id':1,'i8':None,'default':[]},{'id':2,'i8':[],'default':[]},{'id':3,'i8':[None,7],'default':[]}]

@pytest.mark.parametrize('state',['wal','flushed'])
def test_stored_array_inverted_definition_fails_only_when_segment_build_is_needed(tmp_path,interpreters,state):
    import shutil
    root=tmp_path/'source.db';run([interpreters[PYTHONS[0]],'-c',REF_WRITE,str(root),state])
    manifest=root/'collections/c/manifest.json';value=json.loads(manifest.read_text()) if manifest.exists() else {'manifest_format_version':2,'version':0,'current_seq':0,'schema_version':1,'active_wal_number':None,'partitions':{'_default':{'data_files':[],'delta_files':[]}},'index_specs':{}}
    value['index_specs']['i8']={'field_name':'i8','index_type':'INVERTED','metric_type':'NONE','build_params':{},'search_params':{}}
    manifest.write_text(json.dumps(value))
    read=r'''
import sys,json
from pymilvus import MilvusClient
from milvus_lite.server_manager import ServerManager
m=ServerManager();c=MilvusClient(m.start_and_get_uri(sys.argv[1]),dedicated=True)
try:
 try:c.load_collection('c');out=['ok']
 except Exception as e:out=[e.code,e.message]
 print(json.dumps(out))
finally:c.close();m.release_all()
'''
    results=[]
    for i,python in enumerate((sys.executable,interpreters[PYTHONS[0]])):
        copy=tmp_path/f'read-{i}.db';shutil.copytree(root,copy)
        results.append(json.loads(run([python,'-c',read,str(copy)])))
    assert results[0]==results[1]==(['ok'] if state=='wal' else [1,'scalar index INVERTED does not support field type ARRAY'])
