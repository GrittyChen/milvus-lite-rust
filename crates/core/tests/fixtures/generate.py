"""用参照实现 v3.2.1（43d1257）重建并核验本目录的小型存储格式样本。

在参照实现的独立 Python 环境中运行：
    <v3>/bin/python crates/core/tests/fixtures/generate.py --verify
    <v3>/bin/python crates/core/tests/fixtures/generate.py --output <临时目录>
"""

import argparse
import json
import shutil
import subprocess
import tempfile
from importlib.metadata import version
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

import milvus_lite
from milvus_lite.schema.arrow_builder import (
    build_data_schema,
    build_delta_schema,
    build_wal_data_schema,
    build_wal_delta_schema,
)
from milvus_lite.schema.persistence import save_schema
from milvus_lite.schema.types import CollectionSchema, DataType, FieldSchema
from milvus_lite.storage.data_file import write_data_file
from milvus_lite.storage.delta_file import write_delta_file
from milvus_lite.storage.manifest import Manifest
from milvus_lite.storage.snapshots import create_snapshot
from milvus_lite.storage.wal import WAL


HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3]


def check_reference():
    assert version("milvus-lite") == "3.2.1"
    assert pa.__version__ == "25.0.1", pa.__version__
    checkout = REPO / "references" / "milvus-lite"
    assert Path(milvus_lite.__file__).resolve().is_relative_to(checkout)
    commit = subprocess.check_output(["git", "-C", str(checkout), "rev-parse", "--short", "HEAD"], text=True).strip()
    assert commit == "43d1257", commit


def generate(output: Path):
    output.mkdir(parents=True, exist_ok=True)
    collection = output / "collection"
    collection.mkdir()
    schema = CollectionSchema(fields=[
        FieldSchema("id", DataType.INT64, is_primary=True),
        FieldSchema("vector", DataType.FLOAT_VECTOR, dim=2),
    ])
    save_schema(schema, "c", str(collection / "schema.json"))
    shutil.copyfile(collection / "schema.json", output / "schema_3x.json")

    manifest = Manifest(str(collection))
    manifest.save()
    shutil.copyfile(collection / "manifest.json", output / "manifest_3x.json")

    wal = WAL(
        str(collection / "wal"), build_wal_data_schema(schema),
        build_wal_delta_schema(schema), 1, sync_mode="batch",
    )
    for seq, pk in [(1, 10), (2, 20)]:
        batch = pa.RecordBatch.from_pydict({
            "_seq": [seq], "_partition": ["_default"],
            "id": [pk], "vector": [[1.0, 2.0]],
        }, schema=build_wal_data_schema(schema))
        wal.write_insert(batch)
    deleted = pa.RecordBatch.from_pydict({
        "id": [10], "_seq": [3], "_partition": ["_all"],
    }, schema=build_wal_delta_schema(schema))
    wal.write_delete(deleted)
    # 保留 WAL 样本，模拟未执行 close_and_delete 的进程退出。
    for writer, sink in ((wal._data_writer, wal._data_sink), (wal._delta_writer, wal._delta_sink)):
        writer.close()
        sink.close()
    shutil.copyfile(collection / "wal" / "wal_data_000001.arrow", output / "wal_data_3x.arrow")
    shutil.copyfile(collection / "wal" / "wal_delta_000001.arrow", output / "wal_delta_3x.arrow")

    partition = collection / "partitions" / "_default"
    data = pa.Table.from_pydict({
        "_seq": [1, 2], "id": [10, 20],
        "vector": [[1.0, 2.0], [3.0, 4.0]],
    }, schema=build_data_schema(schema))
    delta = pa.Table.from_pydict({"id": [10], "_seq": [3]}, schema=build_delta_schema(schema))
    data_rel = write_data_file(data, str(partition), 1, 2)
    delta_rel = write_delta_file(delta, str(partition), 3, 3)
    shutil.copyfile(partition / data_rel, output / "data_3x.parquet")
    shutil.copyfile(partition / delta_rel, output / "delta_3x.parquet")

    manifest.add_data_file("_default", data_rel)
    manifest.add_delta_file("_default", delta_rel)
    manifest.current_seq = 3
    manifest.active_wal_number = 2
    manifest.save()
    create_snapshot(
        str(collection), "c", "s1", "", "schema.json", "manifest.json",
        {"_default": [data_rel]}, {"_default": [delta_rel]}, {}, 3, manifest.version,
    )
    shutil.copyfile(collection / "snapshots" / "metadata" / "s1.json", output / "snapshot_3x.json")


def verify(output: Path):
    for name in ("schema_3x.json", "manifest_3x.json"):
        assert json.loads((output / name).read_bytes()) == json.loads((HERE / name).read_bytes()), name
    for name in ("wal_data_3x.arrow", "wal_delta_3x.arrow"):
        generated = list(pa.ipc.open_stream(output / name))
        checked_in = list(pa.ipc.open_stream(HERE / name))
        assert len(generated) == len(checked_in), name
        assert all(a.equals(b) for a, b in zip(generated, checked_in)), name
    for name in ("data_3x.parquet", "delta_3x.parquet"):
        assert pq.read_table(output / name).equals(pq.read_table(HERE / name)), name
    generated = json.loads((output / "snapshot_3x.json").read_bytes())
    checked_in = json.loads((HERE / "snapshot_3x.json").read_bytes())
    for variable in ("id", "created_at"):
        generated.pop(variable)
        checked_in.pop(variable)
    assert generated == checked_in, "snapshot_3x.json"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--verify", action="store_true")
    group.add_argument("--output", type=Path)
    args = parser.parse_args()
    check_reference()
    if args.verify:
        with tempfile.TemporaryDirectory(prefix="milvus-lite-fixtures-") as directory:
            output = Path(directory)
            generate(output)
            verify(output)
        print("fixtures verified against milvus-lite 3.2.1 (43d1257)")
    else:
        generate(args.output)


if __name__ == "__main__":
    main()
