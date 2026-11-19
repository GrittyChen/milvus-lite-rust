"""tests/dump 的 e2e 与差分共用：要求真实的 pymilvus BulkWriter（0037「测试」）。

依赖来自 .github/ci/requirements/dump30.txt 或 dump26.txt；缺少时这里直接报错，不跳过。
本目录由 pytest 加入 sys.path，子目录的用例因此能导入 dump_cases 与 dump_helpers。
"""

import pytest

try:
    import pymilvus.bulk_writer  # noqa: F401
except ImportError as error:
    raise pytest.UsageError(
        "tests/dump requires pymilvus[bulk_writer]; install .github/ci/requirements/dump30.txt or dump26.txt"
    ) from error
