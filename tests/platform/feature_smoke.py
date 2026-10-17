"""0015/0016 四个切片的安装产物生命周期冒烟；数据目录必须全新且在源码仓库外。"""
import argparse
import hashlib
import importlib.metadata
import json
from pathlib import Path
import platform
import sys

from pymilvus import DataType, MilvusClient
from milvus_lite.server_manager import server_manager_instance as manager


def check(client):
    rows = list(client.get('c', list(range(20)), output_fields=['n', 'd', 'a']))
    assert [r['id'] for r in rows] == list(range(20)), rows
    for row in rows:
        i = row['id']
        assert row['d'] == 7 and row['n'] == (None if i % 2 else i), row
        assert row['a'] == (None if i % 3 == 0 else [i, i + 1]), row
    assert {r['id'] for r in client.query('c', filter='n is null', output_fields=['id'])} == set(range(1, 20, 2))
    assert {r['id'] for r in client.query('c', filter='array_contains(a, 5)', output_fields=['id'])} == {4, 5}
    iterator = client.query_iterator('c', batch_size=3, filter='id >= 0', output_fields=['id'])
    ids = []
    try:
        for _ in range(10):
            page = list(iterator.next())
            if not page:
                break
            ids.extend(row['id'] for row in page)
        else:
            raise AssertionError('query iterator did not finish')
    finally:
        iterator.close()
    assert ids == list(range(20)), ids
    hits = client.search('c', [[0., 0.]], limit=20, output_fields=['n', 'a'],
                         search_params={'metric_type': 'L2', 'params': {'radius': .0065, 'range_filter': .0003}})[0]
    assert [h['id'] for h in hits] == list(range(2, 9)), hits
    iterator = client.search_iterator('c', [[0., 0.]], batch_size=7, output_fields=['n', 'a'],
                                      search_params={'metric_type': 'L2'})
    ids = []
    try:
        for _ in range(10):
            page = list(iterator.next())
            if not page:
                break
            ids.extend(h['id'] for h in page)
        else:
            raise AssertionError('search iterator did not finish')
    finally:
        iterator.close()
    assert ids == list(range(20)), ids


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--work-dir', required=True, type=Path)
    parser.add_argument('--repo', required=True, type=Path)
    args = parser.parse_args()
    work, repo = args.work_dir.resolve(), args.repo.resolve()
    if work == repo or repo in work.parents:
        raise ValueError('work directory must be outside repository')
    work.mkdir(parents=True, exist_ok=False)
    path = str(work / 'features.db')
    stages = []
    for stage in ['active', 'reopened', 'flushed', 'reopened_after_flush']:
        client = MilvusClient(path, dedicated=True)
        try:
            if stage == 'active':
                schema = client.create_schema(auto_id=False, enable_dynamic_field=False)
                schema.add_field('id', DataType.INT64, is_primary=True)
                schema.add_field('vector', DataType.FLOAT_VECTOR, dim=2)
                schema.add_field('n', DataType.INT64, nullable=True)
                schema.add_field('d', DataType.INT64, default_value=7)
                schema.add_field('a', DataType.ARRAY, element_type=DataType.INT64, max_capacity=4, nullable=True)
                indices = client.prepare_index_params()
                indices.add_index('vector', index_type='FLAT', metric_type='L2')
                client.create_collection('c', schema=schema, index_params=indices)
                result = client.insert('c', [{'id': i, 'vector': [i / 100, 0.],
                    'n': None if i % 2 else i, 'a': None if i % 3 == 0 else [i, i + 1]}
                    for i in reversed(range(20))])
                assert result['insert_count'] == 20, result
            else:
                client.load_collection('c')
            if stage == 'flushed':
                client.flush('c')
            check(client)
            stages.append(stage)
        finally:
            client.close()
            manager.release_server(path)
    import milvus_lite
    native = Path(milvus_lite.__file__).parent
    libraries = list(native.glob('_native*.so'))
    identity = {'python': sys.version, 'machine': platform.machine(), 'sdk': importlib.metadata.version('pymilvus'),
                'version': milvus_lite.__version__, 'stages': stages, 'passed': True,
                'probe_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    if libraries:
        assert len(libraries) == 1
        identity['native_sha256'] = hashlib.sha256(libraries[0].read_bytes()).hexdigest()
    print(json.dumps(identity))


if __name__ == '__main__':
    main()
