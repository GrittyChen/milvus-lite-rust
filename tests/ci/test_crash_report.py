"""容器 job 的崩溃报告（0020 公共约定 10）：只取本 job 时间标记之后的 core，缺 gdb 或无法解压时只打印原因。"""

import os
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / ".github" / "ci"))

import crash_report  # noqa: E402


def layout(tmp_path):
    temp, cores = tmp_path / "temp", tmp_path / "coredump"
    temp.mkdir()
    cores.mkdir()
    old = cores / "core.python.1001.boot.10.1"
    old.write_bytes(b"old")
    os.utime(old, (1000, 1000))
    (temp / "crash-marker").touch()
    os.utime(temp / "crash-marker", (2000, 2000))
    return temp, cores


def test_only_cores_after_the_marker_are_analysed(tmp_path, monkeypatch):
    temp, cores = layout(tmp_path)
    new = cores / "core.python.1001.boot.20.1"
    new.write_bytes(b"new")
    os.utime(new, (3000, 3000))
    seen = []
    monkeypatch.setattr(crash_report.shutil, "which", lambda name: "/usr/bin/gdb")
    monkeypatch.setattr(crash_report, "analyse", seen.extend)
    crash_report.container(temp, cores)
    assert seen == [new]


def test_without_marker_or_gdb_nothing_is_analysed(tmp_path, monkeypatch, capsys):
    temp, cores = layout(tmp_path)
    new = cores / "core.python.1001.boot.20.1"
    new.write_bytes(b"new")
    os.utime(new, (3000, 3000))
    monkeypatch.setattr(crash_report, "analyse", lambda cores: (_ for _ in ()).throw(AssertionError(cores)))
    monkeypatch.setattr(crash_report.shutil, "which", lambda name: None)
    crash_report.container(temp, cores)
    assert "gdb is not available" in capsys.readouterr().out
    (temp / "crash-marker").unlink()
    crash_report.container(temp, cores)
    assert "no crash marker" in capsys.readouterr().out


def test_compressed_core_needs_compression_zstd(tmp_path, monkeypatch, capsys):
    temp, cores = layout(tmp_path)
    new = cores / "core.python.1001.boot.20.1.zst"
    new.write_bytes(b"compressed")
    os.utime(new, (3000, 3000))
    monkeypatch.setattr(crash_report.shutil, "which", lambda name: "/usr/bin/gdb")
    monkeypatch.setattr(crash_report, "analyse", lambda cores: (_ for _ in ()).throw(AssertionError(cores)))
    # 与解释器版本无关地模拟缺少 compression.zstd（Python 3.14 以前没有）。
    monkeypatch.setitem(sys.modules, "compression", None)
    crash_report.container(temp, cores)
    assert "needs Python 3.14" in capsys.readouterr().out
