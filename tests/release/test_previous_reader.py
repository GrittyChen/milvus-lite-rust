"""上一版本兼容探针的前置核对：代码片段取自互通用例，上一版本身份不符时不写任何目录。"""

from pathlib import Path

import pytest

import previous_reader as pr

REPO = Path(__file__).resolve().parents[2]


def test_snippets_come_from_interop_tests():
    for test, name in [("test_nullable_scalars.py", "RUST_WRITE"), ("test_nullable_scalars.py", "READ_CHANGE"),
                       ("test_partition_key.py", "WRITE"), ("test_partition_key.py", "READ_APPEND")]:
        code = pr.snippet(REPO, test, name)
        assert "MilvusClient" in code and code in (REPO / "tests/storage_interop" / test).read_text()
    with pytest.raises(SystemExit, match="cannot locate"):
        pr.snippet(REPO, "test_partition_key.py", "MISSING")


def fake_python(tmp_path, output):
    path = tmp_path / "python"
    path.write_text(f"#!/bin/sh\necho '{output}'\n")
    path.chmod(0o755)
    return path


@pytest.mark.parametrize("output", ["0.3.0rc1 " + "a" * 64, "0.3.0rc2 " + "b" * 64])
def test_identity_mismatch_stops_before_writing(tmp_path, output):
    work = tmp_path / "work"
    old = fake_python(tmp_path, output)
    with pytest.raises(SystemExit, match="previous reader is"):
        pr.probe(REPO, work, tmp_path / "unused", old, "0.3.0rc2", "a" * 64)
    assert sorted(p.name for p in work.iterdir()) == ["old-identity.log", "old-identity.out"]


def test_missing_compaction_fails(tmp_path, monkeypatch):
    calls = []

    def run(work, name, python, code, args):
        calls.append(name)
        return "0.3.0rc2 " + "a" * 64 if name == "old-identity" else ""
    monkeypatch.setattr(pr, "run", run)
    monkeypatch.setattr(pr, "data_files", lambda database, partition: ["a.parquet", "b.parquet"])
    with pytest.raises(SystemExit, match="default-compacted did not compact"):
        pr.probe(REPO, tmp_path / "work", "new", "old", "0.3.0rc2", "a" * 64)
    assert calls[-1] == "write-default-compacted"
