"""崩溃报告（0020 公共约定 10）：容器 job 只取本 job 时间标记之后的 core，缺 gdb 或无法解压时只打印原因；macOS 等到
报告集合静止后再读，先后写出的多份报告都收集，单份不等满上限，总等待不超过上限。"""

import os
from pathlib import Path
import sys

import pytest

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


class Clock:
    """假时钟：sleep 推进时间，报告按设定的出现时刻与可解析时刻变化。"""

    def __init__(self, reports):
        self.now, self.reports = 0.0, reports  # {名称: (出现时刻, 可解析时刻)}

    def sleep(self, seconds):
        self.now += seconds

    def listing(self):
        visible = sorted((appear, name) for name, (appear, _) in self.reports.items() if appear <= self.now)
        # 可解析之前签名随“写入进度”变化，模拟 ReportCrash 仍在写。
        return [(name, "done" if self.now >= self.reports[name][1] else self.now) for _, name in visible]

    def parse(self, name):
        return {} if self.now >= self.reports[name][1] else None


def wait(clock):
    return crash_report.wait_reports(clock.listing, clock.parse, clock=lambda: clock.now, sleep=clock.sleep)


def test_second_report_written_after_the_first_is_collected():
    # 注入演练 40173299185 的 macos-15-intel：第一份约 5 秒后写完，第二份更晚才出现。
    clock = Clock({"first.ips": (3, 5), "second.ips": (15, 18)})
    assert wait(clock) == ["first.ips", "second.ips"]
    assert clock.now == pytest.approx(18 + crash_report.REPORT_QUIET, abs=2)


def test_single_report_does_not_wait_for_the_deadline():
    clock = Clock({"only.ips": (3, 5)})
    assert wait(clock) == ["only.ips"]
    assert clock.now < 5 + crash_report.REPORT_QUIET + 2 < crash_report.REPORT_WAIT


def test_reports_that_keep_arriving_are_bounded_by_the_deadline():
    clock = Clock({f"r{i}.ips": (i * 10, i * 10 + 1) for i in range(20)})
    reports = wait(clock)
    assert clock.now == pytest.approx(crash_report.REPORT_WAIT, abs=0.01)
    assert reports == [f"r{i}.ips" for i in range(10)]


def test_unfinished_report_is_waited_for_until_the_deadline():
    clock = Clock({"writing.ips": (3, 10_000)})
    assert wait(clock) == ["writing.ips"]
    assert clock.now == pytest.approx(crash_report.REPORT_WAIT, abs=0.01)


def test_no_report_waits_the_full_deadline_as_before():
    clock = Clock({})
    assert wait(clock) == []
    assert clock.now == pytest.approx(crash_report.REPORT_WAIT, abs=0.01)


def test_rewrite_after_parsable_restarts_the_quiet_window():
    # 报告在 5 秒可解析，12 秒时又被改写（签名变化）：静止窗口从 12 秒重新计。
    clock = Clock({"rewritten.ips": (3, 5)})
    listing = clock.listing
    clock.listing = lambda: [(name, ("rewrite" if clock.now >= 12 else sig)) for name, sig in listing()]
    assert wait(clock) == ["rewritten.ips"]
    assert clock.now >= 12 + crash_report.REPORT_QUIET
    assert clock.now < 12 + crash_report.REPORT_QUIET + 2


def test_report_removed_while_reading_is_not_ready(tmp_path):
    assert crash_report.parsed(tmp_path / "gone.ips") is None


def test_report_removed_between_glob_and_stat_is_skipped(tmp_path, monkeypatch):
    kept, gone = tmp_path / "kept.ips", tmp_path / "gone.ips"
    for path in (kept, gone):
        path.write_text("header\n{}")
        os.utime(path, (3000, 3000))
    real_stat = Path.stat

    def stat(self, *args, **kwargs):
        # glob 已列出 gone.ips，stat 时它已被移走。
        if self.name == "gone.ips":
            raise FileNotFoundError(self)
        return real_stat(self, *args, **kwargs)

    monkeypatch.setattr(Path, "stat", stat)
    assert [path for path, _ in crash_report.report_listing(tmp_path, 2000)] == [kept]
