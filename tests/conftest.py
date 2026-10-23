"""独立数据库用例不继承此前服务地址上的 SDK 状态；pytest 进程派生子进程时不运行 grpcio 的 fork 处理。"""

import sys

import grpc_fork

# 根 conftest 先于各子目录的 conftest 与测试模块导入，须在任何模块导入 pymilvus 或 grpc 之前执行；
# 不需要 SDK 的工具自检环境可以没有 grpcio。
grpc_fork.disable_fork_handlers(required=False)

import pytest  # noqa: E402


@pytest.fixture(autouse=True)
def isolated_pymilvus_cache():
    # 2.6.17 和 3.0.2 均按端点缓存；端口复用会使新数据库继承旧 schema。
    # 未加载 SDK 缓存模块时不做任何事；同一用例内的重开仍保留缓存。
    cache = sys.modules.get("pymilvus.client.cache")
    if cache is not None:
        cache.GlobalCache._reset_for_testing()
