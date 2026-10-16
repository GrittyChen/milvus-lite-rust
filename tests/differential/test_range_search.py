"""0016 B：范围阈值、有限窗口、三路线输出与真实 SDK V1 的逐项差分。"""
import importlib.util
import json
import math
from pathlib import Path
import struct

import grpc
import pytest
from pymilvus import DataType
from pymilvus.grpc_gen import common_pb2, milvus_pb2_grpc
from compare import hits, tolerance
from test_reopen import pair
from test_scenarios import index

_path = Path(__file__).resolve().parents[1]/'compatibility_audit/range_cases.py'
_spec = importlib.util.spec_from_file_location('range_cases', _path)
cases = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(cases)


def iterator_pages(client, metric, batch, limit):
    iterator = client.search_iterator('c', data=[[0.,0.]] if metric=='L2' else [[1.,0.]],
        batch_size=batch, limit=limit, output_fields=['n','a'], search_params={'metric_type':metric})
    pages = []
    try:
        for _ in range(100):
            page = list(iterator.next())
            pages.append([{'id':h['id'],'distance':h['distance'],'entity':dict(h['entity'])} for h in page])
            if not page: return pages
        raise AssertionError('iterator did not finish')
    finally: iterator.close()


@pytest.mark.parametrize('metric',['L2','IP','COSINE'])
@pytest.mark.parametrize('state',['active','mixed','segment','reopened','compacted'])
def test_range_matrix_float64_window_output_and_sdk(pair,metric,state):
    d=pair.open()
    def create(c):
        s=c.create_schema(auto_id=False,enable_dynamic_field=True)
        s.add_field('id',DataType.INT64,is_primary=True)
        s.add_field('vector',DataType.FLOAT_VECTOR,dim=2)
        s.add_field('n',DataType.INT64,nullable=True)
        s.add_field('a',DataType.ARRAY,element_type=DataType.INT64,max_capacity=4,nullable=True)
        c.create_collection('c',schema=s,index_params=index(metric))
    d.step('create',create)
    rows=[{'id':i,'vector':([.5,1.] if i==2 else [float(i),0.]) if metric=='L2' else [float(i),1.],
           'n':None if i%2==0 else i,'a':None if i%2==0 else [i],'d':i} for i in range(1,33)]
    spans=[(i,i+8) for i in range(0,32,8)] if state=='compacted' else [(0,16),(16,32)]
    for start,end in spans:
        d.step('insert',lambda c:c.insert('c',rows[start:end]))
        if state=='compacted' or state=='mixed' and start==0:d.step('flush',lambda c:c.flush('c'))
    if state in ('segment','reopened'):d.step('flush',lambda c:c.flush('c'))
    if state in ('reopened','compacted'):
        pair.close();d=pair.open();d.step('load',lambda c:c.load_collection('c'))
        if state=='compacted':
            for root in (pair.rust_path,pair.ref_path):
                m=json.loads((Path(root)/'collections/c/manifest.json').read_text())
                assert len(m['partitions']['_default']['data_files'])==1
    vectors={r['id']:r['vector'] for r in rows}
    for label,request in cases.matrix(metric):
        outputs=[]
        for uri in pair.uris:
            with grpc.insecure_channel(uri.removeprefix('http://')) as channel:
                outputs.append(cases.outcome(milvus_pb2_grpc.MilvusServiceStub(channel).Search(request)))
        a,b=outputs
        assert bool(b['code'])==label.startswith('error/'),(label,b)
        assert {k:v for k,v in a.items() if k!='scores'}=={k:v for k,v in b.items() if k!='scores'},(metric,state,label,outputs)
        ph=common_pb2.PlaceholderGroup.FromString(request.placeholder_group)
        queries=[struct.unpack('<'+'f'*(len(v)//4),v) for v in ph.placeholders[0].values]
        cursor=0
        for q,k in zip(queries,b['topks']):
            for pk,actual,expected in zip(b['ids'][cursor:cursor+k],a['scores'][cursor:cursor+k],b['scores'][cursor:cursor+k]):
                assert math.isfinite(actual) and abs(actual-expected)<=tolerance(metric,expected,q,vectors[pk]),(label,pk,actual,expected)
            cursor+=k
    for batch,limit in [(1,-1),(7,-1),(7,19)]:
        expected=2 if metric=='L2' and batch==1 else (32 if limit<0 else limit)
        def check(a,b):
            if a[0]!='ok' or b[0]!='ok':return 'iterator failed'
            if [[h['id'] for h in p] for p in a[1]] != [[h['id'] for h in p] for p in b[1]]:return 'page IDs differ'
            return hits(metric,[[0.,0.]] if metric=='L2' else [[1.,0.]],vectors,k=expected)(
                ('ok',[[h for p in a[1] for h in p]]),('ok',[[h for p in b[1] for h in p]]))
        result=d.step('iterator',lambda c:iterator_pages(c,metric,batch,limit),check)
        assert sum(map(len,result[1]))==expected
    d.step('release',lambda c:c.release_collection('c'))
    released=[]
    for uri in pair.uris:
        with grpc.insecure_channel(uri.removeprefix('http://')) as channel:
            stub=milvus_pb2_grpc.MilvusServiceStub(channel)
            released.append([cases.outcome(stub.Search(cases.request(metric,extra,expr=expr)))
                for extra,expr in [([('radius','10')],''),([('radius','"bad"')],''),([('radius','10')],'id >')]])
    assert released[0]==released[1] and [r['code'] for r in released[0]]==[101,1,101]


@pytest.mark.parametrize('metric',['L2','IP','COSINE'])
@pytest.mark.parametrize('varchar',[False,True])
@pytest.mark.parametrize('empty',[False,True])
def test_search_iterator_empty_or_tied_scores_with_varchar_keys(pair,metric,varchar,empty):
    d=pair.open()
    def create(c):
        s=c.create_schema(auto_id=False,enable_dynamic_field=False)
        s.add_field('id',DataType.VARCHAR if varchar else DataType.INT64,is_primary=True,
                    **({'max_length':32} if varchar else {}))
        s.add_field('vector',DataType.FLOAT_VECTOR,dim=2)
        s.add_field('n',DataType.INT64,nullable=True)
        s.add_field('a',DataType.ARRAY,element_type=DataType.INT64,max_capacity=4,nullable=True)
        c.create_collection('c',schema=s,index_params=index(metric))
    d.step('create',create)
    ids=[f'键-{i}' if varchar else i for i in range(20)]
    if not empty:
        d.step('insert',lambda c:c.insert('c',[{'id':pk,'vector':[1.,0.],'n':None,'a':None} for pk in ids]))
    else:
        outputs=[]
        for uri in pair.uris:
            with grpc.insecure_channel(uri.removeprefix('http://')) as channel:
                r=milvus_pb2_grpc.MilvusServiceStub(channel).Search(cases.request(metric,[('radius','10')]))
                assert r.status.code==0 and list(r.results.topks)==[0,0]
                outputs.append(r.SerializeToString())
        assert outputs[0]==outputs[1]
    def compare(a,b):
        if a[0]!='ok' or b[0]!='ok':return 'iterator failed'
        return hits(metric,[[0.,0.]] if metric=='L2' else [[1.,0.]],{pk:[1.,0.] for pk in ids},k=0 if empty else 20)(
            ('ok',[[h for p in a[1] for h in p]]),('ok',[[h for p in b[1] for h in p]]))
    result=d.step('tied-or-empty',lambda c:iterator_pages(c,metric,7,-1),compare)
    assert {h['id'] for p in result[1] for h in p}==(set() if empty else set(ids))


def test_range_boundary_membership_stays_within_frozen_distance_budget(pair):
    # 0016 B 经负责人确认：仅阈值误差带内允许成员差异，不能放宽带外成员或改变严格比较符。
    fixture=json.loads((_path.parent/'fixtures/range_boundary_v321.json').read_text())
    q=fixture['q'];vectors=fixture['vectors'];d=pair.open()
    def create(c):
        s=c.create_schema(auto_id=False,enable_dynamic_field=False)
        s.add_field('id',DataType.INT64,is_primary=True)
        s.add_field('vector',DataType.FLOAT_VECTOR,dim=128)
        c.create_collection('c',schema=s)
    d.step('create',create)
    d.step('insert',lambda c:c.insert('c',[{'id':i,'vector':v} for i,v in enumerate(vectors)]))
    def search(uri,extra=()):
        req=cases.request('L2',extra,queries=[q],topk=32)
        req.ClearField('output_fields')
        with grpc.insecure_channel(uri.removeprefix('http://')) as channel:
            result=milvus_pb2_grpc.MilvusServiceStub(channel).Search(req)
            assert result.status.code==0,result.status
            return dict(zip(result.results.ids.int_id.data,result.results.scores))
    ordinary=[search(uri) for uri in pair.uris]
    assert set(ordinary[0])==set(ordinary[1])==set(range(32))
    def public(score):
        # 对本 fixture 的正常有限值，平方结果舍回 float32 后再开方可恢复内部 float32 范数。
        norm=struct.unpack('<f',struct.pack('<f',math.sqrt(score)))[0]
        return float(norm)**2
    scores=[{pk:public(v) for pk,v in result.items()} for result in ordinary]
    budgets={pk:tolerance('L2',scores[1][pk],q,vectors[pk]) for pk in range(32)}
    for pk in range(32):assert abs(scores[0][pk]-scores[1][pk])<=budgets[pk]
    for key,accept in [('radius',lambda value,bound:value<bound),('range_filter',lambda value,bound:value>=bound)]:
        for bound in [fixture['radius'],fixture['radius']-.01,fixture['radius']+.01]:
            results=[search(uri,[(key,repr(bound))]) for uri in pair.uris]
            for side in range(2):
                assert set(results[side])=={pk for pk,value in scores[side].items() if accept(value,bound)}
            for pk in set(results[0])^set(results[1]):
                assert abs(scores[1][pk]-bound)<=budgets[pk],(key,bound,pk,scores,budgets[pk])


def test_v1_iterator_observes_changes_between_requests(pair):
    d=pair.open()
    def create(c):
        s=c.create_schema(auto_id=False,enable_dynamic_field=False)
        s.add_field('id',DataType.INT64,is_primary=True)
        s.add_field('vector',DataType.FLOAT_VECTOR,dim=2)
        s.add_field('n',DataType.INT64,nullable=True)
        s.add_field('a',DataType.ARRAY,element_type=DataType.INT64,max_capacity=4,nullable=True)
        c.create_collection('c',schema=s,index_params=index('L2'))
    d.step('create',create)
    row=lambda i:{'id':i,'vector':[i/100,0.],'n':None,'a':None}
    d.step('insert',lambda c:c.insert('c',[row(i) for i in range(20)]))
    def run(c):
        iterator=c.search_iterator('c',data=[[0.,0.]],batch_size=5,output_fields=['n','a'],search_params={'metric_type':'L2'})
        try:
            pages=[list(iterator.next())]
            assert [h['id'] for h in pages[0]]==list(range(5))
            c.delete('c',ids=[10])
            assert list(c.get('c',[10],output_fields=['id']))==[]
            assert list(c.insert('c',[row(25)])['ids'])==[25]
            c.flush('c')
            for _ in range(20):
                page=list(iterator.next());pages.append(page)
                if not page:return [[{'id':h['id'],'distance':h['distance'],'entity':dict(h['entity'])} for h in page] for page in pages]
            raise AssertionError('iterator did not finish')
        finally:iterator.close()
    def compare(a,b):
        if a[0]!='ok' or b[0]!='ok':return 'iterator failed'
        return hits('L2',[[0.,0.]],{i:[i/100,0.] for i in [*range(20),25]},k=20)(
            ('ok',[[h for p in a[1] for h in p]]),('ok',[[h for p in b[1] for h in p]]))
    result=d.step('mutation-between-pages',run,compare)
    assert {h['id'] for p in result[1] for h in p}==(set(range(20))-{10})|{25}
