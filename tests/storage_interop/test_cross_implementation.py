"""M2 存储文件矩阵：参照实现 v3.2.1（43d1257）与 Rust 双向读写。"""

import json
import os
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
    imported = run([
        result[PYTHONS[0]],
        "-c",
        "from pathlib import Path; import milvus_lite; print(Path(milvus_lite.__file__).resolve())",
    ])
    assert Path(imported).is_relative_to(REPO / "references" / "milvus-lite"), (
        f"reference interpreter imports {imported}, not the local checkout"
    )
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
