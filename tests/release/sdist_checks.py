"""从正式 sdist 独立验证源码构建与拒绝边界（0021 第 9 条）。

在仓库外解包，核对产品 / 构建输入与冻结身份一致；以根入口与 `-m` 入口构建，经 PEP 517 安装并做读写冒烟；
在 sdist 上执行 MSRV（默认与完整 FI）；free-threaded、PyPy 与 Windows 目标必须以构建脚本的明确原因失败。
构建使用 sdist 内独立的目标目录，不复用仓库的构建产物。每个通过的阶段写一份阶段文件（scope 为 sdist）。
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile

import stage

EXCLUDED_FROM_SDIST = ("clippy.toml", "rust-toolchain.toml")
REJECTIONS = {
    "reject-free-threaded": "free-threaded Python is not supported",
    "reject-pypy": "only GIL-enabled CPython is supported",
    "reject-windows": "Windows is not supported",
}


class Runner:
    def __init__(self, work, env):
        self.work, self.env, self.logs = work, env, work / "logs"
        self.logs.mkdir()

    def __call__(self, name, command, cwd, extra=None, expect_failure=None):
        log = self.logs / f"{name}.log"
        with log.open("w") as out:
            env = {k: v for k, v in {**self.env, **(extra or {})}.items() if v is not None}
            result = subprocess.run([str(c) for c in command], cwd=cwd, env=env, stdout=out, stderr=subprocess.STDOUT)
        text = log.read_text(errors="replace")
        if expect_failure is None:
            if result.returncode:
                raise SystemExit(f"{name} failed ({result.returncode}): {text[-4000:]}")
        elif result.returncode == 0 or f"milvus-lite-rust build rejected: {expect_failure}" not in text:
            raise SystemExit(f"{name} was not rejected for {expect_failure!r} (exit {result.returncode}): {text[-4000:]}")
        return text


def unpack(sdist, version, work):
    with tarfile.open(sdist) as bundle:
        tops = {name.split("/", 1)[0] for name in bundle.getnames()}
        if tops != {f"milvus_lite_rust-{version}"}:
            raise SystemExit(f"sdist must contain a single milvus_lite_rust-{version} directory, found {sorted(tops)}")
        bundle.extractall(work / "unpacked", filter="data")
    return work / "unpacked" / f"milvus_lite_rust-{version}"


def check_inputs(source, product):
    for name in EXCLUDED_FROM_SDIST:
        if (source / name).exists():
            raise SystemExit(f"sdist must not contain {name}")
    verified = 0
    for name, digest in sorted(product.items()):
        if name in EXCLUDED_FROM_SDIST:
            continue
        path = source / name
        if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != digest:
            raise SystemExit(f"sdist build input differs from frozen commit: {name}")
        verified += 1
    return verified


def checks(args):
    work = args.work_dir.resolve()
    repo = args.repo.resolve()
    # 各步骤在解包目录或工作目录中运行，路径参数一律取绝对路径（解释器不解析软链）。
    for name in ("sdist", "product_inputs", "constraints", "build_constraints", "stage_dir"):
        setattr(args, name, Path(os.path.abspath(getattr(args, name))))
    if work == repo or repo in work.parents:
        raise SystemExit("work directory must be outside repository")
    work.mkdir(parents=True)
    tool = os.path.abspath(args.tool_python)
    env = {**os.environ, "CARGO_TARGET_DIR": str(work / "target"), "PYO3_PYTHON": tool, "PYTHONDONTWRITEBYTECODE": "1"}
    for key in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
        if env.get(key):
            raise SystemExit(f"sdist checks require default build flags: {key}")
    run, record = Runner(work, env), lambda name, counts=None: stage.write(args.stage_dir, "sdist", name, counts or {})
    source = unpack(args.sdist, args.version, work)
    record("inputs", {"product_inputs": check_inputs(source, json.loads(args.product_inputs.read_text()))})

    for name, options in (("build-root", []), ("build-manifest", ["-m", "crates/py/Cargo.toml"])):
        out = work / name
        run(name, [tool, "-m", "maturin", "build", "--release", "--locked", *options, "-o", out], source)
        wheels = sorted(out.glob(f"milvus_lite_rust-{args.version}-*.whl"))
        if len(wheels) != 1:
            raise SystemExit(f"{name} produced {wheels}")
        record(name)

    venv = work / "pep517"
    python = venv / "bin/python"
    run("pep517-venv", ["uv", "venv", "--python", tool, venv], work)
    run("pep517-install", ["uv", "pip", "install", "--python", python, "--constraints", args.constraints,
                           "--build-constraints", args.build_constraints, source, f"pymilvus=={args.sdk}"], work)
    run("pep517-smoke", [python, repo / "tests/packaging/install_smoke.py", "--work-dir", work / "smoke-data", "--mode", "sdk"], work)
    installed = run("pep517-version", [python, "-c", "import importlib.metadata as m; print(m.version('milvus-lite-rust'))"], work)
    if installed.strip() != args.version:
        raise SystemExit(f"PEP 517 install reports {installed.strip()}")
    record("pep517-install")

    for name, features in (("msrv-default", []), ("msrv-fi", ["--features", "milvus-lite-py/fault-injection"])):
        run(name, ["cargo", f"+{args.msrv}", "check", "--workspace", "--all-targets", "--locked", *features], source,
            {"CARGO_TARGET_DIR": str(work / "target-msrv")})
        record(name)

    rejections = {"reject-free-threaded": ({"PYO3_PYTHON": os.path.abspath(args.free_threaded_python)}, []),
                  "reject-pypy": ({"PYO3_PYTHON": os.path.abspath(args.pypy_python)}, []),
                  "reject-windows": ({"PYO3_NO_PYTHON": "1"}, ["--target", "x86_64-pc-windows-msvc"])}
    for name, (extra, options) in rejections.items():
        extra = {**extra, "CARGO_TARGET_DIR": str(work / f"target-{name}")}
        if "PYO3_NO_PYTHON" in extra:
            extra["PYO3_PYTHON"] = None
        run(name, ["cargo", "check", "-p", "milvus-lite-py", "--locked", *options], source, extra, REJECTIONS[name])
        record(name)
    return {"verified": True, "work": str(work)}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--sdist", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--product-inputs", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--stage-dir", type=Path, required=True)
    parser.add_argument("--tool-python", type=Path, required=True, help="GIL CPython with maturin")
    parser.add_argument("--constraints", type=Path, required=True)
    parser.add_argument("--build-constraints", type=Path, required=True)
    parser.add_argument("--sdk", default="3.0.2")
    parser.add_argument("--msrv", default="1.89.0")
    parser.add_argument("--free-threaded-python", type=Path, required=True)
    parser.add_argument("--pypy-python", type=Path, required=True)
    args = parser.parse_args(argv)
    print(json.dumps(checks(args)))


if __name__ == "__main__":
    sys.exit(main())
