"""分片工具不能以遗漏、重复、额外、乱序、跳过或失败的用例输出通过，也不能拆开会话组或遗留子进程。"""
import json
import os
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

import pytest
import shard

SHARD = Path(shard.__file__)

CONFTEST = '''
import pytest

@pytest.fixture(scope="session")
def svc():
    return []
'''

# 会话组用例共享 svc 并断言执行顺序：被拆进不同进程或重排都会失败。
# 名字的字母序与收集顺序不同，按 nodeid 排序执行的实现也会失败。
GROUP = '''
def test_gc(svc):
    svc.append("c")
    assert svc == ["c"]

def test_ga(svc):
    svc.append("a")
    assert svc == ["c", "a"]

def test_gb(svc):
    svc.append("b")
    assert svc == ["c", "a", "b"]
'''

PLAIN = '''
import os
import tempfile
import time
from pathlib import Path
import pytest

MODE = os.environ.get("FAKE_MODE", "")

@pytest.mark.parametrize("value", ["a b", 'q"x', "[n >= 2]", "plain"], ids=lambda v: v)
def test_param(value):
    # 假套件只经分片工具运行：每个进程的临时目录必须是该分片独占的 TMPDIR。
    assert Path(tempfile.gettempdir()).name.endswith("-tmpdir")
    assert value

@pytest.mark.parametrize("n", range(6))
def test_number(n):
    if MODE == f"fail:{n}":
        assert False
    if MODE == f"skip:{n}":
        pytest.skip("构造的 skip")
    if MODE == f"xfail:{n}":
        pytest.xfail("构造的 xfail")
    if MODE == "slow":
        time.sleep(60)

@pytest.mark.xfail(MODE == "xpass", reason="构造的 xpass", strict=False)
def test_maybe_xpass():
    pass
'''

TOTAL, GROUP_SIZE = 14, 3


def make_suite(root):
    suite = root / "suite"
    suite.mkdir()
    (suite / "conftest.py").write_text(CONFTEST)
    (suite / "test_group.py").write_text(GROUP)
    (suite / "test_plain.py").write_text(PLAIN)
    return suite


def run_tool(*args, env=None, preexec_fn=None):
    return subprocess.run([sys.executable, str(SHARD), *map(str, args)], capture_output=True, text=True,
                          env={**os.environ, **(env or {})}, preexec_fn=preexec_fn)


def plan_args(root, suite, out, shards=3, order="balanced", extra=(), expect=TOTAL, group=GROUP_SIZE,
              fixtures=("svc",)):
    return ["plan", "--python", sys.executable, "--suite", suite, "--cwd", root, "--out", out, "--shards", shards,
            "--order", order, *[x for f in fixtures for x in ("--group-fixture", f)],
            "--expect", expect, "--expect-group", group, *extra]


def plan(root, suite, shards=3, order="balanced", extra=(), name=None):
    out = root / (name or f"plan-{order}-{shards}")
    result = run_tool(*plan_args(root, suite, out, shards, order, extra))
    assert result.returncode == 0, result.stdout + result.stderr
    return out / "plan.json"


def execute(root, plan_path, name="run", env=None, preexec_fn=None):
    out = root / name
    result = run_tool("run", "--plan", plan_path, "--out", out, env=env, preexec_fn=preexec_fn)
    return result, out, json.loads((out / "summary.json").read_text())


def records(out):
    return {p.stem.removeprefix("shard-"): json.loads(p.read_text())
            for p in out.glob("shard-*.json")}


def rows(pairs):
    return [{"nodeid": nodeid, "group": group} for nodeid, group in pairs]


def test_assign_is_deterministic_keeps_group_whole_and_covers_every_case():
    collected = rows([("t::a", False), ("t::g2", True), ("t::b", False), ("t::g1", True),
                      ("t::c", False), ("t::d", False)])
    durations = {"t::a": 5.0, "t::b": 1.0, "t::c": 1.0, "t::d": 1.0, "t::g1": 1.0, "t::g2": 1.0}
    for order in shard.ORDERS:
        first = shard.assign(collected, 3, order, durations, [])
        assert first == shard.assign(collected, 3, order, durations, [])
        flat = [nodeid for part in first[0] for nodeid in part]
        assert sorted(flat) == sorted(row["nodeid"] for row in collected)
        holder, = [part for part in first[0] if "t::g1" in part]
        assert holder.index("t::g2") < holder.index("t::g1")
    assert shard.assign(collected, 3, "round-robin", {}, [])[0] != shard.assign(collected, 3, "reverse", {}, [])[0]


def test_balanced_uses_durations_and_breaks_ties_by_nodeid():
    collected = rows([("t::a", False), ("t::b", False), ("t::c", False), ("t::d", False)])
    heavy = shard.assign(collected, 2, "balanced", {"t::c": 10.0, "t::a": 1.0, "t::b": 1.0, "t::d": 1.0}, [])[0]
    assert ["t::c"] in heavy
    tied = shard.assign(collected, 2, "balanced", {}, [])[0]
    assert tied == [["t::a", "t::c"], ["t::b", "t::d"]]


def test_assign_rejects_unknown_duplicate_or_group_serial_entries():
    collected = rows([("t::a", False), ("t::g", True)])
    for serial in (["t::missing"], ["t::a", "t::a"], ["t::g"]):
        with pytest.raises(ValueError):
            shard.assign(collected, 2, "balanced", {}, serial)
    with pytest.raises(ValueError):
        shard.assign(collected, 0, "balanced", {}, [])


def test_sharded_run_matches_collection_keeps_group_order_and_isolates_processes(tmp_path):
    suite = make_suite(tmp_path)
    plan_path = plan(tmp_path, suite, shards=3)
    data = json.loads(plan_path.read_text())
    nodeids = [row["nodeid"] for row in data["collected"]]
    assert len(nodeids) == TOTAL and sum(row["group"] for row in data["collected"]) == GROUP_SIZE
    assert {'suite/test_plain.py::test_param[q"x]', "suite/test_plain.py::test_param[a b]",
            "suite/test_plain.py::test_param[[n >= 2]]"} <= set(nodeids)
    result, out, summary = execute(tmp_path, plan_path)
    assert result.returncode == 0 and summary["passed"], summary
    assert summary["counts"] == {"executed": TOTAL, "collected": TOTAL, "group": GROUP_SIZE}
    assert summary["expect"] == TOTAL and summary["expect_group"] == GROUP_SIZE
    assert summary["group_fixtures"] == ["svc"] and summary["pytest_args"] == []
    assert summary["plan_file_sha256"] == shard._sha256(plan_path)
    holders = [r for r in records(out).values() if any(row["nodeid"].endswith("test_gc") for row in r["results"])]
    assert len(holders) == 1
    order = [row["nodeid"].rsplit("::", 1)[1] for row in holders[0]["results"] if "test_group.py" in row["nodeid"]]
    assert order == ["test_gc", "test_ga", "test_gb"]
    launched = json.loads((out / "processes.json").read_text())["processes"]
    assert len(launched) == 3
    for key in ("--basetemp=", "--shard-record=", "--junitxml="):
        values = [next(a for a in row["argv"] if a.startswith(key)) for row in launched]
        assert len(set(values)) == 3
    assert len({row["tmpdir"] for row in launched}) == 3
    assert all("no:cacheprovider" in row["argv"] for row in launched)


def test_serial_group_given_out_of_order_runs_in_collection_order(tmp_path):
    suite = make_suite(tmp_path)
    late, early = "suite/test_plain.py::test_number[3]", "suite/test_plain.py::test_number[1]"
    plan_path = plan(tmp_path, suite, shards=2, extra=("--serial", late, "--serial", early))
    assert json.loads(plan_path.read_text())["serial"] == [early, late]
    result, out, summary = execute(tmp_path, plan_path)
    assert result.returncode == 0 and summary["passed"], summary
    assert [row["nodeid"] for row in records(out)["serial"]["results"]] == [early, late]


def test_serial_group_runs_after_shards_and_counts_in_union(tmp_path):
    suite = make_suite(tmp_path)
    nodeid = "suite/test_plain.py::test_number[0]"
    result, out, summary = execute(tmp_path, plan(tmp_path, suite, shards=2, extra=("--serial", nodeid)))
    assert result.returncode == 0 and summary["passed"], summary
    launched = json.loads((out / "processes.json").read_text())["processes"]
    serial, = [row for row in launched if row["index"] == "serial"]
    assert serial["started"] >= max(row["finished"] for row in launched if row["index"] != "serial")
    assert [row["nodeid"] for row in records(out)["serial"]["results"]] == [nodeid]


@pytest.mark.parametrize("mode,outcome", [("fail:2", "failed"), ("skip:3", "skipped"),
                                          ("xfail:4", "xfailed"), ("xpass", "xpassed")])
def test_non_passing_outcome_fails_the_run(tmp_path, mode, outcome):
    suite = make_suite(tmp_path)
    result, _, summary = execute(tmp_path, plan(tmp_path, suite), env={"FAKE_MODE": mode})
    assert result.returncode == 1 and not summary["passed"]
    assert any("非通过结果" in p and outcome in p for p in summary["problems"]), summary


def test_tests_whose_call_phase_never_ran_are_not_passed(tmp_path):
    suite = make_suite(tmp_path)
    result, _, summary = execute(tmp_path, plan(tmp_path, suite, extra=("--", "--setup-plan")))
    assert result.returncode == 1
    assert any("非通过结果" in p and "not-run" in p for p in summary["problems"]), summary


@pytest.mark.parametrize("change", ["add", "remove"])
def test_collection_drift_after_planning_is_refused(tmp_path, change):
    suite = make_suite(tmp_path)
    plan_path = plan(tmp_path, suite)
    if change == "add":
        (suite / "test_new.py").write_text("def test_new():\n    pass\n")
    else:
        (suite / "test_group.py").unlink()
    result, out, summary = execute(tmp_path, plan_path)
    assert result.returncode == 1 and not summary["passed"]
    assert "拒绝执行" in "".join(p.read_text() for p in out.glob("shard-*.log"))


def test_plan_outside_suite_tree_keeps_rootdir(tmp_path_factory):
    # 计划与套件不在同一祖先链：空格形式的路径选项会让 pytest 把计划路径算进 rootdir。
    root = tmp_path_factory.mktemp("suite-root")
    suite = make_suite(root)
    elsewhere = tmp_path_factory.mktemp("plan-root")
    result = run_tool(*plan_args(root, suite, elsewhere / "plan"))
    assert result.returncode == 0, result.stderr
    result, _, summary = execute(elsewhere, elsewhere / "plan" / "plan.json")
    assert result.returncode == 0 and summary["passed"], summary


@pytest.mark.parametrize("variant,message", [
    ({"fixtures": ("svx",)}, "会话组 0 项与预期 3 不符"),
    ({"group": 0}, "会话组 3 项与预期 0 不符"),
    ({"expect": 15}, "与预期 15 不符"),
])
def test_plan_rejects_expectation_mismatches(tmp_path, variant, message):
    suite = make_suite(tmp_path)
    result = run_tool(*plan_args(tmp_path, suite, tmp_path / "p", **variant))
    assert result.returncode != 0 and message in result.stderr, result.stderr


def test_module_level_skip_is_rejected_at_planning(tmp_path):
    suite = make_suite(tmp_path)
    (suite / "test_skipped.py").write_text("import pytest\npytest.skip('模块级', allow_module_level=True)\n")
    result = run_tool(*plan_args(tmp_path, suite, tmp_path / "p"))
    assert result.returncode != 0 and "模块级 skip" in result.stderr


@pytest.mark.parametrize("name", shard.FORBIDDEN_ENV)
def test_hidden_pytest_options_from_environment_are_rejected(tmp_path, name):
    suite = make_suite(tmp_path)
    result = run_tool(*plan_args(tmp_path, suite, tmp_path / "p"), env={name: "-k number"})
    assert result.returncode != 0 and name in result.stderr


def test_ignored_signals_in_driver_are_reset_for_shards(tmp_path):
    suite = make_suite(tmp_path)

    def ignore():
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        signal.signal(signal.SIGQUIT, signal.SIG_IGN)

    result, out, summary = execute(tmp_path, plan(tmp_path, suite), preexec_fn=ignore)
    assert summary["passed"], summary
    assert json.loads((out / "processes.json").read_text())["driver_signals"]["SIGINT"] == "SIG_IGN"
    assert all(r["sigint"] == "default_int_handler" and r["sigquit"] == "SIG_DFL" and r["blocked"] == []
               for r in records(out).values())


def test_durations_from_previous_records_change_the_allocation(tmp_path):
    suite = make_suite(tmp_path)
    _, out, summary = execute(tmp_path, plan(tmp_path, suite, order="round-robin"))
    assert summary["passed"]
    timing = tmp_path / "timing.json"
    heavy = "suite/test_plain.py::test_number[5]"
    timing.write_text(json.dumps({"results": [{"nodeid": heavy, "duration": 1000.0}]}))
    paths = sorted(out.glob("shard-*.json")) + [timing]
    data = json.loads(plan(tmp_path, suite, extra=[x for p in paths for x in ("--durations", p)]).read_text())
    assert [heavy] in data["shards"]
    assert [row["path"] for row in data["inputs"]["durations"]] == [str(p) for p in paths]


def _wait_for_shards(out, count):
    deadline = time.time() + 30
    while time.time() < deadline:
        logs = list(out.glob("shard-*.log"))
        if len(logs) == count and all(p.stat().st_size for p in logs):
            return
        time.sleep(0.2)
    raise AssertionError("分片未按时启动")


def _gone(pid):
    try:
        os.killpg(pid, 0)
    except ProcessLookupError:
        return True
    return False


@pytest.mark.parametrize("sig", [signal.SIGINT, signal.SIGTERM, signal.SIGHUP])
def test_driver_signal_terminates_every_shard_and_records_failure(tmp_path, sig):
    suite = make_suite(tmp_path)
    plan_path = plan(tmp_path, suite)
    out = tmp_path / "run"
    driver = subprocess.Popen([sys.executable, str(SHARD), "run", "--plan", str(plan_path), "--out", str(out)],
                              env={**os.environ, "FAKE_MODE": "slow"}, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    _wait_for_shards(out, 3)
    time.sleep(1)
    driver.send_signal(sig)
    assert driver.wait(timeout=30) != 0
    launched = json.loads((out / "processes.json").read_text())["processes"]
    interrupted, = [row for row in launched if row["index"] == "interrupted"]
    assert sig.name in interrupted["reason"]
    pids = [row["pid"] for row in launched if "pid" in row]
    assert len(pids) == 3 and all(_gone(pid) for pid in pids)
    summary = shard.verify(plan_path, out)
    assert not summary["passed"] and any("执行被中断" in p for p in summary["problems"])


def test_launch_failure_midway_terminates_started_shards(tmp_path, monkeypatch):
    suite = make_suite(tmp_path)
    plan_path = plan(tmp_path, suite)
    monkeypatch.setenv("FAKE_MODE", "slow")
    real, started = shard._launch, []

    def flaky(*args):
        if started:
            raise BlockingIOError(35, "进程数达到上限")
        item = real(*args)
        started.append(item["process"].pid)
        return item

    monkeypatch.setattr(shard, "_launch", flaky)
    with pytest.raises(BlockingIOError):
        shard.main(["run", "--plan", str(plan_path), "--out", str(tmp_path / "run")])
    assert len(started) == 1 and _gone(started[0])
    launched = json.loads((tmp_path / "run" / "processes.json").read_text())["processes"]
    assert any(row["index"] == "interrupted" for row in launched)
    assert any(row.get("pid") == started[0] and row.get("terminated") for row in launched)


@pytest.fixture(scope="module")
def passing(tmp_path_factory):
    root = tmp_path_factory.mktemp("passing")
    suite = make_suite(root)
    plan_path = plan(root, suite)
    result, out, summary = execute(root, plan_path)
    assert result.returncode == 0 and summary["passed"], summary
    assert len(list(out.glob("shard-*.json"))) == 3
    return plan_path, out


def _edit_record(out, change, index=0):
    path = sorted(out.glob("shard-*.json"))[index]
    record = json.loads(path.read_text())
    change(record)
    path.write_text(json.dumps(record))


def tamper_missing(out):
    _edit_record(out, lambda r: r["results"].pop())


def tamper_duplicate(out):
    first = json.loads(sorted(out.glob("shard-*.json"))[0].read_text())
    _edit_record(out, lambda r: r["results"].append(first["results"][0]), index=1)


def tamper_extra(out):
    _edit_record(out, lambda r: r["results"].append(
        {"nodeid": "suite/test_plain.py::test_ghost", "outcome": "passed", "duration": 0}))


def tamper_order(out):
    _edit_record(out, lambda r: r["results"].reverse())


def tamper_sigint(out):
    _edit_record(out, lambda r: r.update(sigint="SIG_IGN"))


def tamper_sigquit(out):
    _edit_record(out, lambda r: r.update(sigquit="SIG_IGN"))


def tamper_blocked(out):
    _edit_record(out, lambda r: r.update(blocked=["SIGINT"]))


def tamper_exitstatus(out):
    _edit_record(out, lambda r: r.update(exitstatus=1))


def tamper_collect_error(out):
    _edit_record(out, lambda r: r.update(collect_errors=["suite/test_x.py"]))


def tamper_selection(out):
    _edit_record(out, lambda r: r["selection"].update(deselected=r["selection"]["deselected"] - 1))


def tamper_selection_text(out):
    _edit_record(out, lambda r: r.update(selection="0"))


def tamper_blocked_text(out):
    _edit_record(out, lambda r: r.update(blocked="SIGINT"))


def tamper_results_text(out):
    _edit_record(out, lambda r: r.update(results="passed"))


def tamper_sigint_null(out):
    _edit_record(out, lambda r: r.update(sigint=None))


def tamper_selection_null(out):
    _edit_record(out, lambda r: r.update(selection=None))


def tamper_outcome_missing(out):
    _edit_record(out, lambda r: r["results"][0].pop("outcome"))


def tamper_record_corrupt(out):
    path = sorted(out.glob("shard-*.json"))[0]
    path.write_text(path.read_text()[:40])


def tamper_record(out):
    sorted(out.glob("shard-*.json"))[0].unlink()


def tamper_returncode(out):
    path = out / "processes.json"
    data = json.loads(path.read_text())
    data["processes"][0]["returncode"] = 1
    path.write_text(json.dumps(data))


def tamper_processes_key(out):
    (out / "processes.json").write_text(json.dumps({"driver_signals": {}}))


def tamper_interrupted(out):
    path = out / "processes.json"
    data = json.loads(path.read_text())
    data["processes"].append({"index": "interrupted", "returncode": None, "reason": "构造"})
    path.write_text(json.dumps(data))


def tamper_junit(out):
    sorted(out.glob("shard-*.xml"))[0].unlink()


def tamper_junit_count(out):
    path = sorted(out.glob("shard-*.xml"))[0]
    path.write_text(path.read_text().replace('skipped="0"', 'skipped="1"', 1))


def tamper_junit_corrupt(out):
    sorted(out.glob("shard-*.xml"))[0].write_text("<testsuites")


@pytest.mark.parametrize("tamper,expected", [
    (tamper_missing, "实际执行与分配不符"), (tamper_duplicate, "同一用例被执行多次"),
    (tamper_extra, "执行并集与收集全集不符"), (tamper_order, "执行顺序与计划不同"),
    (tamper_sigint, "SIGINT / SIGQUIT"), (tamper_sigquit, "SIGINT / SIGQUIT"), (tamper_blocked, "屏蔽"),
    (tamper_exitstatus, "pytest 退出状态 1"), (tamper_collect_error, "收集阶段出现错误"),
    (tamper_selection, "筛选记录与计划不符"), (tamper_selection_null, "筛选记录与计划不符"),
    (tamper_selection_text, "输出缺失或损坏"), (tamper_blocked_text, "输出缺失或损坏"),
    (tamper_results_text, "输出缺失或损坏"), (tamper_sigint_null, "输出缺失或损坏"),
    (tamper_outcome_missing, "输出缺失或损坏"), (tamper_record_corrupt, "输出缺失或损坏"),
    (tamper_record, "输出缺失或损坏"), (tamper_returncode, "退出码 1"),
    (tamper_processes_key, "计划或进程记录缺失或损坏"), (tamper_interrupted, "执行被中断"),
    (tamper_junit, "输出缺失或损坏"), (tamper_junit_count, "JUnit 计数"), (tamper_junit_corrupt, "输出缺失或损坏"),
])
def test_verify_rejects_tampered_outputs_and_overwrites_old_summary(passing, tmp_path, tamper, expected):
    plan_path, out = passing
    copy = tmp_path / "copy"
    shutil.copytree(out, copy)
    assert shard.verify(plan_path, copy)["passed"]
    tamper(copy)
    summary = shard.verify(plan_path, copy)
    assert not summary["passed"] and any(expected in p for p in summary["problems"]), summary["problems"]
    assert json.loads((copy / "summary.json").read_text())["passed"] is False


def test_unexpected_verify_error_leaves_failed_summary(passing, tmp_path, monkeypatch):
    plan_path, out = passing
    copy = tmp_path / "copy"
    shutil.copytree(out, copy)
    assert shard.verify(plan_path, copy)["passed"]

    def broken(*args):
        raise RuntimeError("意外错误")

    monkeypatch.setattr(shard, "_check_process", broken)
    with pytest.raises(RuntimeError):
        shard.verify(plan_path, copy)
    assert json.loads((copy / "summary.json").read_text())["passed"] is False


def test_verify_with_unreadable_plan_writes_failed_summary(passing, tmp_path):
    _, out = passing
    copy = tmp_path / "copy"
    shutil.copytree(out, copy)
    broken = tmp_path / "plan.json"
    broken.write_text("{")
    summary = shard.verify(broken, copy)
    assert not summary["passed"] and json.loads((copy / "summary.json").read_text())["passed"] is False
