"""0016 A：排序先于分页；原始协议与 SDK 逐项、逐字对照 3.x。"""
import json

import grpc
import pytest
from pymilvus import DataType
from pymilvus.grpc_gen import milvus_pb2, milvus_pb2_grpc

from test_reopen import pair


def pages(client, **kwargs):
    iterator = client.query_iterator('c', output_fields=['id', 'n', 'a'], **kwargs)
    result = []
    try:
        for _ in range(20):
            page = list(iterator.next())
            result.append(page)
            if not page:
                return result
        raise AssertionError('iterator did not finish')
    finally:
        iterator.close()


@pytest.mark.parametrize('varchar', [False, True])
@pytest.mark.parametrize('state', ['active', 'mixed', 'segment', 'reopened', 'compacted'])
def test_query_iterator_order_pagination_and_null_output(pair, varchar, state):
    diff = pair.open()
    ids = ['f', 'a', 'é', 'd', 'b', '中'] if varchar else [9, -3, 4, 0, 7, 2]
    def create(c):
        s = c.create_schema(auto_id=False, enable_dynamic_field=True)
        s.add_field('id', DataType.VARCHAR if varchar else DataType.INT64,
                    is_primary=True, **({'max_length': 32} if varchar else {}))
        s.add_field('vector', DataType.FLOAT_VECTOR, dim=2)
        s.add_field('n', DataType.INT64, nullable=True)
        s.add_field('a', DataType.ARRAY, element_type=DataType.INT64,
                    max_capacity=4, nullable=True)
        c.create_collection('c', schema=s)
    diff.step('create', create)
    rows = [{'id': pk, 'vector': [float(i), 0.], 'n': None if i % 2 == 0 else i,
             'a': None if i % 2 == 0 else [i], 'd': i} for i, pk in enumerate(ids)]
    slices = [(0, 2), (2, 3), (3, 4), (4, 6)] if state == 'compacted' else [(0, 3), (3, 6)]
    for start, end in slices:
        diff.step('insert', lambda c: c.insert('c', rows[start:end]))
        if state == 'compacted' or state == 'mixed' and start == 0:
            diff.step('flush', lambda c: c.flush('c'))
    if state in ('segment', 'reopened'):
        diff.step('flush', lambda c: c.flush('c'))
    if state in ('reopened', 'compacted'):
        pair.close(); diff = pair.open()
        diff.step('load', lambda c: c.load_collection('c'))
        if state == 'compacted':
            from pathlib import Path
            for root in (pair.rust_path, pair.ref_path):
                manifest = json.loads((Path(root)/'collections/c/manifest.json').read_text())
                assert len(manifest['partitions']['_default']['data_files']) == 1

    expressions = ['', 'id in ' + json.dumps(ids + ids[:1], ensure_ascii=False), 'n is null',
                   'n is null or d == 3', 'array_contains(a, 3)', 'id >']
    outputs = []
    for uri in pair.uris:
        with grpc.insecure_channel(uri.removeprefix('http://')) as channel:
            stub = milvus_pb2_grpc.MilvusServiceStub(channel)
            outcomes = []
            for flag in (None, 'true', 'false', '0', 'garbage'):
                for expr in expressions:
                    for limit, offset in [(2, 1), (0, 0), (-1, 0), (2, -1), (99, 2)]:
                        req = milvus_pb2.QueryRequest(collection_name='c', expr=expr,
                                                     output_fields=['id', 'n', 'a'])
                        if flag is not None:
                            req.query_params.add(key='iterator', value=flag)
                        req.query_params.add(key='limit', value=str(limit))
                        req.query_params.add(key='offset', value=str(offset))
                        r = stub.Query(req)
                        assert r.status.code == (6 if expr == 'id >' else 0), r.status
                        outcomes.append((r.status.code, r.status.error_code, r.status.reason,
                                         list(r.output_fields), r.primary_field_name,
                                         [f.SerializeToString() for f in r.fields_data]))
            outputs.append(outcomes)
    assert outputs[0] == outputs[1]
    expected = [{key: row[key] for key in ('id', 'n', 'a')}
                for row in sorted(rows, key=lambda row: row['id'])]
    for batch, limit, offset in [(2, -1, 0), (3, 5, 0), (2, 3, 2)]:
        result = diff.step('pages', lambda c: pages(c, batch_size=batch, limit=limit, offset=offset))
        assert [r for page in result[1] for r in page] == expected[offset:][:None if limit < 0 else limit]
        assert result[1][-1] == []
    diff.step('filtered-pages', lambda c: pages(c, batch_size=1, filter='n is null or d == 3'))
    def create_index(c):
        params = c.prepare_index_params()
        params.add_index('vector', index_type='FLAT', metric_type='L2')
        c.create_index('c', params)
    diff.step('index', create_index)
    diff.step('release', lambda c: c.release_collection('c'))
    released = []
    for uri in pair.uris:
        with grpc.insecure_channel(uri.removeprefix('http://')) as channel:
            req = milvus_pb2.QueryRequest(collection_name='c', expr='id >', output_fields=['id'])
            req.query_params.add(key='iterator', value='false')
            r = milvus_pb2_grpc.MilvusServiceStub(channel).Query(req)
            released.append((r.status.code, r.status.error_code, r.status.reason))
    assert released[0] == released[1] and released[0][0] == 101
    diff.step('count', lambda c: c.query('c', output_fields=['count(*)']))
    diff.step('released-sdk', lambda c: pages(c, batch_size=2), error=True)
