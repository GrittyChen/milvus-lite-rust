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
