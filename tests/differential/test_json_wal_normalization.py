"""0018：原始 JSON 列的精确 WAL 文本、父有效位及重开后的可观察行为。"""
import json
import os
import subprocess

import grpc
import pytest
from pymilvus import DataType
from pymilvus.grpc_gen import milvus_pb2 as rpc, milvus_pb2_grpc, schema_pb2 as pb

from conftest import ENV
from test_reopen import pair
from test_scenarios import index


INSPECT = r'''
import json,sys,pyarrow as pa
from pathlib import Path
rows=[]
for path in sorted((Path(sys.argv[1])/'collections/c/wal').glob('wal_data_*.arrow')):
 for batch in pa.ipc.open_stream(path):
  batch.validate(full=True)
  for row in batch.to_pylist():
   rows.append([row['id'],row['_partition'],row['j'],row['$meta']])
assert rows
print(json.dumps(rows,ensure_ascii=False))
'''


@pytest.mark.parametrize('key', [False, True])
@pytest.mark.parametrize('mutation', ['Insert', 'Upsert'])
@pytest.mark.parametrize('layout', ['json-null', 'compact', 'full'])
def test_raw_json_wal_text_masks_and_lifecycle(pair, key, mutation, layout):
    d = pair.open()
    schema = d.rust.create_schema(enable_dynamic_field=True)
    schema.add_field('id', DataType.INT64, is_primary=True)
    schema.add_field('k', DataType.INT64, is_partition_key=key)
    schema.add_field('vector', DataType.FLOAT_VECTOR, dim=2)
    schema.add_field('j', DataType.JSON, nullable=True)
    d.step('create', lambda c: c.create_collection('c', schema=schema, index_params=index('L2')))
    raw = [
        ' {"z":1E2,"a":"中文\\t\\u0000","wide":184467440737095516160} '.encode(),
        b'null' if layout == 'json-null' else b'{"masked":true}',
        b'{"negative_zero":-0.0,"exponent":1e-12,"min":5e-324,"nested":[true,null,{}]}',
    ]
    expected = [json.loads(raw[0]), None, json.loads(raw[2])]
    fields = [
        pb.FieldData(field_name='id', type=5, scalars=pb.ScalarField(long_data=pb.LongArray(data=[1, 2, 3]))),
        pb.FieldData(field_name='k', type=5, scalars=pb.ScalarField(long_data=pb.LongArray(data=[0, 1, 11]))),
        pb.FieldData(field_name='vector', type=101, vectors=pb.VectorField(dim=2, float_vector=pb.FloatArray(data=[1., 0., 2., 0., 3., 0.]))),
        pb.FieldData(field_name='j', type=23, valid_data=[] if layout == 'json-null' else [True, False, True],
                     scalars=pb.ScalarField(json_data=pb.JSONArray(data=[raw[0], raw[2]] if layout == 'compact' else raw))),
        pb.FieldData(field_name='$meta', type=23, is_dynamic=True,
                     scalars=pb.ScalarField(json_data=pb.JSONArray(data=[b'{"z":{"b":1,"a":2},"a":"unicode"}'] * 3))),
    ]
    if mutation == 'Upsert':
        d.step('seed', lambda c: c.insert('c', [{'id': i, 'k': k, 'vector': [1., 0.], 'j': {'old': i}, 'z': {'b': 1, 'a': 2}, 'a': 'unicode'}
                                              for i, k in [(1, 0), (2, 1), (3, 11)]]))
    results = []
    for uri in pair.uris:
        with grpc.insecure_channel(uri.removeprefix('http://')) as channel:
            call = getattr(milvus_pb2_grpc.MilvusServiceStub(channel), mutation)
            result = call(getattr(rpc, mutation + 'Request')(collection_name='c', num_rows=3, fields_data=fields))
            assert (result.status.code, result.status.error_code, result.status.reason) == (0, 0, '')
            results.append((list(result.IDs.int_id.data), list(result.succ_index), list(result.err_index),
                            result.insert_cnt, result.upsert_cnt))
    assert results[0] == results[1]
    assert results[0][:3] == ([1, 2, 3], [0, 1, 2], [])
    wal = [json.loads(subprocess.check_output([os.environ[ENV], '-B', '-c', INSPECT, path], text=True))
           for path in (pair.rust_path, pair.ref_path)]
    assert wal[0] == wal[1]
    for stage in ('active', 'flushed', 'reopened'):
        if stage == 'flushed':
            d.step('flush', lambda c: c.flush('c'))
        elif stage == 'reopened':
            pair.close()
            d = pair.open()
            d.step('load', lambda c: c.load_collection('c'))
        result = d.step(stage + '-get', lambda c: c.get('c', ids=[1, 2, 3], output_fields=['id', 'j']))
        assert result == ('ok', [{'id': i, 'j': value} for i, value in enumerate(expected, 1)])
        for expression in ('j is null', 'j["z"] == 100', 'a == "unicode"'):
            d.step(stage + '-filter', lambda c: c.query('c', filter=expression, output_fields=['id', 'j']))
        d.step(stage + '-search', lambda c: c.search('c', [[0., 0.]], limit=3, output_fields=['id', 'j']))
