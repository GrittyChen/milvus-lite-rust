"""0017 B：key 单/多桶的编码、写入、Upsert、读取与进程内峰值。"""
import argparse
import copy
import hashlib
import importlib.metadata as metadata
import json
import math
import os
from pathlib import Path
import resource
import sys
import time


def database_path(path):
    repo=Path(__file__).resolve().parents[2]
    if path.exists() or path.is_symlink() or not path.parent.is_dir() or path.resolve().is_relative_to(repo):
        raise ValueError('database 必须是仓库外的新路径，父目录必须存在')
    return path


def exact(actual,expected):
    if json.dumps(actual,ensure_ascii=False,sort_keys=True,allow_nan=False)!=json.dumps(expected,ensure_ascii=False,sort_keys=True,allow_nan=False):
        raise RuntimeError('结果的行、值或类型不同')


def sdk_payload(expected):
    return [{**copy.deepcopy(row), 'vector': [1., 0.]} for row in expected]


def check_sdk_response(result, method, rows):
    exact(result['insert_count' if method == 'Insert' else 'upsert_count'], rows)
    exact(list(result['ids']), list(range(rows)))


def samples(count,call,check):
    times=[]
    for i in range(count+1):
        start=time.perf_counter();value=call();elapsed=time.perf_counter()-start
        check(value)
        if i:times.append(elapsed)
    return times


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--database',required=True,type=Path)
    parser.add_argument('--mode',choices=['default','single','multi'],required=True)
    parser.add_argument('--workload',choices=['read','plain','json','array'],required=True)
    parser.add_argument('--rows',type=int,default=5000)
    parser.add_argument('--samples',type=int,default=5)
    parser.add_argument('--build',required=True)
    parser.add_argument('--transport',choices=['raw','sdk'],default='raw')
    a=parser.parse_args();database_path(a.database)
    if a.rows<100 or a.samples<3:parser.error('rows 至少100，samples 至少3')
    if a.workload=='read' and a.transport!='raw':parser.error('transport 仅适用于写入专项')
    import grpc
    from pymilvus import MilvusClient,DataType
    from pymilvus.grpc_gen import schema_pb2 as pb,milvus_pb2 as rpc
    from milvus_lite.server_manager import ServerManager
    import milvus_lite
    manager=ServerManager();uri=manager.start_and_get_uri(str(a.database));assert uri
    c=MilvusClient(uri,dedicated=True);out={}
    try:
        s=c.create_schema(enable_dynamic_field=False);s.add_field('id',DataType.INT64,is_primary=True)
        s.add_field('k',DataType.INT64,is_partition_key=a.mode!='default')
        dim=128 if a.workload=='read' else 2
        s.add_field('vector',DataType.FLOAT_VECTOR,dim=dim)
        if a.workload=='read':s.add_field('n',DataType.INT64)
        else:
            kwargs={'element_type':DataType.INT64,'max_capacity':32} if a.workload=='array' else {}
            s.add_field('value',{'plain':DataType.INT64,'json':DataType.JSON,'array':DataType.ARRAY}[a.workload],**kwargs)
        idx=c.prepare_index_params();idx.add_index('vector',index_type='FLAT',metric_type='L2')
        c.create_collection('c',schema=s,index_params=idx)
        described=c.describe_collection('c')
        keys=[f['name'] for f in described['fields'] if f.get('is_partition_key')]
        exact(keys,[] if a.mode=='default' else ['k'])
        if a.mode!='default':
            exact(sorted(c.list_partitions('c')),sorted(['_default']+['_pk_'+str(i) for i in range(16)]))
        def key(i):return 0 if a.mode=='single' else i%16
        if a.workload=='read':
            for start in range(0,a.rows,5000):
                c.insert('c',[{'id':i,'k':key(i),'n':i,'vector':[float(i)]+[1.]*(dim-1)} for i in range(start,min(a.rows,start+5000))])
            c.flush('c')
            def check_query(rows):
                exact(sorted(list(rows),key=lambda r:r['id']),[{'id':i} for i in range(a.rows//2,a.rows)])
            out['s1_query']=samples(a.samples,lambda:c.query('c',filter=f'n >= {a.rows//2}',output_fields=['id'],limit=a.rows),check_query)
            def check_search(result):
                assert len(result)==32
                for hits in result:
                    exact([h['id'] for h in hits],list(range(10)))
                    for h in hits:
                        expected=float(h['id']**2)
                        if not math.isfinite(h['distance']) or abs(h['distance']-expected)>max(1e-5*abs(expected),1e-6):
                            raise RuntimeError('检索距离超过既有预算')
            out['batch_search']=samples(a.samples,lambda:c.search('c',[[0.]+[1.]*(dim-1)]*32,limit=10,search_params={'metric_type':'L2'}),check_search)
        else:
            n=a.rows;value=1 if a.workload=='plain' else {'body':'x'*2048,'values':list(range(32))} if a.workload=='json' else list(range(32))
            fd=[pb.FieldData(field_name='id',type=5,scalars=pb.ScalarField(long_data=pb.LongArray(data=range(n)))),
                pb.FieldData(field_name='k',type=5,scalars=pb.ScalarField(long_data=pb.LongArray(data=[key(i) for i in range(n)]))),
                pb.FieldData(field_name='vector',type=101,vectors=pb.VectorField(dim=2,float_vector=pb.FloatArray(data=[1.,0.]*n)))]
            v=pb.FieldData(field_name='value',type={'plain':5,'json':23,'array':22}[a.workload])
            if a.workload=='plain':v.scalars.long_data.data.extend([value]*n)
            elif a.workload=='json':v.scalars.json_data.data.extend([json.dumps(value).encode()]*n)
            else:
                v.scalars.array_data.element_type=5
                for _ in range(n):v.scalars.array_data.data.add().long_data.data.extend(value)
            fd.append(v)
            expected=[{'id':i,'k':key(i),'value':value} for i in range(n)]
            sdk_rows=sdk_payload(expected) if a.transport=='sdk' else None
            def check(r):
                if a.transport=='raw':
                    exact((r.status.code,r.status.error_code,r.status.reason),(0,0,''))
                    exact(list(r.IDs.int_id.data),list(range(n)));exact(list(r.succ_index),list(range(n)))
                    exact(r.insert_cnt if method=='Insert' else r.upsert_cnt,n)
                    exact(list(r.err_index),[])
                else:
                    check_sdk_response(r,method,n)
                exact(sorted(list(c.query('c',filter='',output_fields=['id','k','value'],limit=n)),key=lambda r:r['id']),expected)
            with grpc.insecure_channel(uri.removeprefix('http://'),options=[('grpc.max_send_message_length',-1),('grpc.max_receive_message_length',-1)]) as channel:
                for method in ['Insert','Upsert']:
                    raw=getattr(rpc,method+'Request')(collection_name='c',num_rows=n,fields_data=fd).SerializeToString()
                    call=channel.unary_unary('/milvus.proto.milvus.MilvusService/'+method,request_serializer=lambda v:v,response_deserializer=rpc.MutationResult.FromString)
                    operation=(lambda:call(raw)) if a.transport=='raw' else (lambda:getattr(c,method.lower())('c',sdk_rows))
                    out[method.lower()]=samples(a.samples,operation,check)
        # 当前进程高水位，含固定客户端输入。C++ 独立子进程不在该口径内。
        peak=resource.getrusage(resource.RUSAGE_SELF).ru_maxrss/(1024**2 if sys.platform=='darwin' else 1024)
        start=time.perf_counter();c.flush('c');out['flush']=[time.perf_counter()-start]
        exact(list(c.query('c',filter='',output_fields=['count(*)'])),[{'count(*)':a.rows}])
        native=Path(milvus_lite.__file__).parent/'_native.abi3.so'
        result={'build':a.build,'mode':a.mode,'workload':a.workload,'rows':a.rows,'dim':dim,'samples':a.samples,
                'write_transport':None if a.workload=='read' else a.transport,
                'seconds':out,'process_peak_rss_mib':peak,'rss_scope':'Python client plus in-process service; excludes external C++ server',
                'native_sha256':hashlib.sha256(native.read_bytes()).hexdigest() if native.exists() else None,
                'script_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                'packages':{p:metadata.version(p) for p in ['pymilvus','numpy']},'python':sys.version,
                'load_average':os.getloadavg(),'thread_env':{k:os.environ.get(k) for k in ['OMP_NUM_THREADS','OPENBLAS_NUM_THREADS','RAYON_NUM_THREADS']}}
    finally:
        try:c.close()
        finally:manager.release_all()
    print(json.dumps(result,sort_keys=True))


if __name__=='__main__':main()
