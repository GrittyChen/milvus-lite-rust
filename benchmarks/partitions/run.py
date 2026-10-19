"""0017：默认路径回归及分区选择读取；逐次核对后输出全部样本。"""
import argparse
import hashlib
import importlib.metadata as metadata
import json
import math
import os
from pathlib import Path
import time


def database_path(path):
    repo=Path(__file__).resolve().parents[2]
    if path.exists() or path.is_symlink() or not path.parent.is_dir() or path.resolve().is_relative_to(repo):
        raise ValueError('database 必须在仓库外、父目录已存在且目标不存在')
    return path


def check_rows(actual, expected):
    # HybridExtraList 对 JSON 的自定义 __eq__ 与逐项比较不同，核验独立的规范文本。
    if json.dumps(list(actual),sort_keys=True,ensure_ascii=False,allow_nan=False) != json.dumps(expected,sort_keys=True,ensure_ascii=False,allow_nan=False):
        raise RuntimeError('读回的行集、次序、JSON 或 ARRAY 类型不同')


def measure(samples, call, check):
    timings=[]
    for sample in range(samples+1):
        start=time.perf_counter();result=call();elapsed=time.perf_counter()-start
        check(result)
        if sample:timings.append(elapsed)
    return timings


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--database',type=Path,required=True)
    p.add_argument('--rows',type=int,default=100000);p.add_argument('--samples',type=int,default=5)
    p.add_argument('--partitioned',action='store_true');p.add_argument('--build',required=True);a=p.parse_args()
    database_path(a.database)
    if a.rows<100 or a.rows%16 or a.samples<3:p.error('rows 至少 100 且为 16 倍数，samples 至少 3')
    import grpc
    from pymilvus import MilvusClient,DataType
    from pymilvus.grpc_gen import milvus_pb2 as rpc,schema_pb2 as pb
    from milvus_lite.server_manager import ServerManager
    import milvus_lite
    manager=ServerManager();uri=manager.start_and_get_uri(str(a.database));assert uri
    c=MilvusClient(uri,dedicated=True);out={}
    try:
        c.create_collection('c',dimension=2,metric_type='L2')
        parts=[f'p{i:02}' for i in range(16)] if a.partitioned else ['_default']
        if a.partitioned:
            for part in parts:c.create_partition('c',part)
        for i,part in enumerate(parts):
            ids=list(range(i*a.rows//len(parts),(i+1)*a.rows//len(parts)))
            for start in range(0,len(ids),5000):
                c.insert('c',[{'id':pk,'vector':[pk/a.rows,1.],'n':pk} for pk in ids[start:start+5000]],partition_name=part)
            c.flush('c')
        selections=[[],parts[:1],parts[::2]] if a.partitioned else [[]]
        for selection in selections:
            ids=[pk for pk in range(a.rows) if not selection or parts[pk*len(parts)//a.rows] in selection]
            expected=[{'id':pk} for pk in ids if pk>=a.rows//2]
            label='all' if not selection else ','.join(selection)
            def check_query(rows):
                check_rows(rows,expected)
            out[label+'/query']=measure(a.samples,lambda:c.query('c',filter=f'n >= {a.rows//2}',partition_names=selection,output_fields=['id'],limit=a.rows),check_query)
            def check_search(results):
                assert len(results)==32
                for hits in results:
                    assert [h['id'] for h in hits]==ids[:10],(label,hits)
                    for hit in hits:
                        assert math.isfinite(hit['distance']) and abs(hit['distance']-(hit['id']/a.rows)**2)<=1e-6
            out[label+'/batch-search']=measure(a.samples,lambda:c.search('c',[[0.,1.]]*32,partition_names=selection,limit=10,search_params={'metric_type':'L2'}),check_search)
        with grpc.insecure_channel(uri.removeprefix('http://')) as channel:
            raw_insert=channel.unary_unary('/milvus.proto.milvus.MilvusService/Insert',request_serializer=lambda data:data,response_deserializer=rpc.MutationResult.FromString)
            for kind in ['plain','json','array']:
                name='write_'+kind;s=c.create_schema(enable_dynamic_field=False)
                s.add_field('id',DataType.INT64,is_primary=True);s.add_field('vector',DataType.FLOAT_VECTOR,dim=2)
                kwargs={'element_type':DataType.INT64,'max_capacity':32} if kind=='array' else {}
                s.add_field('value',{'plain':DataType.INT64,'json':DataType.JSON,'array':DataType.ARRAY}[kind],**kwargs)
                indexes=c.prepare_index_params();indexes.add_index('vector',index_type='FLAT',metric_type='L2');c.create_collection(name,schema=s,index_params=indexes)
                n=min(5000,a.rows);value=1 if kind=='plain' else {'body':'x'*2048,'values':list(range(32))} if kind=='json' else list(range(32))
                data=pb.ScalarField(long_data=pb.LongArray(data=[value]*n)) if kind=='plain' else pb.ScalarField(json_data=pb.JSONArray(data=[json.dumps(value).encode()]*n)) if kind=='json' else pb.ScalarField(array_data=pb.ArrayArray(data=[pb.ScalarField(long_data=pb.LongArray(data=value))]*n,element_type=5))
                request=rpc.InsertRequest(collection_name=name,num_rows=n,fields_data=[pb.FieldData(field_name='id',type=5,scalars=pb.ScalarField(long_data=pb.LongArray(data=list(range(n))))),pb.FieldData(field_name='vector',type=101,vectors=pb.VectorField(dim=2,float_vector=pb.FloatArray(data=[1.,0.]*n))),pb.FieldData(field_name='value',type={'plain':5,'json':23,'array':22}[kind],scalars=data)]).SerializeToString()
                def check_write(response):
                    assert not response.status.code and not response.status.error_code and not response.status.reason
                    assert response.insert_cnt==n and list(response.IDs.int_id.data)==list(range(n)) and list(response.succ_index)==list(range(n))
                    rows=c.query(name,filter='id>=0',output_fields=['id','value'],limit=n)
                    expected=[{'id':pk,'value':value} for pk in range(n)]
                    check_rows(rows,expected)
                out[kind+'/insert']=measure(a.samples,lambda:raw_insert(request),check_write)
                out[kind+'/flush']=[]
                for _ in range(a.samples):
                    check_write(raw_insert(request));start=time.perf_counter();c.flush(name);out[kind+'/flush'].append(time.perf_counter()-start)
                    rows=c.query(name,filter='id>=0',output_fields=['id','value'],limit=n)
                    check_rows(sorted(rows,key=lambda r:r['id']),[{'id':pk,'value':value} for pk in range(n)])
        identity={'build':a.build,'rows':a.rows,'partitioned':a.partitioned,'pymilvus':metadata.version('pymilvus'),'module':milvus_lite.__file__,
                  'script_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),'load_average':os.getloadavg()}
        try:
            from milvus_lite import _native
        except ImportError:identity['reference']=metadata.version('milvus-lite')
        else:identity['native_sha256']=hashlib.sha256(Path(_native.__file__).read_bytes()).hexdigest()
        print(json.dumps({'identity':identity,'seconds':out}))
    finally:c.close();manager.release_all()


if __name__=='__main__':main()
