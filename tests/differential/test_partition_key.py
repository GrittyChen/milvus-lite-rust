"""0017 B：key 路由/请求前 Upsert 基线；B1/B2/B3 另以具名测试冻结。"""
import hashlib
import json

import grpc
import pytest
from pymilvus import DataType, MilvusClient
from pymilvus.grpc_gen import milvus_pb2 as rpc, milvus_pb2_grpc, schema_pb2 as pb

from test_reopen import pair
from test_scenarios import index


def schema(varchar_pk=False, varchar_key=False, auto=False, default=None):
    s=MilvusClient.create_schema(enable_dynamic_field=True)
    s.add_field('id',DataType.VARCHAR if varchar_pk else DataType.INT64,is_primary=True,auto_id=auto,
                **({'max_length':64} if varchar_pk else {}))
    s.add_field('k',DataType.VARCHAR if varchar_key else DataType.INT64,is_partition_key=True,nullable=True,
                **({'max_length':64} if varchar_key else {}),**({'default_value':default} if default is not None else {}))
    s.add_field('vector',DataType.FLOAT_VECTOR,dim=2)
    s.add_field('n',DataType.INT64,nullable=True)
    s.add_field('a',DataType.ARRAY,element_type=DataType.INT8,max_capacity=8)
    s.add_field('j',DataType.JSON,nullable=True)
    return s


def bucket(value):
    return '_pk_'+str(int.from_bytes(hashlib.md5(str(value).encode()).digest(),'big')%16)


@pytest.mark.parametrize('varchar_pk',[False,True])
@pytest.mark.parametrize('varchar_key',[False,True])
@pytest.mark.parametrize('state',['active','segment','mixed','reopen','compacted'])
def test_partition_key_full_read_chain_and_upsert_request_base(pair,varchar_pk,varchar_key,state):
    d=pair.open();d.step('create',lambda c:c.create_collection('c',schema=schema(varchar_pk,varchar_key),index_params=index('L2')))
    pk=lambda i:f'键{i}' if varchar_pk else i
    key=lambda i:str(i) if varchar_key else i
    def rows(start,end):
        return [{'id':pk(i),'k':None if i==2 else key(i%3),'vector':[float(i),0.],
                 'n':None if i%2 else i,'a':[i],'j':{'n':i},'x':0,'y':0} for i in range(start,end)]
    d.step('insert',lambda c:c.insert('c',rows(0,4),partition_name='../ignored'))
    if state!='active':d.step('flush',lambda c:c.flush('c'))
    if state in ['mixed','reopen','compacted']:d.step('tail',lambda c:c.insert('c',rows(4,7)))
    if state=='compacted':
        for i in range(8,14):
            d.step('generation',lambda c:c.insert('c',rows(i,i+1)))
            d.step('flush-generation',lambda c:c.flush('c'))
    if state in ['reopen','compacted']:
        pair.close();d=pair.open();d.step('load',lambda c:c.load_collection('c'))
    # 不同动态键集的真实 SDK 请求；后一行不得继承前一行尚未发布的 x。
    d.step('partial-duplicate',lambda c:c.upsert('c',[{'id':pk(1),'x':1},{'id':pk(1),'y':2}],partial_update=True))
    result=d.step('get-base',lambda c:c.get('c',ids=[pk(1)],output_fields=['id','k','x','y']))
    assert result==('ok',[{'id':pk(1),'k':key(1),'x':0,'y':2}])
    d.step('change-key-duplicate',lambda c:c.upsert('c',[{'id':pk(0),'k':key(1)},{'id':pk(0),'k':key(0),'y':5}],partial_update=True))
    for parts in [[],['_default'],[bucket(key(0))],[bucket(None)],[bucket(key(1)),bucket(key(0))],['unknown']]:
        for expr in ['', 'n is null','not (n == 2)','array_contains(a, 1)','j["n"] >= 1','x == 0','k is null',
                     'id in '+json.dumps([pk(1),pk(0),pk(1),pk(99)],ensure_ascii=False)]:
            d.step('query',lambda c:c.query('c',filter=expr,partition_names=parts,output_fields=['id','k','n','a','j','x','y']))
        d.step('count',lambda c:c.query('c',filter='n is null',partition_names=parts,output_fields=['count(*)']))
        d.step('get',lambda c:c.get('c',ids=[pk(1),pk(0),pk(1),pk(99)],partition_names=parts,output_fields=['id','k']))
        def pages(c):
            it=c.query_iterator('c',batch_size=2,partition_names=parts,output_fields=['id','k'])
            try:
                result=[]
                for _ in range(20):
                    page=list(it.next());result.append(page)
                    if not page:return result
                raise AssertionError('iterator did not end')
            finally:it.close()
        d.step('iterator',pages)
        for params in [{},{'radius':100.,'range_filter':0.}]:
            d.step('search',lambda c:c.search('c',[[1.,0.],[2.,0.]],limit=8,partition_names=parts,
                     search_params={'metric_type':'L2','params':params},output_fields=['k','a','n']))
    def search_pages(c):
        it=c.search_iterator('c',data=[[1.,0.]],batch_size=2,limit=8,output_fields=['k','a'],
                             search_params={'metric_type':'L2','params':{'radius':100.}})
        try:
            pages=[]
            for _ in range(20):
                page=list(it.next());pages.append([{'id':h['id'],'distance':h['distance'],'entity':dict(h['entity'])} for h in page])
                if not page:return pages
            raise AssertionError('search iterator did not end')
        finally:it.close()
    d.step('search-iterator-v1',search_pages)
    d.step('delete',lambda c:c.delete('c',ids=[pk(1)],partition_name=bucket(key(0))))
    d.step('flush-delete',lambda c:c.flush('c'))
    d.step('deleted',lambda c:c.get('c',ids=[pk(1)],output_fields=['id']))
    # 标量索引/范围与 A 相同；手动分区拒绝必须在 unknown/default 分派之前。
    for part in ['unknown','_default',bucket(key(0))]:
        d.step('create-rejected',lambda c:c.create_partition('c',part),error=True)
        d.step('drop-rejected',lambda c:c.drop_partition('c',part),error=True)


@pytest.mark.parametrize('varchar',[False,True])
@pytest.mark.parametrize('use_default',[False,True])
def test_partition_key_nullable_default_and_hash_edges(pair,varchar,use_default):
    d=pair.open();default='' if varchar else 0
    d.step('create',lambda c:c.create_collection('c',schema=schema(varchar_key=varchar,default=default if use_default else None),index_params=index('L2')))
    keys=['', 'None','é','e\u0301','普通','0','1'] if varchar else [-(2**63),2**63-1,0,1,-1]
    rows=[{'id':i,'k':k,'vector':[float(i),0.],'a':[],'j':None} for i,k in enumerate(keys)]
    rows.extend([{'id':100,'k':None,'vector':[0.,0.],'a':[],'j':None},{'id':101,'vector':[0.,0.],'a':[],'j':None}])
    d.step('insert',lambda c:c.insert('c',rows))
    for part in ['_default']+['_pk_'+str(i) for i in range(16)]:
        d.step('bucket',lambda c:c.query('c',filter='',partition_names=[part],output_fields=['id','k']))
    d.step('flush',lambda c:c.flush('c'));pair.close();d=pair.open();d.step('load',lambda c:c.load_collection('c'))
    d.step('all',lambda c:c.query('c',filter='',output_fields=['id','k']))


@pytest.mark.parametrize('change',['two','primary','float','no-vector-and-two','no-pk-and-type'])
def test_partition_key_schema_error_priority_raw(pair,change):
    pair.open();outputs=[]
    for uri in pair.uris:
        with grpc.insecure_channel(uri.removeprefix('http://')) as channel:
            stub=milvus_pb2_grpc.MilvusServiceStub(channel)
            s=pb.CollectionSchema(name='c');s.fields.add(name='id',data_type=5,is_primary_key=True)
            s.fields.add(name='k',data_type=5,is_partition_key=True)
            f=s.fields.add(name='v',data_type=101);f.type_params.add(key='dim',value='2')
            if change in ['two','no-vector-and-two']:s.fields[0].is_partition_key=True
            if change=='primary':s.fields[0].is_partition_key=True;s.fields[1].is_partition_key=False
            if change in ['float','no-pk-and-type']:s.fields[1].data_type=10
            if change=='no-vector-and-two':del s.fields[2]
            if change=='no-pk-and-type':s.fields[0].is_primary_key=False
            r=stub.CreateCollection(rpc.CreateCollectionRequest(collection_name='c',schema=s.SerializeToString()))
            outputs.append((r.code,r.error_code,r.reason))
    assert outputs[0]==outputs[1]
    assert outputs[0][0]==6


def auto_rows(bad=False):
    return [{'k':k,'vector':[1.,0.],'a':([128] if bad and i==1 else [i+1]),'j':{'tag':i}}
            for i,k in enumerate([0,1,0])]


def test_named_b1_auto_ids_differ_only_by_approved_bucket_sequence(pair):
    from compare import exact
    d=pair.open();d.step('create',lambda c:c.create_collection('c',schema=schema(auto=True),index_params=index('L2')))
    def compare(a,b):
        if a[0]!='ok' or b[0]!='ok':return 'expected successful auto-ID insertion'
        if a[1]['ids']!=[1,3,2] or b[1]['ids']!=[1,2,3]:return 'auto IDs do not follow the frozen B1/3.x contracts'
        return exact(('ok',{k:v for k,v in a[1].items() if k!='ids'}),('ok',{k:v for k,v in b[1].items() if k!='ids'}))
    d.step('B1-insert',lambda c:c.insert('c',auto_rows()),compare)
    for client,expected in [(d.rust,[1,2]),(d.ref,[1,3])]:
        actual=list(client.query('c',filter='',partition_names=['_pk_10'],output_fields=['id','k','a','j']))
        assert actual==[{'id':expected[0],'k':0,'a':[1],'j':{'tag':0}}, {'id':expected[1],'k':0,'a':[3],'j':{'tag':2}}]
    d.step('count',lambda c:c.query('c',filter='',output_fields=['count(*)']))


def test_named_b2_later_array_conversion_failure_rejects_all_rust_rows_but_3x_retains_prefix(pair):
    d=pair.open();d.step('create',lambda c:c.create_collection('c',schema=schema(auto=True),index_params=index('L2')))
    d.step('same-error',lambda c:c.insert('c',auto_rows(bad=True)),error=True)
    assert list(d.rust.query('c',filter='',output_fields=['id','a']))==[]
    assert list(d.ref.query('c',filter='',output_fields=['id','a']))==[{'id':1,'a':[1]},{'id':3,'a':[3]}]
    # B2 不消耗编号；参照已在失败前为三行分配 auto ID。
    tail=[{'k':1,'vector':[1.,0.],'a':[],'j':None}]
    assert list(d.rust.insert('c',tail)['ids'])==[1]
    assert list(d.ref.insert('c',tail)['ids'])==[4]
    assert list(d.rust.get('c',ids=[1],output_fields=['a']))==[{'id':1,'a':[]}]
    assert list(d.ref.get('c',ids=[1,3,4],output_fields=['a']))==[{'id':1,'a':[1]},{'id':3,'a':[3]},{'id':4,'a':[]}]


def test_named_b3_imported_3x_bad_prefix_rejects_auto_overwrite_while_reference_reuses_id(pair):
    import shutil
    from pathlib import Path
    from compare import run
    d=pair.open();d.step('create',lambda c:c.create_collection('c',schema=schema(auto=True),index_params=index('L2')))
    result=run(lambda c:c.insert('c',auto_rows(bad=True)),d.ref)
    assert result==('err',1,0,'Value 128 too large to fit in C integer type')
    pair.close()
    # 两个服务已退出、各自 LOCK 已释放；只复制本用例拥有的 collection。
    rust=Path(pair.rust_path)/'collections/c';reference=Path(pair.ref_path)/'collections/c'
    rust.rename(pair.root/'empty-rust-collection.saved');shutil.copytree(reference,rust)
    d=pair.open();d.step('load',lambda c:c.load_collection('c'))
    d.step('same-imported-prefix',lambda c:c.get('c',ids=[1,3],output_fields=['id','a']))
    tail=[{'k':1,'vector':[1.,0.],'a':[9],'j':None}]
    assert run(lambda c:c.insert('c',tail),d.rust)==('err',1,0,'storage error: auto-generated primary key 3 would overwrite an existing row')
    response=run(lambda c:c.insert('c',tail),d.ref)
    assert response[0]=='ok' and response[1]['ids']==[3]
    assert list(d.rust.get('c',ids=[1,3],output_fields=['a']))==[{'id':1,'a':[1]},{'id':3,'a':[3]}]
    assert list(d.ref.get('c',ids=[1,3],output_fields=['a']))==[{'id':1,'a':[1]},{'id':3,'a':[9]}]


@pytest.mark.parametrize('default',[2**63,-(2**63)-1,2**128,-(2**128)])
def test_legacy_out_of_range_key_default_fails_business_validation_before_routing(pair,default):
    from pathlib import Path
    d=pair.open();d.step('create',lambda c:c.create_collection('c',schema=schema(default=0)))
    pair.close()
    for path in [pair.rust_path,pair.ref_path]:
        file=Path(path)/'collections/c/schema.json';value=json.loads(file.read_text())
        next(f for f in value['fields'] if f['name']=='k')['default_value']=default
        file.write_text(json.dumps(value))
    pair.open();responses=[]
    for uri in pair.uris:
        with grpc.insecure_channel(uri.removeprefix('http://')) as channel:
            stub=milvus_pb2_grpc.MilvusServiceStub(channel)
            fd=[pb.FieldData(field_name='vector',type=101,vectors=pb.VectorField(dim=2,float_vector=pb.FloatArray(data=[1.,0.]))),
                pb.FieldData(field_name='a',type=22,scalars=pb.ScalarField(array_data=pb.ArrayArray(element_type=2,data=[pb.ScalarField(long_data=pb.LongArray())])))]
            reasons=[]
            for primary in [False,True]:
                fields=list(fd)
                if primary:fields.append(pb.FieldData(field_name='id',type=5,scalars=pb.ScalarField(long_data=pb.LongArray(data=[1]))))
                r=stub.Insert(rpc.InsertRequest(collection_name='c',num_rows=1,fields_data=fields))
                reasons.append((r.status.code,r.status.error_code,r.status.reason))
                assert not r.IDs.int_id.data and not r.succ_index and not r.err_index
            count=stub.Query(rpc.QueryRequest(collection_name='c',output_fields=['count(*)']))
            assert list(count.fields_data[0].scalars.long_data.data)==[0]
            responses.append(reasons)
    assert responses[0]==responses[1]==[(6,6,"primary key 'id' missing or None"),
        (6,6,f"field 'k' value {default} does not match dtype DataType.INT64")]
