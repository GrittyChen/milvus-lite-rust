"""防止验收分析器漏掉成功系统调用的顺序或同步缺口。"""

import importlib.util
from pathlib import Path

import pytest


def calls():
    base = "/tmp/probe/collections/c"
    sync = lambda path: f"fsync(3<{path}>) = 0"
    files = ("schema.json", "manifest.json.tmp", "partitions/_default/data/data_000001_000001.parquet",
             "partitions/_default/data/data_000002_000002.parquet",
             "partitions/_default/delta/delta_000003_000003.parquet",
             "wal/wal_data_000002.arrow", "wal/wal_delta_000002.arrow")
    result = [sync(base + "/" + file) for file in files]
    result += [sync(directory) for directory in (base, "/tmp/probe/collections", "/tmp/probe", "/tmp",
                                                base + "/partitions", base + "/partitions/_default",
                                                base + "/partitions/_default/data", base + "/partitions/_default/delta", base + "/wal")]
    result += [f'rename("{base}/manifest.json.tmp", "{base}/manifest.json") = 0', sync(base)]
    for kind in ("data", "delta"):
        path = base + f"/wal/wal_{kind}_000002.arrow"
        result += [sync(path), f'unlink("{path}") = 0', sync(base + "/wal")]
    return result


def test_checker_requires_pre_commit_sync_and_each_unlink_directory_sync(tmp_path):
    spec = importlib.util.spec_from_file_location("checker", Path(__file__).with_name("check_strace.py"))
    checker = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(checker)
    prefix = tmp_path / "trace"
    write = lambda lines: prefix.with_suffix(".123").write_text("".join(f"{i + 1}.000000 {line}\n" for i, line in enumerate(lines)))
    original = calls()
    write(original)
    assert checker.check(str(prefix))["sync_after_each_unlink"]
    for removed in range(len(original)):
        write([line for i, line in enumerate(original) if i != removed])
        with pytest.raises(AssertionError):
            checker.check(str(prefix))
