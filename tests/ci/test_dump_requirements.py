"""tests/dump 的 BulkWriter 依赖锁：macOS x86_64 上必须有 wheel（0037「依赖环境」）。

argon2-cffi-bindings 26.1.0 在 macOS 上只有 arm64 wheel，cryptography 自 49.0.0 起不再提供 macOS x86_64 wheel；
发行工作流在 macOS x86_64 上以 --no-build 安装。同时核对实际安装的 .txt 与约束来源 .in，任一边漏改都会被发现。
"""

from pathlib import Path
import re

import pytest

REQ = Path(__file__).resolve().parents[2] / ".github" / "ci" / "requirements"
CEILINGS = {"argon2-cffi-bindings": 26, "cryptography": 49}


@pytest.mark.parametrize("name", ["dump30", "dump26", "ref25"])
def test_bulk_writer_locks_stay_below_the_macos_x86_64_ceilings(name):
    lock = (REQ / f"{name}.txt").read_text()
    source = (REQ / f"{name}.in").read_text().splitlines()
    for package, ceiling in CEILINGS.items():
        assert f"{package}<{ceiling}" in source, f"{name}.in lacks {package}<{ceiling}"
        pins = re.findall(rf"^{re.escape(package)}==(\d+)\.", lock, re.MULTILINE)
        assert pins, f"{name}.txt does not pin {package}"
        assert all(int(major) < ceiling for major in pins), f"{name}.txt pins {package} {pins}"
