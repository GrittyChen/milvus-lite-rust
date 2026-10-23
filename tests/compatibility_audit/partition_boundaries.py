"""0017：隔离原始 gRPC 路径与文件系统别名调查；只使用新建的仓库外目录。"""

import argparse
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import re
import struct
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import grpc_fork  # noqa: E402

# 本驱动导入 pymilvus 后还会派生子进程，须在任何导入 grpc 的模块之前执行（见 tests/grpc_fork.py）。
grpc_fork.disable_fork_handlers()

from run import REF, outside


NAMES = ['empty', 'dot', 'dotdot', 'relative', 'absolute', 'slash', 'backslash',
         'nul', 'unicode', 'reserved', 'colon', 'long']
CASES = NAMES + ['case-alias', 'unicode-alias', 'symlink', 'partition-order', 'segment-order']


def stable_result(value, root):
    raw = json.dumps(value, ensure_ascii=True, allow_nan=False).replace(str(root), '<work>')
    result = json.loads(raw)
    # 删除父目录时先遇到哪个已删除子项由文件系统枚举决定，不是协议顺序。
    if result.get('name') == '..' and 'drop' in result:
        reason = result['drop']['reason']
        if re.fullmatch(r"\[Errno 2\] No such file or directory: '<work>/test.db/collections/c/partitions/\.\./(data|delta|indexes)'", reason):
            print(json.dumps({'directory_enumeration_reason': reason}))
            result['drop']['reason'] = '[Errno 2] No such file or directory: <removed collection entry>'
    return result


def checked_target(root, name):
    if '\0' in name:
        return None
    target = (root / 'test.db/collections/c/partitions' / name).resolve()
    if not target.is_relative_to(root):
        raise ValueError('partition target escaped disposable root')
    return target


def observe(fn):
    try:
        fn()
        return {'ok': True}
    except Exception as error:
        return {'ok': False, 'type': type(error).__name__, 'reason': str(error)}


def manifest(root):
    p = root / 'test.db/collections/c/manifest.json'
    if not p.exists():
        return {'missing': True}
    value = json.loads(p.read_text())
    return {k: value[k] for k in ['partitions', 'version', 'current_seq']}


def scenario(root, label):
    import grpc
    from pymilvus import DataType, MilvusClient
    from pymilvus.grpc_gen import common_pb2 as common, milvus_pb2 as rpc, schema_pb2 as pb
    from pymilvus.grpc_gen import milvus_pb2_grpc
    from milvus_lite.server_manager import ServerManager

    manager = ServerManager()
    uri = manager.start_and_get_uri(str(root / 'test.db'))
    assert uri
    client = MilvusClient(uri, dedicated=True)
    schema = client.create_schema(auto_id=False, enable_dynamic_field=False)
    schema.add_field('id', DataType.INT64, is_primary=True)
    schema.add_field('vector', DataType.FLOAT_VECTOR, dim=2)
    schema.add_field('tag', DataType.VARCHAR, max_length=32)
    index = client.prepare_index_params()
    index.add_index('vector', index_type='FLAT', metric_type='L2')
    client.create_collection('c', schema=schema, index_params=index)
    channel = grpc.insecure_channel(uri.removeprefix('http://'))
    stub = milvus_pb2_grpc.MilvusServiceStub(channel)

    def call(method, request):
        try:
            response = getattr(stub, method)(request, timeout=20)
        except grpc.RpcError as error:
            return {'grpc': error.code().name, 'reason': error.details()}
        status = response if isinstance(response, common.Status) else response.status
        result = {'code': status.code, 'error_code': status.error_code, 'reason': status.reason}
        if hasattr(response, 'value'):
            result['value'] = response.value
        if hasattr(response, 'partition_names'):
            result['partitions'] = list(response.partition_names)
        if method == 'Query':
            result['columns'] = {f.field_name: list(f.scalars.long_data.data if f.field_name == 'id'
                                                  else f.scalars.string_data.data) for f in response.fields_data}
        if method == 'Search':
            result['ids'] = list(response.results.ids.int_id.data)
        return result

    def create(name):
        # 所有可能被参照拼接的路径都必须落在本案例自己的可丢弃目录内。
        checked_target(root, name)
        return call('CreatePartition', rpc.CreatePartitionRequest(collection_name='c', partition_name=name))

    def insert(name, pk, tag, vector=None):
        if vector is None:
            vector = [float(pk), 0.]
        return call('Insert', rpc.InsertRequest(collection_name='c', partition_name=name, num_rows=1,
            fields_data=[pb.FieldData(field_name='id', type=5, scalars=pb.ScalarField(long_data=pb.LongArray(data=[pk]))),
                         pb.FieldData(field_name='tag', type=21, scalars=pb.ScalarField(string_data=pb.StringArray(data=[tag]))),
                         pb.FieldData(field_name='vector', type=101, vectors=pb.VectorField(dim=2, float_vector=pb.FloatArray(data=vector)))]))

    def flush():
        return call('Flush', rpc.FlushRequest(collection_names=['c']))

    def query(parts=()):
        return call('Query', rpc.QueryRequest(collection_name='c', partition_names=parts,
            expr='id >= 0', output_fields=['id', 'tag']))

    def search(parts=()):
        ph = common.PlaceholderGroup()
        ph.placeholders.add(tag='$0', type=common.FloatVector).values.append(struct.pack('<ff', 0., 0.))
        r = rpc.SearchRequest(collection_name='c', partition_names=parts, nq=1,
            dsl_type=common.BoolExprV1, placeholder_group=ph.SerializeToString())
        for k, v in [('anns_field', 'vector'), ('topk', '20'), ('metric_type', 'L2'), ('params', '{}')]:
            r.search_params.add(key=k, value=v)
        return call('Search', r)

    def view():
        return {'show': call('ShowPartitions', rpc.ShowPartitionsRequest(collection_name='c')), 'query': query(), 'search': search()}

    assert insert('_default', 10, 'canary')['code'] == 0
    assert flush()['code'] == 0
    result = {'before': view()}
    if label in NAMES:
        names = dict(empty='', dot='.', dotdot='..', relative='../escape', absolute=str(root/'outside'),
                     slash='nested/name', backslash='back\\slash', nul='bad\0name', unicode='分区',
                     reserved='_all', colon='a:b', long='x'*256)
        name = names[label]
        result.update(name=name, create=create(name))
        result['has'] = call('HasPartition', rpc.HasPartitionRequest(collection_name='c', partition_name=name))
        result['manifest_after_create'] = manifest(root)
        if result['create']['code'] == 0:
            result['insert'] = insert(name, 1, label)
            result['flush'] = flush()
            result['selected'] = query([name])
            result['before_drop'] = view()
            result['parquet_outside_collection'] = sorted(str(p.relative_to(root)) for p in root.rglob('*.parquet')
                if not p.is_relative_to(root/'test.db/collections/c'))
            result['drop'] = call('DropPartition', rpc.DropPartitionRequest(collection_name='c', partition_name=name))
    elif label in ['case-alias', 'unicode-alias', 'partition-order', 'segment-order']:
        a, b = {'case-alias': ('p1', 'P1'), 'unicode-alias': ('Café', 'Cafe\u0301'),
                'partition-order': ('z', 'a'), 'segment-order': ('z', 'a')}[label]
        result['create_a'], result['create_b'] = create(a), create(b)
        assert result['create_a']['code'] == result['create_b']['code'] == 0
        base = root/'test.db/collections/c/partitions'
        result['physical_alias'] = os.path.samestat((base/a).stat(), (base/b).stat())
        writes = [(a, 1), (b, 2)] + ([(a, 3)] if label == 'segment-order' else [])
        for name, pk in writes:
            assert insert(name, pk, name, [0., 0.])['code'] == 0
            assert flush()['code'] == 0
        result['live'] = view()
        result['selected_reverse'] = query([b, a])
        if label in ['case-alias', 'unicode-alias']:
            refs = [base/a/f for f in manifest(root)['partitions'][a]['data_files']]
            result['drop_b'] = call('DropPartition', rpc.DropPartitionRequest(collection_name='c', partition_name=b))
            result['a_files_exist_after_drop'] = [p.exists() for p in refs]
            result['selected_a_after_drop'] = query([a])
    elif label == 'symlink':
        target = root/'outside'; target.mkdir()
        marker = target/'owned-canary'; marker.write_text('disposable')
        link = root/'test.db/collections/c/partitions/link'
        link.symlink_to(target, target_is_directory=True)
        result['create'] = create('link')
        assert result['create']['code'] == 0
        result['insert'] = insert('link', 1, 'symlink')
        result['flush'] = flush()
        result['parquet_outside_collection'] = sorted(str(p.relative_to(root)) for p in target.rglob('*.parquet'))
        result['drop'] = call('DropPartition', rpc.DropPartitionRequest(collection_name='c', partition_name='link'))
        result['outside_marker_survives'] = marker.exists()
    result['after'] = view()
    result['manifest_after'] = manifest(root)
    channel.close()
    client.close()
    result['close'] = observe(manager.release_all)
    result['manifest_after_close'] = manifest(root)
    reopened = ServerManager()
    try:
        uri = reopened.start_and_get_uri(str(root/'test.db'))
        result['reopen_started'] = bool(uri)
        if uri:
            channel = grpc.insecure_channel(uri.removeprefix('http://'))
            stub = milvus_pb2_grpc.MilvusServiceStub(channel)
            result['reopen_before_load'] = view()
            result['reopen_load'] = call('LoadCollection', rpc.LoadCollectionRequest(collection_name='c'))
            result['reopen'] = view()
            channel.close()
    finally:
        result['reopen_close'] = observe(reopened.release_all)
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--reference', type=Path, required=True)
    p.add_argument('--work-dir', type=Path, required=True)
    p.add_argument('--case', choices=CASES)
    p.add_argument('--verify', type=Path)
    p.add_argument('--variant')
    args = p.parse_args()
    if args.verify and not args.variant:
        p.error('--verify requires --variant')
    reference = args.reference.resolve(); root = outside(args.work_dir)
    if root == reference or reference in root.parents:
        raise ValueError('work directory must be outside reference checkout')
    assert subprocess.check_output(['git', '-C', str(reference), 'rev-parse', 'HEAD'], text=True).strip() == REF
    import milvus_lite, pyarrow
    assert Path(milvus_lite.__file__).resolve().is_relative_to(reference)
    assert pyarrow.__version__ in {'15.0.0', '25.0.1'}
    sdk = importlib.metadata.version('pymilvus')
    assert sdk in {'2.6.17', '3.0.2'}
    root.mkdir(parents=True, exist_ok=False)
    if args.case:
        result = scenario(root, args.case)
        (root/'result.json').write_text(json.dumps(stable_result(result, root))+'\n')
        return
    cases = {}
    for name in CASES:
        target = root/name
        with (root/(name+'.log')).open('w') as log:
            child = subprocess.run([sys.executable, __file__, '--reference', str(reference), '--work-dir', str(target), '--case', name],
                env={**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'}, stdout=log, stderr=subprocess.STDOUT, timeout=90)
        assert child.returncode == 0, (name, child.returncode)
        cases[name] = json.loads((target/'result.json').read_text())
    result = {'reference': REF, 'script_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'arrow': pyarrow.__version__, 'pymilvus': sdk, 'platform': sys.platform, 'cases': cases}
    if args.verify:
        expected = json.loads(args.verify.read_text())
        assert expected['reference'] == REF
        assert expected['variants'][args.variant]['cases'] == cases, 'partition boundary fixture differs'
    (root/'result.json').write_text(json.dumps(result, indent=2)+'\n')
    print(json.dumps({'count': len(cases), 'arrow': pyarrow.__version__, 'pymilvus': sdk, 'platform': sys.platform}))


if __name__ == '__main__':
    main()
