"""Linux CPython 3.15 测试环境的 orjson 受控构建（0021 第 11 条的唯一现场编译例外）。

orjson 3.13.0 的 cp315 Linux wheel 只有 manylinux_2_39，无法装进 glibc 2.28 的测试环境。本工具在固定的
manylinux_2_28 镜像内，从固定哈希的 sdist 以 `--locked` 构建一次，核对产物架构与最高 GLIBC 依赖不超过 2.28，
并记录构建身份。同一平台两条 SDK 的环境安装这同一份 wheel，装好后以 check-installed 核对实际加载的模块
就是这份构建产物。它是被测试的客户端依赖，不进入正式附件。
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import zipfile

VERSION = "3.13.0"
SDIST_SHA256 = "d1de5eb04485110c5da4c657e49168995d55e076b1ce60f1a042e254f4186c4f"
MAX_GLIBC = (2, 28)
MACHINES = {0x3E: "x86_64", 0xB7: "aarch64"}


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def elf_machine(data):
    if data[:4] != b"\x7fELF" or data[4] != 2:
        raise SystemExit("native module is not a 64-bit ELF file")
    order = "little" if data[5] == 1 else "big"
    machine = int.from_bytes(data[18:20], order)
    if machine not in MACHINES:
        raise SystemExit(f"unexpected ELF machine 0x{machine:x}")
    return MACHINES[machine]


def max_glibc(objdump_output):
    versions = {tuple(int(x) for x in v.split(".")) for v in re.findall(r"GLIBC_(\d+(?:\.\d+)+)", objdump_output)}
    if not versions:
        raise SystemExit("no GLIBC symbol versions found")
    return max(versions)


def tool_version(command):
    result = subprocess.run(command, capture_output=True, text=True, check=True)
    return (result.stdout or result.stderr).strip().splitlines()[0]


def build(sdist, python, maturin, work, out):
    if sha256(sdist) != SDIST_SHA256:
        raise SystemExit(f"orjson sdist hash {sha256(sdist)} is not the pinned {SDIST_SHA256}")
    work, out = Path(work).resolve(), Path(out).resolve()
    work.mkdir(parents=True)
    with tarfile.open(sdist) as bundle:
        bundle.extractall(work, filter="data")
    source = work / f"orjson-{VERSION}"
    env = {**os.environ, "CARGO_TARGET_DIR": str(work / "target")}
    for key in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
        if env.get(key):
            raise SystemExit(f"orjson build requires default build flags: {key}")
    subprocess.run([maturin, "build", "--release", "--locked", "--compatibility", "manylinux_2_28", "-i", python, "-o", out],
                   cwd=source, env=env, check=True)
    wheels = sorted(out.glob(f"orjson-{VERSION}-cp315-cp315-manylinux_2_28_*.whl"))
    if len(wheels) != 1 or len(list(out.iterdir())) != 1:
        raise SystemExit(f"expected one manylinux_2_28 cp315 wheel, found {sorted(p.name for p in out.iterdir())}")
    wheel = wheels[0]
    with zipfile.ZipFile(wheel) as bundle:
        natives = [n for n in bundle.namelist() if n.startswith("orjson/") and n.endswith(".so")]
        if len(natives) != 1:
            raise SystemExit(f"expected one native module, found {natives}")
        native = work / "native.so"
        native.write_bytes(bundle.read(natives[0]))
    machine = elf_machine(native.read_bytes())
    if not wheel.name.endswith(f"_{machine}.whl"):
        raise SystemExit(f"{wheel.name} does not match ELF machine {machine}")
    glibc = max_glibc(subprocess.run(["objdump", "-T", native], capture_output=True, text=True, check=True).stdout)
    if glibc > MAX_GLIBC:
        raise SystemExit(f"orjson requires GLIBC_{'.'.join(map(str, glibc))}, above 2.28")
    return {
        "sdist": {"version": VERSION, "sha256": SDIST_SHA256},
        "wheel": {"name": wheel.name, "sha256": sha256(wheel), "elf_machine": machine, "max_glibc": ".".join(map(str, glibc)),
                  "native": natives[0], "native_sha256": sha256(native)},
        "toolchain": {"python": tool_version([python, "--version"]), "maturin": tool_version([maturin, "--version"]),
                      "rustc": tool_version(["rustc", "--version"]), "cargo": tool_version(["cargo", "--version"]),
                      "cc": tool_version(["cc", "--version"])},
    }


LOADED = "import json, sys, orjson; print(json.dumps({'version': orjson.__version__, 'file': sys.modules['orjson.orjson'].__file__}))"


def check_installed(record, python):
    """在测试环境中导入 orjson，核对版本与实际加载的原生模块等于记录中的构建产物；只看安装器选了哪个 wheel 不够。"""
    loaded = json.loads(subprocess.run([python, "-I", "-c", LOADED], capture_output=True, text=True, check=True).stdout)
    wheel = record["wheel"]
    if loaded["version"] != VERSION or Path(loaded["file"]).name != Path(wheel["native"]).name \
            or sha256(loaded["file"]) != wheel["native_sha256"]:
        raise SystemExit(f"{python} loads orjson {loaded}, not the controlled build {wheel['name']}")
    return loaded


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    b = commands.add_parser("build")
    b.add_argument("--sdist", type=Path, required=True)
    b.add_argument("--python", required=True, help="CPython 3.15 interpreter in the manylinux_2_28 image")
    b.add_argument("--maturin", required=True)
    b.add_argument("--work-dir", type=Path, required=True)
    b.add_argument("--out", type=Path, required=True)
    b.add_argument("--record", type=Path, required=True)
    c = commands.add_parser("check-installed")
    c.add_argument("--record", type=Path, required=True)
    c.add_argument("--python", required=True, help="interpreter of the environment under test")
    args = parser.parse_args(argv)
    if args.command == "check-installed":
        print(json.dumps(check_installed(json.loads(args.record.read_text()), args.python), sort_keys=True))
        return
    record = build(args.sdist, args.python, args.maturin, args.work_dir, args.out)
    args.record.write_text(json.dumps(record, sort_keys=True, indent=2) + "\n")
    print(json.dumps(record, sort_keys=True))


if __name__ == "__main__":
    sys.exit(main())
