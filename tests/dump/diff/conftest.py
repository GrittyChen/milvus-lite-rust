"""dump 的差分：两侧分别建相同数据、运行各自的 dump，比较产物（0037「测试」）。

3.x 装在另一个 venv 里，由 MILVUS_LITE_3X_PYTHON 指定（同 tests/differential），且须装有 pymilvus[bulk_writer]。
没有指定时直接报错，不跳过。
"""

import os
import subprocess

import pytest

ENV = "MILVUS_LITE_3X_PYTHON"
# 对照版本（AGENTS.md「参照实现」），升级时与 tests/differential/conftest.py 同步修改。
REF_VERSION = "3.2.2"


def pytest_configure(config):
    python = os.environ.get(ENV)
    if not python:
        raise pytest.UsageError(f"{ENV} must point to the python of a venv with milvus-lite 3.x installed")
    probe = subprocess.run(
        [python, "-c", "import milvus_lite, pymilvus.bulk_writer; print(milvus_lite.__version__)"],
        capture_output=True, text=True, check=False,
    )
    if probe.returncode != 0:
        raise pytest.UsageError(f"{ENV} must have milvus-lite and pymilvus[bulk_writer]: {probe.stderr.strip()[-500:]}")
    if probe.stdout.strip() != REF_VERSION:
        raise pytest.UsageError(f"{ENV} has milvus-lite {probe.stdout.strip()}, expected {REF_VERSION}")


@pytest.fixture(scope="session")
def ref_python():
    return os.environ[ENV]
