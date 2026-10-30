"""M2 存储文件矩阵：参照实现 v3.2.1（43d1257）与 Rust 双向读写。"""

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest


REPO = Path(__file__).resolve().parents[2]
PYTHONS = (
    "MILVUS_LITE_3X_PYTHON",
    "MILVUS_LITE_ARROW15_PYTHON",
    "MILVUS_LITE_ARROW25_PYTHON",
)


@pytest.mark.parametrize("writer", ["rust", "reference"])
@pytest.mark.parametrize("arrow", ["MILVUS_LITE_ARROW15_PYTHON", "MILVUS_LITE_ARROW25_PYTHON"])
def test_compacted_directory_cross_opens_updates_and_keeps_physical_order(tmp_path, interpreters, writer, arrow):
    root=tmp_path / 'compacted.db'
    write="""
import sys
from pymilvus import MilvusClient,DataType
from milvus_lite.server_manager import server_manager_instance as manager
path=sys.argv[1]
client=MilvusClient(path,dedicated=True)
schema=MilvusClient.create_schema(auto_id=False,enable_dynamic_field=True)
schema.add_field('id',DataType.INT64,is_primary=True)
schema.add_field('vector',DataType.FLOAT_VECTOR,dim=2)
indexes=client.prepare_index_params();indexes.add_index('vector',index_type='FLAT',metric_type='L2')
client.create_collection('c',schema=schema,index_params=indexes)
for part in range(4):
 client.insert('c',[{'id':pk,'vector':[1.,0.],'tag':pk} for pk in range(part*10,(part+1)*10)])
 client.flush('c')
client.close();manager.release_server(path)
"""
    read="""
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as manager
path=sys.argv[1]
client=MilvusClient(path,dedicated=True)
try:
 client.load_collection('c')
 rows=client.query('c',filter='',limit=40,output_fields=['id','tag'])
 assert [row['id'] for row in rows]==list(range(40)),rows
 assert client.query('c',filter='tag >= 35',output_fields=['count(*)'])==[{'count(*)':5}]
 client.upsert('c',[{'id':1,'vector':[2.,0.],'tag':99}]);client.delete('c',ids=[2]);client.flush('c')
finally:client.close();manager.release_server(path)
"""
    verify="""
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as manager
path=sys.argv[1];client=MilvusClient(path,dedicated=True)
try:
 client.load_collection('c')
 assert client.query('c',output_fields=['count(*)'])==[{'count(*)':39}]
 assert list(client.get('c',[1,2],output_fields=['tag','vector']))==[{'id':1,'tag':99,'vector':[2.,0.]}]
finally:client.close();manager.release_server(path)
"""
    writer_python=sys.executable if writer=='rust' else interpreters[PYTHONS[0]]
    reader_python=interpreters[PYTHONS[0]] if writer=='rust' else sys.executable
    run([writer_python,'-c',write,str(root)],cwd=tmp_path)
    collection=root / 'collections/c'
    manifest=json.loads((collection / 'manifest.json').read_text())
    assert len(manifest['partitions']['_default']['data_files'])==1
    parquet=collection / 'partitions/_default' / manifest['partitions']['_default']['data_files'][0]
    checked=run([interpreters[arrow],'-c',"import pyarrow.parquet as pq,sys; t=pq.read_table(sys.argv[1]);t.validate(full=True);assert t.num_rows==40;print(t.column('_seq').to_pylist())",str(parquet)])
    assert json.loads(checked)==list(range(1,41))
    run([reader_python,'-c',read,str(root)],cwd=tmp_path)
    run([writer_python,'-c',verify,str(root)],cwd=tmp_path)


@pytest.mark.parametrize("writer", ["rust", "reference"])
@pytest.mark.parametrize("storage", ["wal", "flushed"])
def test_top_level_dynamic_filters_read_the_other_implementations_directory(tmp_path, interpreters, writer, storage):
    root=tmp_path/'dynamic.db'
    write="""
import os,sys
from pymilvus import MilvusClient,DataType
from milvus_lite.server_manager import server_manager_instance
path,storage=sys.argv[1:]
client=MilvusClient(path,dedicated=True)
schema=MilvusClient.create_schema(auto_id=False,enable_dynamic_field=True)
schema.add_field('id',DataType.INT64,is_primary=True)
schema.add_field('vector',DataType.FLOAT_VECTOR,dim=2)
indexes=client.prepare_index_params();indexes.add_index('vector',index_type='FLAT',metric_type='L2')
client.create_collection('c',schema=schema,index_params=indexes)
client.insert('c',[{'id':1,'vector':[1.,0.],'category':'tech','x':1},
                   {'id':2,'vector':[0.,1.],'category':'news','x':1.5},
                   {'id':3,'vector':[1.,1.],'category':None,'x':'1'},
                   {'id':4,'vector':[2.,1.]}])
if storage=='wal':os._exit(0)
client.flush('c');client.close();server_manager_instance.release_server(path)
"""
    read="""
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance
path=sys.argv[1]
client=MilvusClient(path,dedicated=True)
try:
 client.load_collection('c')
 assert client.query('c',filter="category in ['tech']",output_fields=['id'])==[{'id':1}]
 assert client.query('c',filter="$meta['category'] != 'tech'",output_fields=['id'])==[{'id':2}]
 assert client.query('c',filter='x == 1',output_fields=['id'])==[{'id':1}]
 assert client.query('c',filter='missing != 1',output_fields=['count(*)'])==[{'count(*)':0}]
 assert client.search('c',[[1.,0.]],filter="category in ['tech']",limit=1,search_params={'metric_type':'L2'})[0][0]['id']==1
finally:
 client.close();server_manager_instance.release_server(path)
"""
    python=sys.executable if writer=='rust' else interpreters[PYTHONS[0]]
    reader=interpreters[PYTHONS[0]] if writer=='rust' else sys.executable
    run([python,'-c',write,str(root),storage],cwd=tmp_path)
    wal=root/'collections/c/wal'
    assert bool(list(wal.glob('wal_data_*.arrow')))==(storage=='wal')
    run([reader,'-c',read,str(root)],cwd=tmp_path)


UNBUILDABLE = {
    'IVF_PQ': "index_type='IVF_PQ' is reserved for a future phase; supported: HNSW, HNSW_SQ, IVF_FLAT, IVF_SQ8, BRUTE_FORCE",
    'INVERTED': 'scalar index INVERTED does not support field type FLOAT_VECTOR',
    'DISKANN': "unknown index_type: 'DISKANN'",
    'FOO': "unknown index_type: 'FOO'",
}


@pytest.mark.parametrize("storage", ["flushed", "deleted", "wal"])
@pytest.mark.parametrize("index_type", [*UNBUILDABLE, "HNSW", "SPARSE_INVERTED_INDEX"])
def test_index_definitions_3x_cannot_build_fail_load_like_3x(tmp_path, interpreters, index_type, storage):
    # 3.x 的 create_index 不校验向量字段的 index_type；空 collection 上建定义不触发 build，
    # 之后的 load 才为有行的已提交 segment 建索引。只有 WAL 时不建，被删除的行仍算物理行。
    write="""
import os,sys
from pymilvus import MilvusClient,DataType
from milvus_lite.server_manager import server_manager_instance as manager
path,index_type,storage=sys.argv[1:]
client=MilvusClient(path,dedicated=True)
schema=MilvusClient.create_schema(auto_id=False)
schema.add_field('id',DataType.INT64,is_primary=True)
schema.add_field('vector',DataType.FLOAT_VECTOR,dim=2)
client.create_collection('c',schema=schema)
indexes=client.prepare_index_params();indexes.add_index('vector',index_type=index_type,metric_type='L2')
client.create_index('c',indexes)
client.release_collection('c')
client.insert('c',[{'id':1,'vector':[1.,0.]},{'id':2,'vector':[0.,1.]}])
if storage=='wal':os._exit(0)
client.flush('c')
if storage=='deleted':client.delete('c',ids=[1,2]);client.flush('c')
client.close();manager.release_server(path)
"""
    read="""
import json,sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as manager
path,storage=sys.argv[1:]
client=MilvusClient(path,dedicated=True)
def step(call):
 try:return call()
 except Exception as error:return [getattr(error,'code',None),getattr(error,'message',str(error))]
def load():client.load_collection('c');return 'ok'
def search():return [hit['id'] for hit in client.search('c',[[1.,0.]],limit=2,search_params={'metric_type':'L2'})[0]]
def state():return str(client.get_load_state('c')['state'])
out=[]
try:
 out+=[step(load),state(),step(search)]
 if storage=='wal':
  client.flush('c');out+=[step(load),step(search)]
  client.release_collection('c');out+=[step(load),state()]
finally:client.close();manager.release_server(path)
print(json.dumps(out))
"""
    root=tmp_path/'written.db'
    run([interpreters[PYTHONS[0]],'-c',write,str(root),index_type,storage],cwd=tmp_path)
    collection=root/'collections/c'
    manifest=json.loads((collection/'manifest.json').read_text())
    assert manifest['index_specs']['vector']['index_type']==index_type
    assert manifest['index_specs']['vector']['build_params']=={}
    assert len(manifest['partitions']['_default']['data_files'])==(storage!='wal')
    assert bool(list((collection/'wal').glob('wal_data_*.arrow')))==(storage=='wal')
    results={}
    for side,python in [('reference',interpreters[PYTHONS[0]]),('rust',sys.executable)]:
        copy=tmp_path/f'{side}.db'
        shutil.copytree(root,copy)
        results[side]=json.loads(run([python,'-c',read,str(copy),storage],cwd=tmp_path))
    error=[1,UNBUILDABLE[index_type]] if index_type in UNBUILDABLE else None
    not_loaded=[101,"Collection 'c' is in state 'released'; call load() before search/get/query"]
    if storage=='wal':
        expected=['ok','Loaded',[1,2],'ok',[1,2]]+([error,'NotLoad'] if error else ['ok','Loaded'])
    elif error:
        expected=[error,'NotLoad',not_loaded]
    else:
        expected=['ok','Loaded',[] if storage=='deleted' else [1,2]]
    assert results['reference']==expected
    assert results['rust']==expected


def run(args, *, cwd=REPO):
    result = subprocess.run(args, cwd=cwd, text=True, capture_output=True, check=False)
    assert result.returncode == 0, (
        f"command failed: {args!r}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
    )
    return result.stdout.strip()


@pytest.fixture(scope="session")
def interpreters():
    missing = [name for name in PYTHONS if not os.environ.get(name)]
    if missing:
        raise pytest.UsageError(f"storage interop requires: {', '.join(missing)}")
    result = {name: os.environ[name] for name in PYTHONS}
    commit = run(["git", "-C", str(REPO / "references" / "milvus-lite"), "rev-parse", "--short", "HEAD"])
    assert commit == "43d1257", f"reference checkout is {commit}, expected 43d1257"
    for name, python in result.items():
        expected = "3.2.1" if name == PYTHONS[0] else ("15." if name == PYTHONS[1] else "25.")
        expression = (
            'from importlib.metadata import version; print(version("milvus-lite"))'
            if name == PYTHONS[0]
            else "import pyarrow; print(pyarrow.__version__)"
        )
        actual = run([python, "-c", expression])
        valid = actual == expected if name == PYTHONS[0] else actual.startswith(expected)
        assert valid, f"{name} has {actual}, expected {expected}"
    # Arrow 15/25 既校验文件，也运行参照引擎；提前报告缺失依赖，避免到子用例才失败。
    for name, python in result.items():
        probe = subprocess.run([
            python, "-c",
            "import json; from pathlib import Path; from importlib.metadata import version; "
            "import milvus_lite, numpy; from milvus_lite.db import MilvusLite; "
            "print(json.dumps({'path': str(Path(milvus_lite.__file__).resolve()), "
            "'version': version('milvus-lite'), 'numpy': numpy.__version__}))",
        ], cwd=REPO, text=True, capture_output=True, check=False)
        if probe.returncode != 0:
            raise pytest.UsageError(
                f"{name} ({python}) cannot import the reference engine milvus_lite.db.MilvusLite; "
                f"prepare this interpreter as described in tests/storage_interop/README.md.\n{probe.stderr}"
            )
        info = json.loads(probe.stdout)
        assert info["version"] == "3.2.1", f"{name} has milvus-lite {info['version']}, expected 3.2.1"
        assert Path(info["path"]).is_relative_to(REPO / "references" / "milvus-lite"), (
            f"{name} imports {info['path']}, not the local reference checkout"
        )
        if name == PYTHONS[1]:
            assert info["numpy"] == "1.26.4", f"{name} has NumPy {info['numpy']}, expected 1.26.4"
    return result


@pytest.fixture(scope="session")
def rust_binary():
    run(["cargo", "build", "-p", "milvus-lite-core", "--example", "storage_interop", "--quiet"])
    suffix = ".exe" if os.name == "nt" else ""
    return REPO / "target" / "debug" / "examples" / f"storage_interop{suffix}"


def rust(binary, operation, root):
    output = run([str(binary), operation, str(root)])
    return json.loads(output) if operation == "inspect" else None


REF_SCHEMA = """
from milvus_lite.schema.types import CollectionSchema, FieldSchema, DataType
schema = CollectionSchema(fields=[
    FieldSchema("id", DataType.INT64, is_primary=True),
    FieldSchema("vector", DataType.FLOAT_VECTOR, dim=2),
])
"""


def reference_create(python, root, *, flush):
    code = f"""
import os, sys
from milvus_lite.db import MilvusLite
{REF_SCHEMA}
db = MilvusLite(sys.argv[1])
collection = db.create_collection("c", schema)
collection.insert([{{"id": 1, "vector": [1.0, 2.0]}}, {{"id": 2, "vector": [3.0, 4.0]}}])
{'collection.flush()' if flush else ''}
collection.delete([1])
{'collection.flush(); db.close()' if flush else 'os._exit(0)'}
"""
    run([python, "-c", code, str(root)], cwd=root.parent)


def reference_read_and_extend(python, root):
    code = """
import json, sys
from milvus_lite.db import MilvusLite
db = MilvusLite(sys.argv[1])
collection = db.get_collection("c")
print(json.dumps({"count": collection.num_entities, "id2": collection.get([2])}))
collection.insert([{"id": 3, "vector": [5.0, 6.0]}])
db.close()
"""
    return json.loads(run([python, "-c", code, str(root)], cwd=root.parent))


@pytest.mark.parametrize("reader", ["rust", "reference"])
@pytest.mark.parametrize("entry", ["index_first", "query_first", "empty_flush", "close_only"])
def test_data_only_reference_directory_supports_first_metadata_and_empty_commits(tmp_path, interpreters, reader, entry):
    root = tmp_path / "data-only.db"
    writer = f"""
import sys
from milvus_lite.db import MilvusLite
{REF_SCHEMA}
db = MilvusLite(sys.argv[1])
collection = db.create_collection('c', schema)
collection.insert([{{'id': 1, 'vector': [1.0, 2.0]}}, {{'id': 2, 'vector': [3.0, 4.0]}}])
collection.flush()
db.close()
"""
    run([interpreters[PYTHONS[0]], "-c", writer, str(root)], cwd=tmp_path)
    collection = root / "collections/c"
    partition = collection / "partitions/_default"
    assert partition.joinpath("data").is_dir()
    assert not partition.joinpath("delta").exists()
    old_manifest = json.loads(collection.joinpath("manifest.json").read_text())
    assert old_manifest["partitions"]["_default"]["data_files"]
    assert old_manifest["partitions"]["_default"]["delta_files"] == []
    originals = {path.name: path.read_bytes() for path in partition.joinpath("data").glob("*.parquet")}
    verify = """
import sys
from pathlib import Path
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance
path, entry = sys.argv[1:]
client = MilvusClient(path, dedicated=True)
try:
    if entry == 'query_first':
        client.load_collection('c')
        assert client.search('c', [[1.0, 2.0]], limit=2, search_params={'metric_type': 'L2'})[0][0]['id'] == 1
    if entry in ('index_first', 'query_first'):
        indexes = client.prepare_index_params()
        indexes.add_index('vector', index_type='FLAT', metric_type='L2')
        client.create_index('c', indexes)
        assert not Path(path).joinpath('collections/c/partitions/_default/delta').exists()
    elif entry == 'empty_flush':
        client.flush('c')
    client.load_collection('c')
    rows = list(client.get('c', ids=[1, 2], output_fields=['id', 'vector']))
    assert rows == [{'id': 1, 'vector': [1.0, 2.0]}, {'id': 2, 'vector': [3.0, 4.0]}], rows
    assert list(client.query('c', filter='', output_fields=['count(*)'])) == [{'count(*)': 2}]
    assert client.search('c', [[1.0, 2.0]], limit=2, search_params={'metric_type': 'L2'})[0][0]['id'] == 1
finally:
    client.close()
    server_manager_instance.release_server(path)
"""
    python = sys.executable if reader == "rust" else interpreters[PYTHONS[0]]
    run([python, "-c", verify, str(root), entry], cwd=tmp_path)
    assert {path.name: path.read_bytes() for path in partition.joinpath("data").glob("*.parquet")} == originals
    final_manifest = json.loads(collection.joinpath("manifest.json").read_text())
    assert final_manifest["partitions"]["_default"]["data_files"] == old_manifest["partitions"]["_default"]["data_files"]
    assert final_manifest["partitions"]["_default"]["delta_files"] == []


def test_rust_delete_and_upsert_files_open_in_reference(tmp_path, interpreters):
    root = tmp_path / "mutations.db"
    rust_code = """
import sys
from pymilvus import DataType, MilvusClient
from milvus_lite.server_manager import server_manager_instance
path = sys.argv[1]
schema = MilvusClient.create_schema(auto_id=False)
schema.add_field('id', DataType.INT64, is_primary=True)
schema.add_field('vector', DataType.FLOAT_VECTOR, dim=2)
client = MilvusClient(path)
client.create_collection('c', schema=schema)
client.insert('c', [{'id': 1, 'vector': [1.0, 0.0]}, {'id': 2, 'vector': [0.0, 1.0]}])
client.flush('c')
client.delete('c', ids=[1])
client.upsert('c', [{'id': 2}], partial_update=True)
client.flush('c')
client.close()
server_manager_instance.release_server(path)
"""
    reference_code = """
import sys
from milvus_lite.db import MilvusLite
db = MilvusLite(sys.argv[1])
collection = db.get_collection('c')
assert collection.num_entities == 1
assert collection.get([1]) == []
row = collection.get([2])[0]
assert row['id'] == 2 and row['vector'] == [0.0, 1.0]
db.close()
"""
    run([sys.executable, "-c", rust_code, str(root)], cwd=tmp_path)
    arrow_code = """
import sys
from pathlib import Path
import pyarrow.parquet as pq
part = Path(sys.argv[1]) / 'collections' / 'c' / 'partitions' / '_default'
data = sorted(part.joinpath('data').glob('*.parquet'))
delta = sorted(part.joinpath('delta').glob('*.parquet'))
assert len(data) == 2 and len(delta) == 1
assert sorted(pk for path in data for pk in pq.read_table(path).column('id').to_pylist()) == [1, 2, 2]
assert pq.read_table(delta[0]).column('id').to_pylist() == [1]
"""
    for name in (PYTHONS[1], PYTHONS[2]):
        run([interpreters[name], "-c", arrow_code, str(root)], cwd=tmp_path)
    run([interpreters[PYTHONS[0]], "-c", reference_code, str(root)], cwd=tmp_path)


@pytest.mark.parametrize("reader", ["rust", "reference"])
def test_confirmed_mixed_wal_survives_rust_process_crash(tmp_path, interpreters, reader):
    root = tmp_path / "mixed-wal.db"
    writer = """
import os, sys
from pymilvus import DataType, MilvusClient
path = sys.argv[1]
schema = MilvusClient.create_schema(auto_id=False)
schema.add_field('id', DataType.INT64, is_primary=True)
schema.add_field('vector', DataType.FLOAT_VECTOR, dim=2)
client = MilvusClient(path)
client.create_collection('c', schema=schema)
client.insert('c', [{'id': 1, 'vector': [1.0, 0.0]}, {'id': 2, 'vector': [0.0, 1.0]}])
client.flush('c')
assert client.delete('c', ids=[1]) == [1]
assert client.upsert('c', [{'id': 2, 'vector': [2.0, 0.0]}])['upsert_count'] == 1
assert client.insert('c', [{'id': 3, 'vector': [0.0, 2.0]}])['insert_count'] == 1
os._exit(0)
"""
    run([sys.executable, "-c", writer, str(root)], cwd=tmp_path)
    collection = root / "collections" / "c"
    wal = collection / "wal"
    assert sorted(path.name for path in wal.glob("*.arrow")) == [
        "wal_data_000002.arrow", "wal_delta_000002.arrow",
    ]
    manifest = json.loads((collection / "manifest.json").read_text())
    assert manifest["current_seq"] == 2
    assert manifest["active_wal_number"] == 2

    verify = """
import sys
from pymilvus import MilvusClient
client = MilvusClient(sys.argv[1])
assert client.query('c', output_fields=['count(*)']) == [{'count(*)': 2}]
assert client.get('c', ids=[1]) == []
rows = client.get('c', ids=[2, 3], output_fields=['vector'])
assert list(rows) == [
    {'id': 2, 'vector': [2.0, 0.0]},
    {'id': 3, 'vector': [0.0, 2.0]},
], rows
assert [row['id'] for row in client.query('c', filter='id in [1, 2, 3]')] == [2, 3]
assert [hit['id'] for hit in client.search('c', data=[[2.0, 0.0]], limit=2)[0]] == [2, 3]
client.close()
"""
    python = sys.executable if reader == "rust" else interpreters[PYTHONS[0]]
    run([python, "-c", verify, str(root)], cwd=tmp_path)


def test_real_database_path_alternates_reference_and_rust(tmp_path, interpreters):
    root = tmp_path / "alternating.db"
    reference_create(interpreters[PYTHONS[0]], root, flush=True)
    rust_code = """
import json, sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance
path = sys.argv[1]
client = MilvusClient(path)
assert client.list_collections() == ["c"]
assert client.query("c", output_fields=["count(*)"]) == [{"count(*)": 1}]
client.insert("c", [{"id": 3, "vector": [5.0, 6.0]}])
client.flush("c")
client.close()
server_manager_instance.release_server(path)
"""
    run([sys.executable, "-c", rust_code, str(root)], cwd=tmp_path)
    code = """
import sys
from milvus_lite.db import MilvusLite
db = MilvusLite(sys.argv[1])
collection = db.get_collection("c")
assert collection.num_entities == 2
assert collection.get([3])[0]["id"] == 3
db.close()
"""
    run([interpreters[PYTHONS[0]], "-c", code, str(root)], cwd=tmp_path)


ALTERNATING_PRELUDE = """
import sys
from pymilvus import DataType, MilvusClient
from milvus_lite.server_manager import server_manager_instance as manager
path = sys.argv[1]
client = MilvusClient(path, dedicated=True)
schema = MilvusClient.create_schema()
schema.add_field('id', DataType.INT64, is_primary=True)
schema.add_field('vector', DataType.FLOAT_VECTOR, dim=2)
indexes = client.prepare_index_params()
indexes.add_index('vector', index_type='FLAT', metric_type='L2')
try:
{body}
finally:
    client.close()
    manager.release_server(path)
"""


def alternate(tmp_path, interpreters, first, root, steps):
    """两个实现轮流各开一个进程执行一步，`first` 先行。"""
    pythons = {"rust": sys.executable, "reference": interpreters[PYTHONS[0]]}
    order = [first, "reference" if first == "rust" else "rust"] * len(steps)
    for who, step in zip(order, steps):
        body = "\n".join("    " + line for line in step.strip().splitlines())
        run([pythons[who], "-c", ALTERNATING_PRELUDE.format(body=body), str(root)], cwd=tmp_path)


@pytest.mark.parametrize("first", ["rust", "reference"])
def test_aliases_written_by_one_implementation_resolve_and_change_in_the_other(tmp_path, interpreters, first):
    """0023 A 互通：别名的创建、改指、解析与经别名删除在两边交替执行，`aliases.json` 互相读写。"""
    root = tmp_path / "aliases.db"
    steps = [
        # 第一方：两个集合与两个别名，其中一个以别名为目标（保存解析后的名字）。
        """
for name, pk in [('c1', 1), ('c2', 2)]:
    client.create_collection(name, schema=schema, index_params=indexes)
    client.insert(name, [{'id': pk, 'vector': [1.0, 0.0]}])
client.create_alias('c1', 'a')
client.create_alias('a', 'b')
""",
        # 第二方：读取对方写的别名，经别名读写，再改指。
        """
assert client.describe_alias('b')['collection_name'] == 'c1'
assert sorted(client.list_aliases('c1')['aliases']) == ['a', 'b']
client.load_collection('a')
assert client.query('a', filter='', output_fields=['id']) == [{'id': 1}]
client.insert('b', [{'id': 3, 'vector': [0.0, 1.0]}])
client.alter_alias('c2', 'a')
client.create_alias('a', 'z')
""",
        # 第一方：看到对方的改动，经别名删除目标集合及其全部别名。
        """
assert client.describe_alias('a')['collection_name'] == 'c2'
assert client.describe_alias('z')['collection_name'] == 'c2'
client.load_collection('c1')
client.load_collection('a')
assert client.query('c1', filter='', output_fields=['count(*)']) == [{'count(*)': 2}]
assert client.query('a', filter='', output_fields=['id']) == [{'id': 2}]
client.drop_collection('z')
assert client.list_collections() == ['c1']
""",
        # 第二方：只剩指向 c1 的别名。
        """
assert client.list_aliases()['aliases'] == ['b']
assert client.describe_alias('b')['collection_name'] == 'c1'
assert client.list_collections() == ['c1']
""",
    ]
    alternate(tmp_path, interpreters, first, root, steps)
    assert json.loads((root / "aliases.json").read_text()) == {"b": "c1"}


@pytest.mark.parametrize("first", ["rust", "reference"])
def test_collection_properties_altered_by_one_implementation_are_read_and_changed_by_the_other(
    tmp_path, interpreters, first
):
    """0023 A 互通：两边交替修改集合属性；进程内为插入顺序，对方重开后按文件中的（排序）顺序读回。"""
    root = tmp_path / "properties.db"
    steps = [
        """
client.create_collection('c', schema=schema, index_params=indexes)
client.insert('c', [{'id': 1, 'vector': [1.0, 0.0]}])
client.create_alias('c', 'a')
client.alter_collection_properties('a', {'zeta': 1, 'alpha': '中文', 'mid': 'x'})
assert list(client.describe_collection('c')['properties']) == ['zeta', 'alpha', 'mid']
""",
        """
assert client.describe_collection('c')['properties'] == {'alpha': '中文', 'mid': 'x', 'zeta': '1'}
assert list(client.describe_collection('c')['properties']) == ['alpha', 'mid', 'zeta']
client.drop_collection_properties('c', ['mid'])
client.alter_collection_properties('a', {'beta': 'b', 'alpha': 'A'})
client.load_collection('c')
assert client.query('c', filter='', output_fields=['id']) == [{'id': 1}]
""",
        """
assert list(client.describe_collection('c')['properties'].items()) == [('alpha', 'A'), ('beta', 'b'), ('zeta', '1')]
client.insert('c', [{'id': 2, 'vector': [0.0, 1.0]}])
""",
        """
client.load_collection('c')
assert client.query('c', filter='', output_fields=['count(*)']) == [{'count(*)': 2}]
assert list(client.describe_collection('a')['properties']) == ['alpha', 'beta', 'zeta']
""",
    ]
    alternate(tmp_path, interpreters, first, root, steps)
    schema = json.loads((root / "collections/c/schema.json").read_text())
    assert schema["collection_name"] == "c"
    assert schema["properties"] == {"alpha": "A", "beta": "b", "zeta": "1"}


@pytest.mark.parametrize("first", ["rust", "reference"])
def test_collections_renamed_by_one_implementation_open_and_rename_in_the_other(tmp_path, interpreters, first):
    """0023 A 互通：两边交替改名（含经别名），别名改指与 `schema.json` 的名字互相可读；改名后按重开为 released。"""
    root = tmp_path / "rename.db"
    steps = [
        """
client.create_collection('c', schema=schema, index_params=indexes)
client.insert('c', [{'id': 1, 'vector': [1.0, 0.0]}])
client.create_alias('c', 'a')
client.rename_collection('a', 'd')
assert client.list_collections() == ['d']
assert str(client.get_load_state('d')['state']) == 'NotLoad'
""",
        """
assert client.list_collections() == ['d']
assert client.describe_alias('a')['collection_name'] == 'd'
assert str(client.get_load_state('d')['state']) == 'NotLoad'
client.load_collection('a')
assert client.query('a', filter='', output_fields=['id']) == [{'id': 1}]
client.insert('d', [{'id': 2, 'vector': [0.0, 1.0]}])
client.rename_collection('d', 'e')
""",
        """
assert client.list_collections() == ['e']
assert client.describe_alias('a')['collection_name'] == 'e'
client.load_collection('e')
assert client.query('e', filter='', output_fields=['count(*)']) == [{'count(*)': 2}]
client.create_collection('d', schema=schema)
""",
    ]
    alternate(tmp_path, interpreters, first, root, steps)
    assert json.loads((root / "collections/e/schema.json").read_text())["collection_name"] == "e"
    assert json.loads((root / "aliases.json").read_text()) == {"a": "e"}


RENAME_INTENT = "milvus-lite-rust.rename-intent.json"


@pytest.mark.parametrize("state", ["intent-then-read", "intent-then-rename", "reference-crash"])
def test_unfinished_renames_cross_open_between_implementations(tmp_path, interpreters, state):
    """0023 A 互通：本实现改名未完成（目录已改名、`schema.json` 与别名未改写、意图仍在）的目录由 3.x 打开并改动，
    本实现再打开时按意图收尾或删除意图；3.x 改名中途退出留下的目录（名字与目录不同、别名仍指向旧名）本实现可直接打开。"""
    root = tmp_path / "unfinished.db"
    alternate(tmp_path, interpreters, "rust", root, ["""
client.create_collection('c', schema=schema)
client.insert('c', [{'id': 1, 'vector': [1.0, 0.0]}])
client.create_alias('c', 'a')
"""])
    (root / "collections/c").rename(root / "collections/d")
    if state.startswith("intent"):
        (root / RENAME_INTENT).write_text(
            json.dumps({"format_version": 1, "operation": "rename_collection", "old": "c", "new": "d"}))
    reference = {
        "intent-then-read": "client.insert('d', [{'id': 2, 'vector': [0.0, 1.0]}])",
        "intent-then-rename": "client.rename_collection('d', 'f')",
        "reference-crash": "assert client.list_collections() == ['d']",
    }[state]
    alternate(tmp_path, interpreters, "reference", root, [reference])
    rust = {
        # 新名存在、旧名不存在：补写名字与别名，3.x 的写入仍在。
        "intent-then-read": """
assert client.list_collections() == ['d']
assert client.describe_alias('a')['collection_name'] == 'd'
assert client.query('a', filter='', output_fields=['count(*)']) == [{'count(*)': 2}]
""",
        # 3.x 又改了名：精确名字与路径都不存在，只删除意图，别名保持 3.x 写出的样子。
        "intent-then-rename": """
assert client.list_collections() == ['f']
assert client.describe_alias('a')['collection_name'] == 'c'
assert client.describe_collection('f')['collection_name'] == 'f'
""",
        "reference-crash": """
assert client.list_collections() == ['d']
assert client.describe_collection('d')['collection_name'] == 'd'
assert client.describe_alias('a')['collection_name'] == 'c'
assert client.query('d', filter='', output_fields=['count(*)']) == [{'count(*)': 1}]
""",
    }[state]
    alternate(tmp_path, interpreters, "rust", root, [rust])
    assert not (root / RENAME_INTENT).exists()
    if state == "intent-then-read":
        assert json.loads((root / "collections/d/schema.json").read_text())["collection_name"] == "d"
        assert json.loads((root / "aliases.json").read_text()) == {"a": "d"}
    elif state == "reference-crash":
        assert json.loads((root / "collections/d/schema.json").read_text())["collection_name"] == "c"


def test_real_database_path_recovers_reference_crash_wal(tmp_path, interpreters):
    root = tmp_path / "crashed.db"
    reference_create(interpreters[PYTHONS[0]], root, flush=False)
    rust_code = """
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance
path = sys.argv[1]
client = MilvusClient(path)
assert client.query("c", output_fields=["count(*)"]) == [{"count(*)": 1}]
assert [row["id"] for row in client.get("c", ids=[1, 2])] == [2]
client.insert("c", [{"id": 3, "vector": [5.0, 6.0]}])
client.close()
server_manager_instance.release_server(path)
"""
    run([sys.executable, "-c", rust_code, str(root)], cwd=tmp_path)
    code = """
import sys
from milvus_lite.db import MilvusLite
db = MilvusLite(sys.argv[1])
assert db.get_collection("c").num_entities == 2
db.close()
"""
    run([interpreters[PYTHONS[0]], "-c", code, str(root)], cwd=tmp_path)


def test_json_and_meta_parquet_bytes_match_reference_at_float_tie(tmp_path, interpreters):
    reference = tmp_path / "reference_json.db"
    rust = tmp_path / "rust_json.db"
    reference_code = """
import struct, sys
from milvus_lite.db import MilvusLite
from milvus_lite.schema.types import CollectionSchema, FieldSchema, DataType
value = float(struct.unpack('!f', bytes.fromhex('445146a9'))[0])
schema = CollectionSchema(fields=[
    FieldSchema('id', DataType.INT64, is_primary=True),
    FieldSchema('vector', DataType.FLOAT_VECTOR, dim=2),
    FieldSchema('j', DataType.JSON),
], enable_dynamic_field=True)
db = MilvusLite(sys.argv[1])
collection = db.create_collection('c', schema)
collection.insert([{'id': 1, 'vector': [1.0, 0.0], 'j': {'x': value}, 'score': value}])
collection.flush()
db.close()
"""
    rust_code = """
import struct, sys
from pymilvus import MilvusClient, DataType
from milvus_lite.server_manager import server_manager_instance
value = float(struct.unpack('!f', bytes.fromhex('445146a9'))[0])
path = sys.argv[1]
schema = MilvusClient.create_schema(enable_dynamic_field=True)
schema.add_field('id', DataType.INT64, is_primary=True)
schema.add_field('vector', DataType.FLOAT_VECTOR, dim=2)
schema.add_field('j', DataType.JSON)
client = MilvusClient(path)
client.create_collection('c', schema=schema)
client.insert('c', [{'id': 1, 'vector': [1.0, 0.0], 'j': {'x': value}, 'score': value}])
client.flush('c')
client.close()
server_manager_instance.release_server(path)
"""
    run([interpreters[PYTHONS[0]], "-c", reference_code, str(reference)], cwd=tmp_path)
    run([sys.executable, "-c", rust_code, str(rust)], cwd=tmp_path)
    read_code = """
import json, sys
from pathlib import Path
import pyarrow.parquet as pq
root = Path(sys.argv[1]) / 'collections' / 'c'
manifest = json.loads((root / 'manifest.json').read_text())
relative = manifest['partitions']['_default']['data_files'][0]
table = pq.read_table(root / 'partitions' / '_default' / relative)
print(json.dumps({'j': table.column('j').to_pylist()[0], 'meta': table.column('$meta').to_pylist()[0]}))
"""
    expected = {'j': '{"x": 837.1040649414062}', 'meta': '{"score": 837.1040649414062}'}
    for root in (reference, rust):
        actual = json.loads(run([interpreters[PYTHONS[0]], "-c", read_code, str(root)], cwd=tmp_path))
        assert actual == expected


@pytest.mark.parametrize("delete_first", [False, True])
def test_equal_seq_wal_order_matches_reference_memtable(tmp_path, interpreters, rust_binary, delete_first):
    root = tmp_path / "equal_seq.db"
    code = """
import json, sys
from pathlib import Path
import pyarrow as pa
from milvus_lite.schema.types import CollectionSchema, FieldSchema, DataType
from milvus_lite.schema.arrow_builder import build_wal_data_schema, build_wal_delta_schema
from milvus_lite.storage.memtable import MemTable
root, fixture, delete_first = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3] == 'true'
collection = root / 'collections' / 'c'
wal = collection / 'wal'
wal.mkdir(parents=True)
(collection / 'schema.json').write_bytes(fixture.read_bytes())
schema = CollectionSchema(fields=[
    FieldSchema('id', DataType.INT64, is_primary=True),
    FieldSchema('vector', DataType.FLOAT_VECTOR, dim=2),
])
data = pa.RecordBatch.from_pydict({'_seq': [7], '_partition': ['_default'], 'id': [1], 'vector': [[1.0, 0.0]]}, schema=build_wal_data_schema(schema))
delta = pa.RecordBatch.from_pydict({'id': [1], '_seq': [7], '_partition': ['_all']}, schema=build_wal_delta_schema(schema))
operations = [('delta', delta), ('data', data)] if delete_first else [('data', data), ('delta', delta)]
table = MemTable(schema)
for number, (kind, batch) in enumerate(operations, 1):
    path = wal / f'wal_{kind}_{number:06}.arrow'
    with pa.OSFile(str(path), 'wb') as sink:
        with pa.ipc.new_stream(sink, batch.schema) as writer:
            writer.write_batch(batch)
    (table.apply_delete if kind == 'delta' else table.apply_insert)(batch)
print(json.dumps({'live_count': int(table.get(1) is not None)}))
"""
    reference = json.loads(run([
        interpreters[PYTHONS[0]], "-c", code, str(root),
        str(REPO / "crates/core/tests/fixtures/schema_3x.json"),
        "true" if delete_first else "false",
    ], cwd=tmp_path))
    actual = rust(rust_binary, "inspect", root)
    assert actual["live_count"] == reference["live_count"]


def test_lazy_open_reports_truncated_wal_prefix(tmp_path, interpreters):
    root = tmp_path / "truncated.db"
    reference_create(interpreters[PYTHONS[0]], root, flush=False)
    wal = root / "collections" / "c" / "wal" / "wal_data_000001.arrow"
    content = wal.read_bytes()
    wal.write_bytes(content[:-20])
    code = """
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance
path = sys.argv[1]
client = MilvusClient(path)
client.query('c', output_fields=['count(*)'])
client.close()
server_manager_instance.release_server(path)
"""
    result = subprocess.run(
        [sys.executable, "-c", code, str(root)],
        cwd=tmp_path, text=True, capture_output=True, check=False,
    )
    assert result.returncode == 0, result.stderr
    assert "recovered only a validated WAL prefix" in result.stderr


def test_rust_index_spec_survives_reference_reopen(tmp_path, interpreters):
    root = tmp_path / "indexed.db"
    rust_code = """
import sys
from pymilvus import MilvusClient, DataType
from milvus_lite.server_manager import server_manager_instance
path = sys.argv[1]
schema = MilvusClient.create_schema()
schema.add_field('id', DataType.INT64, is_primary=True)
schema.add_field('vector', DataType.FLOAT_VECTOR, dim=2)
indexes = MilvusClient.prepare_index_params()
indexes.add_index(field_name='vector', index_type='FLAT', metric_type='L2')
client = MilvusClient(path)
client.create_collection('c', schema=schema, index_params=indexes)
client.insert('c', [{'id': 1, 'vector': [1.0, 0.0]}])
client.flush('c')
client.close()
server_manager_instance.release_server(path)
"""
    reference_code = """
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance
path = sys.argv[1]
client = MilvusClient(path)
assert client.describe_index('c', 'vector')['metric_type'] == 'L2'
client.load_collection('c')
assert client.query('c', output_fields=['count(*)']) == [{'count(*)': 1}]
client.close()
server_manager_instance.release_server(path)
"""
    run([sys.executable, "-c", rust_code, str(root)], cwd=tmp_path)
    run([interpreters[PYTHONS[0]], "-c", reference_code, str(root)], cwd=tmp_path)


def check_pyarrow(python, root, *, wal):
    code = """
import sys
from pathlib import Path
import pyarrow as pa
import pyarrow.parquet as pq
root = Path(sys.argv[1]) / "collections" / "c"
if sys.argv[2] == "wal":
    data = list(pa.ipc.open_stream(root / "wal" / "wal_data_000001.arrow"))
    delta = list(pa.ipc.open_stream(root / "wal" / "wal_delta_000001.arrow"))
    for batch in data + delta:
        batch.validate(full=True)
    assert len(data) == len(delta) == 1
    assert data[0].column("id").to_pylist() == [1, 2]
    assert delta[0].column("id").to_pylist() == [1]
else:
    data_path = next((root / "partitions" / "_default" / "data").glob("*.parquet"))
    delta_path = next((root / "partitions" / "_default" / "delta").glob("*.parquet"))
    data, delta = pq.read_table(data_path), pq.read_table(delta_path)
    assert data.column("id").to_pylist() == [2]
    assert delta.column("id").to_pylist() == [1]
print(pa.__version__)
"""
    return run([python, "-c", code, str(root), "wal" if wal else "parquet"], cwd=root.parent)


def rewrite_with_pyarrow(python, root, *, wal):
    code = """
import os, sys
from pathlib import Path
import pyarrow as pa
import pyarrow.parquet as pq
root = Path(sys.argv[1]) / "collections" / "c"
if sys.argv[2] == "wal":
    paths = sorted((root / "wal").glob("*.arrow"))
    for path in paths:
        reader = pa.ipc.open_stream(path)
        schema, batches = reader.schema, list(reader)
        tmp = path.with_suffix(".arrow.tmp")
        sink = pa.OSFile(str(tmp), "wb")
        writer = pa.ipc.new_stream(sink, schema)
        for batch in batches:
            writer.write_batch(batch)
        writer.close()
        sink.close()
        os.replace(tmp, path)
else:
    paths = sorted((root / "partitions" / "_default").glob("*/*.parquet"))
    for path in paths:
        table = pq.read_table(path)
        tmp = path.with_suffix(".tmp.parquet")
        pq.write_table(table, tmp, compression="snappy")
        os.replace(tmp, path)
"""
    run([python, "-c", code, str(root), "wal" if wal else "parquet"], cwd=root.parent)


def test_reference_flushed_directory_opens_in_rust(tmp_path, interpreters, rust_binary):
    root = tmp_path / "reference.db"
    reference_create(interpreters[PYTHONS[0]], root, flush=True)
    assert rust(rust_binary, "inspect", root) == {
        "live_count": 1,
        "ids": [2],
        "next_seq": 4,
        "next_wal_number": 3,
    }


def test_reference_crash_wal_opens_in_rust(tmp_path, interpreters, rust_binary):
    root = tmp_path / "reference-crash.db"
    reference_create(interpreters[PYTHONS[0]], root, flush=False)
    result = rust(rust_binary, "inspect", root)
    assert result["live_count"] == 1
    assert result["ids"] == [2]
    assert result["next_seq"] == 4


def test_rust_flushed_directory_opens_in_reference_and_pyarrow(tmp_path, interpreters, rust_binary):
    root = tmp_path / "rust-flushed.db"
    rust(rust_binary, "write-flushed", root)
    for name in PYTHONS[1:]:
        assert check_pyarrow(interpreters[name], root, wal=False).startswith("15." if name == PYTHONS[1] else "25.")
    result = reference_read_and_extend(interpreters[PYTHONS[0]], root)
    assert result["count"] == 1
    assert result["id2"] == [{"id": 2, "vector": [3.0, 4.0]}]
    reopened = rust(rust_binary, "inspect", root)
    assert reopened["ids"] == [2, 3]
    assert reopened["next_seq"] == 5


def test_rust_wal_opens_in_reference_and_pyarrow(tmp_path, interpreters, rust_binary):
    root = tmp_path / "rust-wal.db"
    rust(rust_binary, "write-wal", root)
    for name in PYTHONS[1:]:
        assert check_pyarrow(interpreters[name], root, wal=True).startswith("15." if name == PYTHONS[1] else "25.")
    result = reference_read_and_extend(interpreters[PYTHONS[0]], root)
    assert result["count"] == 1
    assert result["id2"] == [{"id": 2, "vector": [3.0, 4.0]}]
    assert rust(rust_binary, "inspect", root)["ids"] == [2, 3]


@pytest.mark.parametrize("python_name", PYTHONS[1:])
def test_pyarrow_written_wal_and_parquet_open_in_rust(
    tmp_path, interpreters, rust_binary, python_name
):
    python = interpreters[python_name]
    wal_root = tmp_path / "pyarrow-wal.db"
    rust(rust_binary, "write-wal", wal_root)
    rewrite_with_pyarrow(python, wal_root, wal=True)
    wal_result = rust(rust_binary, "inspect", wal_root)
    assert wal_result["live_count"] == 1
    assert wal_result["ids"] == [2]
    assert wal_result["next_seq"] == 4

    parquet_root = tmp_path / "pyarrow-parquet.db"
    rust(rust_binary, "write-flushed", parquet_root)
    rewrite_with_pyarrow(python, parquet_root, wal=False)
    parquet_result = rust(rust_binary, "inspect", parquet_root)
    assert parquet_result["live_count"] == 1
    assert parquet_result["ids"] == [2]
    assert parquet_result["next_seq"] == 4


@pytest.fixture(scope="session")
def directory_crash_binary():
    output = run([
        "cargo", "test", "-p", "milvus-lite-core", "--features", "fault-injection",
        "--test", "crash_recovery", "--no-run", "--message-format=json",
    ])
    artifacts = [json.loads(line) for line in output.splitlines()]
    executables = [
        item["executable"] for item in artifacts
        if item.get("reason") == "compiler-artifact"
        and item.get("target", {}).get("name") == "crash_recovery"
        and item.get("executable")
    ]
    assert len(executables) == 1
    return executables[0]


@pytest.mark.parametrize("stage", ["create-before", "create-after", "drop-before", "drop-after"])
def test_reference_reads_rust_directory_rename_crash_boundary(
    tmp_path, interpreters, directory_crash_binary, stage,
):
    root = tmp_path / "directory-crash.db"
    environment = dict(os.environ, MILVUS_LITE_DIRECTORY_CRASH=stage, MILVUS_LITE_CRASH_DIR=str(root))
    result = subprocess.run(
        [directory_crash_binary, "--exact", "collection_directory_rename_crashes_preserve_commit_boundary", "--nocapture"],
        cwd=tmp_path, env=environment, text=True, capture_output=True, check=False,
    )
    assert result.returncode == 88, result.stdout + result.stderr
    assert (root / "collections/c/schema.json").exists() == (stage in ("create-after", "drop-before"))
    code = """
import json, sys
from milvus_lite.db import MilvusLite
path, stage = sys.argv[1:]
db = MilvusLite(path)
expected = stage in ('create-after', 'drop-before')
assert db.has_collection('c') == expected
if expected:
    col = db.get_collection('c')
    assert col.num_entities == (1 if stage == 'drop-before' else 0)
    if stage == 'drop-before':
        assert col.get([1])[0]['v'] == [1.0, 0.0]
        assert db.describe_alias('old')['collection'] == 'c'
db.close()
print(json.dumps({'exists': expected}))
"""
    actual = json.loads(run([interpreters[PYTHONS[0]], "-c", code, str(root), stage], cwd=tmp_path))
    assert actual["exists"] == (stage in ("create-after", "drop-before"))
    assert not any((root / ".database-staging").iterdir())


@pytest.mark.parametrize("reader", ["rust", "reference"])
def test_staging_unlink_failure_does_not_block_healthy_reads_and_retries(
    tmp_path, interpreters, reader,
):
    root = tmp_path / "staging.db"
    reference_create(interpreters[PYTHONS[0]], root, flush=True)
    staging = root / ".database-staging"
    staging.mkdir(exist_ok=True)
    residue = staging / "orphan.tmp"
    residue.write_bytes(b"residue")
    python = sys.executable if reader == "rust" else interpreters[PYTHONS[0]]
    code = """
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance
path = sys.argv[1]
client = MilvusClient(path)
assert client.query('c', output_fields=['count(*)']) == [{'count(*)': 1}]
assert client.has_collection('c')
assert client.describe_collection('c')['collection_name'] == 'c'
assert client.get('c', ids=[2])[0]['id'] == 2
client.close()
server_manager_instance.release_server(path)
"""
    # 若以 root 运行而不能制造删除失败，显式失败，不能静默失去故障覆盖。
    staging.chmod(0o555)
    try:
        result = subprocess.run([python, "-c", code, str(root)], cwd=tmp_path, text=True, capture_output=True, check=False)
        assert result.returncode == 0, result.stderr
        assert residue.read_bytes() == b"residue"
        if reader == "rust":
            assert "database staging cleanup deferred" in result.stderr
    finally:
        staging.chmod(0o755)
    run([python, "-c", code, str(root)], cwd=tmp_path)
    assert not residue.exists()


def test_reference_staging_entry_stat_failure_does_not_block_open(tmp_path, interpreters):
    root = tmp_path / "staging-stat.db"
    reference_create(interpreters[PYTHONS[0]], root, flush=True)
    code = """
import errno, os, sys
from pathlib import Path
from unittest.mock import patch
from milvus_lite.db import MilvusLite
root = Path(sys.argv[1])
entry = root / '.database-staging' / 'unknown-type'
entry.write_bytes(b'residue')
original_stat = os.stat
observed = []
def fail_entry_stat(path, *args, **kwargs):
    if os.fspath(path) == str(entry):
        observed.append(path)
        raise OSError(errno.EIO, 'injected staging entry stat error')
    return original_stat(path, *args, **kwargs)
with patch('os.stat', side_effect=fail_entry_stat):
    db = MilvusLite(str(root))
    assert db.get_collection('c').num_entities == 1
    db.close()
assert observed, 'entry stat injection did not fire'
# isdir 将错误转换为 False 后，参照实现继续走 remove；Rust 的对应类型判定错误会拒绝打开并保留残留。
assert not entry.exists()
"""
    run([interpreters[PYTHONS[0]], "-c", code, str(root)], cwd=tmp_path)


def test_staging_root_symlink_is_rejected_without_deleting_external_files(tmp_path, interpreters):
    root = tmp_path / "staging-link.db"
    reference_create(interpreters[PYTHONS[0]], root, flush=True)
    staging = root / ".database-staging"
    staging.rmdir()
    outside = tmp_path / "outside"
    outside.mkdir()
    victim = outside / "victim"
    victim.write_bytes(b"keep")
    staging.symlink_to(outside, target_is_directory=True)
    rust_code = """
import sys
from milvus_lite.server_manager import server_manager_instance
assert server_manager_instance.start_and_get_uri(sys.argv[1]) is None
"""
    run([sys.executable, "-c", rust_code, str(root)], cwd=tmp_path)
    assert victim.read_bytes() == b"keep"
    reference_code = """
import sys
from milvus_lite.db import MilvusLite
db = MilvusLite(sys.argv[1])
assert db.get_collection('c').num_entities == 1
db.close()
"""
    run([interpreters[PYTHONS[0]], "-c", reference_code, str(root)], cwd=tmp_path)
    assert not victim.exists()


@pytest.mark.parametrize("reader", PYTHONS)
def test_every_wal_byte_cut_and_middle_corruption_matches_reader(
    tmp_path, interpreters, rust_binary, reader,
):
    import struct

    root = tmp_path / "wal-matrix"
    rust(rust_binary, "write-wal-matrix", root)
    ends = json.loads((root / "ends.json").read_text())
    cases, expected_counts = [], []
    originals = {}
    for kind in ("data", "delta"):
        original = (root / f"wal_{kind}_000001.arrow").read_bytes()
        originals[kind] = original
        for length in range(len(original) + 1):
            name = f"{kind}-cut-{length}.arrow"
            (root / name).write_bytes(original[:length])
            cases.append((name, kind))
            expected_counts.append(sum(end <= length for end in ends[kind]))
        for mode in ("bad-length", "zero-middle"):
            content = bytearray(original)
            first, second, _ = ends[kind]
            if mode == "bad-length":
                content[first + 4:first + 8] = b"\xff" * 4
            else:
                content[first:second] = b"\x00" * (second - first)
            name = f"{kind}-{mode}.arrow"
            (root / name).write_bytes(content)
            cases.append((name, kind))
            expected_counts.append(1)
    # 没有校验和，结构仍有效的向量载荷变化会被双方接受；不能误称能检出任意损坏。
    payload = bytearray(originals["data"])
    vector_at = payload.find(struct.pack("<4f", 1.0, 2.0, 3.0, 4.0))
    assert vector_at >= 0
    payload[vector_at + 3] ^= 0x80
    (root / "payload-flip.arrow").write_bytes(payload)
    cases.append(("payload-flip.arrow", "data"))
    expected_counts.append(3)
    (root / "cases.json").write_text(json.dumps(cases))
    actual = json.loads(run([str(rust_binary), "read-wal-matrix", str(root)]))
    assert [len(batches) for batches in actual] == expected_counts
    assert actual[-1][0]["vectors"][0] == -1.0
    code = """
import json, logging, struct, sys
from pathlib import Path
import pyarrow as pa
logging.disable(logging.CRITICAL)
root, mode = Path(sys.argv[1]), sys.argv[2]
if mode == 'reference':
    from milvus_lite.storage.wal import _read_wal_file
result = []
for name, kind in json.loads((root / 'cases.json').read_text()):
    path = root / name
    if mode == 'reference':
        batches = _read_wal_file(str(path))
    else:
        batches = []
        try:
            with pa.OSFile(str(path), 'rb') as source:
                for batch in pa.ipc.open_stream(source):
                    batch.validate(full=True)
                    batches.append(batch)
        except (pa.ArrowInvalid, OSError):
            pass
    summaries = []
    for batch in batches:
        summary = {'seq': batch.column('_seq').to_pylist(), 'ids': batch.column('id').to_pylist()}
        if kind == 'data':
            summary['vectors'] = [number for value in batch.column('vector').to_pylist() for number in value]
        summaries.append(summary)
    result.append(summaries)
print(json.dumps(result))
"""
    expected = json.loads(run([
        interpreters[reader], "-c", code, str(root),
        "reference" if reader == PYTHONS[0] else "pyarrow",
    ], cwd=tmp_path))
    assert actual == expected
    for kind, original in originals.items():
        assert (root / f"wal_{kind}_000001.arrow").read_bytes() == original


@pytest.mark.parametrize("operation", ["insert", "upsert", "delete"])
@pytest.mark.parametrize("existing", [False, True])
@pytest.mark.parametrize("reader", ["rust", "reference"])
@pytest.mark.parametrize("retained", [False, True])
def test_failed_enospc_batch_is_absent_after_rust_crash(
    tmp_path, interpreters, directory_crash_binary, operation, existing, reader, retained,
):
    root = tmp_path / "failed-wal.db"
    case = f"{operation}-{'existing' if existing else 'first'}-tail-{'retained' if retained else 'stop'}"
    env = dict(os.environ, MILVUS_LITE_FAILED_WAL_CASE=case, MILVUS_LITE_CRASH_DIR=str(root))
    result = subprocess.run([
        directory_crash_binary, "--exact",
        "failed_wal_append_then_process_exit_never_applies_failed_insert_upsert_or_delete", "--nocapture",
    ], cwd=tmp_path, env=env, text=True, capture_output=True, check=False)
    assert result.returncode == 89, result.stdout + result.stderr
    wal_files = list((root / "collections/c/wal").glob("wal_*.arrow"))
    assert bool(wal_files) == retained
    if retained:
        kind = "delta" if operation == "delete" else "data"
        assert any(path.name.startswith(f"wal_{kind}_") for path in wal_files)
        assert "WAL failure recovery flush" in result.stderr
        assert "BeforeReplace" in result.stderr
    expected = [1]
    if operation != "delete" or not existing:
        expected.append(2)
    if operation != "delete" and existing:
        expected.append(3)
    code = """
import json, sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance
path, encoded = sys.argv[1:]
expected = json.loads(encoded)
client = MilvusClient(path)
assert client.query('c', output_fields=['count(*)']) == [{'count(*)': len(expected)}]
rows = client.get('c', ids=[1,2,3,4,5,6], output_fields=['id','v'])
assert [row['id'] for row in rows] == expected
assert all(row['v'] == [1.0, 0.0] for row in rows)
assert list(client.query('c', filter='id in [1,2,3,4,5,6]', output_fields=['id','v'])) == list(rows)
hits = client.search('c', data=[[1.0, 0.0]], anns_field='v', limit=6, search_params={'metric_type': 'L2'}, output_fields=['v'])[0]
assert sorted(hit['id'] for hit in hits) == expected
assert all(hit['distance'] == 0.0 and hit['entity']['v'] == [1.0, 0.0] for hit in hits)
client.close()
server_manager_instance.release_server(path)
"""
    python = sys.executable if reader == "rust" else interpreters[PYTHONS[0]]
    run([python, "-c", code, str(root), json.dumps(expected)], cwd=tmp_path)


def test_reference_uncaught_wal_schema_error_fails_collection_access(tmp_path, interpreters):
    import struct

    root = tmp_path / "unsupported-wal.db"
    reference_create(interpreters[PYTHONS[0]], root, flush=False)
    path = root / "collections/c/wal/wal_data_000001.arrow"
    data = bytearray(path.read_bytes())

    # 定位 schema 首个 Int 字段的宽度，而不把当前编码器的某个绝对字节位置当成合同。
    def slot(table, field):
        vtable = table - struct.unpack_from("<i", data, table)[0]
        offset = struct.unpack_from("<H", data, vtable + 4 + field * 2)[0]
        assert offset != 0
        return table + offset

    def indirect(offset):
        return offset + struct.unpack_from("<I", data, offset)[0]

    message = 8 + struct.unpack_from("<I", data, 8)[0]
    schema = indirect(slot(message, 2))
    fields = indirect(slot(schema, 1))
    field = indirect(fields + 4)
    integer = indirect(slot(field, 3))
    width = slot(integer, 0)
    assert struct.unpack_from("<i", data, width)[0] == 64
    data[width] ^= 0x20
    assert struct.unpack_from("<i", data, width)[0] == 96
    path.write_bytes(data)
    code = """
import sys
import pyarrow as pa
from milvus_lite.db import MilvusLite
from milvus_lite.storage.wal import _read_wal_file
root = sys.argv[1]
db = MilvusLite(root)
for operation in (lambda: _read_wal_file(root + '/collections/c/wal/wal_data_000001.arrow'), lambda: db.get_collection('c')):
    try:
        operation()
    except pa.ArrowNotImplementedError as error:
        assert 'Integers with more than 64 bits not implemented' in str(error)
    else:
        raise AssertionError('schema corruption did not raise the uncaught exception')
db.close()
"""
    run([interpreters[PYTHONS[0]], "-c", code, str(root)], cwd=tmp_path)
    assert path.read_bytes() == data
    rust_code = """
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance
path = sys.argv[1]
client = MilvusClient(path)
assert client.query('c', output_fields=['count(*)']) == [{'count(*)': 0}]
assert list(client.get('c', ids=[1,2])) == []
client.close()
server_manager_instance.release_server(path)
"""
    result = subprocess.run(
        [sys.executable, "-c", rust_code, str(root)], cwd=tmp_path,
        text=True, capture_output=True, check=False,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert "recovered only a validated WAL prefix" in result.stderr
    assert "Int type with bit width of 96" in result.stderr
    assert "not supported" in result.stderr
