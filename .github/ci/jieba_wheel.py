"""对照层参照环境的 jieba 0.42.1（0036 决策 G；0020 与 0021 第 11 条登记的例外）。

jieba 在 PyPI 上只有 sdist，参照环境的固定依赖又只用 wheel 安装。本工具在独立 job 中从固定哈希的 sdist 构建一次
wheel，构建后端 setuptools 由 `requirements/build.txt` 固定；核对 wheel 里的 `jieba/dict.txt` 就是本仓
`crates/core/data/jieba/dict.txt` 的来源，并记录 wheel 的哈希。sdist 中 jieba 自带的测试句（`test/test.py`）一并
取出，供固定真值核验使用。各测试 job 安装这同一份 wheel，装好后以 check-installed 核对实际导入的模块就是它。
jieba 是参照侧的依赖，不进入本实现的发行物。
"""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import zipfile

VERSION = "0.42.1"
SDIST_SHA256 = "055ca12f62674fafed09427f176506079bc135638a14e23e25be909131928db2"
DICT_SHA256 = "7197c3211ddd98962b036cdf40324d1ea2bfaa12bd028e68faa70111a88e12a8"
WHEEL = f"jieba-{VERSION}-py3-none-any.whl"
REPO = Path(__file__).resolve().parents[2]
BUILD_CONSTRAINTS = REPO / ".github/ci/requirements/build.txt"


def pinned_setuptools(constraints=BUILD_CONSTRAINTS):
    pins = [line.split("==", 1)[1].strip() for line in Path(constraints).read_text().splitlines()
            if line.startswith("setuptools==")]
    if len(pins) != 1:
        raise SystemExit(f"{constraints} must pin setuptools exactly once, found {pins}")
    return pins[0]


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def build(sdist, uv, python, out):
    if sha256(Path(sdist).read_bytes()) != SDIST_SHA256:
        raise SystemExit(f"jieba sdist hash is not the pinned {SDIST_SHA256}")
    setuptools = pinned_setuptools()
    out = Path(out).resolve()
    out.mkdir(parents=True)
    subprocess.run([uv, "build", "--wheel", "--python", python, "--build-constraint", str(BUILD_CONSTRAINTS),
                    "-o", str(out / "dist"), str(sdist)], check=True)
    wheels = sorted(p.name for p in (out / "dist").iterdir() if p.name != ".gitignore")
    if wheels != [WHEEL]:
        raise SystemExit(f"expected {WHEEL}, found {wheels}")
    wheel = out / WHEEL
    (out / "dist" / WHEEL).rename(wheel)
    shutil.rmtree(out / "dist")
    with zipfile.ZipFile(wheel) as bundle:
        generator = [line for line in bundle.read(f"jieba-{VERSION}.dist-info/WHEEL").decode().splitlines()
                     if line.startswith("Generator: ")]
        # 构建约束失效时 uv 会静默换用最新的 setuptools；以 wheel 记录的构建后端为准核对。
        if generator != [f"Generator: setuptools ({setuptools})"]:
            raise SystemExit(f"wheel was built by {generator}, not the pinned setuptools {setuptools}")
        if sha256(bundle.read("jieba/dict.txt")) != DICT_SHA256:
            raise SystemExit("jieba/dict.txt in the wheel is not the pinned dictionary")
    with tarfile.open(sdist) as bundle:
        test = bundle.extractfile(f"jieba-{VERSION}/test/test.py").read()
    (out / "test.py").write_bytes(test)
    return {"sdist": {"version": VERSION, "sha256": SDIST_SHA256},
            "wheel": {"name": WHEEL, "sha256": sha256(wheel.read_bytes()), "generator": generator,
                      "dict_sha256": DICT_SHA256},
            "test_py_sha256": sha256(test)}


LOADED = "import json, jieba; print(json.dumps({'version': jieba.__version__, 'file': jieba.__file__}))"


def check_installed(record, python):
    """导入 jieba，核对版本与实际读取的词典；装进去的 wheel 由安装命令里的本地文件决定，这里核对结果。"""
    done = subprocess.run([python, "-I", "-W", "ignore", "-c", LOADED], capture_output=True, text=True, check=False)
    if done.returncode != 0:
        raise SystemExit(f"{python} cannot import jieba (exit {done.returncode}):\n{done.stderr}")
    loaded = json.loads(done.stdout)
    dictionary = Path(loaded["file"]).parent / "dict.txt"
    if loaded["version"] != VERSION or sha256(dictionary.read_bytes()) != record["wheel"]["dict_sha256"]:
        raise SystemExit(f"{python} loads jieba {loaded}, not the controlled build {record['wheel']['name']}")
    return loaded


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    b = commands.add_parser("build")
    b.add_argument("--sdist", type=Path, required=True)
    b.add_argument("--uv", required=True)
    b.add_argument("--python", required=True, help="interpreter that runs the setuptools build")
    b.add_argument("--out", type=Path, required=True)
    c = commands.add_parser("check-installed")
    c.add_argument("--record", type=Path, required=True)
    c.add_argument("--wheel", type=Path, required=True, help="the wheel that the test job installs")
    c.add_argument("--python", required=True, nargs="+", help="interpreters of the reference environments")
    args = parser.parse_args(argv)
    if args.command == "check-installed":
        record = json.loads(args.record.read_text())
        if sha256(args.wheel.read_bytes()) != record["wheel"]["sha256"]:
            raise SystemExit(f"{args.wheel} is not the wheel built by the jieba job")
        for python in args.python:
            print(json.dumps(check_installed(record, python), sort_keys=True))
        return
    record = build(args.sdist, args.uv, args.python, args.out)
    (args.out / "record.json").write_text(json.dumps(record, sort_keys=True, indent=2) + "\n")
    print(json.dumps(record, sort_keys=True))


if __name__ == "__main__":
    sys.exit(main())
