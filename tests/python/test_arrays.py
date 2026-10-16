"""ARRAY 走真实 SDK；父 null、空列表及非空列表不得混同。"""
import pytest
from pymilvus import DataType
from test_nullable_scalars import client_at

@pytest.mark.parametrize('dtype,good',[(DataType.BOOL,True),(DataType.INT8,7),(DataType.INT16,7),(DataType.INT32,7),(DataType.INT64,7),(DataType.FLOAT,1.5),(DataType.DOUBLE,1.5),(DataType.VARCHAR,'中文')])
def test_array_values_and_empty_results_across_flush_reopen(tmp_path,dtype,good):
    path=tmp_path/'arrays.db'
    with client_at(path) as c:
        s=c.create_schema(auto_id=False,enable_dynamic_field=False);s.add_field('id',DataType.INT64,is_primary=True);s.add_field('v',DataType.FLOAT_VECTOR,dim=2);s.add_field('x',DataType.ARRAY,element_type=dtype,max_capacity=3,max_length=8,nullable=True)
        p=c.prepare_index_params();p.add_index('v',index_type='FLAT',metric_type='L2');c.create_collection('c',schema=s,index_params=p)
        c.insert('c',[{'id':1,'v':[1.,0.],'x':None},{'id':2,'v':[2.,0.],'x':[]},{'id':3,'v':[3.,0.],'x':[good]}]);c.flush('c')
    with client_at(path) as c:
        c.load_collection('c');assert list(c.query('c',output_fields=['x']))==[{'id':1,'x':None},{'id':2,'x':[]},{'id':3,'x':[good]}]
        assert list(c.query('c',filter='id < 0',output_fields=['x']))==[]
        hits=c.search('c',[[0.,0.],[0.,0.]],limit=3,output_fields=['x'],search_params={'metric_type':'L2'})
        for result in hits:assert [r['entity']['x'] for r in result]==[None,[],[good]]
        c.upsert('c',[{'id':3}],partial_update=True);assert list(c.get('c',[3],output_fields=['x']))==[{'id':3,'x':[good]}]
        c.upsert('c',[{'id':3,'x':None}],partial_update=True);assert list(c.get('c',[3],output_fields=['x']))==[{'id':3,'x':None}]
