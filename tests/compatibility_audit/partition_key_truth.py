"""0017 B 参照调查：分桶顺序、确认边界及进程退出；不代表产品验收。"""

import argparse
import copy
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import subprocess
import sys

from run import REF, outside


def bucket(value):
    return '_pk_' + str(int(hashlib.md5(str(value).encode()).hexdigest(), 16) % 16)


def owned_root(path, case, *, create=False):
    root = outside(path)
    expected = {'root': str(root), 'case': case, 'script_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    owner = root/'tool-owner.json'
    if create:
        root.mkdir(parents=True, exist_ok=False)
        with owner.open('x') as file: json.dump(expected, file)
    else:
        if json.loads(owner.read_text()) != expected:
            raise ValueError('恢复路径不是同版本工具创建的该场景目录')
        if any(p.is_symlink() for p in root.rglob('*')):
            raise ValueError('恢复目录不能包含符号链接')
    return root


def attempt(fn):
    try:
        return {'ok': True, 'value': fn()}
    except Exception as error:
        return {'ok': False, 'type': type(error).__name__, 'code': getattr(error, 'code', None), 'reason': str(error)}


def row(pk, key, tag, array=None):
    result = {'k': key, 'v': [0., 0.], 'tag': tag, 'a': [1] if array is None else array}
    if pk is not None:
        result['id'] = pk
    return result


def direct_cases():
    cases = {}
    for dtype, values, default in [('int', [-(2**63), -1, 0, 1, 2**63-1, None], 42),
                                   ('str', ['', 'a', 'é', 'e\u0301', '中文', 'None', ' null ', '1', None], '填')]:
        values = [row(i+1, key, 'r'+str(i)) for i, key in enumerate(values)]
        values.append({k: v for k, v in row(99, None, 'missing').items() if k != 'k'})
        cases[dtype+'-routes'] = {'dtype': dtype, 'rows': values}
        cases[dtype+'-default'] = {'dtype': dtype, 'default': default, 'rows': [row(1, None, 'null'),
            {k: v for k, v in row(2, None, 'missing').items() if k != 'k'}]}
        cases[dtype+'-zero-default'] = {**copy.deepcopy(cases[dtype+'-default']), 'default': 0 if dtype == 'int' else ''}
    base = [row(1, 0, 'a-first'), row(1, 1, 'b'), row(1, 0, 'a-last')]
    cases['grouped-duplicate'] = {'rows': base}
    cases['grouped-auto'] = {'auto': True, 'rows': [row(None, r['k'], r['tag']) for r in base]}
    for rekey in [False, True]:
        cases['upsert-'+('rekey' if rekey else 'merge')] = {'initial': [row(1, 0, 'old')], 'upsert': True,
            'rows': [{'id': 1, 'tag': 'new', **({'k': 1} if rekey else {})}]}
    for auto in [False, True]:
        for failure in ['arrow-late', 'arrow-first', 'arrow-same', 'validation-late', 'wal-late']:
            for crash in [False, True]:
                rows = [row(None if auto else 1, 0, 'a-first'), row(None if auto else 2, 1, 'b'), row(None if auto else 3, 0, 'a-last')]
                if failure.startswith('arrow'):
                    rows[1]['a'] = [128]
                    if failure == 'arrow-first': rows = [rows[1], rows[0], rows[2]]
                    if failure == 'arrow-same': rows[1]['k'] = 0
                elif failure == 'validation-late': rows[1]['v'] = [0.]
                cases[f'{failure}-auto{int(auto)}-'+('crash' if crash else 'live')] = {'auto': auto, 'rows': rows, 'crash': crash,
                    'injection': 'wal-late' if failure == 'wal-late' else None}
        cases[f'after-first-bucket-auto{int(auto)}'] = {'auto': auto, 'crash': True, 'injection': 'after-first',
            'rows': [row(None if auto else 1, 0, 'a-first'), row(None if auto else 2, 1, 'b'), row(None if auto else 3, 0, 'a-last')]}
    for source in ['arrow-late-auto1-live', 'wal-late-auto1-live']:
        cases[source.replace('-live', '-close')] = {**copy.deepcopy(cases[source]), 'followup_live': False, 'recover_followup': True}
    return cases


def direct_schema(config):
    from milvus_lite.schema.types import CollectionSchema, DataType as T, FieldSchema as F
    key = F('k', T.VARCHAR if config.get('dtype') == 'str' else T.INT64, is_partition_key=True, nullable=True,
            **({'max_length': 64} if config.get('dtype') == 'str' else {}))
    if 'default' in config: key.default_value = config['default']
    return CollectionSchema([F('id', T.INT64, is_primary=True, auto_id=config.get('auto', False)), key,
        F('v', T.FLOAT_VECTOR, dim=2), F('tag', T.VARCHAR, max_length=64),
        F('a', T.ARRAY, element_type=T.INT8, max_capacity=4)], enable_dynamic_field=False)


def capture(collection):
    collection._wait_for_bg(timeout=30)
    parts = collection.list_partitions()
    physical = []
    for pk, (batch, index, _seq) in collection._memtable.pk_index_snapshot():
        value = collection._memtable._insert_batches[batch].slice(index, 1).to_pylist()[0]
        physical.append({k: value[k] for k in ['id', 'k', 'tag', '_partition', '_seq']})
    cols = ['id', 'k', 'tag', '_seq']
    segments = [{'partition': part, 'file': relative, 'rows': [{k: row[k] for k in cols} for row in segment.table.to_pylist()]}
                for (part, relative), segment in collection._segment_cache.items()]
    disk_path = Path(collection._data_dir)/'manifest.json'
    disk = json.loads(disk_path.read_text()) if disk_path.exists() else None
    wal = []
    import pyarrow as pa
    for path in sorted((Path(collection._data_dir)/'wal').glob('wal_data_*.arrow')):
        with path.open('rb') as stream:
            batches = [[{k: row[k] for k in [*cols, '_partition']} for row in batch.to_pylist()] for batch in pa.ipc.open_stream(stream)]
        wal.append({'file': path.name, 'batches': batches})
    return {'disk_manifest': None if disk is None else {'partitions': list(disk['partitions']), 'current_seq': disk['current_seq']},
        'segments': segments, 'wal': wal, 'next_seq': collection._next_seq, 'next_auto_id': collection._next_auto_id,
        'manifest_seq': collection._manifest.current_seq, 'partitions': parts,
        'all': collection.query(output_fields=['id', 'k', 'tag']),
        'by_partition': {p: collection.query(output_fields=['id', 'k', 'tag'], partition_names=[p]) for p in parts},
        'active': physical}


def save(path, value):
    with path.open('w') as file:
        json.dump(value, file, ensure_ascii=False, indent=2, allow_nan=False); file.write('\n'); file.flush(); os.fsync(file.fileno())


def seed(root, config, case):
    from milvus_lite.db import MilvusLite
    root = owned_root(root, case, create=True)
    db = MilvusLite(str(root/'case.db')); c = db.create_collection('c', direct_schema(config))
    if config.get('initial'):
        c.insert(copy.deepcopy(config['initial'])); c.flush(); c._wait_for_bg(timeout=30)
    initial = capture(c)
    # 故障替身仅作用于此实例，退出点在真实完整桶写入后；不修改参照 checkout。
    if config.get('injection') == 'wal-late':
        original = c._wal.write_insert; calls = 0
        def fail_later(batch):
            nonlocal calls
            calls += 1
            if calls == 2: raise OSError('injected second-bucket WAL append failure')
            return original(batch)
        c._wal.write_insert = fail_later
    elif config.get('injection') == 'after-first':
        original = c._apply; calls = 0
        def exit_after_first(op):
            nonlocal calls
            original(op); calls += 1
            if calls == 1:
                save(root/'seed.json', {'before': initial, 'outcome': {'interrupted_after_first_bucket': True}, 'after': capture(c)})
                os._exit(73)
        c._apply = exit_after_first
    result = attempt(lambda: (c.upsert if config.get('upsert') else c.insert)(copy.deepcopy(config['rows']), partition_name='missing-ignored'))
    value = {'before': initial, 'outcome': result, 'after': capture(c)}
    save(root/'seed.json', value)
    if config.get('crash'): os._exit(73)
    if not result['ok'] and config.get('followup_live', True):
        value['followup'] = attempt(lambda: c.insert([row(None if config.get('auto') else 100, 1, 'followup')]))
        value['after_followup'] = capture(c)
    c.flush(); c._wait_for_bg(timeout=30); value['flushed'] = capture(c)
    db.close(); save(root/'seed.json', value)


def recover(root, config, case):
    from milvus_lite.db import MilvusLite
    root = owned_root(root, case)
    db = MilvusLite(str(root/'case.db')); c = db.get_collection('c')
    value = {'reopened': capture(c)}
    if config.get('crash') or config.get('recover_followup'):
        value['followup'] = attempt(lambda: c.insert([row(None if config.get('auto') else 100, 1, 'after-reopen')]))
        value['after_followup'] = capture(c)
        c.flush(); c._wait_for_bg(timeout=30); value['flushed'] = capture(c)
    db.close(); save(root/'recover.json', value)


def reference_guard(reference):
    import milvus_lite, pyarrow
    actual = subprocess.check_output(['git', '-C', str(reference), 'rev-parse', 'HEAD'], text=True).strip()
    if actual != REF or not Path(milvus_lite.__file__).resolve().is_relative_to(reference):
        raise ValueError('参照 commit 或模块来源不符')
    if subprocess.check_output(['git', '-C', str(reference), 'status', '--porcelain'], text=True):
        raise ValueError('参照 checkout 必须干净')
    sdk = importlib.metadata.version('pymilvus')
    if pyarrow.__version__ not in {'15.0.0', '25.0.1'} or sdk not in {'2.6.17', '3.0.2'}:
        raise ValueError('参照环境版本不符')
    return {'reference': REF, 'arrow': pyarrow.__version__, 'pymilvus': sdk,
            'python': sys.version, 'platform': sys.platform, 'script_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}


def rpc_cases(root, *, crash=False):
    import grpc
    from pymilvus import MilvusClient
    from pymilvus.grpc_gen import common_pb2 as common, milvus_pb2 as rpc, schema_pb2 as pb, milvus_pb2_grpc
    from milvus_lite.server_manager import ServerManager
    manager = ServerManager(); uri = manager.start_and_get_uri(str(root/'case.db')); assert uri
    channel = grpc.insecure_channel(uri.removeprefix('http://')); stub = milvus_pb2_grpc.MilvusServiceStub(channel)
    client = MilvusClient(uri, dedicated=True)
    def status(response):
        s = response if isinstance(response, common.Status) else response.status
        return {'code': s.code, 'error_code': s.error_code, 'reason': s.reason}
    def schema(*, auto=False, dtype=5, nullable=True, default=None):
        s = pb.CollectionSchema(name='ignored', autoID=auto)
        s.fields.add(name='id', data_type=5, is_primary_key=True, autoID=auto)
        k = s.fields.add(name='k', data_type=dtype, is_partition_key=True, nullable=nullable)
        if dtype == 21: k.type_params.add(key='max_length', value='64')
        if default is not None:
            if isinstance(default, str): k.default_value.string_data = default
            else: k.default_value.long_data = default
        v = s.fields.add(name='v', data_type=101); v.type_params.add(key='dim', value='2')
        tag = s.fields.add(name='tag', data_type=21); tag.type_params.add(key='max_length', value='64')
        a = s.fields.add(name='a', data_type=22, element_type=2); a.type_params.add(key='max_capacity', value='4')
        return s
    def create(name, s, num=0):
        r = stub.CreateCollection(rpc.CreateCollectionRequest(collection_name=name, schema=s.SerializeToString(), num_partitions=num), timeout=20)
        value = {'status': status(r)}
        if r.code == 0:
            d = stub.DescribeCollection(rpc.DescribeCollectionRequest(collection_name=name), timeout=20)
            value['describe'] = {'status': status(d), 'num_partitions': d.num_partitions,
                'fields': [{'name': f.name, 'dtype': f.data_type, 'is_primary': f.is_primary_key, 'auto_id': f.autoID,
                    'key': f.is_partition_key, 'nullable': f.nullable, 'default_slot': f.default_value.WhichOneof('data'),
                    'default': getattr(f.default_value, f.default_value.WhichOneof('data')) if f.default_value.WhichOneof('data') else None} for f in d.schema.fields]}
            p = stub.ShowPartitions(rpc.ShowPartitionsRequest(collection_name=name), timeout=20)
            value['partitions'] = list(p.partition_names)
        return value
    def raw(name, records, *, auto=False, dtype=5, upsert=False):
        fields = []
        for field, typ in [('id', 5), ('k', dtype), ('tag', 21), ('v', 101), ('a', 22)]:
            if not any(field in r for r in records): continue
            f = pb.FieldData(field_name=field, type=typ); vals = [r.get(field) for r in records]
            if field == 'k' and any(v is None for v in vals): f.valid_data.extend(v is not None for v in vals)
            present = [v for v in vals if v is not None]
            if typ == 5: f.scalars.long_data.data.extend(present)
            elif typ == 21: f.scalars.string_data.data.extend(present)
            elif typ == 101:
                f.vectors.dim = 2; f.vectors.float_vector.data.extend(x for v in present for x in v)
            else:
                f.scalars.array_data.element_type = 2
                for v in present: f.scalars.array_data.data.add().int_data.data.extend(v)
            fields.append(f)
        request = (rpc.UpsertRequest if upsert else rpc.InsertRequest)(collection_name=name, num_rows=len(records), fields_data=fields, partition_name='missing-ignored')
        response = (stub.Upsert if upsert else stub.Insert)(request, timeout=20)
        return {'status': status(response), 'ids': list(response.IDs.int_id.data),
                'success_index': list(response.succ_index), 'error_index': list(response.err_index)}
    def query(name, parts=()):
        r = stub.Query(rpc.QueryRequest(collection_name=name, expr='', output_fields=['id', 'k', 'tag'], partition_names=parts), timeout=20)
        if r.status.code: return {'status': status(r)}
        cols = {}
        for f in r.fields_data:
            vals = list(f.scalars.long_data.data if f.type == 5 else f.scalars.string_data.data)
            if f.valid_data:
                assert len(vals) == len(f.valid_data)
                vals = [v if valid else None for v, valid in zip(vals, f.valid_data, strict=True)]
            cols[f.field_name] = vals
        return [{k: v[i] for k, v in cols.items()} for i in range(len(cols.get('id', [])))]
    result = {}
    try:
        if not crash:
            variants = [('normal', schema(), 0), ('string', schema(dtype=21), 0), ('required', schema(nullable=False), 0),
                ('default', schema(default=42), 0), ('string-default', schema(dtype=21, default='填'), 0)]
            variants += [('num-'+str(n).replace('-', 'minus'), schema(), n) for n in [-1, 1, 2, 32]]
            variants += [('dtype-'+str(t), schema(dtype=t), 0) for t in [1, 2, 4, 10, 23]]
            s = schema(dtype=22); s.fields[1].element_type = 5; variants.append(('array-key', s, 0))
            s = schema(); s.fields[0].is_partition_key = True; variants.append(('two-keys', s, 0))
            s = schema(); s.fields[0].is_partition_key = True; s.fields[1].is_partition_key = False; variants.append(('primary-key', s, 0))
            s = schema(); s.fields[1].is_partition_key = False; s.fields[2].is_partition_key = True; variants.append(('vector-key', s, 0))
            for i, (label, s, num) in enumerate(variants): result['schema/'+label] = create('schema'+str(i), s, num)
        mutations = ['arrow-late'] if crash else ['valid', 'arrow-late', 'arrow-first', 'arrow-same', 'validation-late', 'null', 'missing', 'default', 'required']
        for auto in ([True] if crash else [False, True]):
            for index, mode in enumerate(mutations):
                name = 'c' if crash else 'raw'+str(int(auto))+str(index)
                created = create(name, schema(auto=auto, nullable=mode != 'required', default=42 if mode == 'default' else None)); assert created['status']['code'] == 0
                records = [row(None if auto else 1, 0, 'a-first'), row(None if auto else 2, 1, 'b'), row(None if auto else 3, 0, 'a-last')]
                if mode.startswith('arrow'):
                    records[1]['a'] = [128]
                    if mode == 'arrow-first': records = [records[1], records[0], records[2]]
                    if mode == 'arrow-same': records[1]['k'] = 0
                elif mode == 'validation-late': records[1]['a'] = [1]*5
                elif mode in ['null', 'required']: records[1]['k'] = None
                elif mode in ['missing', 'default']:
                    records = [row(None if auto else 1, 0, 'missing')]; records[0].pop('k')
                value = {'outcome': raw(name, records, auto=auto), 'after': query(name)}
                if crash:
                    save(root/'seed.json', value); os._exit(73)
                value['followup'] = raw(name, [row(None if auto else 100, 1, 'followup')], auto=auto)
                value['after_followup'] = query(name)
                value['flush'] = status(stub.Flush(rpc.FlushRequest(collection_names=[name]), timeout=20))
                value['flushed'] = query(name)
                result['raw/'+mode+'/auto'+str(int(auto))] = value
        if not crash:
            for auto in [False, True]:
                for index, mode in enumerate(['valid', 'arrow-late', 'validation-late', 'default']):
                    name = 'sdk'+str(int(auto))+str(index); assert create(name, schema(auto=auto, default=42 if mode == 'default' else None))['status']['code'] == 0
                    records = [row(None if auto else 1, 0, 'a-first'), row(None if auto else 2, 1, 'b'), row(None if auto else 3, 0, 'a-last')]
                    if mode == 'arrow-late': records[1]['a'] = [128]
                    if mode == 'validation-late': records[1]['a'] = [1]*5
                    if mode == 'default':
                        records = [row(None if auto else 1, 0, 'missing')]; records[0].pop('k')
                    def sdk_insert():
                        r = client.insert(name, records, partition_name='missing-ignored')
                        return {'insert_count': r['insert_count'], 'ids': list(r['ids'])}
                    outcome = attempt(sdk_insert)
                    result['sdk/'+mode+'/auto'+str(int(auto))] = {'outcome': outcome, 'after': query(name)}
            for dtype, keys, default in [(5, [-(2**63), -1, 0, 1, 2**63-1, None], 42),
                                         (21, ['', 'é', 'e\u0301', '中文', 'None', ' null ', None], '填')]:
                for default_index, selected_default in enumerate([None, default, 0 if dtype == 5 else '']):
                    records = [row(i+1, key, 'r'+str(i)) for i, key in enumerate(keys)]
                    records.append({k: v for k, v in row(99, None, 'missing').items() if k != 'k'})
                    for through_sdk in [False, True]:
                        label = f'route-{dtype}-default{default_index}-sdk{int(through_sdk)}'
                        name = label.replace('-', '_')
                        assert create(name, schema(dtype=dtype, default=selected_default))['status']['code'] == 0
                        if through_sdk:
                            response = client.insert(name, copy.deepcopy(records), partition_name='missing-ignored')
                            outcome = {'insert_count': response['insert_count'], 'ids': list(response['ids'])}
                        else: outcome = raw(name, records, dtype=dtype)
                        result[label] = {'outcome': outcome, 'all': query(name),
                            'by_bucket': {p: query(name, [p]) for p in sorted({bucket(r.get('k') if r.get('k') is not None else selected_default) for r in records})}}
            for partial in [False, True]:
                for default in [None, 42]:
                    name = f'sdk_upsert_{int(partial)}_{int(default is not None)}'
                    assert create(name, schema(default=default))['status']['code'] == 0
                    raw(name, [row(1, 0, 'old')]); stub.Flush(rpc.FlushRequest(collection_names=[name]), timeout=20)
                    data = row(1, None, 'new'); data.pop('k')
                    def sdk_upsert():
                        response = client.upsert(name, [data], partial_update=partial)
                        return {'upsert_count': response['upsert_count'], 'ids': list(response['ids'])}
                    result[name] = {'outcome': attempt(sdk_upsert), 'all': query(name),
                        'old_bucket': query(name, [bucket(0)]), 'none_bucket': query(name, [bucket(None)]),
                        'default_bucket': query(name, [bucket(42)])}
            name = 'upsert'; assert create(name, schema())['status']['code'] == 0
            raw(name, [row(1, 0, 'old')]); stub.Flush(rpc.FlushRequest(collection_names=[name]), timeout=20)
            result['raw/upsert-rekey'] = {'outcome': raw(name, [{'id': 1, 'k': 1, 'tag': 'new'}], upsert=True),
                'all': query(name), 'old_bucket': query(name, [bucket(0)]), 'new_bucket': query(name, [bucket(1)])}
            result['manual/create'] = status(stub.CreatePartition(rpc.CreatePartitionRequest(collection_name=name, partition_name='p'), timeout=20))
            result['manual/drop-default'] = status(stub.DropPartition(rpc.DropPartitionRequest(collection_name=name, partition_name='_default'), timeout=20))
            result['manual/drop-missing'] = status(stub.DropPartition(rpc.DropPartitionRequest(collection_name=name, partition_name='missing'), timeout=20))
    finally:
        channel.close(); client.close(); manager.release_all()
    return result


def verify_truth(expected, actual):
    def canonical(value):
        return json.dumps(value, ensure_ascii=False, sort_keys=True, allow_nan=False, separators=(',', ':'))
    if expected['reference'] != REF or actual['reference'] != REF or canonical(expected['cases']) != canonical(actual['cases']):
        raise AssertionError('partition key reference truth differs')


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--reference', type=Path, required=True); p.add_argument('--work-dir', type=Path, required=True)
    p.add_argument('--mode', choices=['direct', 'rpc', 'all'], default='all')
    p.add_argument('--case'); p.add_argument('--phase', choices=['seed', 'recover', 'rpc-seed'])
    p.add_argument('--verify', type=Path)
    args = p.parse_args(); reference = args.reference.resolve(); root = outside(args.work_dir)
    if root == reference or reference in root.parents: raise ValueError('工作目录必须在参照目录外')
    identity = reference_guard(reference)
    if args.phase == 'rpc-seed':
        root = owned_root(root, 'rpc-crash', create=True); rpc_cases(root, crash=True); return
    if args.phase:
        config = direct_cases()[args.case]
        (seed if args.phase == 'seed' else recover)(root, config, args.case); return
    root.mkdir(parents=True, exist_ok=False); results = {}
    if args.mode in ['direct', 'all']:
        for name, config in direct_cases().items():
            target = root/name
            for phase, expected in [('seed', 73 if config.get('crash') else 0), ('recover', 0)]:
                with (root/(name+'-'+phase+'.log')).open('w') as log:
                    process = subprocess.run([sys.executable, __file__, '--reference', str(reference), '--work-dir', str(target),
                        '--case', name, '--phase', phase], env={**os.environ, 'PYTHONDONTWRITEBYTECODE': '1', 'OPENBLAS_NUM_THREADS': '1', 'OMP_NUM_THREADS': '1'}, stdout=log, stderr=subprocess.STDOUT, timeout=90)
                if process.returncode != expected: raise RuntimeError(f'{name}/{phase}: {process.returncode}, expected {expected}')
            results[name] = {'input': config, 'seed': json.loads((target/'seed.json').read_text()), 'recover': json.loads((target/'recover.json').read_text())}
    if args.mode in ['rpc', 'all']:
        results['rpc'] = rpc_cases(root)
        target = root/'raw-crash'
        with (root/'raw-crash-seed.log').open('w') as log:
            process = subprocess.run([sys.executable, __file__, '--reference', str(reference), '--work-dir', str(target), '--phase', 'rpc-seed'],
                env={**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'}, stdout=log, stderr=subprocess.STDOUT, timeout=90)
        assert process.returncode == 73, process.returncode
        recover(target, {'auto': True, 'crash': True}, 'rpc-crash')
        results['raw-crash'] = {'seed': json.loads((target/'seed.json').read_text()), 'recover': json.loads((target/'recover.json').read_text())}

    value = {**identity, 'cases': results}
    if args.verify:
        expected = json.loads(args.verify.read_text())
        verify_truth(expected, value)
    save(root/'result.json', value)
    print(json.dumps({**{k: value[k] for k in ['reference', 'arrow', 'pymilvus', 'script_sha256']},
        'direct_cases': len(results)-int('rpc' in results)-int('raw-crash' in results),
        'rpc_groups': len(results.get('rpc', {}))+int('raw-crash' in results)}))


if __name__ == '__main__': main()
