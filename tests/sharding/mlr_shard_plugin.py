"""分片插件：落盘收集结果、按计划筛选并逐项记录实际执行结果；不改变任何用例。

合同见 docs/decisions/0019-test-sharding.md。由 shard.py 以 `-p mlr_shard_plugin` 加载，
每个 pytest 进程只加载一次，状态因此放在模块级。
"""
import json
import signal
from pathlib import Path

import pytest

_collect_skipped = []
_collect_errors = []
_results = {}
_selection = None
_signals = {}


def pytest_addoption(parser):
    group = parser.getgroup("mlr-shard")
    group.addoption("--shard-collect-out", help="收集结果写入此 JSON")
    group.addoption("--shard-group-fixture", action="append", default=[],
                    help="使用该 fixture 的用例属于会话组，可重复")
    group.addoption("--shard-plan", help="计划文件")
    group.addoption("--shard-index", help="本进程执行的分片编号或 serial")
    group.addoption("--shard-record", help="实际执行结果写入此 JSON")


def _name(handler):
    return handler.name if isinstance(handler, signal.Handlers) else getattr(handler, "__name__", repr(handler))


def pytest_configure(config):
    # 后台启动继承 SIG_IGN 或被屏蔽时 Python 不安装 default_int_handler，信号用例会失真；记录供汇总核对。
    _signals.update(sigint=_name(signal.getsignal(signal.SIGINT)),
                    sigquit=_name(signal.getsignal(signal.SIGQUIT)),
                    blocked=sorted(sig.name for sig in signal.pthread_sigmask(signal.SIG_BLOCK, [])))


@pytest.hookimpl(hookwrapper=True)
def pytest_make_collect_report(collector):
    outcome = yield
    report = outcome.get_result()
    if report.failed:
        _collect_errors.append(report.nodeid)
    elif report.skipped:
        _collect_skipped.append(report.nodeid)


@pytest.hookimpl(trylast=True)
def pytest_collection_modifyitems(session, config, items):
    global _selection
    plan_path = config.getoption("--shard-plan")
    if plan_path is None:
        return
    plan = json.loads(Path(plan_path).read_text())
    collected = [item.nodeid for item in items]
    if collected != [row["nodeid"] for row in plan["collected"]]:
        raise pytest.UsageError("本次收集与计划全集不一致，拒绝执行")
    index = config.getoption("--shard-index")
    assigned = plan["serial"] if index == "serial" else plan["shards"][int(index)]
    wanted = set(assigned)
    if len(wanted) != len(assigned) or not wanted <= set(collected):
        raise pytest.UsageError("计划分片含重复或不存在的用例")
    # 保持收集顺序：会话组成员之间的相对顺序与串行执行相同。
    keep = [item for item in items if item.nodeid in wanted]
    drop = [item for item in items if item.nodeid not in wanted]
    if drop:
        config.hook.pytest_deselected(items=drop)
    items[:] = keep
    _selection = {"index": index, "assigned": len(assigned), "deselected": len(drop)}


def pytest_collection_finish(session):
    out = session.config.getoption("--shard-collect-out")
    if out is None:
        return
    fixtures = set(session.config.getoption("--shard-group-fixture"))
    # fixturenames 是依赖的传递闭包，经 diff 间接使用会话服务的用例同样命中。
    rows = [{"nodeid": item.nodeid, "group": bool(fixtures & set(getattr(item, "fixturenames", ())))}
            for item in session.items]
    Path(out).write_text(json.dumps({
        "collected": rows,
        "collect_skipped": _collect_skipped,
        "collect_errors": _collect_errors,
        "group_fixtures": sorted(fixtures),
    }, ensure_ascii=False, indent=1) + "\n")


def pytest_runtest_logreport(report):
    # 只有 call 阶段确实通过才记为 passed；未执行 call（如 --setup-plan）保持 not-run，
    # rerun 等未知结果原样保留，均不算通过。
    row = _results.setdefault(report.nodeid, {"outcome": "not-run", "duration": 0.0})
    row["duration"] += report.duration
    if row["outcome"] == "failed":
        return
    if report.outcome not in ("passed", "failed", "skipped"):
        row["outcome"] = report.outcome
    elif report.failed:
        row["outcome"] = "failed"
    elif hasattr(report, "wasxfail"):
        row["outcome"] = "xfailed" if report.skipped else "xpassed"
    elif report.skipped:
        row["outcome"] = "skipped"
    elif report.when == "call" and row["outcome"] == "not-run":
        row["outcome"] = "passed"


def pytest_sessionfinish(session, exitstatus):
    out = session.config.getoption("--shard-record")
    if out is None:
        return
    Path(out).write_text(json.dumps({
        "exitstatus": int(exitstatus),
        **_signals,
        "selection": _selection,
        "collect_skipped": _collect_skipped,
        "collect_errors": _collect_errors,
        "results": [{"nodeid": nodeid, **row} for nodeid, row in _results.items()],
    }, ensure_ascii=False, indent=1) + "\n")
