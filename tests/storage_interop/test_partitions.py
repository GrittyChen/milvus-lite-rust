"""0017 A：命名分区 WAL / Parquet、删除转存与 snapshot 钉住双向互通。"""
import hashlib
import json
import shutil
import sys

import pytest
from test_cross_implementation import PYTHONS, interpreters, run

WRITE = r'''
import os,sys
from pymilvus import MilvusClient,DataType
from milvus_lite.server_manager import server_manager_instance as m
path,state=sys.argv[1:];c=MilvusClient(path,dedicated=True)
s=c.create_schema(enable_dynamic_field=True);s.add_field('id',DataType.INT64,is_primary=True)
s.add_field('vector',DataType.FLOAT_VECTOR,dim=2);s.add_field('n',DataType.INT64,nullable=True)
s.add_field('a',DataType.ARRAY,element_type=DataType.INT64,max_capacity=8)
p=c.prepare_index_params();p.add_index('vector',index_type='FLAT',metric_type='L2');c.create_collection('c',schema=s,index_params=p)
for part in ['z','a']:c.create_partition('c',part)
for part,pk in [('z',1),('a',2)]:
 c.insert('c',[{'id':pk,'vector':[float(pk),0.],'n':None,'a':[pk],'extra':pk}],partition_name=part)
 if state!='wal':c.flush('c')
if state=='compacted':
 for pk in range(10,18):
  c.insert('c',[{'id':pk,'vector':[float(pk),0.],'n':None,'a':[pk],'extra':pk}],partition_name='z' if pk%2==0 else 'a');c.flush('c')
if state=='wal':os._exit(0)
c.close();m.release_server(path)
'''
READ = r'''
import json,sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as m
c=MilvusClient(sys.argv[1],dedicated=True);c.load_collection('c');out={}
for part in ['z','a']:
 out[part]=sorted(c.query('c',filter='',partition_names=[part],output_fields=['id','n','a','extra']),key=lambda r:r['id'])
print(json.dumps(out,sort_keys=True));c.close();m.release_server(sys.argv[1])
'''
INSPECT = r'''
import json,sys,pyarrow as pa,pyarrow.parquet as pq
from pathlib import Path
root=Path(sys.argv[1])/'collections/c';state=sys.argv[2];m=json.loads((root/'manifest.json').read_text())
assert set(m['partitions'])=={'_default','z','a'}
if state=='wal':
 labels=[]
 for path in (root/'wal').glob('wal_data_*.arrow'):
  for batch in pa.ipc.open_stream(path):
   batch.validate(full=True);labels.extend(batch.column(batch.schema.get_field_index('_partition')).to_pylist())
 assert labels==['z','a'],labels
else:
 for part in ['z','a']:
  files=m['partitions'][part]['data_files'];assert files
  if state=='compacted':assert len(files)<5,files
  for name in files:
   t=pq.read_table(root/'partitions'/part/name);t.validate(full=True);assert '_partition' not in t.column_names
print('ok')
'''


@pytest.mark.parametrize('writer', ['rust', 'reference15', 'reference25'])
@pytest.mark.parametrize('state', ['wal', 'flushed', 'compacted'])
def test_named_partition_encoding_and_bidirectional_reads(tmp_path, interpreters, writer, state):
    python = sys.executable if writer == 'rust' else interpreters[PYTHONS[1 if writer == 'reference15' else 2]]
    path = tmp_path/'original.db'; run([python, '-c', WRITE, str(path), state])
    for env in PYTHONS[1:]: assert run([interpreters[env], '-c', INSPECT, str(path), state]).strip() == 'ok'
    expected = {part: [{'id': pk, 'n': None, 'a': [pk], 'extra': pk} for pk in ids]
                for part, ids in [('z', [1] + ([10,12,14,16] if state == 'compacted' else [])),
                                  ('a', [2] + ([11,13,15,17] if state == 'compacted' else []))]}
    for i, reader in enumerate([sys.executable, interpreters[PYTHONS[1]], interpreters[PYTHONS[2]]]):
        copy = tmp_path/f'reader{i}.db'; shutil.copytree(path, copy)
        assert json.loads(run([reader, '-c', READ, str(copy)])) == expected


@pytest.mark.parametrize('arrow', PYTHONS[1:])
def test_drop_transfers_global_delta_and_preserves_snapshot_files(tmp_path, interpreters, arrow):
    path=tmp_path/'pinned.db';run([interpreters[arrow], '-c', WRITE, str(path), 'flushed'])
    script=r'''
import sys
from milvus_lite.db import MilvusLite
d=MilvusLite(sys.argv[1]);c=d.get_collection('c');c.delete([2],partition_name='z');c.flush();d.create_snapshot('c','saved');d.close()
'''
    run([interpreters[arrow], '-c', script, str(path)])
    root=path/'collections/c'
    saved=json.loads(next((root/'snapshots/metadata').glob('*.json')).read_text())
    files=[root/'partitions'/part/name for kind in ['data_files','delta_files','index_files'] for part,names in saved[kind].items() for name in names]
    files+=list((root/'snapshots').rglob('*.json'))
    before={p:hashlib.sha256(p.read_bytes()).hexdigest() for p in files}
    drop=r'''
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as m
c=MilvusClient(sys.argv[1],dedicated=True);c.drop_partition('c','z');c.close();m.release_server(sys.argv[1])
'''
    run([sys.executable,'-c',drop,str(path)])
    assert all(hashlib.sha256(p.read_bytes()).hexdigest()==digest for p,digest in before.items())
    manifest=json.loads((root/'manifest.json').read_text())
    assert 'z' not in manifest['partitions'] and manifest['partitions']['_default']['delta_files']
    read=r'''
import json,sys
from milvus_lite.db import MilvusLite
d=MilvusLite(sys.argv[1]);c=d.get_collection('c');c.load();assert c.query(output_fields=['id'])==[];d.close();print('ok')
'''
    for reader in PYTHONS[1:]: assert run([interpreters[reader],'-c',read,str(path)]).strip()=='ok'
