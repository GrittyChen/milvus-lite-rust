"""顶层动态字段的 RPC、数据源与生命周期语义，以 3.x 43d1257 为真值。"""

import pytest
from pymilvus import DataType,MilvusClient

from compare import hits,run
from test_scenarios import index
from test_reopen import pair


def schema():
    value=MilvusClient.create_schema(auto_id=False,enable_dynamic_field=True)
    value.add_field('id',DataType.INT64,is_primary=True)
    value.add_field('vector',DataType.FLOAT_VECTOR,dim=2)
    value.add_field('age',DataType.INT64)
    return value


@pytest.mark.parametrize('expr,expected',[
    ("category == 'tech'",[1]),("'tech' == category",[1]),("category != 'tech'",[2]),
    ("category < 'tech'",[2]),("category <= 'tech'",[1,2]),("category > 'news'",[1]),
    ("category >= 'tech'",[1]),("category in ['tech','ai']",[1]),
    ("$meta['category'] == 'tech'",[1]),("category in []",[]),
    ("category == 'tech' and age >= 1",[1]),("missing != 1",[]),
])
def test_dynamic_comparisons_membership_and_nulls_across_rpcs(diff,expr,expected):
    name='dynamic_rpc'
    diff.step('create',lambda c:c.create_collection(name,schema=schema(),index_params=index('L2')))
    rows=[{'id':1,'age':1,'vector':[0.,0.],'category':'tech'},
          {'id':2,'age':2,'vector':[1.,0.],'category':'news'},
          {'id':3,'age':3,'vector':[2.,0.],'category':None},
          {'id':4,'age':4,'vector':[3.,0.]}]
    diff.step('insert',lambda c:c.insert(name,rows))
    for phase in ('active','committed'):
        if phase=='committed':diff.step('flush',lambda c:c.flush(name))
        result=diff.step(phase+'_query',lambda c:c.query(name,filter=expr,output_fields=['id']))
        assert result==('ok',[{'id':pk} for pk in expected])
        assert diff.step(phase+'_count',lambda c:c.query(name,filter=expr,output_fields=['count(*)']))==('ok',[{'count(*)':len(expected)}])
        diff.step(phase+'_page',lambda c:c.query(name,filter=expr,output_fields=['id'],limit=1,offset=1))
        diff.step(phase+'_search',lambda c:c.search(name,[[0.,0.]],filter=expr,limit=4,search_params={'metric_type':'L2'}),hits('L2',[[0.,0.]],{r['id']:r['vector'] for r in rows},k=len(expected)))
    diff.step('delete',lambda c:c.delete(name,filter=expr))
    assert diff.step('remaining',lambda c:c.query(name,output_fields=['id']))==('ok',[{'id':pk} for pk in range(1,5) if pk not in expected])


def test_dynamic_batch_binding_and_failure_priority_across_active_chunks_and_reopen(pair):
    diff=pair.open();name='dynamic_sources'
    diff.step('create',lambda c:c.create_collection(name,schema=schema(),index_params=index('L2')))
    rows=[{'id':pk,'age':1,'vector':[1.,0.],'x':0.0} for pk in range(32)]
    diff.step('first_batch',lambda c:c.insert(name,rows))
    diff.step('heterogeneous_batch',lambda c:c.insert(name,[{'id':32,'age':1,'vector':[1.,0.],'x':'other'}]))
    expr='x in [-0.0]'
    def check(label):
        assert diff.step(label,lambda c:c.query(name,filter=expr,output_fields=['count(*)']))==('ok',[{'count(*)':32}])
    check('active')
    diff.step('flush',lambda c:c.flush(name))
    diff.step('new_source',lambda c:c.insert(name,[{'id':40,'age':1,'vector':[1.,0.],'x':0.0}]))
    check('separate_sources')
    pair.close();diff=pair.open();diff.step('load',lambda c:c.load_collection(name));check('reopen')
    diff.step('partial_update',lambda c:c.upsert(name,[{'id':0,'x':1}],partial_update=True))
    assert diff.step('modified',lambda c:c.query(name,filter=expr,output_fields=['count(*)']))==('ok',[{'count(*)':31}])


@pytest.mark.parametrize('values,expr,error',[
    ([1,0,None],"x in ['1']",False),([True,False,None],"x in [1.5]",False),
    ([1,'1',None],"x == 9223372036854775808",False),
    ([2**63,1,None],"age < 0 and x == 1",True),
    ([[1],1,None],"x in [1]",True),
    ([1,'1',None],"age < 0 and x in [1]",False),
    ([None,None,None],"x in ['1'] and age == 9223372036854775808",False),
    ([None,None,None],"x in [true] and age == 9223372036854775808",True),
    ([[1],1,None],"x <= 1 and x in [1]",True),
    ([[1],1,None],"x != 1 and x in [1]",True),
])
def test_dynamic_mixed_values_and_runtime_errors_preserve_failed_deletes(diff,values,expr,error):
    name='dynamic_types'
    diff.step('create',lambda c:c.create_collection(name,schema=schema(),index_params=index('L2')))
    rows=[{'id':i,'age':1,'vector':[float(i),0.],'x':x} for i,x in enumerate(values)]
    diff.step('insert',lambda c:c.insert(name,rows))
    diff.step('query',lambda c:c.query(name,filter=expr,output_fields=['id']),error=error)
    diff.step('count',lambda c:c.query(name,filter=expr,output_fields=['count(*)']),error=error)
    before=diff.step('before',lambda c:c.query(name,output_fields=['*']))
    diff.step('delete',lambda c:c.delete(name,filter=expr),error=error)
    if error:assert diff.step('after',lambda c:c.query(name,output_fields=['*']))==before


def test_dynamic_scope_rejects_nested_and_other_unimplemented_constructs(diff):
    name='dynamic_scope'
    diff.step('create',lambda c:c.create_collection(name,schema=schema(),index_params=index('L2')))
    diff.step('meta_in_parser_contract',lambda c:c.query(name,filter="$meta['x'] in [1]"),error=True)
    for expr in ["x['key'] == 1","$meta['x']['key'] == 1","x in [1] or x == 2","not x == 1","x not in [1]","x is null","x like 'a%'","x == age","age in [1]","id in [1] and x == 1"]:
        assert run(lambda c:c.query(name,filter=expr),diff.rust)[:2]==('rpc','UNIMPLEMENTED'),expr


@pytest.mark.parametrize('committed',[False,True])
@pytest.mark.parametrize('x,y,expr',[
    (1,[1],"x < 'c' and y in [1]"),
    (True,{'a':1},"x > 'a' and y in [1]"),
    ([1],[1],"x <= 1 and y in [1]"),
    (None,[1],"x < 'c' and y in [1]"),
])
def test_unknown_ordering_continues_to_errors_and_failed_delete_preserves_rows(diff,committed,x,y,expr):
    name='dynamic_unknown'
    diff.step('create',lambda c:c.create_collection(name,schema=schema(),index_params=index('L2')))
    rows=[{'id':1,'age':1,'vector':[0.,0.],'x':x,'y':y},
          {'id':2,'age':1,'vector':[1.,0.],'x':'b','y':1}]
    diff.step('insert',lambda c:c.insert(name,rows))
    if committed:diff.step('flush',lambda c:c.flush(name))
    before=diff.step('before',lambda c:c.query(name,output_fields=['*']))
    diff.step('query',lambda c:c.query(name,filter=expr),error=True)
    diff.step('count',lambda c:c.query(name,filter=expr,output_fields=['count(*)']),error=True)
    diff.step('search',lambda c:c.search(name,[[0.,0.]],filter=expr,limit=2,search_params={'metric_type':'L2'}),error=True)
    diff.step('delete',lambda c:c.delete(name,filter=expr),error=True)
    assert diff.step('after',lambda c:c.query(name,output_fields=['*']))==before
