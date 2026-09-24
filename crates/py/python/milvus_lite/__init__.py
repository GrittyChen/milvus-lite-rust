"""milvus-lite-rust：以 Rust 实现的非官方 milvus-lite，与 Milvus / Zilliz 无隶属关系。

pymilvus 遇到 ``.db`` URI 时导入 ``milvus_lite.server_manager``（见 AGENTS.md「pymilvus 接入点」）。
"""

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

__version__ = _metadata.version("milvus-lite-rust")
