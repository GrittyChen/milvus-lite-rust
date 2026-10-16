"""0015：Arrow 15/25 与 Rust 的空值/默认值目录双向与交替写入。"""

import json
import sys
from pathlib import Path

import pytest
from test_cross_implementation import PYTHONS, interpreters, run


REFERENCE_WRITE = r'''
import os,sys
from milvus_lite.db import MilvusLite
from milvus_lite.schema.types import CollectionSchema,FieldSchema,DataType
path,state=sys.argv[1:]
db=MilvusLite(path)
s=CollectionSchema([FieldSchema('id',DataType.INT64,is_primary=True),FieldSchema('vector',DataType.FLOAT_VECTOR,dim=2),
 FieldSchema('x',DataType.INT64,nullable=True),FieldSchema('flag',DataType.BOOL,default_value=False),
 FieldSchema('title',DataType.VARCHAR,max_length=8,default_value=''),FieldSchema('j',DataType.JSON,nullable=True)])
c=db.create_collection('c',s);c.create_index('vector',{'index_type':'FLAT','metric_type':'L2'})
for part in range(4 if state=='compacted' else 1):
 first=part*3+1
 c.insert([{'id':first,'vector':[float(first),0.],'x':None,'j':None,'flag':None,'title':None},
           {'id':first+1,'vector':[float(first+1),0.],'x':7,'j':{'k':part}},
           {'id':first+2,'vector':[float(first+2),0.],'x':0,'j':None}])
 if state!='wal':c.flush();c._wait_for_bg(timeout=30)
if state=='wal':os._exit(0)
db.close()
'''

RUST_WRITE = r'''
import os,sys
from pymilvus import MilvusClient,DataType
from milvus_lite.server_manager import server_manager_instance as manager
path,state=sys.argv[1:];c=MilvusClient(path,dedicated=True)
s=c.create_schema(auto_id=False,enable_dynamic_field=False)
s.add_field('id',DataType.INT64,is_primary=True);s.add_field('vector',DataType.FLOAT_VECTOR,dim=2)
s.add_field('x',DataType.INT64,nullable=True);s.add_field('flag',DataType.BOOL,default_value=False)
s.add_field('title',DataType.VARCHAR,max_length=8,default_value='');s.add_field('j',DataType.JSON,nullable=True)
p=c.prepare_index_params();p.add_index('vector',index_type='FLAT',metric_type='L2');c.create_collection('c',schema=s,index_params=p)
for part in range(4 if state=='compacted' else 1):
 first=part*3+1
 c.insert('c',[{'id':first,'vector':[float(first),0.],'x':None,'j':None,'flag':None,'title':None},
               {'id':first+1,'vector':[float(first+1),0.],'x':7,'j':{'k':part}},
               {'id':first+2,'vector':[float(first+2),0.],'x':0,'j':None}])
 if state!='wal':c.flush('c')
if state=='wal':os._exit(0)
c.close();manager.release_server(path)
'''

INSPECT = r'''
import sys,json
from pathlib import Path
import pyarrow as pa
import pyarrow.parquet as pq
root=Path(sys.argv[1])/'collections/c';state=sys.argv[2]
if state=='wal':
 batches=[batch for p in sorted((root/'wal').glob('wal_data_*.arrow')) for batch in pa.ipc.open_stream(p)]
 tables=[pa.Table.from_batches([batch]) for batch in batches]
else:
 m=json.loads((root/'manifest.json').read_text())
 files=m['partitions']['_default']['data_files']
 if state=='compacted':assert len(files)==1,files
 tables=[pq.read_table(root/'partitions/_default'/name) for name in files]
assert tables
for t in tables:
 t.validate(full=True)
 assert t.schema.field('x').nullable and t.schema.field('j').nullable
 assert t.column('x').null_count*3==t.num_rows
 assert t.column('j').null_count*3==t.num_rows*2
 assert t.column('flag').to_pylist()==[False]*t.num_rows
 assert t.column('title').to_pylist()==['']*t.num_rows
print(sum(t.num_rows for t in tables))
'''

READ_CHANGE = r'''
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as manager
path,n,change=sys.argv[1:];n=int(n);c=MilvusClient(path,dedicated=True)
try:
 c.load_collection('c')
 rows=sorted(c.query('c',filter='id >= 0',output_fields=['id','x','flag','title','j'],limit=n+2),key=lambda r:r['id'])
 expected=[]
 for pk in range(1,n+1):
  if change=='verify' and pk==3:continue
  pos=(pk-1)%3;part=(pk-1)//3
  expected.append({'id':pk,'x':None if pos==0 else 7 if pos==1 else 0,'flag':False,'title':'','j':{'k':part} if pos==1 else None})
 if change=='verify':
  expected[1]['x']=None;expected[1]['j']=None
  expected.append({'id':n+1,'x':None,'flag':False,'title':'','j':None})
 assert rows==expected,(rows,expected)
 null_ids=[r['id'] for r in expected if r['x'] is None]
 assert sorted(r['id'] for r in c.query('c',filter='x is null',output_fields=['id'],limit=n+2))==null_ids
 result=c.search('c',[[0.,0.]],filter='x is null',limit=n+2,output_fields=['x'],search_params={'metric_type':'L2'})[0]
 assert [r['id'] for r in result]==null_ids,result
 assert all(r['entity']['x'] is None for r in result),result
 if change=='change':
  c.upsert('c',[{'id':2,'x':None,'j':None,'flag':None,'title':None}],partial_update=True)
  c.delete('c',ids=[3]);c.insert('c',[{'id':n+1,'vector':[float(n+1),0.],'x':None,'j':None}]);c.flush('c')
finally:c.close();manager.release_server(path)
'''


@pytest.mark.parametrize("writer", ["rust", "reference15", "reference25"])
@pytest.mark.parametrize("state", ["wal", "flushed", "compacted"])
def test_nullable_files_cross_open_and_alternate_writes(tmp_path, interpreters, writer, state):
    path = tmp_path / "nullable.db"
    python = sys.executable if writer == "rust" else interpreters[PYTHONS[1 if writer == "reference15" else 2]]
    run([python, "-c", RUST_WRITE if writer == "rust" else REFERENCE_WRITE, str(path), state], cwd=tmp_path)
    n = 12 if state == "compacted" else 3
    for arrow in PYTHONS[1:]:
        assert int(run([interpreters[arrow], "-c", INSPECT, str(path), state], cwd=tmp_path)) == n
    reader = interpreters[PYTHONS[0]] if writer == "rust" else sys.executable
    verifier = sys.executable if writer == "rust" else interpreters[PYTHONS[0]]
    run([reader, "-c", READ_CHANGE, str(path), str(n), "change"], cwd=tmp_path)
    run([verifier, "-c", READ_CHANGE, str(path), str(n), "verify"], cwd=tmp_path)
    schema = json.loads((path / "collections/c/schema.json").read_text())
    assert schema["schema_format_version"] == 2
    fields = {field["name"]: field for field in schema["fields"]}
    assert fields["x"]["nullable"] and fields["j"]["nullable"]
    assert fields["flag"]["default_value"] is False and fields["title"]["default_value"] == ""
