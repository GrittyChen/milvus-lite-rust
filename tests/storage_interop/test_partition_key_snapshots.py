"""0017 B：snapshot 钉住与 key 分区合并/全局删除的交叉。"""
import hashlib
import json
import sys

import pytest
from test_cross_implementation import PYTHONS, interpreters, run
from test_partition_key import WRITE


@pytest.mark.parametrize('arrow',PYTHONS[1:])
def test_key_snapshot_files_survive_actual_compaction_delete_and_index_intent(tmp_path,interpreters,arrow):
    path=tmp_path/'key-pinned.db';run([interpreters[arrow],'-c',WRITE,str(path),'flushed'])
    snapshot=r'''
import sys
from milvus_lite.db import MilvusLite
d=MilvusLite(sys.argv[1]);c=d.get_collection('c');c.create_index('k',{'index_type':'INVERTED'});c.load();d.create_snapshot('c','saved');d.close()
'''
    run([interpreters[arrow],'-c',snapshot,str(path)])
    root=path/'collections/c';saved=json.loads(next((root/'snapshots/metadata').glob('*.json')).read_text())
    pinned=[root/'partitions'/part/name for kind in ['data_files','delta_files','index_files'] for part,names in saved[kind].items() for name in names]
    pinned+=list((root/'snapshots').rglob('*.json'))
    assert pinned
    before={p:hashlib.sha256(p.read_bytes()).hexdigest() for p in pinned}
    mutate=r'''
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as m
c=MilvusClient(sys.argv[1],dedicated=True);c.load_collection('c');c.delete('c',ids=[2],partition_name='_pk_10');c.flush('c')
for i in range(10,19):
 c.insert('c',[{'id':i,'k':0,'vector':[float(i),0.],'a':[i],'j':{'i':i},'extra':i}]);c.flush('c')
c.release_collection('c');c.drop_index('c','k_idx');c.close();m.release_server(sys.argv[1]);print('ok')
'''
    assert run([sys.executable,'-c',mutate,str(path)]).strip()=='ok'
    assert all(hashlib.sha256(p.read_bytes()).hexdigest()==digest for p,digest in before.items())
    manifest=json.loads((root/'manifest.json').read_text())
    assert len(manifest['partitions']['_pk_10']['data_files'])<10
    assert 'k' not in manifest['index_specs']
    read=r'''
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as m
c=MilvusClient(sys.argv[1],dedicated=True);c.load_collection('c')
assert list(c.get('c',ids=[2],output_fields=['id']))==[]
assert sorted(r['id'] for r in c.query('c',filter='',output_fields=['id']))==[1,3,*range(10,19)]
c.close();m.release_server(sys.argv[1]);print('ok')
'''
    for python in [sys.executable,interpreters[arrow]]:
        assert run([python,'-c',read,str(path)]).strip()=='ok'
    assert all(hashlib.sha256(p.read_bytes()).hexdigest()==digest for p,digest in before.items())
