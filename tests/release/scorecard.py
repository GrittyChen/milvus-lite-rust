"""发行候选的阶段矩阵（0021 第 9、12 条）：固定平台、阶段、计数键与预期数量，是候选结果计数的唯一来源。

工作流以 `scorecard.py expect <scope> <stage> <key>` 取分片工具的 `--expect` 等预期值，候选结果按同一矩阵校验；
测试数量变化时只改这里，并随同一提交评审。
"""

import sys

FORMAT = 1
POSITIVE = ">0"
PLATFORMS = ("linux-x86_64", "linux-aarch64", "macos-arm64", "macos-x86_64")
SDKS = ("2.6.17", "3.0.2")
SUITE_PYTHONS = ("3.10", "3.15")
SMOKE_PYTHONS = ("3.11", "3.12", "3.13", "3.14")

PYTHON_SUITE = 485
DIFFERENTIAL = (431, 118)
INTEROP = 248
FI_RECOVERY = 5
PREVIOUS_READER_CASES = 6
NOT_USED = "not-used"

# PID 1 的基础镜像按公开仓库摘要固定（0021 第 10 条）；工作流从这里取，候选结果按这里核对。
PID1_IMAGES = {
    "linux-x86_64": "python@sha256:db6ee5170ebce0f4d165af242e8e77addc08bd9ffc37e8b1815752cf027aa4f8",
    "linux-aarch64": "python@sha256:f3902572f936b9e9a717644920d5c40d2c412d554625a9533bb1130f5afc1b8b",
}


def sharded(collected, group=0):
    return {"collected": collected, "executed": collected, "group": group}


RUST = {"passed": POSITIVE, "failed": 0, "ignored": 0}
ENVIRONMENT = {
    # orjson_cp315：Linux CPython 3.15 阶段使用的受控构建 orjson（0021 第 11 条）；pid1_image：PID 1 阶段实际使用的
    # 基础镜像摘要与镜像 ID（0021 第 10 条）。两者在其他阶段均为 not-used。
    "linux": ("runner_image", "kernel", "glibc", "docker", "rustc", "uv", "orjson_cp315", "pid1_image"),
    "macos": ("runner_image", "macos", "xcode", "rustc", "uv"),
}


# 构建机的环境（0021 第 5 条的工具版本）：四个正式 wheel 各在本平台构建，sdist 与 Linux x86_64 wheel 同机构建。
BUILD_ENVIRONMENT = {
    "linux": ("runner_image", "kernel", "glibc", "rustc", "cargo", "maturin", "uv"),
    "macos": ("runner_image", "macos", "xcode", "rustc", "cargo", "maturin", "uv"),
}


def build_environment_keys(platform):
    return BUILD_ENVIRONMENT["macos" if platform.startswith("macos-") else "linux"]


def platform_stages(platform):
    stages = {"artifacts-formal": {}, "artifacts-fi": {}, "rust-default": RUST, "rust-fi": RUST,
              "interop": sharded(INTEROP)}
    for python in SUITE_PYTHONS:
        for sdk in SDKS:
            stages[f"python-{python}-{sdk}"] = sharded(PYTHON_SUITE)
    for python in SMOKE_PYTHONS:
        stages[f"smoke-{python}"] = {}
    for sdk in SDKS:
        stages[f"fi-recovery-{sdk}"] = sharded(FI_RECOVERY)
        stages[f"differential-{sdk}"] = sharded(*DIFFERENTIAL)
    if platform.startswith("linux-"):
        stages.update({"lock": {}, "strace": {"successful_calls": POSITIVE}, "pid1": {}})
    if platform == "linux-x86_64":
        stages["previous-reader"] = {"cases": PREVIOUS_READER_CASES}
    return stages


SDIST_STAGES = {"inputs": {"product_inputs": POSITIVE}, "build-root": {}, "build-manifest": {}, "pep517-install": {},
                "msrv-default": {}, "msrv-fi": {}, "reject-free-threaded": {}, "reject-pypy": {}, "reject-windows": {}}

MATRIX = {**{platform: platform_stages(platform) for platform in PLATFORMS}, "sdist": SDIST_STAGES}


def environment_keys(scope):
    """sdist 在 Linux x86_64 的 manylinux 容器中检查，记录与 Linux 相同的环境键。"""
    return ENVIRONMENT["macos" if scope.startswith("macos-") else "linux"]


def count_ok(rule, value):
    if type(value) is not int:
        return False
    return value > 0 if rule == POSITIVE else value == rule


def main(argv):
    if len(argv) == 2 and argv[0] == "pid1-image":
        print(PID1_IMAGES[argv[1]])
        return
    if len(argv) != 4 or argv[0] != "expect":
        raise SystemExit("usage: scorecard.py expect <scope> <stage> <count-key> | pid1-image <scope>")
    rule = MATRIX[argv[1]][argv[2]][argv[3]]
    if rule == POSITIVE:
        raise SystemExit(f"{argv[1]}/{argv[2]}/{argv[3]} has no fixed expected value")
    print(rule)


if __name__ == "__main__":
    main(sys.argv[1:])
