"""0017：分区选择与 V1 search iterator 的交叉对照，使用仓库外新目录。"""
import argparse
import hashlib
import importlib.metadata as metadata
import json
import os
from pathlib import Path
import sys

from run import outside


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--work-dir',type=Path,required=True);a=p.parse_args()
    if not os.environ.get('MILVUS_LITE_3X_PYTHON'):p.error('需要 MILVUS_LITE_3X_PYTHON 指向固定参照解释器')
    root=outside(a.work_dir);root.mkdir(parents=True,exist_ok=False)
    sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'differential'))
    from test_reopen import ReopenPair
    from test_partitions import schema
    from test_scenarios import index
    from compare import hits
    import milvus_lite._native as native
    classes=set();checks=0
    for varchar in [False,True]:
        case=root/('varchar' if varchar else 'int64');case.mkdir();pair=ReopenPair(case);name='iteration_s' if varchar else 'iteration_i'
        key=lambda n:f'键{n}' if varchar else n
        original={key(i):[1.,(i+1)/100] for i in [0,1,2,4]};new={key(2):[1.,.09],key(3):[1.,.04]}
        def rows(vectors):return [{'id':pk,'vector':vector,'n':None if pk==key(1) else 7,'a':[1,2]} for pk,vector in vectors.items()]
        try:
            d=pair.open();d.step('create',lambda c:c.create_collection(name,schema=schema(varchar),index_params=index('L2')))
            for part in ['p1','p2']:d.step('partition',lambda c:c.create_partition(name,part))
            d.step('old',lambda c:c.insert(name,rows({k:v for k,v in original.items() if k!=key(4)}),partition_name='p1'));d.step('flush',lambda c:c.flush(name))
            d.step('new',lambda c:c.insert(name,rows(new),partition_name='p2'))
            d.step('active',lambda c:c.insert(name,rows({key(4):original[key(4)]}),partition_name='p1'))
            for state in ['mixed','reopened']:
                if state=='reopened':pair.close();d=pair.open();d.step('load',lambda c:c.load_collection(name))
                for parts,vectors in [([],original|new),(['p1'],original),(['p2'],new),(['p2','p1'],original|new),(['missing'],{})]:
                    for ranged in [False,True]:
                        def pages(c):
                            it=c.search_iterator(name,data=[[1.,0.]],partition_names=parts,batch_size=2,limit=-1,
                                output_fields=['n','a'],search_params={'metric_type':'L2','params':{'radius':1.,'range_filter':0.} if ranged else {}})
                            classes.add(type(it).__name__);result=[]
                            try:
                                for _ in range(12):
                                    page=list(it.next());result.append([{'id':h['id'],'distance':h['distance'],'entity':dict(h['entity'])} for h in page])
                                    if not page:return result
                                raise AssertionError('search iterator 未终止')
                            finally:it.close()
                        def check(a,b):
                            if a[0]!='ok' or b[0]!='ok':return 'search iterator failed'
                            if [[h['id'] for h in page] for page in a[1]] != [[h['id'] for h in page] for page in b[1]]:return 'page IDs differ'
                            return hits('L2',[[1.,0.]],vectors,k=len(vectors))(
                                ('ok',[[h for page in a[1] for h in page]]),('ok',[[h for page in b[1] for h in page]]))
                        d.step('partition-search-iteration',pages,check);checks+=1
        finally:pair.close()
    assert checks==40 and classes=={'SearchIterator'},(checks,classes)
    result={'checks':checks,'iterator_classes':sorted(classes),'pymilvus':metadata.version('pymilvus'),
            'native_sha256':hashlib.sha256(Path(native.__file__).read_bytes()).hexdigest(),
            'script_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    (root/'result.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result))


if __name__=='__main__':main()
