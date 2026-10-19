"""0017 A：显式分区选择先于版本判定；对照参照 43d1257。"""
import json

import grpc
import pytest
from pymilvus import DataType, MilvusClient
from pymilvus.grpc_gen import milvus_pb2 as rpc, milvus_pb2_grpc

from test_reopen import pair
from test_scenarios import index


def schema(varchar=False):
    s = MilvusClient.create_schema(enable_dynamic_field=True)
    s.add_field('id', DataType.VARCHAR if varchar else DataType.INT64, is_primary=True,
                **({'max_length': 64} if varchar else {}))
    s.add_field('vector', DataType.FLOAT_VECTOR, dim=2)
    s.add_field('n', DataType.INT64, nullable=True)
    s.add_field('a', DataType.ARRAY, element_type=DataType.INT64, max_capacity=16)
    return s


def query_pages(c, parts):
    it = c.query_iterator('c', batch_size=1, partition_names=parts, output_fields=['id', 'n', 'a'])
    try:
        pages = []
        for _ in range(10):
            page = list(it.next()); pages.append(page)
            if not page: return pages
        raise AssertionError('iterator did not finish')
    finally: it.close()


@pytest.mark.parametrize('state', ['active', 'segment', 'mixed', 'reopen', 'compacted'])
@pytest.mark.parametrize('varchar', [False, True])
def test_partition_selection_read_write_and_iterators(pair, state, varchar):
    d = pair.open()
    d.step('create', lambda c: c.create_collection('c', schema=schema(varchar), index_params=index('L2')))
    for part in ['z', 'a']:
        d.step('partition', lambda c: c.create_partition('c', part))
    pk = lambda n: f'键{n}' if varchar else n
    def rows(start, end):
        return [{'id': pk(i), 'vector': [1., 0.], 'n': None if i % 2 else i, 'a': [i], 'd': i} for i in range(start, end)]
    d.step('z', lambda c: c.insert('c', rows(0, 3), partition_name='z'))
    if state != 'active': d.step('flush', lambda c: c.flush('c'))
    d.step('a', lambda c: c.insert('c', rows(1, 4), partition_name='a'))
    if state in ['segment', 'reopen', 'compacted']: d.step('flush', lambda c: c.flush('c'))
    if state == 'compacted':
        for i in range(4):
            d.step('repeat', lambda c: c.insert('c', rows(10+i, 11+i), partition_name='z'))
            d.step('repeat-flush', lambda c: c.flush('c'))
    if state in ['reopen', 'compacted']:
        pair.close(); d = pair.open(); d.step('load', lambda c: c.load_collection('c'))
    for parts in [[], ['z'], ['a'], ['a', 'z'], ['z', 'z', 'missing'], ['missing']]:
        for expr in ['', 'n is null', 'not (n == 2)', 'array_contains(a, 1)', 'd >= 2',
                     'id in ' + json.dumps([pk(2), pk(1), pk(2), pk(99)], ensure_ascii=False)]:
            d.step('query', lambda c: c.query('c', filter=expr, partition_names=parts, output_fields=['id', 'n', 'a']))
        d.step('count', lambda c: c.query('c', filter='n is null', partition_names=parts, output_fields=['count(*)']))
        d.step('get', lambda c: c.get('c', ids=[pk(2), pk(1), pk(2), pk(99)], partition_names=parts, output_fields=['id', 'n', 'a']))
        d.step('iterator', lambda c: query_pages(c, parts))
        for params in [{}, {'radius': 1.0}, {'radius': 1.0, 'range_filter': 0.0}]:
            d.step('search', lambda c: c.search('c', [[1., 0.], [1., 0.]], partition_names=parts,
                                              search_params={'metric_type': 'L2', 'params': params}, limit=10,
                                              output_fields=['n', 'a']))
    d.step('upsert', lambda c: c.upsert('c', [{'id': pk(1), 'n': 42}], partition_name='z', partial_update=True))
    d.step('delete', lambda c: c.delete('c', ids=[pk(2)], partition_name='a'))
    d.step('query-after', lambda c: c.query('c', filter='', partition_names=['z'], output_fields=['id', 'n', 'a']))


@pytest.mark.parametrize('committed', [False, True])
def test_partition_delete_reinsert_branch(pair, committed):
    d = pair.open()
    d.step('create', lambda c: c.create_collection('c', dimension=2, metric_type='L2'))
    for part in ['p1', 'p2']: d.step('partition', lambda c: c.create_partition('c', part))
    d.step('old', lambda c: c.insert('c', [{'id': 1, 'vector': [1., 0.]}], partition_name='p1'))
    d.step('flush-old', lambda c: c.flush('c'))
    d.step('delete', lambda c: c.delete('c', ids=[1]))
    if committed: d.step('flush-delete', lambda c: c.flush('c'))
    d.step('new', lambda c: c.insert('c', [{'id': 1, 'vector': [2., 0.]}], partition_name='p2'))
    result = d.step('local', lambda c: c.query('c', filter='', partition_names=['p1'], output_fields=['id']))
    assert result == ('ok', [] if committed else [{'id': 1}])
    pair.close(); d = pair.open(); d.step('load', lambda c: c.load_collection('c'))
    d.step('reopened-local', lambda c: c.query('c', filter='', partition_names=['p1'], output_fields=['id']))


def test_partition_rpc_status_fields_and_load_order(pair):
    d = pair.open(); d.step('create', lambda c: c.create_collection('c', dimension=2))
    outputs = []
    for uri in pair.uris:
        with grpc.insecure_channel(uri.removeprefix('http://')) as channel:
            stub = milvus_pb2_grpc.MilvusServiceStub(channel)
            results = []
            for name in ['z', 'a', 'z', '_default']:
                results.append(stub.CreatePartition(rpc.CreatePartitionRequest(collection_name='c', partition_name=name)).SerializeToString())
            for collection in ['c', 'absent']:
                for name in ['a', 'unknown', '../unsafe']:
                    results.append(stub.HasPartition(rpc.HasPartitionRequest(collection_name=collection, partition_name=name)).SerializeToString())
                    results.append(stub.GetPartitionStatistics(rpc.GetPartitionStatisticsRequest(collection_name=collection, partition_name=name)).SerializeToString())
            for released in [False, True]:
                if released: stub.ReleaseCollection(rpc.ReleaseCollectionRequest(collection_name='c'))
                for names in [[], ['z', 'unknown', 'z'], ['unknown']]:
                    results.append(stub.ShowPartitions(rpc.ShowPartitionsRequest(collection_name='c', partition_names=names)).SerializeToString())
                for expr in ['', 'id >']:
                    results.append(stub.Query(rpc.QueryRequest(collection_name='c', partition_names=['unknown'], expr=expr, output_fields=['count(*)'])).SerializeToString())
            for name in ['unknown', '_default', 'z']:
                results.append(stub.DropPartition(rpc.DropPartitionRequest(collection_name='c', partition_name=name)).SerializeToString())
            outputs.append(results)
    assert outputs[0] == outputs[1]

@pytest.mark.parametrize('mutation', ['Insert', 'Upsert'])
def test_partition_mutation_error_priority_and_failed_requests_write_nothing(pair, mutation):
    from pymilvus.grpc_gen import schema_pb2 as pb
    d=pair.open();d.step('create',lambda c:c.create_collection('c',schema=schema(),index_params=index('L2')))
    d.step('partition',lambda c:c.create_partition('c','p'))
    outcomes=[]
    for uri in pair.uris:
        with grpc.insecure_channel(uri.removeprefix('http://')) as channel:
            stub=milvus_pb2_grpc.MilvusServiceStub(channel);responses=[]
            for part in ['missing','p']:
                for mode in ['missing-pk','vector-shape','bad-array','valid']:
                    fields=[pb.FieldData(field_name='vector',type=101,vectors=pb.VectorField(dim=2,float_vector=pb.FloatArray(data=[1.] if mode=='vector-shape' else [1.,0.])))]
                    if mode!='missing-pk':fields.append(pb.FieldData(field_name='id',type=5,scalars=pb.ScalarField(long_data=pb.LongArray(data=[1]))))
                    element=pb.ScalarField(string_data=pb.StringArray(data=['bad'])) if mode=='bad-array' else pb.ScalarField(long_data=pb.LongArray(data=[1]))
                    fields.append(pb.FieldData(field_name='a',type=22,scalars=pb.ScalarField(array_data=pb.ArrayArray(data=[element],element_type=5))))
                    req=getattr(rpc,mutation+'Request')(collection_name='c',partition_name=part,num_rows=1,fields_data=fields)
                    r=getattr(stub,mutation)(req);responses.append((r.status.code,r.status.error_code,r.status.reason))
                    assert (r.status.code==0)==(mode=='valid' and part=='p'),r.status
                    if mode!='valid' or part=='missing':
                        count=stub.Query(rpc.QueryRequest(collection_name='c',output_fields=['count(*)']))
                        assert list(count.fields_data[0].scalars.long_data.data)==[0]
            outcomes.append(responses)
    assert outcomes[0]==outcomes[1]
