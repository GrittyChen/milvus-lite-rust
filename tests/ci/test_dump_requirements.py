"""tests/dump 的 BulkWriter 依赖锁：macOS x86_64 上必须有 wheel（0037「依赖环境」）。

argon2-cffi-bindings 26.1.0 在 macOS 上只有 arm64 wheel，cryptography 自 49.0.0 起不再提供 macOS x86_64 wheel；
发行工作流在 macOS x86_64 上以 --no-build 安装。同时核对实际安装的 .txt 与约束来源 .in，任一边漏改都会被发现。
"""

from pathlib import Path
import re

import pytest

REQ = Path(__file__).resolve().parents[2] / ".github" / "ci" / "requirements"
CEILINGS = {"argon2-cffi-bindings": 26, "cryptography": 49}


@pytest.mark.parametrize("name", ["dump30", "dump26", "ref25", "ref26"])
def test_bulk_writer_locks_stay_below_the_macos_x86_64_ceilings(name):
    lock = (REQ / f"{name}.txt").read_text()
    source = (REQ / f"{name}.in").read_text().splitlines()
    for package, ceiling in CEILINGS.items():
        assert f"{package}<{ceiling}" in source, f"{name}.in lacks {package}<{ceiling}"
        pins = re.findall(rf"^{re.escape(package)}==(\d+)\.", lock, re.MULTILINE)
        assert pins, f"{name}.txt does not pin {package}"
        assert all(int(major) < ceiling for major in pins), f"{name}.txt pins {package} {pins}"


def test_ref26_differs_from_ref25_only_in_pyarrow():
    """ref26 只为 0038 的空向量互通放宽 pyarrow 的截止日期；整体放宽日期会改动其余共同包（0037 已否决）。"""
    def pins(name):
        lines = (REQ / f"{name}.txt").read_text().splitlines()
        return [line for line in lines if line and not line.startswith(("#", " "))]
    ref25, ref26 = pins("ref25"), pins("ref26")
    assert [line for line in ref25 if not line.startswith("pyarrow==")] == [
        line for line in ref26 if not line.startswith("pyarrow==")]
    assert "pyarrow==25.0.1" in ref25 and "pyarrow==26.0.1" in ref26
    # 生成命令也只多放宽 pyarrow 一项（日期不限）；其余参数（含各锁共用的 pymilvus 放宽）与 ref25 相同。
    header25 = (REQ / "ref25.txt").read_text().splitlines()[1]
    header26 = (REQ / "ref26.txt").read_text().splitlines()[1]
    relaxed = re.findall(r" --exclude-newer-package pyarrow=\S+", header26)
    assert len(relaxed) == 1, header26
    assert header26.replace(relaxed[0], "").replace("ref26", "ref25") == header25, header26
    assert "--exclude-newer 2026-10-08T00:00:00Z " in header25, header25
