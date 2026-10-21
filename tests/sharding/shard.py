"""候选测试的确定性分片：plan 收集并分配，run 并发执行后核对，verify 离线重做核对。

合同见 docs/decisions/0019-test-sharding.md。只改变执行方式，不改变用例；
任何遗漏、重复、额外、乱序、跳过或失败都使核对不通过。
"""
import argparse
import hashlib
import json
import os
import signal
import statistics
import subprocess
import time
import xml.etree.ElementTree as ET
from pathlib import Path

PLUGIN_DIR = Path(__file__).resolve().parent
PLUGIN_ARGS = ["-p", "no:cacheprovider", "-p", "mlr_shard_plugin"]
ORDERS = ("balanced", "round-robin", "reverse")
# 这些变量能在命令行之外改变收集或插件集合，使计划与执行不再可比。
FORBIDDEN_ENV = ("PYTEST_ADDOPTS", "PYTEST_PLUGINS")


class Interrupted(Exception):
    pass


def _handler_name(handler):
    return handler.name if isinstance(handler, signal.Handlers) else getattr(handler, "__name__", repr(handler))


def _env(extra=None):
    present = [name for name in FORBIDDEN_ENV if os.environ.get(name)]
    if present:
        raise SystemExit(f"环境中设置了 {present}，拒绝执行")
    env = dict(os.environ)
    env["PYTHONPATH"] = os.pathsep.join([str(PLUGIN_DIR)] + ([env["PYTHONPATH"]] if env.get("PYTHONPATH") else []))
    env.update(extra or {})
    return env


def _sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def collect(python, suite, cwd, out, group_fixtures, pytest_args):
    target = out / "collected.json"
    # 路径选项一律用 = 形式：pytest 在加载插件前的预解析会把空格形式的已有路径当作位置参数，改变 rootdir。
    argv = [str(python), "-m", "pytest", str(suite), "--collect-only", "-q", *PLUGIN_ARGS,
            f"--shard-collect-out={target}", *[f"--shard-group-fixture={name}" for name in group_fixtures],
            *pytest_args]
    env = _env()
    with (out / "collect.log").open("w") as log:
        code = subprocess.run(argv, cwd=cwd, env=env, stdout=log, stderr=subprocess.STDOUT).returncode
    if code != 0 or not target.exists():
        raise SystemExit(f"收集失败，退出码 {code}，见 {out / 'collect.log'}")
    data = json.loads(target.read_text())
    nodeids = [row["nodeid"] for row in data["collected"]]
    if data["collect_errors"] or data["collect_skipped"]:
        raise SystemExit(f"收集阶段出现错误或模块级 skip：{data['collect_errors'] + data['collect_skipped']}")
    if not nodeids or len(set(nodeids)) != len(nodeids):
        raise SystemExit("收集结果为空或含重复 nodeid")
    return data, target


def load_durations(paths):
    durations = {}
    for path in paths:
        for row in json.loads(Path(path).read_text())["results"]:
            durations[row["nodeid"]] = row["duration"]
    return durations


def assign(collected, shards, order, durations, serial):
    """按会话组与单个用例划分单元后分配；同一输入总得到同一结果。"""
    if shards < 1:
        raise ValueError("分片数至少为 1")
    nodeids = [row["nodeid"] for row in collected]
    group = [row["nodeid"] for row in collected if row["group"]]
    serial = list(serial)
    unknown = set(serial) - set(nodeids)
    if unknown or len(set(serial)) != len(serial):
        raise ValueError(f"串行组含不存在或重复的用例：{sorted(unknown)}")
    if set(serial) & set(group):
        raise ValueError("会话组不可拆分，其成员不能单独列入串行组")
    units, placed = [], False
    for nodeid in nodeids:
        if nodeid in serial:
            continue
        if nodeid in group:
            if not placed:
                units.append(group)
                placed = True
        else:
            units.append([nodeid])
    plan = [[] for _ in range(shards)]
    if order == "balanced":
        known = list(durations.values())
        fallback = statistics.median(known) if known else 1.0
        weight = lambda unit: sum(durations.get(nodeid, fallback) for nodeid in unit)
        load = [0.0] * shards
        for unit in sorted(units, key=lambda unit: (-weight(unit), unit[0])):
            index = min(range(shards), key=lambda i: (load[i], i))
            plan[index].extend(unit)
            load[index] += weight(unit)
    elif order in ("round-robin", "reverse"):
        for i, unit in enumerate(units if order == "round-robin" else list(reversed(units))):
            plan[i % shards].extend(unit)
    else:
        raise ValueError(f"未知分配方式 {order}")
    # 分片内按收集顺序执行，计划也按收集顺序保存，verify 据此核对执行顺序。
    position = {nodeid: i for i, nodeid in enumerate(nodeids)}
    return ([sorted(shard, key=position.__getitem__) for shard in plan],
            sorted(serial, key=position.__getitem__))


def command_plan(args):
    out = Path(args.out).resolve()
    out.mkdir(parents=True, exist_ok=False)
    cwd = Path(args.cwd).resolve()
    data, collected_path = collect(args.python, args.suite, cwd, out, args.group_fixture, args.pytest_args)
    group_size = sum(row["group"] for row in data["collected"])
    if len(data["collected"]) != args.expect:
        raise SystemExit(f"收集数量 {len(data['collected'])} 与预期 {args.expect} 不符")
    if group_size != args.expect_group:
        raise SystemExit(f"会话组 {group_size} 项与预期 {args.expect_group} 不符，检查 --group-fixture")
    durations = load_durations(args.durations)
    shards, serial = assign(data["collected"], args.shards, args.order, durations, args.serial)
    plan = {
        "suite": str(args.suite), "cwd": str(cwd), "python": str(args.python), "pytest_args": args.pytest_args,
        "group_fixtures": data["group_fixtures"], "expect": args.expect, "expect_group": args.expect_group,
        "shard_count": args.shards, "order": args.order,
        "collected": data["collected"], "shards": shards, "serial": serial,
        "inputs": {"collected_sha256": _sha256(collected_path),
                   "durations": [{"path": str(Path(p).resolve()), "sha256": _sha256(p)} for p in args.durations]},
    }
    (out / "plan.json").write_text(json.dumps(plan, ensure_ascii=False, indent=1) + "\n")
    print(json.dumps({"plan": str(out / "plan.json"), "collected": len(data["collected"]), "group": group_size,
                      "shards": [len(s) for s in shards], "serial": len(serial)}, ensure_ascii=False))


def _processes(plan):
    rows = [(str(i), shard) for i, shard in enumerate(plan["shards"]) if shard]
    if plan["serial"]:
        rows.append(("serial", plan["serial"]))
    return rows


HANDLED = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)


def _child_signals(mask):
    def reset():
        # 驱动在 run 期间捕获 SIGINT，exec 会把被捕获的信号恢复为默认；SIGQUIT 不被捕获，
        # 继承的 SIG_IGN 会跨 exec 保留，须显式复位。
        signal.signal(signal.SIGQUIT, signal.SIG_DFL)
        # 启动期间驱动临时屏蔽了中断信号，子进程恢复驱动原有的屏蔽集，不继承这次临时屏蔽。
        signal.pthread_sigmask(signal.SIG_SETMASK, mask)
    return reset


def _launch(plan, plan_path, out, index, env, mask):
    tmpdir = out / f"shard-{index}-tmpdir"
    tmpdir.mkdir()
    argv = [plan["python"], "-m", "pytest", plan["suite"], *plan["pytest_args"], *PLUGIN_ARGS,
            f"--shard-plan={plan_path}", f"--shard-index={index}",
            f"--shard-record={out / f'shard-{index}.json'}",
            f"--junitxml={out / f'shard-{index}.xml'}", f"--basetemp={out / f'shard-{index}-basetemp'}"]
    log = (out / f"shard-{index}.log").open("w")
    try:
        process = subprocess.Popen(argv, cwd=plan["cwd"], env={**env, "TMPDIR": str(tmpdir)},
                                   stdout=log, stderr=subprocess.STDOUT,
                                   preexec_fn=_child_signals(mask), start_new_session=True)
    except BaseException:
        log.close()
        raise
    return {"index": index, "argv": argv, "tmpdir": str(tmpdir), "process": process, "log": log,
            "started": time.time()}


def _row(item, **extra):
    return {"index": item["index"], "pid": item["process"].pid, "argv": item["argv"], "tmpdir": item["tmpdir"],
            "returncode": item["process"].returncode, "started": item["started"], **extra}


def _wait(running, rows):
    pending = list(running)
    while pending:
        for item in list(pending):
            if item["process"].poll() is not None:
                item["log"].close()
                rows.append(_row(item, finished=time.time()))
                pending.remove(item)
        if pending:
            time.sleep(0.2)


def _killpg(process, sig):
    # 进程可能恰在检查与发信号之间退出；已退出或已回收的组不影响对其余分片的清理。
    try:
        os.killpg(process.pid, sig)
    except (ProcessLookupError, PermissionError):
        pass


def _terminate(running, rows):
    for item in running:
        if item["process"].poll() is None:
            _killpg(item["process"], signal.SIGTERM)
    deadline = time.time() + 10
    for item in running:
        try:
            item["process"].wait(timeout=max(0.0, deadline - time.time()))
        except subprocess.TimeoutExpired:
            _killpg(item["process"], signal.SIGKILL)
            item["process"].wait()
        item["log"].close()
        if not any(row.get("pid") == item["process"].pid for row in rows):
            rows.append(_row(item, finished=time.time(), terminated=True))


def command_run(args):
    if os.name == "nt":
        raise SystemExit("分片执行只支持 POSIX 平台")
    plan_path = Path(args.plan).resolve()
    plan = json.loads(plan_path.read_text())
    out = Path(args.out).resolve()
    out.mkdir(parents=True, exist_ok=False)
    env = _env()
    original = {sig: signal.getsignal(sig) for sig in HANDLED}
    mask = signal.pthread_sigmask(signal.SIG_BLOCK, [])
    rows, running = [], []

    def record():
        (out / "processes.json").write_text(json.dumps(
            {"driver_signals": {sig.name: _handler_name(h) for sig, h in original.items()}, "processes": rows},
            ensure_ascii=False, indent=1) + "\n")

    def interrupt(signum, frame):
        raise Interrupted(signal.Signals(signum).name)

    def launch(index):
        # 信号若落在 Popen 返回与登记之间，新分片会漏出清理范围；启动期间屏蔽，登记后再处理。
        signal.pthread_sigmask(signal.SIG_BLOCK, HANDLED)
        try:
            running.append(_launch(plan, plan_path, out, index, env, mask))
        finally:
            signal.pthread_sigmask(signal.SIG_SETMASK, mask)

    # 无论驱动继承了何种处理函数（包括后台启动的 SIG_IGN），中断都必须清理子进程组。
    for sig in HANDLED:
        signal.signal(sig, interrupt)
    try:
        # 一个分片失败不提前终止其他分片；串行组在全部分片结束后单独运行。
        # 逐个登记已启动的分片，启动中途失败时也能清理它们。
        for index, _ in _processes(plan):
            if index != "serial":
                launch(index)
        _wait(running, rows)
        if plan["serial"]:
            running = []
            launch("serial")
            _wait(running, rows)
        running = []
    except BaseException as error:
        _terminate(running, rows)
        rows.append({"index": "interrupted", "returncode": None, "reason": repr(error)})
        record()
        raise
    finally:
        for sig, handler in original.items():
            signal.signal(sig, handler)
    record()
    summary = verify(plan_path, out)
    print(json.dumps({k: summary[k] for k in ("passed", "counts", "problems")}, ensure_ascii=False))
    raise SystemExit(0 if summary["passed"] else 1)


def _junit(path):
    root = ET.parse(path).getroot()
    suites = [root] if root.tag == "testsuite" else list(root.iter("testsuite"))
    return {key: sum(int(s.get(key, 0)) for s in suites) for key in ("tests", "failures", "errors", "skipped")}


def _validate_record(record):
    # 记录由插件生成；类型不符视为损坏，避免例如 set("SIGINT") 这类按字符拆开的误判放行。
    lists = ("blocked", "collect_errors", "collect_skipped", "results")
    if (not isinstance(record, dict) or not isinstance(record.get("exitstatus"), int)
            or not all(isinstance(record.get(key), str) for key in ("sigint", "sigquit"))
            or not all(isinstance(record.get(key), list) for key in lists)
            or not all(isinstance(item, str) for key in lists[:3] for item in record[key])
            or not all(isinstance(row, dict) and isinstance(row.get("nodeid"), str)
                       and isinstance(row.get("outcome"), str) for row in record["results"])
            or not (record.get("selection") is None or isinstance(record["selection"], dict))):
        raise ValueError("执行记录结构不符")


def _check_process(plan, out, index, assigned, launched, problems):
    name = f"shard-{index}"
    row = launched.get(index)
    if row is None or row.get("returncode") != 0:
        problems.append(f"{name} 退出码 {None if row is None else row.get('returncode')}")
    record = json.loads((out / f"{name}.json").read_text())
    _validate_record(record)
    if record["exitstatus"] != 0:
        problems.append(f"{name} pytest 退出状态 {record['exitstatus']}")
    if (record["sigint"] != "default_int_handler" or record["sigquit"] != "SIG_DFL"
            or {"SIGINT", "SIGQUIT"} & set(record["blocked"])):
        problems.append(f"{name} SIGINT / SIGQUIT 处理为 {record['sigint']} / {record['sigquit']}，"
                        f"屏蔽 {record['blocked']}")
    selection = record["selection"] or {}
    if (selection.get("index") != index or selection.get("assigned") != len(assigned)
            or selection.get("assigned", 0) + selection.get("deselected", -1) != len(plan["collected"])):
        problems.append(f"{name} 筛选记录与计划不符：{selection}")
    if record["collect_errors"] or record["collect_skipped"]:
        problems.append(f"{name} 收集阶段出现错误或 skip")
    ran = [row["nodeid"] for row in record["results"]]
    if sorted(ran) != sorted(assigned):
        problems.append(f"{name} 实际执行与分配不符：缺少 {len(set(assigned) - set(ran))}，"
                        f"额外 {len(set(ran) - set(assigned))}，重复 {len(ran) - len(set(ran))}")
    elif ran != assigned:
        problems.append(f"{name} 执行顺序与计划不同")
    bad = [row for row in record["results"] if row["outcome"] != "passed"]
    if bad:
        problems.append(f"{name} 非通过结果 {len(bad)} 项：{[(r['nodeid'], r['outcome']) for r in bad[:5]]}")
    junit = _junit(out / f"{name}.xml")
    if junit != {"tests": len(assigned), "failures": 0, "errors": 0, "skipped": 0}:
        problems.append(f"{name} JUnit 计数 {junit} 与分配 {len(assigned)} 不符")
    return ran


def verify(plan_path, out):
    """任何输入缺失、损坏或不符都写出不通过的汇总；先覆盖旧汇总，门槛不会读到过期的通过结果。"""
    out = Path(out)
    summary_path = out / "summary.json"
    summary_path.write_text(json.dumps({"passed": False, "problems": ["核对未完成"]}, ensure_ascii=False) + "\n")
    problems, executed = [], []
    plan = None
    try:
        plan = json.loads(Path(plan_path).read_text())
        collected = [row["nodeid"] for row in plan["collected"]]
        if len(collected) != plan["expect"] or sum(row["group"] for row in plan["collected"]) != plan["expect_group"]:
            problems.append("计划的收集数量或会话组与其预期不符")
        launched = {row["index"]: row for row in json.loads((out / "processes.json").read_text())["processes"]}
        if "interrupted" in launched:
            problems.append(f"执行被中断：{launched['interrupted'].get('reason')}")
        for index, assigned in _processes(plan):
            try:
                executed.extend(_check_process(plan, out, index, assigned, launched, problems))
            except (OSError, ValueError, KeyError, TypeError, ET.ParseError) as error:
                problems.append(f"shard-{index} 输出缺失或损坏：{error!r}")
        if len(executed) != len(set(executed)):
            problems.append("同一用例被执行多次")
        if set(executed) != set(collected):
            problems.append(f"执行并集与收集全集不符：缺少 {len(set(collected) - set(executed))}，"
                            f"额外 {len(set(executed) - set(collected))}")
    except (OSError, ValueError, KeyError, TypeError) as error:
        problems.append(f"计划或进程记录缺失或损坏：{error!r}")
    summary = {"passed": not problems, "problems": problems, "counts": {"executed": len(executed)}}
    if isinstance(plan, dict):
        rows = plan.get("collected") or []
        summary["counts"].update(collected=len(rows), group=sum(bool(row.get("group")) for row in rows))
        summary.update({key: plan.get(key) for key in ("suite", "cwd", "python", "pytest_args", "group_fixtures",
                                                       "expect", "expect_group", "shard_count", "order")})
        summary["plan_file_sha256"] = _sha256(plan_path)
    summary_path.write_text(json.dumps(summary, ensure_ascii=False, indent=1) + "\n")
    return summary


def command_verify(args):
    summary = verify(Path(args.plan).resolve(), Path(args.out).resolve())
    print(json.dumps({k: summary[k] for k in ("passed", "counts", "problems")}, ensure_ascii=False))
    raise SystemExit(0 if summary["passed"] else 1)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    plan = sub.add_parser("plan", help="收集并生成确定性分片计划")
    plan.add_argument("--python", required=True)
    plan.add_argument("--suite", required=True)
    plan.add_argument("--cwd", default=".")
    plan.add_argument("--out", required=True, help="新目录，存放收集结果与计划")
    plan.add_argument("--shards", type=int, required=True)
    plan.add_argument("--order", choices=ORDERS, default="balanced")
    plan.add_argument("--group-fixture", action="append", default=[])
    plan.add_argument("--expect", type=int, required=True, help="收集数量，须等于串行基线")
    plan.add_argument("--expect-group", type=int, required=True, help="会话组用例数；无会话组时为 0")
    plan.add_argument("--durations", action="append", default=[], help="上一轮分片执行记录，可重复")
    plan.add_argument("--serial", action="append", default=[], help="串行组 nodeid；入组须有 0019 规定的证据")
    plan.add_argument("pytest_args", nargs="*", help="传给 pytest 的额外参数，放在 -- 之后")
    run = sub.add_parser("run", help="并发执行计划并核对")
    run.add_argument("--plan", required=True)
    run.add_argument("--out", required=True, help="新目录，存放各分片日志、记录与汇总")
    check = sub.add_parser("verify", help="对已有输出重做核对")
    check.add_argument("--plan", required=True)
    check.add_argument("--out", required=True)
    args = parser.parse_args(argv)
    {"plan": command_plan, "run": command_run, "verify": command_verify}[args.command](args)


if __name__ == "__main__":
    main()
