"""0023 A 互通补充：本实现在清空、改名各步骤强制退出后的目录，由 3.x（v3.2.1，43d1257）打开并改动，本实现再打开收尾；
没有 manifest 的 partition key 集合清空后两边的分区与路由一致；改名前后 auto_id 序列在两边延续。

退出点由 crates/core/tests/{truncate,rename}.rs 的 fault-injection 子进程用例提供（环境变量选择阶段），这里只构建
并运行同一个测试二进制，不复制故障注入逻辑。
"""

import json
import os
import subprocess

import pytest
from test_cross_implementation import PYTHONS, REPO, alternate, interpreters, run  # noqa: F401  （fixture）

TRUNCATE_INTENT = "milvus-lite-rust.truncate-intent.json"
RENAME_INTENT = "milvus-lite-rust.rename-intent.json"


def build(test):
    output = run([
        "cargo", "test", "-p", "milvus-lite-core", "--features", "fault-injection",
        "--test", test, "--no-run", "--locked", "--message-format=json",
    ])
    artifacts = [json.loads(line) for line in output.splitlines()]
    executables = [
        item["executable"] for item in artifacts
        if item.get("reason") == "compiler-artifact"
        and item.get("target", {}).get("name") == test
        and item.get("executable")
    ]
    assert len(executables) == 1
    return executables[0]


@pytest.fixture(scope="module")
def truncate_binary():
    return build("truncate")


@pytest.fixture(scope="module")
def rename_binary():
    return build("rename")


def crash(binary, test, variable, stage, root, tmp_path, code=88):
    environment = dict(os.environ, **{variable: stage, "MILVUS_LITE_CRASH_DIR": str(root)})
    result = subprocess.run(
        [binary, "--exact", test, "--nocapture"],
        cwd=tmp_path, env=environment, text=True, capture_output=True, check=False,
    )
    assert result.returncode == code, f"{stage}\n{result.stdout}{result.stderr}"


TRUNCATE_TEST = "faults::process_exit_at_each_truncate_step_reopens_before_or_after"
COUNT = "client.query('c', filter='', output_fields=['count(*)'])"


@pytest.mark.parametrize("stage", [
    ("wal", 88), ("wal-sync", 88), ("intent", 88), ("intent-after", 88), ("manifest", 88), ("before-replace", 86),
])
def test_reference_opens_a_truncate_that_stopped_before_the_commit(tmp_path, interpreters, truncate_binary, stage):
    """清空在提交点之前退出（`wal/` 可能已移入 staging、意图可能已写）：3.x 看到清空前的数据并可继续写，
    本实现随后打开时因 manifest 已被 3.x 改写而只删除意图。"""
    stage, code = stage
    root = tmp_path / "before.db"
    crash(truncate_binary, TRUNCATE_TEST, "MILVUS_LITE_TRUNCATE_CRASH", stage, root, tmp_path, code)
    alternate(tmp_path, interpreters, "reference", root, [f"""
client.load_collection('c')
assert {COUNT} == [{{'count(*)': 2}}]
client.insert('c', [{{'id': 9, 'v': [1.0, 0.0]}}])
client.flush('c')
"""])
    alternate(tmp_path, interpreters, "rust", root, [f"""
client.load_collection('c')
assert {COUNT} == [{{'count(*)': 3}}]
"""])
    assert not (root / "collections/c" / TRUNCATE_INTENT).exists()


@pytest.mark.parametrize("stage", ["rebuild", "snapshots", "files", "intent-remove"])
def test_wal_only_writes_of_the_reference_after_a_committed_truncate_survive_a_kill(
    tmp_path, interpreters, truncate_binary, stage,
):
    """清空已提交、清理未完成（意图仍在）：3.x 打开后只写 WAL、不 flush 就被强杀；本实现再打开时 manifest 仍是
    候选，补做清理并回放 3.x 的 WAL，旧数据不回来。"""
    root = tmp_path / "committed.db"
    crash(truncate_binary, TRUNCATE_TEST, "MILVUS_LITE_TRUNCATE_CRASH", stage, root, tmp_path)
    base = root / "collections/c"
    intent = json.loads((base / TRUNCATE_INTENT).read_text())
    assert (base / "manifest.json").read_text() == intent["candidate_manifest"]
    alternate(tmp_path, interpreters, "reference", root, [f"""
import os
client.load_collection('c')
assert {COUNT} == [{{'count(*)': 0}}]
client.insert('c', [{{'id': 9, 'v': [1.0, 0.0]}}])
os._exit(0)
"""])
    # 3.x 只写了 WAL，没有改 manifest。
    assert (base / "manifest.json").read_text() == intent["candidate_manifest"]
    alternate(tmp_path, interpreters, "rust", root, [f"""
client.load_collection('c')
assert client.query('c', filter='', output_fields=['id']) == [{{'id': 9}}]
"""])
    assert not (base / TRUNCATE_INTENT).exists()
    assert not (base / "snapshots/metadata/s.json").exists()
    assert not any((root / ".database-staging").iterdir())
    alternate(tmp_path, interpreters, "reference", root, [f"""
client.load_collection('c')
assert {COUNT} == [{{'count(*)': 1}}]
"""])


RENAME_TEST = "faults::process_exit_at_each_rename_step_reopens_before_or_after"


@pytest.mark.parametrize("stage,renamed,schema_name,alias_target", [
    ("intent", False, "c", "c"),
    ("intent-after", False, "c", "c"),
    ("directory", False, "c", "c"),
    ("collections-sync", True, "c", "c"),
    ("schema", True, "c", "c"),
    ("schema-after", True, "d", "c"),
    ("aliases", True, "d", "c"),
    ("aliases-after", True, "d", "d"),
    ("intent-remove", True, "d", "d"),
    ("intent-remove-after", True, "d", "d"),
])
def test_reference_opens_a_rename_that_stopped_at_each_step(
    tmp_path, interpreters, rename_binary, stage, renamed, schema_name, alias_target,
):
    """改名各步骤强制退出后，3.x 按目录名打开（数据完整，名字取自 `schema.json`）；本实现随后打开时按意图收尾到
    改名之前或之后，`schema.json` 与别名一致。"""
    root = tmp_path / "rename.db"
    crash(rename_binary, RENAME_TEST, "MILVUS_LITE_RENAME_CRASH", stage, root, tmp_path)
    directory = "d" if renamed else "c"
    assert json.loads((root / f"collections/{directory}/schema.json").read_text())["collection_name"] == schema_name
    assert json.loads((root / "aliases.json").read_text()) == {"a": alias_target}
    alternate(tmp_path, interpreters, "reference", root, [f"""
assert client.list_collections() == ['{directory}', 'other']
client.load_collection('{directory}')
assert client.query('{directory}', filter='', output_fields=['count(*)']) == [{{'count(*)': 2}}]
"""])
    alternate(tmp_path, interpreters, "rust", root, [f"""
assert client.list_collections() == ['{'d' if renamed else 'c'}', 'other']
client.load_collection('{directory}')
assert client.query('{directory}', filter='', output_fields=['count(*)']) == [{{'count(*)': 2}}]
"""])
    assert not (root / RENAME_INTENT).exists()
    final = json.loads((root / f"collections/{directory}/schema.json").read_text())["collection_name"]
    assert final == directory
    assert json.loads((root / "aliases.json").read_text()) == {"a": directory}


KEY_SCHEMA = """
key = MilvusClient.create_schema()
key.add_field('id', DataType.INT64, is_primary=True)
key.add_field('k', DataType.INT64, is_partition_key=True)
key.add_field('vector', DataType.FLOAT_VECTOR, dim=2)
"""
KEY_AFTER_TRUNCATE = KEY_SCHEMA + """
parts = sorted(client.list_partitions('c'))
assert len(parts) == 17, parts
client.truncate_collection('c')
assert sorted(client.list_partitions('c')) == parts
client.insert('c', [{'id': i, 'k': i, 'vector': [1.0, 0.0]} for i in range(5)])
client.flush('c')
"""


def test_truncating_a_key_collection_without_manifest_keeps_the_same_partitions(tmp_path, interpreters):
    """从未 flush 的 partition key 集合（没有 manifest）：3.x 清空后写出 16 个桶，本实现的候选 manifest 只含磁盘上
    已有的分区、桶由注册表补回；两边列出的分区、之后的路由与查询结果一致，且互相可读。"""
    seed = tmp_path / "seed.db"
    alternate(tmp_path, interpreters, "reference", seed, [KEY_SCHEMA + """
client.create_collection('c', schema=key)
client.insert('c', [{'id': i, 'k': i, 'vector': [0.0, 1.0]} for i in range(5)])
import os
os._exit(0)
"""])
    assert not (seed / "collections/c/manifest.json").exists()
    results = {}
    for side in ("reference", "rust"):
        copy = tmp_path / f"{side}.db"
        subprocess.run(["cp", "-R", str(seed), str(copy)], check=True)
        alternate(tmp_path, interpreters, side, copy, [KEY_AFTER_TRUNCATE])
        results[side] = copy
    for side, other in (("reference", "rust"), ("rust", "reference")):
        alternate(tmp_path, interpreters, other, results[side], ["""
client.load_collection('c')
rows = sorted((r['id'], r['k']) for r in client.query('c', filter='', output_fields=['id', 'k']))
assert rows == [(i, i) for i in range(5)], rows
per_partition = {}
for p in client.list_partitions('c'):
    ids = [r['id'] for r in client.query('c', filter='', output_fields=['id'], partition_names=[p])]
    if ids:
        per_partition[p] = sorted(ids)
assert sum(len(v) for v in per_partition.values()) == 5
import json
open(path + '.routes.json', 'w').write(json.dumps(per_partition, sort_keys=True))
"""])
    # 两边写入后每个 id 落入同一个分区。
    routes = {side: json.loads((path.parent / f"{path.name}.routes.json").read_text()) for side, path in results.items()}
    assert routes["rust"] == routes["reference"] and len(routes["rust"]) > 1, routes
    layout = {
        side: sorted(
            str(p.relative_to(path / "collections/c"))
            for p in (path / "collections/c/partitions").glob("*/data/*.parquet")
            if p.is_file()
        ) for side, path in results.items()
    }
    assert [p.split("/data/")[0] for p in layout["rust"]] == [p.split("/data/")[0] for p in layout["reference"]]


@pytest.mark.parametrize("first", ["rust", "reference"])
def test_auto_id_sequence_continues_across_renames_in_both_implementations(tmp_path, interpreters, first):
    """改名前后 auto_id 的下一个值在两边延续：改名不重置序列（与清空相反），未 flush 的行与已落盘的行都计入。"""
    root = tmp_path / "auto.db"
    auto = """
auto = MilvusClient.create_schema(auto_id=True)
auto.add_field('id', DataType.INT64, is_primary=True, auto_id=True)
auto.add_field('vector', DataType.FLOAT_VECTOR, dim=2)
"""
    steps = [
        auto + """
client.create_collection('c', schema=auto)
assert client.insert('c', [{'vector': [1.0, 0.0]}] * 3)['ids'] == [1, 2, 3]
client.flush('c')
assert client.insert('c', [{'vector': [1.0, 0.0]}])['ids'] == [4]
client.rename_collection('c', 'd')
""",
        """
assert client.insert('d', [{'vector': [1.0, 0.0]}] * 2)['ids'] == [5, 6]
client.rename_collection('d', 'e')
""",
        """
assert client.insert('e', [{'vector': [1.0, 0.0]}])['ids'] == [7]
""",
    ]
    alternate(tmp_path, interpreters, first, root, steps)
