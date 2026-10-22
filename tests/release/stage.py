"""验收步骤的阶段文件：每个通过的阶段写一份 {scope, stage, returncode, counts, environment}，由候选结果 job 合并。

环境按阶段矩阵规定的键自动采集；工具缺失时记为 absent，而不是省略键。
"""

import argparse
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys

import scorecard
from candidate import CandidateError, require


def probe(command):
    try:
        result = subprocess.run(command, capture_output=True, text=True, timeout=60)
    except (OSError, subprocess.TimeoutExpired):
        return "absent"
    text = (result.stdout or result.stderr).strip().splitlines()
    return text[0].strip() if result.returncode == 0 and text else "absent"


def orjson():
    """受控构建的 orjson 身份；工作流只在使用它的阶段设置 ORJSON_RECORD。"""
    path = os.environ.get("ORJSON_RECORD")
    if not path:
        return scorecard.NOT_USED
    record = json.loads(Path(path).read_text())
    wheel, tools = record["wheel"], record["toolchain"]
    return (f"{wheel['name']} sha256:{wheel['sha256']} max-glibc {wheel['max_glibc']} from sdist sha256:{record['sdist']['sha256']}"
            f" ({tools['rustc']}; {tools['maturin']})")


def pid1_image():
    """PID 1 探针写出的 identity.json 中实际使用的基础镜像；工作流只在 PID 1 阶段设置 PID1_IDENTITY。"""
    path = os.environ.get("PID1_IDENTITY")
    if not path:
        return scorecard.NOT_USED
    identity = json.loads(Path(path).read_text())
    return f"{identity['base_image']} id {identity['base_image_id']}"


def environment(scope):
    # 容器 job 看不到宿主的 ImageOS / ImageVersion，由工作流以 RUNNER_IMAGE 传入宿主镜像与容器镜像摘要。
    image = os.environ.get("RUNNER_IMAGE") or " ".join(filter(None, [os.environ.get("ImageOS"), os.environ.get("ImageVersion")])) \
        or "not-a-hosted-runner"
    values = {"runner_image": image, "rustc": probe(["rustc", "--version"]), "uv": probe(["uv", "--version"])}
    if scope.startswith("macos-"):
        values.update(macos=probe(["sw_vers", "-productVersion"]), xcode=probe(["xcodebuild", "-version"]))
    else:
        libc, version = platform.libc_ver()
        values.update(kernel=platform.release(), glibc=f"{libc} {version}".strip() or "absent",
                      docker=probe(["docker", "version", "--format", "{{.Server.Version}}"]), orjson_cp315=orjson(),
                      pid1_image=pid1_image())
    require(set(values) == set(scorecard.environment_keys(scope)), f"environment keys for {scope} differ")
    return values


def build_environment(scope, maturin):
    """构建机的工具版本；与验收阶段的环境分开记录，候选结果的 build_environment 按平台核对。"""
    env = environment(scope)
    values = {key: env[key] for key in ("runner_image", "kernel", "glibc", "macos", "xcode", "rustc", "uv") if key in env}
    values.update(cargo=probe(["cargo", "--version"]), maturin=probe([str(maturin), "--version"]))
    require(set(values) == set(scorecard.build_environment_keys(scope)), f"build environment keys for {scope} differ")
    require(all(v != "absent" for k, v in values.items() if k in ("rustc", "cargo", "maturin")), f"build tools missing: {values}")
    return values


def write(directory, scope, stage, counts, env=None):
    require(scope in scorecard.MATRIX and stage in scorecard.MATRIX[scope], f"unknown stage {scope}/{stage}")
    rules = scorecard.MATRIX[scope][stage]
    require(set(counts) == set(rules) and all(scorecard.count_ok(rules[k], counts[k]) for k in rules),
            f"{scope}/{stage} counts {counts} do not match {rules}")
    path = Path(directory) / f"{scope}--{stage}.json"
    item = {"scope": scope, "stage": stage, "returncode": 0, "counts": counts, "environment": env or environment(scope)}
    Path(directory).mkdir(parents=True, exist_ok=True)
    with path.open("x") as f:
        f.write(json.dumps(item, sort_keys=True, indent=2) + "\n")
    return path


def summary_counts(path):
    """分片工具的 summary.json 转为阶段计数；未通过的汇总不产生阶段文件。"""
    summary = json.loads(Path(path).read_text())
    require(summary.get("passed") is True, f"{path} did not pass: {summary.get('problems')}")
    counts = summary.get("counts", {})
    return {key: counts.get(key) for key in ("collected", "executed", "group")}


CARGO = re.compile(r"^test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored;", re.M)
TARGET = re.compile(r"^\s+(?:Running|Doc-tests) ", re.M)


def cargo_counts(path):
    """cargo test 日志中各测试目标的汇总：每个目标一份且为 ok；ignored 与 failed 原样计入，由阶段矩阵要求为 0。

    崩溃与故障注入用例会以子进程重跑本测试二进制，子进程的汇总行也写进同一日志、先于所属目标的汇总，
    因此每个目标只取其段内最后一行，不把子进程的计数算进顶层测试数。
    """
    text = Path(path).read_text(errors="replace")
    sections = TARGET.split(text)[1:]
    require(sections, f"{path} has no cargo test summary")
    results = []
    for section in sections:
        found = CARGO.findall(section)
        require(found, f"{path} has a test target without a summary: {section.splitlines()[0]}")
        results.append(found[-1])
    require(all(r[0] == "ok" for r in results), f"{path} has failing test summaries")
    return {"passed": sum(int(r[1]) for r in results), "failed": sum(int(r[2]) for r in results),
            "ignored": sum(int(r[3]) for r in results)}


def acceptance_counts(path):
    """run_acceptance 的末行 JSON：须为通过且带分片全套计数。"""
    result = json.loads(Path(path).read_text().strip().splitlines()[-1])
    require(result.get("passed") is True and isinstance(result.get("suite"), dict), f"{path} is not a passing suite run")
    return {key: result["suite"].get(key) for key in ("collected", "executed", "group")}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--dir", type=Path, required=True)
    parser.add_argument("--scope", required=True)
    parser.add_argument("--stage")
    source = parser.add_mutually_exclusive_group()
    source.add_argument("--counts", default="{}", help="JSON object of counts")
    source.add_argument("--summary", type=Path, help="sharding tool summary.json")
    source.add_argument("--cargo-log", type=Path, help="cargo test output")
    source.add_argument("--acceptance", type=Path, help="run_acceptance output with --suite")
    parser.add_argument("--build-env", type=Path, metavar="MATURIN",
                        help="instead of a stage, write build-env-<scope>.json for the build machine using this maturin")
    args = parser.parse_args(argv)
    if args.build_env:
        Path(args.dir).mkdir(parents=True, exist_ok=True)
        out = Path(args.dir) / f"build-env-{args.scope}.json"
        out.write_text(json.dumps(build_environment(args.scope, args.build_env), sort_keys=True, indent=2) + "\n")
        print(out)
        return 0
    if not args.stage:
        parser.error("--stage is required unless --build-env is given")
    try:
        if args.summary:
            counts = summary_counts(args.summary)
        elif args.cargo_log:
            counts = cargo_counts(args.cargo_log)
        elif args.acceptance:
            counts = acceptance_counts(args.acceptance)
        else:
            counts = json.loads(args.counts)
        print(write(args.dir, args.scope, args.stage, counts))
    except (CandidateError, json.JSONDecodeError, FileExistsError, OSError) as error:
        print(f"stage not recorded: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
