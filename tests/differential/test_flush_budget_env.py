"""flush 字节预算的环境变量：导入时校验与实际效果，逐项对照 3.x（0027）。"""

import glob
import json
import os
import subprocess
import sys

import pytest

from conftest import ENV

MEMTABLE = "MILVUS_LITE_MEMTABLE_MEMORY_LIMIT_BYTES"
WAL = "MILVUS_LITE_WAL_SIZE_LIMIT_BYTES"


def run(python, code, env, cwd):
    return subprocess.run([python, "-c", code, *env.pop("__args__", [])], cwd=cwd, env=env,
                          capture_output=True, text=True, timeout=120, check=False)


def clean_env(**overrides):
    env = {k: v for k, v in os.environ.items() if k not in (MEMTABLE, WAL)}
    env.update(overrides)
    return env


IMPORT = "import milvus_lite; print('imported')"


def last_line(result):
    lines = result.stderr.strip().splitlines()
    return lines[-1] if lines else ""


@pytest.mark.parametrize("name", [MEMTABLE, WAL])
@pytest.mark.parametrize("value", ["abc", "", "0", "-5", "1.5", "1e3", " ", "0x10"])
def test_invalid_budget_fails_at_import_like_3x(tmp_path, name, value):
    rust = run(sys.executable, IMPORT, clean_env(**{name: value}), tmp_path)
    ref = run(os.environ[ENV], IMPORT, clean_env(**{name: value}), tmp_path)
    assert rust.returncode == ref.returncode != 0
    assert last_line(rust) == last_line(ref)
    assert "must be a positive integer in bytes" in last_line(rust)


@pytest.mark.parametrize("value", ["1", " 12 ", "1_000", "+7", "99999999999999999999999"])
def test_valid_budget_forms_import_like_3x(tmp_path, value):
    env = {MEMTABLE: value, WAL: value}
    rust = run(sys.executable, IMPORT, clean_env(**env), tmp_path)
    ref = run(os.environ[ENV], IMPORT, clean_env(**env), tmp_path)
    assert (rust.returncode, rust.stdout) == (ref.returncode, ref.stdout) == (0, "imported\n"), (rust.stderr, ref.stderr)


WORKLOAD = """
import glob, json, os, sys
from pymilvus import MilvusClient, DataType
path = sys.argv[1]
c = MilvusClient(path)
s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=False)
s.add_field("id", DataType.INT64, is_primary=True)
s.add_field("v", DataType.FLOAT_VECTOR, dim=8)
c.create_collection("c", schema=s)
for _ in range(120):
    c.upsert("c", [{"id": i, "v": [1.0] * 8} for i in range(4)])
root = os.path.join(path, "collections", "c")
print(json.dumps({
    "count": c.query("c", filter="", output_fields=["count(*)"])[0]["count(*)"],
    "flushed": bool(glob.glob(os.path.join(root, "**", "data", "*.parquet"), recursive=True)),
}))
c.close()
"""


@pytest.mark.parametrize("env", [{MEMTABLE: "4096"}, {WAL: "8192"}], ids=["memtable", "wal"])
def test_small_budget_flushes_repeated_upserts_like_3x(tmp_path, env):
    results = []
    for label, python in (("rust", sys.executable), ("ref", os.environ[ENV])):
        environment = clean_env(**env)
        environment["__args__"] = [str(tmp_path / f"{label}.db")]
        result = run(python, WORKLOAD, environment, tmp_path)
        assert result.returncode == 0, result.stderr
        results.append(json.loads(result.stdout.strip().splitlines()[-1]))
    assert results[0] == results[1] == {"count": 4, "flushed": True}
