"""job 失败后打出崩溃进程的原生调用栈（配合 crash_capture.sh）。只用于诊断，自身任何问题都不改变 job 结论。

段错误一类的崩溃概率很低时，事后无法复现；不在第一次发生时留下原生调用栈，就只能靠扩大重复去撞。
Linux：对 $RUNNER_TEMP/cores 中的 core 临时安装 gdb，先完整打印收到信号的线程，再按线程限额打印其余线程。
Linux 容器 job（设置 CRASH_CORE_DIR）：从挂入的宿主机 systemd-coredump 目录取本 job 内产生的 core，解压后用镜像自带的
gdb 同样打印；解压 zstd 需要 Python 3.14 的 compression.zstd。
macOS：从本 job 内产生的 ReportCrash 诊断报告（.ips）中提取异常、触发线程与其余线程的调用帧；多个进程先后崩溃时，
报告逐份异步写出，等到报告集合静止一段时间后才读取，避免只读到第一份。
"""

import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time

MAX_REPORTS = 3
OTHER_THREAD_FRAMES = 25
MAX_OTHER_LINES = 1500
REPORT_WAIT = 90
REPORT_QUIET = 20


def quiet(out):
    """去掉 gdb 读多线程 core 时逐个线程的 "[New LWP n]" 提示。"""
    return "\n".join(line for line in (out or "").splitlines() if not line.startswith("[New LWP "))


def run(command, timeout, env=None):
    try:
        result = subprocess.run(command, capture_output=True, text=True, timeout=timeout, env=env)
    except subprocess.TimeoutExpired:
        return None, f"timed out after {timeout}s: {' '.join(command)}"
    return result.returncode, result.stdout + result.stderr


def install_gdb():
    started = time.monotonic()
    command = ["sudo", "env", "DEBIAN_FRONTEND=noninteractive", "apt-get", "install", "-y", "-q", "--no-install-recommends", "gdb"]
    code, out = run(command, 300)
    if code != 0:
        # 托管镜像的包索引可能过期，刷新一次再装。
        run(["sudo", "apt-get", "update", "-q"], 300)
        code, out = run(command, 300)
    print(f"gdb install: exit {code}, {time.monotonic() - started:.0f}s")
    if code != 0:
        print(out[-2000:])
    return code == 0


def linux(temp):
    cores = sorted((temp / "cores").glob("core.*"), key=lambda p: p.stat().st_mtime)
    print(f"{len(cores)} core file(s): " + ", ".join(f"{p.name} ({p.stat().st_size} bytes)" for p in cores))
    if not cores:
        return
    if not install_gdb():
        return
    # 以 root 运行或降权的进程写出的 core 归 root 所有且只有属主可读。
    run(["sudo", "chmod", "a+r", *map(str, cores)], 60)
    analyse(cores)


def container(temp, folder):
    marker = temp / "crash-marker"
    if not marker.exists():
        print("no crash marker: no test step ran")
        return
    since = marker.stat().st_mtime
    found = sorted((p for p in folder.iterdir() if p.is_file() and p.stat().st_mtime >= since), key=lambda p: p.stat().st_mtime)
    print(f"{len(found)} core file(s) in {folder} since the marker: " + ", ".join(f"{p.name} ({p.stat().st_size} bytes)" for p in found))
    if not found:
        return
    if shutil.which("gdb") is None:
        print("gdb is not available in this image")
        return
    out = temp / "cores-extracted"
    out.mkdir(exist_ok=True)
    cores = []
    for path in found[:MAX_REPORTS]:
        if path.suffix != ".zst":
            cores.append(path)
            continue
        try:
            from compression import zstd
        except ImportError:
            print(f"cannot decompress {path.name}: needs Python 3.14 (compression.zstd)")
            return
        target = out / path.stem
        with zstd.open(path, "rb") as source, target.open("wb") as sink:
            shutil.copyfileobj(source, sink)
        cores.append(target)
    analyse(cores)
    if len(found) > MAX_REPORTS:
        print(f"{len(found) - MAX_REPORTS} later core file(s) not analysed")


def analyse(cores):
    # setup-python 的 LD_LIBRARY_PATH 会让 gdb 自身的 Python 加载错误的 libpython。
    env = {k: v for k, v in os.environ.items() if k != "LD_LIBRARY_PATH"}
    for core in cores[:MAX_REPORTS]:
        _, info = run(["file", "-b", str(core)], 60)
        match = re.search(r"execfn: '([^']+)'", info or "")
        target = [match.group(1), str(core)] if match else ["-c", str(core)]
        print(f"=== {core.name} ({(info or '').strip()})", flush=True)
        # 读入 core 后 gdb 的当前线程就是收到信号的线程：完整打印，不受其余线程截断影响。
        _, out = run(["gdb", "-q", "-batch", "-ex", "bt 200", *target], 180, env)
        print("--- signaled thread")
        print(quiet(out))
        _, out = run(["gdb", "-q", "-batch", "-ex", "info threads", "-ex", f"thread apply all bt {OTHER_THREAD_FRAMES}", *target], 180, env)
        lines = quiet(out).splitlines()
        print(f"--- all threads ({len(lines)} lines)")
        print("\n".join(lines[:MAX_OTHER_LINES]))
        if len(lines) > MAX_OTHER_LINES:
            print(f"... {len(lines) - MAX_OTHER_LINES} more lines omitted")
    if len(cores) > MAX_REPORTS:
        print(f"{len(cores) - MAX_REPORTS} later core file(s) not analysed")


def frames(thread, images, limit):
    rows = []
    for frame in thread.get("frames", [])[:limit]:
        index = frame.get("imageIndex", -1)
        image = images[index] if isinstance(index, int) and 0 <= index < len(images) else {}
        name = image.get("name") or image.get("path") or "?"
        symbol = frame.get("symbol")
        where = f"{symbol} + {frame.get('symbolLocation', 0)}" if symbol else f"+ {frame.get('imageOffset', 0):#x}"
        rows.append(f"    {name}  {where}")
    return rows


def parsed(report):
    """报告完整写出且正文为对象时返回正文，否则返回 None（ReportCrash 可能仍在写，或报告已被移走）。"""
    try:
        _, _, body = report.read_text(errors="replace").partition("\n")
    except FileNotFoundError:
        return None
    try:
        data = json.loads(body)
    except ValueError:
        return None
    return data if isinstance(data, dict) else None


def wait_reports(listing, parse, clock=time.monotonic, sleep=time.sleep, wait=REPORT_WAIT, quiet=REPORT_QUIET):
    """等到报告都可解析、且集合在 quiet 秒内没有新增或改写；总等待不超过 wait 秒。

    ReportCrash 逐个进程符号化并写报告，第二份可能在第一份写完之后才出现，只看“当时已有的都可解析”会漏掉它。
    listing() 返回按时间排序的 [(路径, 签名)]，签名在报告新增或改写时变化。没有报告时等满 wait（与此前相同）。
    """
    deadline = clock() + wait
    seen, settled_since = None, clock()
    while True:
        current = listing()
        if current != seen:
            seen, settled_since = current, clock()
        reports = [path for path, _ in current]
        ready = reports and all(parse(p) is not None for p in reports)
        if (ready and clock() - settled_since >= quiet) or clock() >= deadline:
            return reports
        sleep(min(2, max(0.0, deadline - clock())))


def report_listing(folder, since):
    """时间标记之后的报告，按修改时间排序；签名含大小与修改时间，报告仍在写时会变化。"""
    found = []
    for p in folder.glob("*.ips"):
        try:
            st = p.stat()
        except FileNotFoundError:
            # 列目录与 stat 之间被移走的报告不影响其余报告。
            continue
        if st.st_mtime >= since:
            found.append((st.st_mtime, p, (st.st_size, st.st_mtime_ns)))
    return [(p, signature) for _, p, signature in sorted(found)]


def macos(temp):
    marker = temp / "crash-marker"
    if not marker.exists():
        # 采集脚本没有执行过（例如在测试步骤之前失败），镜像上已有的报告不属于本 job。
        print("no crash marker: no test step ran")
        return
    since = marker.stat().st_mtime
    folder = Path.home() / "Library/Logs/DiagnosticReports"
    # ReportCrash 在进程崩溃后异步符号化并写报告，进程越大越慢。
    reports = wait_reports(lambda: report_listing(folder, since), parsed)
    print(f"{len(reports)} crash report(s) since the marker")
    for report in reports[:MAX_REPORTS]:
        print(f"=== {report.name}")
        try:
            data = parsed(report)
            if data is None:
                print("\n".join(report.read_text(errors="replace").splitlines()[:200]))
                continue
            print(json.dumps({k: data.get(k) for k in ("procPath", "exception", "termination", "faultingThread")}, ensure_ascii=False))
            images = data.get("usedImages", [])
            for index, thread in enumerate(data.get("threads", [])):
                mark = " (triggered)" if thread.get("triggered") else ""
                print(f"  thread {index} {thread.get('name', '')}{mark}")
                print("\n".join(frames(thread, images, 60 if thread.get("triggered") else 15)))
        except Exception as error:  # noqa: BLE001 一份报告异常不影响其余报告。
            print(f"cannot read {report.name}: {error!r}")
    if len(reports) > MAX_REPORTS:
        print(f"{len(reports) - MAX_REPORTS} later report(s) not shown")


def main():
    sys.stdout.reconfigure(line_buffering=True)
    temp = Path(os.environ.get("RUNNER_TEMP", "."))
    try:
        if os.environ.get("CRASH_CORE_DIR"):
            container(temp, Path(os.environ["CRASH_CORE_DIR"]))
        elif sys.platform.startswith("linux"):
            linux(temp)
        else:
            macos(temp)
    except Exception as error:  # noqa: BLE001 诊断失败不能改变 job 结论，也不能吞掉：打印后照常结束。
        print(f"crash report failed: {error!r}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
