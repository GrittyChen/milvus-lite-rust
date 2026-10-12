"""从真实 3.x v3.2.1/43d1257 生成顶层动态字段的组件真值。"""

import argparse
import json
import random
import subprocess
from pathlib import Path

import pyarrow as pa
import milvus_lite
from milvus_lite import __version__
from milvus_lite.schema.types import CollectionSchema, DataType, FieldSchema
from milvus_lite.search.filter.eval import evaluate
from milvus_lite.search.filter.parser import parse_expr
from milvus_lite.search.filter.semantic import compile_expr

REPO = Path(__file__).resolve().parents[2]
OUTPUT = REPO / "crates/core/tests/fixtures/filter_dynamic.json"


def cases():
    expressions = [
        "x == 1", "x != 1", "x < 1", "x <= 1", "x > 1", "x >= 1",
        "1 == x", "1 != x", "1 < x", "1 <= x", "1 > x", "1 >= x",
        "x == true", "x > false", "x == '1'", "x != '1'", "x < '1'",
        "x in []", "x in [1]", "x in [true]", "x in ['1']", "x in [1.5]",
        "x in [-0.0]", "x in [-0.0,1.5]", "x in [0,1.5]", "x in [1.0]",
        "x in [1,9007199254740993.0]", "x in [9007199254740993,1.5]",
        "x == 9223372036854775808", "9223372036854775808 == x", "x == 9007199254740992.0",
        "x == 1 and age > 0.0", "x != 1 and age > 0.0", "age < -1 and x in [1]",
        "x in [] and age == 9223372036854775808", "x == 'a' and age > 0.0",
        "x in [true] and age == 9223372036854775808", "age > 0.0 and x > 1",
        "__meta__x == 'sentinel' and x > 0", "x > 0 and __meta__x == 'sentinel'",
    ]
    groups = [
        [1,0,None], [1.0,-0.0,None], [True,False,None], ['a','1',None],
        [1,'1',None], [None,None,None], [[1],[],None], [{'a':1},{},None],
        [2**63,1,None], [2**63,1.5,None], [2**53+1,1.5,None], [True,1,None],
        [2**53+2,1.5,None], [-2**53-1,1.5,None], [2**63-1,1.5,None],
        [2**63+1,1.5,None], [10**100,1.5,None], [-10**100,1.5,None],
        [[2**63],[],None], [{'a':2**63},{},None],
        [{'a':1},{'a':'1'},None], [[1],['1'],None], [[],1,None],
    ]
    rng = random.Random(42)
    pool = [None,False,True,-1,0,1,1.5,-0.0,'a','1',[],[1],{}, {'a':1},2**53+1,2**63,10**100]
    groups += [[rng.choice(pool) for _ in range(3)] for _ in range(30)]
    for group in groups:
        for expression in expressions:
            for collision in (False,True) if '__meta__' in expression else (False,):
                yield {'values':group,'expr':expression,'collision':collision,'dynamic':True}
    for expression in ["$meta['x'] == 1", "$meta['x'] in [1]", "_seq > 0", "_partition == 'a'", "x in [1,'a']", "x and age > 1"]:
        for dynamic in (False,True):
            yield {'values':[1,None,2], 'expr':expression, 'collision':False, 'dynamic':dynamic}
    for group in groups[:12]:
        for expression in expressions[:28]:
            yield {'values':group,'expr':expression.replace('x', '$meta["x"]'),'collision':False,'dynamic':True}
    strings=['1','01','-1','-0','0x1','0Xffffffffffffffff','-0x1','+1',' 1','1 ','1.0','1e0','.5','1.','true','False','TRUE','0','nan','NaN','inf','Infinity','-inf','٣','1\x00','0x10000000000000000','0x8000000000000000','1e999','1e-999','NaN(foo)','-NaN','+NaN','nan()','nan(a_b)','nan(a-b)','nan(中文)','nan(123)','NaN(foo)tail','infinity']
    for group in groups[:8]+[[-1,-2**63,2**63-1]]:
        for value in strings:
            for tail in ('', ' and age == 9223372036854775808'):
                yield {'values':group,'expr':f'x in [{json.dumps(value)}]'+tail,'collision':False,'dynamic':True}
    for key in ('space key','中文😀','id'):
        yield {'key':key,'values':[1,None,2],'expr':f'$meta[{json.dumps(key)}] == 1','collision':False,'dynamic':True}
    for expression in ('x == 1 and y == 1','y == 1 and x == 1','x == 1 and y in [1]'):
        yield {'values':[1,'1',None],'extra':{'y':[2**63,1,None]},'expr':expression,'collision':False,'dynamic':True}
    # 单个 False 掩码不能区分 Unknown；后续成员查找错误固定三值 and 的求值行为。
    for x,y in [(1,[1]),(True,{'a':1}),([1],[1]),({'a':1},{'b':2}),('b',[1]),(None,[1])]:
        for op in ('==','!=','<','<=','>','>='):
            for literal in ('1', "'c'", 'true', '1.5'):
                for expression in (f'x {op} {literal} and y in [1]',f'{literal} {op} x and y in [1]'):
                    yield {'values':[x,'b',None],'extra':{'y':[y,1,None]},'expr':expression,'collision':False,'dynamic':True}


def generate():
    result = []
    for case in cases():
        fields = [FieldSchema(name='id',dtype=DataType.INT64,is_primary=True),
                  FieldSchema(name='age',dtype=DataType.INT64),
                  FieldSchema(name='vector',dtype=DataType.FLOAT_VECTOR,dim=2)]
        data = {'id':[0,1,2],'age':[2**53+1,1,1], '$meta':[json.dumps({case.get('key','x'):v,**{k:values[i] for k,values in case.get('extra',{}).items()}}) for i,v in enumerate(case['values'])]}
        if case['collision']:
            fields.append(FieldSchema(name='__meta__x',dtype=DataType.VARCHAR))
            data['__meta__x'] = ['sentinel']*3
        schema = CollectionSchema(fields=fields, enable_dynamic_field=case['dynamic'])
        try:
            compiled = compile_expr(parse_expr(case['expr']),schema,source=case['expr'])
            outcome = {'mask':evaluate(compiled,pa.table(data)).to_pylist()}
        except Exception as error:
            outcome = {'error':str(error)}
        result.append({**case,**outcome})
    return {'reference':'3.2.1/43d1257','cases':result}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--verify',action='store_true')
    args = parser.parse_args()
    assert __version__ == '3.2.1'
    assert Path(milvus_lite.__file__).resolve() == (REPO/'references/milvus-lite/milvus_lite/__init__.py').resolve()
    assert subprocess.check_output(['git','-C',str(REPO/'references/milvus-lite'),'rev-parse','--short','HEAD'],text=True).strip() == '43d1257'
    value = generate()
    if args.verify:
        assert json.loads(OUTPUT.read_text()) == value
        print(f"{len(value['cases'])} dynamic cases verified")
    else:
        OUTPUT.write_text(json.dumps(value,ensure_ascii=False,indent=2)+'\n')
        print(f"{len(value['cases'])} dynamic cases generated")


if __name__ == '__main__':
    main()
