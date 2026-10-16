"""普通读取回归及 Query iterator 的首批、全量分页诊断；不覆盖已有数据库。"""
import argparse
import hashlib
import importlib.metadata
import json
from pathlib import Path
import platform
import random
import resource
import statistics
import time


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--work-dir', type=Path, required=True)
    p.add_argument('--rows', type=int, default=100000)
    p.add_argument('--samples', type=int, default=9)
    p.add_argument('--ordered', action='store_true')
    p.add_argument('--varchar', action='store_true')
    args = p.parse_args()
    root = args.work_dir.resolve()
    if root.is_relative_to(Path(__file__).resolve().parents[2]) or root.exists():
        p.error('work-dir 必须为仓库外不存在的目录')
    if not 100 <= args.rows <= 1000000 or args.samples < 3:
        p.error('rows 必须为 100..1000000，samples 至少为 3')
    import grpc
    import milvus_lite._native as native
    from milvus_lite.server_manager import ServerManager
    from pymilvus import DataType, MilvusClient
    from pymilvus.grpc_gen import milvus_pb2, milvus_pb2_grpc
    root.mkdir(parents=True)
    manager = ServerManager()
    uri = manager.start_and_get_uri(str(root/'data.db'))
    client = MilvusClient(uri, dedicated=True)
    pk = (lambda i: f'主键-{i:08d}') if args.varchar else (lambda i: i)
    order = list(range(args.rows)); random.Random(37).shuffle(order)
    try:
        schema = client.create_schema(auto_id=False, enable_dynamic_field=False)
        schema.add_field('id', DataType.VARCHAR if args.varchar else DataType.INT64,
                         is_primary=True, **({'max_length': 32} if args.varchar else {}))
        schema.add_field('v', DataType.FLOAT_VECTOR, dim=2)
        schema.add_field('n', DataType.INT64, nullable=True)
        client.create_collection('c', schema=schema)
        for start in range(0, len(order), 5000):
            values = order[start:start+5000]
            result = client.insert('c', [{'id': pk(i), 'v': [1., 0.],
                                         'n': None if i % 2 == 0 else i} for i in values])
            assert list(result['ids']) == [pk(i) for i in values]
        client.flush('c')
        tests = [('ordinary', 'id >= '+json.dumps(pk(0), ensure_ascii=False), None, [pk(i) for i in order[:100]]),
                 ('get', 'id in '+json.dumps([pk(i) for i in order[:100]], ensure_ascii=False), None, [pk(i) for i in order[:100]])]
        if args.ordered:
            tests += [('ordered', '', 'true', [pk(i) for i in range(100)]),
                      ('ordered-null', 'n is null', 'false', [pk(i) for i in range(0, min(args.rows, 200), 2)])]
        timings = {}
        with grpc.insecure_channel(uri.removeprefix('http://')) as channel:
            stub = milvus_pb2_grpc.MilvusServiceStub(channel)
            for name, expr, iterator, expected in tests:
                req = milvus_pb2.QueryRequest(collection_name='c', expr=expr, output_fields=['id'])
                req.query_params.add(key='limit', value='100')
                if iterator is not None: req.query_params.add(key='iterator', value=iterator)
                times = []
                for sample in range(args.samples + 2):
                    started = time.perf_counter_ns(); response = stub.Query(req)
                    elapsed = (time.perf_counter_ns()-started)/1e6
                    assert response.status.code == response.status.error_code == 0, response.status
                    field = next(f for f in response.fields_data if f.field_name == 'id')
                    actual = list(field.scalars.string_data.data if args.varchar else field.scalars.long_data.data)
                    assert actual == expected, (name, actual[:10], expected[:10])
                    if sample >= 2: times.append(elapsed)
                timings[name] = {'ms': times, 'p50_ms': statistics.median(times)}
        if args.ordered:
            iterator = client.query_iterator('c', batch_size=4096, output_fields=['id', 'n'])
            count = 0; pages = 0; started = time.perf_counter_ns()
            try:
                while True:
                    page = list(iterator.next()); pages += 1
                    if not page: break
                    for offset, row in enumerate(page, start=count):
                        assert row == {'id': pk(offset), 'n': None if offset % 2 == 0 else offset}
                    count += len(page)
            finally: iterator.close()
            assert count == args.rows
            timings['all-pages'] = {'ms': (time.perf_counter_ns()-started)/1e6, 'rows': count, 'pages': pages}
        result = {'rows': args.rows, 'varchar': args.varchar, 'python': platform.python_version(),
                  'pymilvus': importlib.metadata.version('pymilvus'), 'platform': platform.platform(),
                  'native_sha256': hashlib.sha256(Path(native.__file__).read_bytes()).hexdigest(),
                  'script_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                  'maxrss_native_units': resource.getrusage(resource.RUSAGE_SELF).ru_maxrss, 'timings': timings}
        (root/'result.json').write_text(json.dumps(result, indent=2)+'\n')
        print(json.dumps(result))
    finally:
        client.close(); manager.release_all()


if __name__ == '__main__': main()
