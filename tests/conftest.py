"""独立数据库用例不继承此前服务地址上的 SDK 状态。"""

import sys

import pytest


@pytest.fixture(autouse=True)
def isolated_pymilvus_cache():
    # 2.6.17 和 3.0.2 均按端点缓存；端口复用会使新数据库继承旧 schema。
    # 未加载 SDK 缓存模块时不做任何事；同一用例内的重开仍保留缓存。
    cache = sys.modules.get("pymilvus.client.cache")
    if cache is not None:
        cache.GlobalCache._reset_for_testing()
