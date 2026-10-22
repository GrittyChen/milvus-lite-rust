"""从当前源码构建、检查并在洁净环境安装候选；仅末尾输出通过记录。

依赖按调用方给出的完整约束文件安装；端到端全套经 0019 分片工具单片执行并核对收集数，不直接运行 pytest。
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import zipfile

from check_artifacts import check, check_binary


def run(command, *, cwd, log, env=None):
    with Path(log).open("w") as output:
        subprocess.run([str(v) for v in command], cwd=cwd, env=env, stdout=output, stderr=subprocess.STDOUT, check=True)


NATIVE = "import hashlib, pathlib, milvus_lite._native as n; print(hashlib.sha256(pathlib.Path(n.__file__).read_bytes()).hexdigest())"


def wheel_native(wheel):
    with zipfile.ZipFile(wheel) as bundle:
        members = [name for name in bundle.namelist() if name.startswith("milvus_lite/_native") and name.endswith((".so", ".pyd"))]
        if len(members) != 1:
            raise ValueError(f"expected one native module in wheel, found {members}")
        return hashlib.sha256(bundle.read(members[0])).hexdigest()


def installed_native(interpreter, cwd):
    return subprocess.run([str(interpreter), "-c", NATIVE], cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


def run_suite(repo, work, interpreter, expect):
    """经分片工具单片执行 tests/python：零 skip / xpass、收集数恰为 expect，工作目录在仓库外。"""
    plan, out, outside = work / "suite-plan", work / "suite-run", work / "outside"
    outside.mkdir()
    shard = repo / "tests/sharding/shard.py"
    run([sys.executable, shard, "plan", "--python", interpreter, "--suite", repo / "tests/python", "--cwd", outside,
         "--out", plan, "--shards", "1", "--order", "round-robin", "--expect", expect, "--expect-group", "0"],
        cwd=work, log=work / "suite-plan.log")
    run([sys.executable, shard, "run", "--plan", plan / "plan.json", "--out", out], cwd=work, log=work / "suite-run.log")
    summary = json.loads((out / "summary.json").read_text())
    counts = summary.get("counts", {})
    if summary.get("passed") is not True or counts.get("collected") != expect or counts.get("executed") != expect or counts.get("group") != 0:
        raise ValueError(f"suite summary does not show {expect} passed tests: {summary}")
    return counts


def execute(repo, work, python=sys.executable, sdk="3.0.2", suite=False, wheel=None, sdist=None, constraints=None, suite_expect=None):
    repo, work = Path(repo).resolve(), Path(work).resolve()
    if constraints is None or not Path(constraints).is_file():
        raise ValueError("an existing constraints file is required")
    constraints = Path(constraints).resolve()
    if suite and not (type(suite_expect) is int and suite_expect > 0):
        raise ValueError("--suite requires a positive expected test count")
    if (wheel is None) != (sdist is None):
        raise ValueError("wheel and sdist must be provided together")
    if wheel is not None:
        wheel, sdist = Path(wheel).resolve(), Path(sdist).resolve()
        if not wheel.is_file() or not sdist.is_file():
            raise ValueError("existing wheel and sdist files required")
    if work == repo or repo in work.parents:
        raise ValueError("work directory must be outside repository")
    work.mkdir(parents=True, exist_ok=False)
    env = os.environ.copy()
    env["MACOSX_DEPLOYMENT_TARGET"] = "11.0"
    for key in ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"]:
        if env.get(key):
            raise ValueError("release probe requires default CPU/build flags: " + key)
    if wheel is None:
        run([python, "-m", "maturin", "build", "--release", "--locked", "-o", work / "wheels"], cwd=repo, log=work / "build.log", env=env)
        run([python, "-m", "maturin", "sdist", "-o", work / "sdist"], cwd=repo, log=work / "sdist.log", env=env)
        wheels, sources = list((work / "wheels").glob("*.whl")), list((work / "sdist").glob("*.tar.gz"))
        if len(wheels) != 1 or len(sources) != 1:
            raise ValueError("exactly one wheel/sdist required")
        wheel, sdist = wheels[0], sources[0]
    else:
        # 检查与安装使用同一份本次副本，外部源文件之后的改写不改变已核对的字节。
        for source, directory in [(wheel, work / "wheels"), (sdist, work / "sdist")]:
            directory.mkdir()
            shutil.copyfile(source, directory / source.name)
        wheel, sdist = work / "wheels" / wheel.name, work / "sdist" / sdist.name
    identity = check(wheel, sdist, repo)
    identity["binary"] = check_binary(wheel, identity["tags"])
    native = wheel_native(wheel)
    interpreters = {}
    for mode in ["minimal", "sdk"]:
        venv = work / mode
        run(["uv", "venv", "--python", python, venv], cwd=work, log=work / (mode + "-venv.log"))
        interpreter = venv / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
        interpreters[mode] = interpreter
        packages = [wheel] + (["pymilvus==" + sdk, "pytest"] if mode == "sdk" else [])
        # 依赖只用 wheel；需要例外的本地构建产物由调用方经 UV_FIND_LINKS 提供（见 0021 第 11 条）。
        run(["uv", "pip", "install", "--python", interpreter, "--no-build", "--constraints", constraints, *packages], cwd=work, log=work / (mode + "-install.log"))
        installed = installed_native(interpreter, work)
        if installed != native:
            raise ValueError(f"{mode} environment loads native module {installed}, wheel contains {native}")
    suite_counts = None
    for mode, interpreter in interpreters.items():
        extra = ["--peer-python", interpreters["sdk"]] if mode == "minimal" else []
        run([interpreter, repo / "tests/packaging/install_smoke.py", "--work-dir", work / (mode + "-data"), "--mode", mode, *extra], cwd=work, log=work / (mode + "-smoke.log"))
        if mode == "sdk" and suite:
            suite_counts = run_suite(repo, work, interpreter, suite_expect)
    return {"artifacts": identity, "sdk": sdk, "constraints_sha256": hashlib.sha256(constraints.read_bytes()).hexdigest(),
            "native_sha256": native, "suite": suite_counts, "passed": True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work-dir", required=True, type=Path)
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--sdk", choices=["2.6.17", "3.0.2"], default="3.0.2")
    parser.add_argument("--python", default=sys.executable)
    parser.add_argument("--constraints", type=Path, required=True, help="complete pinned dependency set for the SDK environment")
    parser.add_argument("--suite", action="store_true")
    parser.add_argument("--suite-expect", type=int, help="exact number of tests/python tests the suite must collect and pass")
    parser.add_argument("--wheel", type=Path)
    parser.add_argument("--sdist", type=Path)
    args = parser.parse_args()
    if (args.wheel is None) != (args.sdist is None):
        parser.error("--wheel and --sdist must be provided together")
    print(json.dumps(execute(args.repo, args.work_dir, args.python, args.sdk, args.suite, args.wheel, args.sdist,
                             args.constraints, args.suite_expect)))


if __name__ == "__main__":
    main()
