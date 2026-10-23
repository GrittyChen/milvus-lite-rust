"""固定 3.x range search / V1 iterator 的端到端真值；不覆盖已有工作目录。"""
import argparse
import hashlib
import importlib.metadata
import json
from pathlib import Path
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import grpc_fork  # noqa: E402

# 本驱动导入 pymilvus 后还会派生子进程，须在任何导入 grpc 的模块之前执行（见 tests/grpc_fork.py）。
grpc_fork.disable_fork_handlers()

from run import REF, outside
from range_cases import matrix, outcome, request


def collect(client, stub, metric):
    result = {}
    for label, req in matrix(metric):
        value = outcome(stub.Search(req))
        assert bool(value['code']) == label.startswith('error/'), (label, value)
        result[label] = value
    for batch, limit in [(1, -1), (7, -1), (7, 19)]:
        iterator = client.search_iterator('c', data=[[0.,0.]] if metric=='L2' else [[1.,0.]],
            batch_size=batch, limit=limit, output_fields=['n','a'],
            search_params={'metric_type':metric})
        pages = []
        try:
            for _ in range(100):
                page = list(iterator.next())
                pages.append([{'id':h['id'], 'distance':h['distance'], 'entity':dict(h['entity'])} for h in page])
                if not page: break
            else: raise AssertionError('search iterator did not finish')
        finally: iterator.close()
        # V1 的 batch=1 将宽度固定为 0.05，21 次线性探测跨不过下一个 L2 距离间隙。
        expected = 2 if metric == 'L2' and batch == 1 else (32 if limit < 0 else limit)
        assert sum(map(len,pages)) == expected, (metric, batch, limit, pages)
        result[f'sdk/{batch}/{limit}'] = pages
    client.release_collection('c')
    result['released/range'] = outcome(stub.Search(request(metric,[('radius','10')])))
    result['released/bad-radius'] = outcome(stub.Search(request(metric,[('radius','"bad"')])))
    result['released/bad-filter'] = outcome(stub.Search(request(metric,[('radius','10')],expr='id >')))
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--reference', required=True, type=Path)
    p.add_argument('--work-dir', required=True, type=Path)
    p.add_argument('--verify', type=Path)
    args = p.parse_args()
    import grpc, milvus_lite
    from milvus_lite.server_manager import ServerManager
    from pymilvus import MilvusClient, DataType
    from pymilvus.grpc_gen import milvus_pb2_grpc
    assert subprocess.check_output(['git','-C',str(args.reference),'rev-parse','HEAD'],text=True).strip()==REF
    assert Path(milvus_lite.__file__).resolve().is_relative_to(args.reference.resolve())
    root=outside(args.work_dir);root.mkdir(parents=True,exist_ok=False);cases={}
    for metric in ['L2','IP','COSINE']:
        for state in ['active','mixed','segment','reopened','compacted']:
            path=root/f'{metric}-{state}.db';manager=ServerManager()
            client=MilvusClient(manager.start_and_get_uri(str(path)),dedicated=True)
            try:
                schema=client.create_schema(auto_id=False,enable_dynamic_field=True)
                schema.add_field('id',DataType.INT64,is_primary=True)
                schema.add_field('vector',DataType.FLOAT_VECTOR,dim=2)
                schema.add_field('n',DataType.INT64,nullable=True)
                schema.add_field('a',DataType.ARRAY,element_type=DataType.INT64,max_capacity=4,nullable=True)
                index=client.prepare_index_params();index.add_index('vector',index_type='FLAT',metric_type=metric)
                client.create_collection('c',schema=schema,index_params=index)
                rows=[{'id':i,'vector':([.5,1.] if i==2 else [float(i),0.]) if metric=='L2' else [float(i),1.],
                       'n':None if i%2==0 else i,'a':None if i%2==0 else [i],'d':i} for i in range(1,33)]
                if state=='compacted':
                    for start in range(0,32,8):client.insert('c',rows[start:start+8]);client.flush('c')
                elif state=='mixed':client.insert('c',rows[:16]);client.flush('c');client.insert('c',rows[16:])
                else:
                    client.insert('c',rows)
                    if state!='active':client.flush('c')
                if state in ('reopened','compacted'):
                    client.close();manager.release_all()
                    client=MilvusClient(manager.start_and_get_uri(str(path)),dedicated=True);client.load_collection('c')
                    if state=='compacted':assert len(json.loads((path/'collections/c/manifest.json').read_text())['partitions']['_default']['data_files'])==1
                with grpc.insecure_channel(manager.start_and_get_uri(str(path)).removeprefix('http://')) as channel:
                    cases[f'{metric}/{state}']=collect(client,milvus_pb2_grpc.MilvusServiceStub(channel),metric)
            finally:client.close();manager.release_all()
    result={'reference':REF,'pymilvus':importlib.metadata.version('pymilvus'),
            'script_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            'input_sha256':hashlib.sha256(Path(__file__).with_name('range_cases.py').read_bytes()).hexdigest(),'cases':cases}
    (root/'result.json').write_text(json.dumps(result,indent=2,allow_nan=False)+'\n')
    if args.verify:
        expected=json.loads(args.verify.read_text())
        assert expected['reference']==REF and expected['input_sha256']==result['input_sha256'] and expected['cases']==cases
    print(json.dumps({'datasets':len(cases),'observations':sum(len(c) for c in cases.values()),'pymilvus':result['pymilvus']}))


if __name__=='__main__':main()
