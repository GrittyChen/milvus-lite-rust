"""固定 3.x 的 Query iterator 请求与分页真值；只在仓库外生成调查目录。"""

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
from partition_truth import attempt


def scalar(field):
    kind = field.WhichOneof("data")
    if kind == "array_data":
        return {"kind": kind, "element_type": field.array_data.element_type,
                "rows": [scalar(row) for row in field.array_data.data]}
    return {"kind": kind, "values": list(getattr(field, kind).data) if kind else []}


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--reference',required=True,type=Path);p.add_argument('--work-dir',required=True,type=Path);p.add_argument('--verify', type=Path);args=p.parse_args()
    import grpc
    import milvus_lite
    from milvus_lite.server_manager import ServerManager
    from pymilvus import DataType, MilvusClient
    from pymilvus.grpc_gen import milvus_pb2, milvus_pb2_grpc
    assert subprocess.check_output(['git','-C',str(args.reference),'rev-parse','HEAD'],text=True).strip()==REF
    assert Path(milvus_lite.__file__).resolve().is_relative_to(args.reference.resolve())
    root=outside(args.work_dir);root.mkdir(parents=True,exist_ok=False);cases={}
    for dtype,ids in [(DataType.INT64,[9,-3,4,0,7,2]),(DataType.VARCHAR,['f','a','é','d','b','中'])]:
        for state in ['active','mixed','segment','reopened','compacted']:
            path=root/f'{dtype.name}-{state}.db';m=ServerManager();c=MilvusClient(m.start_and_get_uri(str(path)),dedicated=True)
            def connect():
                return MilvusClient(m.start_and_get_uri(str(path)),dedicated=True)
            try:
                s=c.create_schema(auto_id=False,enable_dynamic_field=False);s.add_field('id',dtype,is_primary=True,**({'max_length':32} if dtype==DataType.VARCHAR else {}));s.add_field('vector',DataType.FLOAT_VECTOR,dim=2);s.add_field('n',DataType.INT64,nullable=True);s.add_field('a',DataType.ARRAY,element_type=DataType.INT64,max_capacity=4,nullable=True)
                c.create_collection('c',schema=s)
                rows=[{'id':pk,'vector':[float(i),0.],'n':None if i%2==0 else i,'a':None if i%2==0 else [i]} for i,pk in enumerate(ids)]
                if state=='compacted':
                    for start,end in [(0,2),(2,3),(3,4),(4,6)]:c.insert('c',rows[start:end]);c.flush('c')
                elif state=='mixed':c.insert('c',rows[:3]);c.flush('c');c.insert('c',rows[3:])
                else:
                    c.insert('c',rows)
                    if state!='active':c.flush('c')
                if state in ('reopened','compacted'):
                    c.close();m.release_all();c=connect();c.load_collection('c')
                    if state=='compacted':assert len(json.loads((path/'collections/c/manifest.json').read_text())['partitions']['_default']['data_files'])==1
                case={};uri=m.start_and_get_uri(str(path))
                with grpc.insecure_channel(uri.removeprefix('http://')) as channel:
                    stub=milvus_pb2_grpc.MilvusServiceStub(channel)
                    def raw(expr,flag,limit=2,offset=1,output=('id','n','a')):
                        req=milvus_pb2.QueryRequest(collection_name='c',expr=expr,output_fields=output)
                        if flag is not None:req.query_params.add(key='iterator',value=flag)
                        req.query_params.add(key='limit',value=str(limit));req.query_params.add(key='offset',value=str(offset))
                        r=stub.Query(req)
                        assert r.status.code == (6 if expr == 'id >' else 0), r.status
                        return {'status':{'code':r.status.code,'error_code':r.status.error_code,'reason':r.status.reason},'fields':[{'name':f.field_name,'type':f.type,'valid_data':list(f.valid_data),'scalar':scalar(f.scalars)} for f in r.fields_data]}
                    in_expr='id in '+json.dumps(ids,ensure_ascii=False)
                    for flag in [None,'true','false','0','garbage']:
                        for expr_name,expr in [('all',''),('pk_in',in_expr),('nullable','n is null')]:case[f'raw/{flag}/{expr_name}']=raw(expr,flag)
                    for limit,offset in [(0,0),(-1,0),(2,-1),(2,0),(99,2)]:case[f'pagination/{limit}/{offset}']=raw('', 'true',limit,offset)
                    case['bad-filter']=raw('id >','true')
                    case['bad-output-and-filter']=raw('id >','false',output=['missing'])
                    case['count']=raw('', 'false',output=['count(*)'])
                    for batch,limit,offset in [(2,-1,0),(3,5,0),(2,3,2)]:
                        def run_iterator():
                            iterator=c.query_iterator('c',batch_size=batch,limit=limit,offset=offset,output_fields=['id','n','a'])
                            pages=[]
                            try:
                                for _ in range(10):
                                    page=list(iterator.next());pages.append(page)
                                    if not page:break
                                else:raise AssertionError('iterator did not finish')
                            finally:iterator.close()
                            return pages
                        case[f'sdk/{batch}/{limit}/{offset}']=attempt(run_iterator)
                    c.release_collection('c');case['released/bad-filter']=raw('id >','true');case['released/count']=raw('', 'true',output=['count(*)'])
                cases[f'{dtype.name}/{state}']=case
            finally:c.close();m.release_all()
    result={'reference':REF,'pymilvus':importlib.metadata.version('pymilvus'),'script_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),'cases':cases}
    if args.verify:
        expected=json.loads(args.verify.read_text())
        assert expected['reference']==REF and expected['cases']==cases, 'query iterator fixture differs'
    (root/'result.json').write_text(json.dumps(result,ensure_ascii=True,indent=2)+'\n');print(json.dumps({'datasets':len(cases),'observations':sum(len(v) for v in cases.values()),'pymilvus':result['pymilvus']}))


if __name__=='__main__':main()
