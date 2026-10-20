"""新基准的目录所有权和严格结果门，不执行计时。"""
import importlib.util
from pathlib import Path

import pytest

ROOT=Path(__file__).resolve().parents[2]
SPEC=importlib.util.spec_from_file_location('key_benchmark',ROOT/'benchmarks/partition-key/run.py')
bench=importlib.util.module_from_spec(SPEC);SPEC.loader.exec_module(bench)


@pytest.mark.parametrize('name',['ordinary','é'])
def test_new_outside_path_is_allowed(tmp_path,name):
    assert bench.database_path(tmp_path/name)==tmp_path/name


def test_existing_link_and_repository_paths_are_rejected(tmp_path):
    target=tmp_path/'existing';target.mkdir()
    link=tmp_path/'link';link.symlink_to(tmp_path/'missing')
    for path in [target,link,ROOT/'not-a-benchmark-database',tmp_path/'missing-parent'/'db']:
        with pytest.raises(ValueError):bench.database_path(path)


@pytest.mark.parametrize('value',[True,1.0,None,'1'])
def test_result_types_are_not_coerced(value):
    with pytest.raises(RuntimeError):bench.exact([{'v':value}],[{'v':1}])


def test_sdk_payload_does_not_share_mutable_truth():
    expected=[{'id':0,'k':1,'value':{'items':[True,2]}}, {'id':1,'k':0,'value':None}]
    rows=bench.sdk_payload(expected)
    assert rows==[{'id':0,'k':1,'value':{'items':[True,2]},'vector':[1.,0.]},
                  {'id':1,'k':0,'value':None,'vector':[1.,0.]}]
    rows[0]['value']['items'][0]=False
    rows[0]['vector'][0]=3.
    assert expected==[{'id':0,'k':1,'value':{'items':[True,2]}}, {'id':1,'k':0,'value':None}]
    assert rows[1]['vector']==[1.,0.]


@pytest.mark.parametrize('method',['Insert','Upsert'])
def test_sdk_result_requires_exact_ids_and_count(method):
    count='insert_count' if method=='Insert' else 'upsert_count'
    bench.check_sdk_response({count:2,'ids':[0,1]},method,2)
    bench.check_sdk_response({count:2,'ids':iter([0,1])},method,2)
    for result in [{count:True,'ids':[0,1]}, {count:1,'ids':[0,1]},
                   {count:2,'ids':[1,0]}, {count:2,'ids':[0]}, {count:2,'ids':[False,1]}]:
        with pytest.raises(RuntimeError):bench.check_sdk_response(result,method,2)
