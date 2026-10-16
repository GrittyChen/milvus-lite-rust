"""普通检索回归、范围检索及客户端 V1 对照；只写仓库外新目录。"""
import argparse
import hashlib
import importlib.metadata
import json
import math
import os
from pathlib import Path
import platform
import statistics
import struct
import time


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--work-dir',required=True,type=Path)
    p.add_argument('--rows',default=100000,type=int)
    p.add_argument('--samples',default=9,type=int)
    p.add_argument('--range',action='store_true',dest='ranged')
    args=p.parse_args();root=args.work_dir.resolve()
    if root.exists() or root.is_relative_to(Path(__file__).resolve().parents[2]):
        p.error('work-dir 必须为仓库外不存在的目录')
    if not 5000<=args.rows<=1000000 or args.samples<3:p.error('rows 为 5000..1000000，samples 至少 3')
    import grpc, milvus_lite
    from milvus_lite.server_manager import ServerManager
    from pymilvus import MilvusClient,DataType
    from pymilvus.grpc_gen import common_pb2,milvus_pb2
    root.mkdir(parents=True)
    manager=ServerManager();uri=manager.start_and_get_uri(str(root/'data.db'))
    client=MilvusClient(uri,dedicated=True)
    metadata={'python':platform.python_version(),'platform':platform.platform(),
              'pymilvus':importlib.metadata.version('pymilvus'),
              'packages':{d.metadata['Name']:d.version for d in importlib.metadata.distributions()},
              'thread_environment':{k:os.environ.get(k) for k in ['OMP_NUM_THREADS','OPENBLAS_NUM_THREADS','VECLIB_MAXIMUM_THREADS']},
              'rows':args.rows,'dim':128,'script_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    try:
        import milvus_lite._native as native
    except ImportError: metadata['official_version']=importlib.metadata.version('milvus-lite')
    else:metadata['native_sha256']=hashlib.sha256(Path(native.__file__).read_bytes()).hexdigest()
    try:
        schema=client.create_schema(auto_id=False,enable_dynamic_field=False)
        schema.add_field('id',DataType.INT64,is_primary=True)
        schema.add_field('v',DataType.FLOAT_VECTOR,dim=128)
        index=client.prepare_index_params();index.add_index('v',index_type='FLAT',metric_type='L2')
        client.create_collection('c',schema=schema,index_params=index)
        for start in range(0,args.rows,5000):
            values=list(range(start,min(args.rows,start+5000)))
            response=client.insert('c',[{'id':i,'v':[i/65536]+[0.]*127} for i in values])
            assert response['insert_count']==len(values) and list(response['ids'])==values
        client.flush('c');client.load_collection('c')
        timings={}
        with grpc.insecure_channel(uri.removeprefix('http://')) as channel:
            search=channel.unary_unary('/milvus.proto.milvus.MilvusService/Search',
                request_serializer=lambda data:data,response_deserializer=milvus_pb2.SearchResults.FromString)
            for ranged in ([False,True] if args.ranged else [False]):
                for nq in [1,32]:
                    queries=[[-i/4]+[0.]*127 for i in range(nq)]
                    ph=common_pb2.PlaceholderGroup();v=ph.placeholders.add(tag='$0',type=common_pb2.FloatVector)
                    v.values.extend(struct.pack('<128f',*q) for q in queries)
                    req=milvus_pb2.SearchRequest(collection_name='c',nq=nq,dsl_type=common_pb2.BoolExprV1,
                                                placeholder_group=ph.SerializeToString())
                    for k,v in [('anns_field','v'),('metric_type','L2'),('topk','10'),
                                ('params','{"radius":1000,"range_filter":0}' if ranged else '{}')]:
                        req.search_params.add(key=k,value=v)
                    encoded=req.SerializeToString();samples=[]
                    for sample in range(args.samples+2):
                        started=time.perf_counter_ns();result=search(encoded);elapsed=(time.perf_counter_ns()-started)/1e6
                        assert result.status.code==result.status.error_code==0,result.status
                        r=result.results;assert list(r.topks)==[10]*nq and list(r.ids.int_id.data)==list(range(10))*nq
                        assert len(r.scores)==10*nq
                        for qi,q in enumerate(queries):
                            for pk,actual in enumerate(r.scores[qi*10:(qi+1)*10]):
                                expected=(pk/65536-q[0])**2
                                assert math.isfinite(actual) and abs(actual-expected)<=max(1e-5*expected,1e-6)
                        if sample>=2:samples.append(elapsed)
                    timings[f'{"range" if ranged else "ordinary"}/nq{nq}']={'ms':samples,'p50_ms':statistics.median(samples)}
        if args.ranged:
            durations=[]
            for _ in range(3):
                started=time.perf_counter_ns()
                iterator=client.search_iterator('c',data=[[0.]*128],anns_field='v',batch_size=1000,limit=5000,
                                                search_params={'metric_type':'L2'})
                ids=[]
                try:
                    for _ in range(20):
                        page=list(iterator.next());ids.extend(h['id'] for h in page)
                        if not page:break
                    else:raise AssertionError('iterator did not finish')
                finally:iterator.close()
                assert ids==list(range(5000)),(len(ids),ids[-10:])
                durations.append((time.perf_counter_ns()-started)/1e6)
            timings['iterator5000']={'ms':durations,'p50_ms':statistics.median(durations)}
        metadata['timings']=timings
        (root/'result.json').write_text(json.dumps(metadata,indent=2)+'\n');print(json.dumps(metadata))
    finally:client.close();manager.release_all()


if __name__=='__main__':main()
