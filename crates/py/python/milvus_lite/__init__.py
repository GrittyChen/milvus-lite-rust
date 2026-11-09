"""milvus-lite-rust：以 Rust 实现的非官方 milvus-lite，与 Milvus / Zilliz 无隶属关系。

pymilvus 遇到 ``.db`` URI 时导入 ``milvus_lite.server_manager``（见 AGENTS.md「pymilvus 接入点」）。
"""

import os as _os
from importlib import metadata as _metadata


def _refuse_official_package() -> None:
    # 官方 milvus-lite 与本包共用 milvus_lite 模块，同时安装时后装者覆盖同名文件，
    # 卸载任一方都会弄坏另一方；官方包后装的情形在这里检测不到。
    try:
        _metadata.distribution("milvus-lite")
    except _metadata.PackageNotFoundError:
        return
    raise ImportError(
        "the official 'milvus-lite' package is installed alongside 'milvus-lite-rust'; "
        "both provide the 'milvus_lite' module and cannot coexist. "
        "Run 'pip uninstall milvus-lite milvus-lite-rust' and then reinstall 'milvus-lite-rust'."
    )


_refuse_official_package()

from milvus_lite._native import configure_flush_budgets as _configure_flush_budgets  # noqa: E402


def _byte_budget(env_name: str, default: int) -> int:
    # 文案与解析规则同 3.x `constants.py`；只在导入时读取一次（见 docs/decisions/0027）。
    raw = _os.environ.get(env_name)
    if raw is None:
        return default
    try:
        value = int(raw)
    except ValueError:
        raise ValueError(f"{env_name} must be a positive integer in bytes, got {raw!r}") from None
    if value <= 0:
        raise ValueError(f"{env_name} must be a positive integer in bytes, got {raw!r}")
    return value


_MAX_BUDGET = 2**64 - 1
_configure_flush_budgets(
    min(_byte_budget("MILVUS_LITE_MEMTABLE_MEMORY_LIMIT_BYTES", 128 * 1024 * 1024), _MAX_BUDGET),
    min(_byte_budget("MILVUS_LITE_WAL_SIZE_LIMIT_BYTES", 256 * 1024 * 1024), _MAX_BUDGET),
)

__version__ = _metadata.version("milvus-lite-rust")
