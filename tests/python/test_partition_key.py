"""0017 B 已登记差异：B1 编号、B2 全桶预检、B3 旧行保护。"""
import json

import pytest
from pymilvus.grpc_gen import milvus_pb2 as rpc, schema_pb2 as pb

from test_grpc_translation import env, status_of
from test_partition_safety import tree


def create(stub, *, auto=True, array=True, nullable=True):
    s=pb.CollectionSchema(name='c',enable_dynamic_field=True)
    s.fields.add(name='id',data_type=5,is_primary_key=True,autoID=auto)
    s.fields.add(name='k',data_type=5,is_partition_key=True,nullable=nullable)
    f=s.fields.add(name='vector',data_type=101);f.type_params.add(key='dim',value='2')
    s.fields.add(name='j',data_type=23,nullable=True)
    if array:
        f=s.fields.add(name='a',data_type=22,element_type=2);f.type_params.add(key='max_capacity',value='8')
    result=stub.CreateCollection(rpc.CreateCollectionRequest(collection_name='c',schema=s.SerializeToString()))
    assert status_of(result)==(0,0,'')


def fields(keys=(0,1,0),ids=None,arrays=None):
    n=len(keys)
    out=[pb.FieldData(field_name='k',type=5,scalars=pb.ScalarField(long_data=pb.LongArray(data=keys))),
         pb.FieldData(field_name='vector',type=101,vectors=pb.VectorField(dim=2,float_vector=pb.FloatArray(data=[1.,0.]*n))),
         pb.FieldData(field_name='j',type=23,scalars=pb.ScalarField(json_data=pb.JSONArray(data=[b'{"x":1}']*n))),
         pb.FieldData(field_name='a',type=22,scalars=pb.ScalarField(array_data=pb.ArrayArray(element_type=2,data=[pb.ScalarField(long_data=pb.LongArray(data=a)) for a in (arrays or [[1]]*n)])))]
    if ids is not None:
        concrete=[i for i in ids if i is not None]
        f=pb.FieldData(field_name='id',type=5,scalars=pb.ScalarField(long_data=pb.LongArray(data=concrete)))
        if len(concrete)!=n:f.valid_data.extend(i is not None for i in ids)
        out.append(f)
    return out


def mutation(stub,method,fd,part='../ignored'):
    return getattr(stub,method)(getattr(rpc,method+'Request')(collection_name='c',num_rows=len(fd[0].scalars.long_data.data),fields_data=fd,partition_name=part))


def assert_empty_error(result):
    assert not result.IDs.int_id.data and not result.IDs.str_id.data
    assert not result.succ_index and not result.err_index
    assert not result.insert_cnt and not result.upsert_cnt and not result.delete_cnt


@pytest.mark.parametrize('method',['Insert','Upsert'])
@pytest.mark.parametrize('ids',[None,[None,99,None],[90,None,None]])
def test_b1_generated_ids_are_bucket_seq_while_explicit_ids_keep_identity(env,method,ids):
    client,stub,_=env;create(stub)
    result=mutation(stub,method,fields(ids=ids));assert status_of(result)==(0,0,'')
    expected=[1,3,2] if ids is None else ([1,99,2] if ids[0] is None else [90,3,2])
    assert list(result.IDs.int_id.data)==expected
    assert list(result.succ_index)==[0,1,2]
    assert list(client.query('c',filter='',partition_names=['_pk_10'],output_fields=['id']))==[{'id':expected[0]},{'id':expected[2]}]
    assert list(client.query('c',filter='',partition_names=['_pk_11'],output_fields=['id']))==[{'id':expected[1]}]
    assert list(client.query('c',filter='',partition_names=['_default'],output_fields=['id']))==[]


@pytest.mark.parametrize('method',['Insert','Upsert'])
@pytest.mark.parametrize('keys',[(0,1,0),(1,0,0),(0,0,0)])
def test_b2_array_conversion_failure_has_no_request_wal_rows_or_sequence(env,tmp_path,method,keys):
    client,stub,_=env;create(stub);before=tree(tmp_path/'t.db')
    result=mutation(stub,method,fields(keys=keys,arrays=[[1],[128],[1]]))
    assert status_of(result)==(1,0,'Value 128 too large to fit in C integer type')
    assert_empty_error(result);assert tree(tmp_path/'t.db')==before
    assert client.query('c',filter='',output_fields=['count(*)'])==[{'count(*)':0}]
    result=mutation(stub,method,fields(keys=(0,)))
    assert status_of(result)==(0,0,'') and list(result.IDs.int_id.data)==[1]


@pytest.mark.parametrize('method',['Insert','Upsert'])
@pytest.mark.parametrize('released',[False,True])
@pytest.mark.parametrize('mixed',[False,True])
def test_b3_auto_collision_protects_global_old_row_and_default_error_payload(env,tmp_path,method,released,mixed):
    client,stub,_=env;create(stub)
    seed=mutation(stub,'Insert',fields(keys=(1,1),ids=[3,4]));assert status_of(seed)==(0,0,'')
    if released:client.release_collection('c')
    before=tree(tmp_path/'t.db')
    result=mutation(stub,method,fields(keys=(0,0),ids=[3,None] if mixed else None))
    collision=4 if mixed else 3
    assert status_of(result)==(1,0,f'storage error: auto-generated primary key {collision} would overwrite an existing row')
    assert_empty_error(result);assert tree(tmp_path/'t.db')==before
    if released:client.load_collection('c')
    assert client.get('c',ids=[3,4],output_fields=['id','k'])==[{'id':3,'k':1},{'id':4,'k':1}]
    # 显式重复覆盖仍合法，不可按最终数值是否等于 seq 猜测 auto 身份。
    result=mutation(stub,method,fields(keys=(0,1),ids=[3,3]));assert status_of(result)==(0,0,'')
    assert list(result.IDs.int_id.data)==[3,3]


@pytest.mark.parametrize('method',['Insert','Upsert'])
@pytest.mark.parametrize('bad',['typed','meta','duplicate','masked'])
def test_key_json_error_priority_and_masked_or_duplicate_evidence(env,method,bad):
    client,stub,_=env;create(stub,auto=False)
    fd=fields(keys=(0,),ids=[1]);j=fd[2]
    if bad=='meta':fd.append(pb.FieldData(field_name='$meta',type=23,scalars=pb.ScalarField(json_data=pb.JSONArray(data=[b'{']))))
    else:
        j.scalars.json_data.data[:]=[b'{']
        if bad=='duplicate':fd.append(pb.FieldData(field_name='j',type=23,scalars=pb.ScalarField(json_data=pb.JSONArray(data=[b'{}']))))
        if bad=='masked':j.valid_data.append(False)
    # 主键缺失仍抢在坏 JSON 前；错误请求无任何行。
    without_pk=[f for f in fd if f.field_name!='id']
    result=mutation(stub,method,without_pk)
    assert status_of(result)==(6,6,"primary key 'id' missing or None")
    result=mutation(stub,method,fd)
    assert result.status.code==6 and 'value is not valid JSON' in result.status.reason
    assert client.query('c',filter='',output_fields=['count(*)'])==[{'count(*)':0}]


@pytest.mark.parametrize('num',[-1,0,1,2,32])
def test_raw_num_partitions_is_ignored_and_schema_flag_described(env,num):
    client,stub,_=env
    s=pb.CollectionSchema();s.fields.add(name='id',data_type=5,is_primary_key=True)
    s.fields.add(name='k',data_type=5,is_partition_key=True,nullable=True)
    f=s.fields.add(name='v',data_type=101);f.type_params.add(key='dim',value='2')
    result=stub.CreateCollection(rpc.CreateCollectionRequest(collection_name='c',num_partitions=num,schema=s.SerializeToString()))
    assert status_of(result)==(0,0,'')
    described=stub.DescribeCollection(rpc.DescribeCollectionRequest(collection_name='c'))
    assert described.num_partitions==17
    assert [f.name for f in described.schema.fields if f.is_partition_key]==['k']
    parts=stub.ShowPartitions(rpc.ShowPartitionsRequest(collection_name='c'))
    assert list(parts.partition_names)==sorted(['_default']+['_pk_'+str(i) for i in range(16)])


@pytest.mark.parametrize('method',['Insert','Upsert'])
def test_key_explicit_duplicate_winner_uses_bucket_order_instead_of_input_last_row(env,method):
    client,stub,_=env;create(stub,auto=False)
    fd=fields(ids=[7,7,7]);fd.append(pb.FieldData(field_name='$meta',type=23,scalars=pb.ScalarField(json_data=pb.JSONArray(data=[b'{"tag":"A1"}',b'{"tag":"B"}',b'{"tag":"A2"}']))))
    result=mutation(stub,method,fd)
    assert status_of(result)==(0,0,'') and list(result.IDs.int_id.data)==[7,7,7]
    assert list(client.get('c',ids=[7],output_fields=['k','tag']))==[{'id':7,'k':1,'tag':'B'}]
    client.flush('c')
    assert list(client.get('c',ids=[7],output_fields=['k','tag']))==[{'id':7,'k':1,'tag':'B'}]


@pytest.mark.parametrize('method',['Insert','Upsert'])
def test_b3_collision_cannot_mask_earlier_conversion_or_business_errors(env,tmp_path,method):
    client,stub,_=env;create(stub)
    assert status_of(mutation(stub,'Insert',fields(keys=(1,1),ids=[3,4])))==(0,0,'')
    before=tree(tmp_path/'t.db')
    result=mutation(stub,method,fields(keys=(0,1),arrays=[[1],[128]]))
    assert status_of(result)==(1,0,'Value 128 too large to fit in C integer type')
    assert tree(tmp_path/'t.db')==before
    fd=fields(keys=(0,1));fd[2].scalars.json_data.data[:]=[b'{',b'{}']
    result=mutation(stub,method,fd)
    assert result.status.code==6 and 'value is not valid JSON' in result.status.reason
    assert tree(tmp_path/'t.db')==before
    assert list(client.get('c',ids=[3,4],output_fields=['id']))==[{'id':3},{'id':4}]
