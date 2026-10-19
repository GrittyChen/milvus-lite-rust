"""0017 已登记的目录安全差异：原始协议拒绝危险名字且没有持久化副作用。"""
import hashlib
from pathlib import Path

import pytest
from pymilvus.grpc_gen import milvus_pb2 as rpc

from test_grpc_translation import basic, env, status_of


def tree(root):
    return {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in root.rglob('*') if p.is_file() and p.name != 'LOCK'}


@pytest.mark.parametrize('name', ['', '.', '..', '_all', '../escape', 'a/b', 'a\\b', 'a\0b', 'absolute'])
def test_raw_create_partition_rejects_unsafe_name_before_any_persistent_change(env, tmp_path, name):
    _, stub, _ = env;basic(stub)
    if name == 'absolute':name=str(tmp_path/'outside')
    before=tree(tmp_path/'t.db')
    response=stub.CreatePartition(rpc.CreatePartitionRequest(collection_name='c',partition_name=name))
    assert status_of(response)==(6,6,'invalid partition name: '+repr(name))
    assert tree(tmp_path/'t.db')==before
    assert not stub.HasPartition(rpc.HasPartitionRequest(collection_name='c',partition_name=name)).value
    assert not (tmp_path/'outside').exists()


def test_raw_create_partition_accepts_ordinary_unicode_and_colon_names(env):
    _,stub,_=env;basic(stub)
    names=['普通分区','p:1','é']
    for name in names:
        assert status_of(stub.CreatePartition(rpc.CreatePartitionRequest(collection_name='c',partition_name=name)))==(0,0,'')
    response=stub.ShowPartitions(rpc.ShowPartitionsRequest(collection_name='c'))
    assert list(response.partition_names)==sorted(['_default',*names])
    assert not response.partitionIDs and not response.created_timestamps and not response.created_utc_timestamps


@pytest.mark.parametrize('first,second',[('p1','P1'),('é','e\u0301')])
def test_partition_directory_aliases_follow_actual_filesystem_identity(env,tmp_path,first,second):
    _,stub,_=env;basic(stub)
    assert status_of(stub.CreatePartition(rpc.CreatePartitionRequest(collection_name='c',partition_name=first)))==(0,0,'')
    parts=tmp_path/'t.db/collections/c/partitions'
    alias=(parts/second).exists()
    before=tree(tmp_path/'t.db')
    response=stub.CreatePartition(rpc.CreatePartitionRequest(collection_name='c',partition_name=second))
    if alias:
        assert status_of(response)[:2]==(1,0) and 'same directory' in response.reason
        assert tree(tmp_path/'t.db')==before
        assert not stub.HasPartition(rpc.HasPartitionRequest(collection_name='c',partition_name=second)).value
    else:
        assert status_of(response)==(0,0,'')
        assert (parts/first).stat().st_ino!=(parts/second).stat().st_ino


@pytest.mark.parametrize('registered,requested', [('_default', '_DEFAULT'), ('p1', 'P1'), ('é', 'e\u0301')])
def test_partition_alias_of_registered_but_absent_directory_is_rejected_before_commit(env, tmp_path, registered, requested):
    client, stub, _ = env
    basic(stub)
    if registered != '_default':
        assert status_of(stub.CreatePartition(rpc.CreatePartitionRequest(collection_name='c', partition_name=registered))) == (0, 0, '')
    parts = tmp_path / 't.db/collections/c/partitions'
    if (parts / registered).exists():
        (parts / registered).rmdir()
    probe = tmp_path / 'identity-probe'
    probe.mkdir()
    (probe / registered).mkdir()
    alias = (probe / requested).exists()
    before = tree(tmp_path / 't.db')
    response = stub.CreatePartition(rpc.CreatePartitionRequest(collection_name='c', partition_name=requested))
    if alias:
        assert status_of(response)[:2] == (1, 0) and 'same directory' in response.reason
        assert tree(tmp_path / 't.db') == before
        assert not (parts / requested).exists()
        assert not stub.HasPartition(rpc.HasPartitionRequest(collection_name='c', partition_name=requested)).value
    else:
        assert status_of(response) == (0, 0, '')
    client.insert('c', [{'id': 9, 'v': [9., 0.], 't': 'nine'}], partition_name=registered)
    if not alias:
        client.insert('c', [{'id': 10, 'v': [10., 0.], 't': 'ten'}], partition_name=requested)
    client.flush('c')
    assert client.query('c', 'id >= 0', output_fields=['id'], partition_names=[registered]) == [{'id': 9}]
    if not alias:
        assert (parts / registered).stat().st_ino != (parts / requested).stat().st_ino
        assert client.query('c', 'id >= 0', output_fields=['id'], partition_names=[requested]) == [{'id': 10}]
