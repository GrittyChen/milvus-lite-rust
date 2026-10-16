"""0015 B：ARRAY 的两条 SDK、三路线及持久化生命周期，错误逐字对照。"""
import copy
import json
from pathlib import Path

import grpc
import pytest
from pymilvus import MilvusClient, DataType
from pymilvus.grpc_gen import milvus_pb2, milvus_pb2_grpc, schema_pb2
from test_reopen import pair
from test_scenarios import index
from compare import hits

TYPES = [(DataType.BOOL, True, False), (DataType.INT8, 7, 0), (DataType.INT16, 7, 0),
         (DataType.INT32, 7, 0), (DataType.INT64, 7, 0), (DataType.FLOAT, 1.5, 0.0),
         (DataType.DOUBLE, 1.5, 0.0), (DataType.VARCHAR, 'abc', '')]


def schema(dtype):
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=True)
    s.add_field('id', DataType.INT64, is_primary=True)
    s.add_field('vector', DataType.FLOAT_VECTOR, dim=2)
    s.add_field('x', DataType.ARRAY, element_type=dtype, max_capacity=4, max_length=16, nullable=True)
    s.add_field('n', DataType.INT64)
    s.add_field('j', DataType.JSON, nullable=True)
    return s


@pytest.mark.parametrize('dtype,good,zero', TYPES)
@pytest.mark.parametrize('indexed', [False, True])
def test_arrays_routes_output_upsert_delete_reopen(pair, dtype, good, zero, indexed):
    d = pair.open()
    d.step('create', lambda c: c.create_collection('c', schema=schema(dtype), index_params=index('L2')))
    if indexed:
        def add(c):
            p = c.prepare_index_params(); p.add_index('n', index_type='INVERTED'); c.create_index('c', p)
        d.step('index', add)
    d.step('describe', lambda c: c.describe_collection('c'))
    rows = [{'id': i, 'vector': [float(i), 0.], 'x': x, 'n': i, 'j': {'k': 1}, 'd': i}
            for i, x in [(1, None), (2, []), (3, [good, zero]), (4, [good])]]
    d.step('insert', lambda c: c.insert('c', copy.deepcopy(rows)))
    literal = json.dumps(good)
    expressions = ['x is null', 'x is not null', 'array_length(x) == 0', f'array_contains(x,{literal})',
                   f'array_contains_all(x,[{literal}])', 'array_contains_any(x,[])', 'x[99] is null',
                   f'x[0] == {literal}', 'not (x[0] == 1)', 'x is null or d == 2', 'x == 1 or d == 2',
                   'x is null or j["k"] == 1']
    for stage in ('active', 'flushed', 'reopened'):
        if stage == 'flushed': d.step('flush', lambda c: c.flush('c'))
        elif stage == 'reopened':
            pair.close(); d = pair.open(); d.step('load', lambda c: c.load_collection('c'))
        d.step(stage+'_get', lambda c: c.get('c', [1,2,3,4], output_fields=['x']))
        d.step(stage+'_page', lambda c: c.query('c', output_fields=['x'], offset=1, limit=2))
        d.step(stage+'_empty', lambda c: c.query('c', filter='id < 0', output_fields=['x']))
        for expression in expressions:
            result = d.step(stage+expression, lambda c: c.query('c', filter=expression, output_fields=['x']))
            n = len(result[1])
            assert d.step('count', lambda c: c.query('c', filter=expression, output_fields=['count(*)'])) == ('ok', [{'count(*)':n}])
            d.step('search', lambda c: c.search('c', [[0.,0.],[0.,0.]], filter=expression, limit=4,
                   output_fields=['x'], search_params={'metric_type':'L2'}),
                   hits('L2', [[0.,0.],[0.,0.]], {r['id']:r['vector'] for r in rows}, k=n))
        for expr in ['x == 1', 'x == x', 'x in [1]', 'x[-1] is null']:
            before = {str(p):p.read_bytes() for p in Path(pair.rust_path).rglob('*') if p.is_file()}
            d.step('badquery', lambda c: c.query('c', filter=expr, output_fields=['id']), error=True)
            d.step('baddelete', lambda c: c.delete('c', filter=expr), error=True)
            assert before == {str(p):p.read_bytes() for p in Path(pair.rust_path).rglob('*') if p.is_file()}
    d.step('partial_absent', lambda c: c.upsert('c', [{'id':3,'n':10}], partial_update=True))
    d.step('partial_check', lambda c: c.get('c', [3], output_fields=['x','n']))
    d.step('partial_null', lambda c: c.upsert('c', [{'id':3,'x':None}], partial_update=True))
    d.step('full', lambda c: c.upsert('c', [{'id':4,'vector':[4.,0.],'n':4,'x':[]}]))
    d.step('delete', lambda c: c.delete('c', filter='array_length(x) == 0'))
    d.step('remaining', lambda c: c.query('c', output_fields=['*']))


def array_field(rows, slot='long_data', mask=(), declared=DataType.INT64):
    f=schema_pb2.FieldData(type=DataType.ARRAY,field_name='x',valid_data=mask)
    f.scalars.array_data.element_type=declared
    for row in rows:
        item=f.scalars.array_data.data.add()
        if slot: getattr(item,slot).data.extend(row)
    return f


@pytest.mark.parametrize('case', ['compact','full','empty_slot','wrong_declared','cast','overflow','capacity','mask_length','missing_pk','later_pk'])
@pytest.mark.parametrize('method', ['Insert','Upsert'])
def test_raw_array_layout_and_validation_order(pair, case, method):
    d=pair.open();d.step('create',lambda c:c.create_collection('c',schema=schema(DataType.INT8),index_params=index('L2')))
    fields=[schema_pb2.FieldData(type=DataType.INT64,field_name='id'),schema_pb2.FieldData(type=DataType.FLOAT_VECTOR,field_name='vector'),schema_pb2.FieldData(type=DataType.INT64,field_name='n')]
    fields[0].scalars.long_data.data.extend([1,2]);fields[1].vectors.dim=2;fields[1].vectors.float_vector.data.extend([1.,0.,2.,0.]);fields[2].scalars.long_data.data.extend([1,2])
    x=array_field([[7]],mask=[False,True])
    if case=='full': x=array_field([[999],[7]],mask=[False,True])
    if case=='empty_slot': x=array_field([[],[]],slot=None)
    if case=='wrong_declared': x=array_field([[1],[7]],declared=DataType.VARCHAR)
    if case=='cast': x=array_field([[1.5],[-1.5]],slot='double_data')
    if case=='overflow': x=array_field([[128],[1]])
    if case=='capacity': x=array_field([[1]*5,[1]])
    if case=='mask_length': x=array_field([[1]],mask=[True])
    if case in ('missing_pk','later_pk'):
        x=array_field([[128],[1]])
        if case=='missing_pk': fields=fields[1:]
        else: fields[0].valid_data.extend([True,False])
    fields.append(x)
    responses=[]
    for uri in pair.uris:
        with grpc.insecure_channel(uri.removeprefix('http://')) as channel:
            stub=milvus_pb2_grpc.MilvusServiceStub(channel)
            req=getattr(milvus_pb2,method+'Request')(collection_name='c',fields_data=fields,num_rows=2)
            r=getattr(stub,method)(req);responses.append((r.status.code,r.status.error_code,r.status.reason,list(r.IDs.int_id.data)))
    assert responses[0]==responses[1],responses
    failed=case in ('overflow','capacity','mask_length','missing_pk','later_pk')
    assert bool(responses[0][0])==failed,responses
    result=d.step('after',lambda c:c.query('c',output_fields=['x']))
    assert len(result[1])==(0 if failed else 2),result

@pytest.mark.parametrize('element,capacity', [(0,None),(999,None),(DataType.INT8,'bad'),(DataType.INT8,''),(DataType.INT8,'-1'),(DataType.INT8,'0'),(DataType.INT8,'2'),(DataType.VARCHAR,'2')])
def test_raw_array_schema_describe_capacity_and_scalar_index_rejection(pair,element,capacity):
    pair.open();results=[]
    for uri in pair.uris:
        with grpc.insecure_channel(uri.removeprefix('http://')) as ch:
            stub=milvus_pb2_grpc.MilvusServiceStub(ch)
            s=schema_pb2.CollectionSchema(name='c')
            s.fields.add(name='id',data_type=DataType.INT64,is_primary_key=True)
            v=s.fields.add(name='v',data_type=DataType.FLOAT_VECTOR);v.type_params.add(key='dim',value='2')
            x=s.fields.add(name='x',data_type=DataType.ARRAY,element_type=element,nullable=True)
            x.type_params.add(key='max_length',value='ignored')
            if capacity is not None: x.type_params.add(key='max_capacity',value=capacity)
            r=stub.CreateCollection(milvus_pb2.CreateCollectionRequest(collection_name='c',schema=s.SerializeToString()))
            outcome=[(r.code,r.error_code,r.reason)]
            if r.code==0:
                d=stub.DescribeCollection(milvus_pb2.DescribeCollectionRequest(collection_name='c'))
                f=d.schema.fields[-1];outcome.append((f.name,f.data_type,f.element_type,f.nullable,[(p.key,p.value) for p in f.type_params],f.HasField('default_value')))
                idx=stub.CreateIndex(milvus_pb2.CreateIndexRequest(collection_name='c',field_name='x',extra_params=[{'key':'index_type','value':'INVERTED'}]))
                outcome.append((idx.code,idx.error_code,idx.reason))
            results.append(outcome)
    assert results[0]==results[1],results

@pytest.mark.parametrize('dtype,good,zero', TYPES)
def test_raw_array_output_slots_masks_and_empty_results(pair,dtype,good,zero):
    import struct
    from pymilvus.grpc_gen import common_pb2
    d=pair.open();d.step('create',lambda c:c.create_collection('c',schema=schema(dtype),index_params=index('L2')))
    d.step('insert',lambda c:c.insert('c',[{'id':i,'vector':[float(i),0.],'n':i,'x':x} for i,x in [(1,None),(2,[]),(3,[good,zero]),(4,[good])]]))
    for stage in ('active','flushed','reopened'):
        if stage=='flushed':d.step('flush',lambda c:c.flush('c'))
        elif stage=='reopened':
            pair.close();d=pair.open();d.step('load',lambda c:c.load_collection('c'))
        outputs=[]
        for uri in pair.uris:
            with grpc.insecure_channel(uri.removeprefix('http://')) as channel:
                stub=milvus_pb2_grpc.MilvusServiceStub(channel);encoded=[]
                for expr in ('id >= 0','id in [1]','id in [2]','id < 0'):
                    response=stub.Query(milvus_pb2.QueryRequest(collection_name='c',expr=expr,output_fields=['x']))
                    assert response.status.code==0,response.status
                    field=next(f for f in response.fields_data if f.field_name=='x');encoded.append(field.SerializeToString())
                ph=common_pb2.PlaceholderGroup();value=ph.placeholders.add(tag='$0',type=common_pb2.FloatVector);value.values.extend([struct.pack('<ff',0.,0.)]*2)
                request=milvus_pb2.SearchRequest(collection_name='c',nq=2,dsl_type=common_pb2.BoolExprV1,placeholder_group=ph.SerializeToString(),output_fields=['x'])
                for key,value in [('anns_field','vector'),('topk','4'),('metric_type','L2'),('params','{}')]: request.search_params.add(key=key,value=value)
                response=stub.Search(request);assert response.status.code==0,response.status
                field=next(f for f in response.results.fields_data if f.field_name=='x');encoded.append(field.SerializeToString());outputs.append(encoded)
        assert outputs[0]==outputs[1],(dtype,stage,outputs)

@pytest.mark.parametrize('expression,expected', [('n >= 2 and array_length(x)==0',[2,4]),('n in [1,3] and array_contains(x,7)',[3])])
def test_array_predicates_combine_with_an_existing_scalar_inverted_definition(pair,expression,expected):
    d=pair.open();d.step('create',lambda c:c.create_collection('c',schema=schema(DataType.INT64),index_params=index('L2')))
    def add(c):
        p=c.prepare_index_params();p.add_index('n',index_type='INVERTED');c.create_index('c',p)
    d.step('index',add)
    d.step('insert',lambda c:c.insert('c',[{'id':i,'vector':[float(i),0.],'n':i,'x':x} for i,x in [(1,None),(2,[]),(3,[7]),(4,None)]]))
    for stage in ('active','flushed','reopened'):
        if stage=='flushed':d.step('flush',lambda c:c.flush('c'))
        elif stage=='reopened':pair.close();d=pair.open();d.step('load',lambda c:c.load_collection('c'))
        assert d.step(stage,lambda c:c.query('c',filter=expression,output_fields=['id']))==('ok',[{'id':i} for i in expected])
        assert d.step('count',lambda c:c.query('c',filter=expression,output_fields=['count(*)']))==('ok',[{'count(*)':len(expected)}])
    d.step('delete',lambda c:c.delete('c',filter=expression));d.step('remaining',lambda c:c.query('c',output_fields=['x','n']))
