"""存量 FAISS 非法度量：只有缺少非空 segment 侧车时 load 才失败。"""

import json
import shutil
import sys

import pytest

from test_cross_implementation import interpreters, run, PYTHONS

KINDS = ["AUTOINDEX", "HNSW", "HNSW_SQ", "IVF_FLAT", "IVF_SQ8"]

PREPARE = """
import os
import sys
import faiss
from pymilvus import MilvusClient, DataType
from milvus_lite.server_manager import server_manager_instance as manager

faiss.omp_set_num_threads(1)
path, kind, mode = sys.argv[1:]
c = MilvusClient(path, dedicated=True)
schema = c.create_schema(auto_id=False)
schema.add_field('id', DataType.INT64, is_primary=True)
schema.add_field('v', DataType.FLOAT_VECTOR, dim=2)
params = c.prepare_index_params()
params.add_index('v', index_type=kind, metric_type='L2')
c.create_collection('c', schema=schema, index_params=params)
for start in [0, 150]:
    c.insert('c', [{'id': i, 'v': [float(i % 17), float(i % 23)]} for i in range(start, start + 150)])
    if mode != 'wal':
        c.flush('c')
if mode == 'wal':
    os._exit(0)
if mode == 'deleted':
    c.delete('c', ids=list(range(300)))
    c.flush('c')
c.load_collection('c')
c.close()
manager.release_server(path)
"""

CHECK = """
import sys
from pathlib import Path
from pymilvus import MilvusClient
from pymilvus.grpc_gen import common_pb2, milvus_pb2
from milvus_lite.server_manager import server_manager_instance as manager

path, expected, count, metric = sys.argv[1:]
count = int(count)
c = MilvusClient(path, dedicated=True)
try:
    manifest = Path(path) / 'collections/c/manifest.json'
    # 首次列出 / 描述正常；只在 LoadCollection 扩展检查。
    assert 'c' in c.list_collections()
    assert c.describe_collection('c')['collection_name'] == 'c'
    before = manifest.read_bytes()
    stub = c._get_connection()._stub
    for _ in range(2):
        status = stub.LoadCollection(milvus_pb2.LoadCollectionRequest(collection_name='c'))
        state = c.get_load_state('c')['state']
        if expected == 'fail':
            assert (status.code, status.error_code, status.reason) == (
                1, 0, f"unsupported metric '{metric}'; expected COSINE / L2 / IP"
            )
            assert str(state) == 'NotLoad', state
        else:
            assert status.code == 0, status
            assert str(state) == 'Loaded', state
    assert manifest.read_bytes() == before
    if expected == 'fail':
        for call in [
            lambda: c.query('c', output_fields=['id'], limit=500),
            lambda: c.get('c', [1]),
            lambda: c.search('c', [[1., 0.]], anns_field='v', limit=1),
        ]:
            try:
                call()
            except Exception as e:
                assert e.code == 101, e
                assert "Collection 'c' is in state 'released'; call load() before search/get/query" in e.message, e
            else:
                raise AssertionError('read succeeded after failed load')
        c.drop_index('c', 'v_idx')
        c.load_collection('c')
    assert len(c.query('c', output_fields=['id'], limit=500)) == count
finally:
    c.close()
    manager.release_server(path)
"""


@pytest.fixture(scope="module")
def metric_templates(tmp_path_factory, interpreters):
    root = tmp_path_factory.mktemp("index-load-metric-templates")
    ref = interpreters[PYTHONS[0]]
    result = {}
    for kind in KINDS:
        for mode in ["committed", "deleted", "wal"]:
            path = root / f"{kind}-{mode}.db"
            run([ref, "-c", PREPARE, str(path), kind, mode], cwd=root)
            manifest = json.loads((path / "collections/c/manifest.json").read_text())
            files = manifest["partitions"]["_default"]["data_files"]
            assert len(files) == (0 if mode == "wal" else 2), files
            assert manifest["index_specs"]["v"]["build_params"] == {}
            result[kind, mode] = path
    return result


@pytest.mark.parametrize("kind", KINDS)
@pytest.mark.parametrize("metric", ["BM25", "NONE"])
@pytest.mark.parametrize("mode", ["all", "one", "none", "deleted", "wal"])
def test_faiss_load_metric_matches_reference_per_segment(
    tmp_path, interpreters, metric_templates, kind, metric, mode
):
    source_mode = mode if mode in ["deleted", "wal"] else "committed"
    for label, python in [
        ("reference", interpreters[PYTHONS[0]]),
        ("rust", sys.executable),
    ]:
        path = tmp_path / f"{label}.db"
        shutil.copytree(metric_templates[kind, source_mode], path)
        manifest = path / "collections/c/manifest.json"
        value = json.loads(manifest.read_text())
        value["index_specs"]["v"]["metric_type"] = metric
        manifest.write_text(json.dumps(value))
        files = sorted(
            (path / "collections/c/partitions/_default/indexes").glob("*.idx")
        )
        if source_mode != "wal":
            assert len(files) == 2
        remove = (
            files[:1] if mode == "one" else files if mode in ["none", "deleted"] else []
        )
        for file in remove:
            file.unlink()
        expected = "fail" if mode in ["one", "none", "deleted"] else "ok"
        count = 0 if mode == "deleted" else 300
        run(
            [python, "-c", CHECK, str(path), expected, str(count), metric], cwd=tmp_path
        )


@pytest.mark.parametrize("kind,param", [("HNSW", "M"), ("IVF_FLAT", "nlist")])
def test_bad_params_keep_the_registered_error_reason_difference(
    tmp_path, interpreters, metric_templates, kind, param
):
    check = """
import sys
from pathlib import Path
from pymilvus import MilvusClient
from pymilvus.grpc_gen import milvus_pb2
from milvus_lite.server_manager import server_manager_instance as manager

path, implementation = sys.argv[1:]
c = MilvusClient(path, dedicated=True)
try:
    c.describe_collection('c')
    manifest = Path(path) / 'collections/c/manifest.json'
    before = manifest.read_bytes()
    status = c._get_connection()._stub.LoadCollection(milvus_pb2.LoadCollectionRequest(collection_name='c'))
    assert (status.code, status.error_code) == (1, 0), status
    if implementation == 'reference':
        assert 'invalid literal for int()' in status.reason, status
    else:
        assert status.reason == "unsupported metric 'NONE'; expected COSINE / L2 / IP", status
    assert str(c.get_load_state('c')['state']) == 'NotLoad'
    assert manifest.read_bytes() == before
finally:
    c.close()
    manager.release_server(path)
"""
    for label, python in [
        ("reference", interpreters[PYTHONS[0]]),
        ("rust", sys.executable),
    ]:
        path = tmp_path / f"{label}.db"
        shutil.copytree(metric_templates[kind, "committed"], path)
        file = path / "collections/c/manifest.json"
        value = json.loads(file.read_text())
        value["index_specs"]["v"]["metric_type"] = "NONE"
        value["index_specs"]["v"]["build_params"] = {param: "abc"}
        file.write_text(json.dumps(value))
        for sidecar in (path / "collections/c/partitions/_default/indexes").glob(
            "*.idx"
        ):
            sidecar.unlink()
        run([python, "-c", check, str(path), label], cwd=tmp_path)


def test_reference_snapshot_api_can_produce_a_loadable_bad_metric_definition(
    tmp_path, interpreters, metric_templates
):
    ref = interpreters[PYTHONS[0]]
    original = tmp_path / "original.db"
    shutil.copytree(metric_templates["HNSW", "committed"], original)
    produce = """
import sys
from milvus_lite.db import MilvusLite
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as manager

path = sys.argv[1]
with MilvusLite(path) as db:
    db.create_snapshot('c', 'saved')
c = MilvusClient(path, dedicated=True)
try:
    c.release_collection('c')
    c.drop_index('c', 'v_idx')
    params = c.prepare_index_params()
    params.add_index('v', index_type='HNSW', metric_type='NONE')
    c.create_index('c', params)
finally:
    c.close()
    manager.release_server(path)
"""
    run([ref, "-c", produce, str(original)], cwd=tmp_path)
    value = json.loads((original / "collections/c/manifest.json").read_text())
    assert value["index_specs"]["v"]["metric_type"] == "NONE"
    assert (
        len(
            list((original / "collections/c/partitions/_default/indexes").glob("*.idx"))
        )
        == 2
    )
    for label, python in [("reference", ref), ("rust", sys.executable)]:
        path = tmp_path / f"{label}.db"
        shutil.copytree(original, path)
        run([python, "-c", CHECK, str(path), "ok", "300", "NONE"], cwd=tmp_path)
