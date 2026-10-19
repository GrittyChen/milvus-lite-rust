"""分区可见性组件真值：记录物理候选和两类删除，期望由真实引擎/RPC 单独产生。"""

import argparse
import hashlib
import importlib.metadata
import json
from pathlib import Path
import random
import subprocess

from run import REF, outside


SELECTIONS = [None, ['_default'], ['p1'], ['p2'], ['p1', 'p2'],
              ['_default', 'p1'], ['_default', 'p2'], ['p2', '_default', 'p1'],
              ['missing'], ['missing', 'p1'], ['p1', 'p1']]


def projected(rows):
    return [{'pk': r['id'], 'tag': r['tag']} for r in rows]


def state(collection):
    sources = []
    for (partition, relative), segment in collection._segment_cache.items():
        sources.append({'name': partition+'/'+relative, 'partition': partition,
                        'rows': [{'pk': r['id'], 'seq': r['_seq'], 'tag': r['tag']}
                                 for r in segment.table.to_pylist()]})
    mt = collection._memtable
    active = []
    for pk, (batch, row, seq) in mt.pk_index_snapshot():
        record = mt._insert_batches[batch].slice(row, 1).to_pylist()[0]
        active.append({'pk': pk, 'seq': seq, 'tag': record['tag'], 'partition': record['_partition']})
    sources.append({'name': 'active', 'partition': None, 'rows': active})
    return {'sources': sources,
            'active_deletes': [{'pk': pk, 'seq': entry[0]} for pk, entry in mt.delete_index_snapshot().items()],
            'committed_deletes': [{'pk': pk, 'seq': seq} for pk, seq in collection._delta_index._map.items()]}


def direct(root, kind, scenario):
    from milvus_lite.db import MilvusLite
    from milvus_lite.schema.types import CollectionSchema, DataType, FieldSchema
    path = root/(kind+'-'+scenario+'.db')
    db = MilvusLite(str(path))
    fields = [FieldSchema('id', DataType.INT64 if kind == 'int64' else DataType.VARCHAR,
                         is_primary=True, **({} if kind == 'int64' else {'max_length': 64})),
              FieldSchema('vector', DataType.FLOAT_VECTOR, dim=2), FieldSchema('tag', DataType.VARCHAR, max_length=64)]
    c = db.create_collection('c', CollectionSchema(fields, enable_dynamic_field=False))
    c.create_partition('p1'); c.create_partition('p2')
    def pk(value):
        return value if kind == 'int64' else '键-'+str(value)
    steps = []
    def capture(label):
        c._wait_for_bg(timeout=30)
        # adapter 将空 repeated 字段转换为 None；直接引擎 [] 的混合源差异另存诊断。
        steps.append({'label': label, 'state': state(c),
                      'direct_empty_diagnostic': projected(c.query(partition_names=[], output_fields=['id', 'tag'])), 'queries': [
            {'partitions': parts, 'expected': projected(c.query(partition_names=parts, output_fields=['id', 'tag']))}
            for parts in SELECTIONS]})
    def insert(partition, value, tag):
        c.insert([{'id': pk(value), 'tag': tag, 'vector': [0., 0.]}], partition_name=partition)
    def flush():
        c.flush(); c._wait_for_bg(timeout=30)
    insert('p1', 1, 'old')
    if scenario != 'active-overwrite':
        flush()
    capture('old')
    if scenario.startswith('delete-'):
        c.delete([pk(1)], partition_name='p1')
        capture('deleted-active')
        if scenario == 'delete-committed':
            flush(); capture('deleted-committed')
    if scenario == 'active-fallback':
        insert('p1', 1, 'intermediate'); capture('same-partition-active')
    insert('p2', 1, 'new'); capture('reinserted' if scenario.startswith('delete-') else 'overwritten')
    if scenario == 'random':
        rng = random.Random(17018)
        for i in range(48):
            value = rng.randrange(6); part = rng.choice(['_default', 'p1', 'p2'])
            if i % 7 == 0:
                flush()
            elif i % 3 == 0:
                c.delete([pk(value)], partition_name=part)
            else:
                insert(part, value, 'r'+str(i))
            capture('random-'+str(i))
    flush(); capture('flushed')
    if scenario == 'drop-newest':
        c.drop_partition('p2'); capture('dropped-newest')
    db.close(); db = MilvusLite(str(path)); c = db.get_collection('c'); capture('reopened')
    if scenario.startswith('delete-'):
        for i in range(3):
            insert('p1', 10+i, 'extra'+str(i)); flush()
        assert len(c._manifest.get_data_files('p1')) == 1
        capture('compacted')
        db.close(); db = MilvusLite(str(path)); c = db.get_collection('c'); capture('compacted-reopened')
    db.close()
    return steps


def raw(root, committed):
    import grpc
    from pymilvus import DataType, MilvusClient
    from pymilvus.grpc_gen import common_pb2 as common, milvus_pb2 as rpc, schema_pb2 as pb
    from pymilvus.grpc_gen import milvus_pb2_grpc
    from milvus_lite.server_manager import ServerManager
    manager = ServerManager()
    uri = manager.start_and_get_uri(str(root/('raw-'+str(committed)+'.db')))
    assert uri
    client = MilvusClient(uri, dedicated=True)
    schema = client.create_schema(auto_id=False, enable_dynamic_field=False)
    schema.add_field('id', DataType.INT64, is_primary=True)
    schema.add_field('vector', DataType.FLOAT_VECTOR, dim=2)
    schema.add_field('tag', DataType.VARCHAR, max_length=64)
    client.create_collection('c', schema=schema)
    channel = grpc.insecure_channel(uri.removeprefix('http://'))
    stub = milvus_pb2_grpc.MilvusServiceStub(channel)
    def call(method, request):
        response = getattr(stub, method)(request, timeout=20)
        status = response if isinstance(response, common.Status) else response.status
        assert status.code == 0, (method, status)
        return response
    for partition in ['p1', 'p2']:
        call('CreatePartition', rpc.CreatePartitionRequest(collection_name='c', partition_name=partition))
    def insert(partition, tag):
        call('Insert', rpc.InsertRequest(collection_name='c', partition_name=partition, num_rows=1, fields_data=[
            pb.FieldData(field_name='id', type=5, scalars=pb.ScalarField(long_data=pb.LongArray(data=[1]))),
            pb.FieldData(field_name='tag', type=21, scalars=pb.ScalarField(string_data=pb.StringArray(data=[tag]))),
            pb.FieldData(field_name='vector', type=101, vectors=pb.VectorField(dim=2, float_vector=pb.FloatArray(data=[0., 0.])))]))
    def flush():
        call('Flush', rpc.FlushRequest(collection_names=['c']))
    def capture():
        result = []
        for parts in [[], ['p1'], ['p2']]:
            r = call('Query', rpc.QueryRequest(collection_name='c', partition_names=parts, expr='id >= 0', output_fields=['id', 'tag']))
            columns = {f.field_name: list(f.scalars.long_data.data if f.field_name == 'id' else f.scalars.string_data.data) for f in r.fields_data}
            result.append({'partitions': parts, 'rows': [{'pk': pk, 'tag': tag} for pk, tag in zip(columns['id'], columns['tag'], strict=True)]})
        return result
    try:
        insert('p1', 'old'); flush()
        call('Delete', rpc.DeleteRequest(collection_name='c', partition_name='p1', expr='id in [1]'))
        result = {'deleted': capture()}
        if committed:
            flush()
        insert('p2', 'new'); result['reinserted'] = capture()
        flush(); result['flushed'] = capture()
        expected = [] if committed else [{'pk': 1, 'tag': 'old'}]
        assert result['reinserted'][1]['rows'] == expected
        assert result['flushed'][1]['rows'] == expected
    finally:
        channel.close(); client.close(); manager.release_all()
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--reference', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--verify', type=Path)
    args = parser.parse_args()
    reference = args.reference.resolve(); root = outside(args.work_dir)
    assert not root.is_relative_to(reference)
    assert subprocess.check_output(['git', '-C', str(reference), 'rev-parse', 'HEAD'], text=True).strip() == REF
    import milvus_lite, pyarrow
    assert Path(milvus_lite.__file__).resolve().is_relative_to(reference)
    assert pyarrow.__version__ in {'15.0.0', '25.0.1'}
    sdk = importlib.metadata.version('pymilvus')
    assert sdk in {'2.6.17', '3.0.2'}
    root.mkdir(parents=True, exist_ok=False)
    cases = {kind+'/'+name: direct(root, kind, name) for kind in ['int64', 'varchar']
             for name in ['delete-active', 'delete-committed', 'active-overwrite', 'segment-overwrite', 'active-fallback', 'drop-newest', 'random']}
    cases['raw/delete-active'] = raw(root, False)
    cases['raw/delete-committed'] = raw(root, True)
    result = {'reference': REF, 'cases': cases}
    if args.verify:
        assert json.loads(args.verify.read_text()) == result, 'visibility truth differs'
    (root/'fixture.json').write_text(json.dumps(result, ensure_ascii=False, indent=2)+'\n')
    metadata = {'script_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), 'arrow': pyarrow.__version__,
                'pymilvus': sdk, 'cases': len(cases), 'steps': sum(len(v) for k,v in cases.items() if not k.startswith('raw/'))}
    (root/'metadata.json').write_text(json.dumps(metadata, indent=2)+'\n')
    print(json.dumps(metadata))


if __name__ == '__main__':
    main()
