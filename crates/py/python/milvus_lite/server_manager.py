"""pymilvus 的接入点：``server_manager_instance.start_and_get_uri(path)``。

模块路径、实例名、方法签名与返回约定都是对外契约，与 3.x 一致。生命周期规则见
docs/decisions/0003-in-process-server-lifecycle.md：
- 以 ``os.path.abspath`` 为键复用服务；启动在表锁内完成，同路径并发首次打开只启动一次。
- atexit 时停止本进程创建的服务。
- fork 出的子进程对继承来的路径返回父进程的 URI，且永不停止继承来的服务。
"""

from __future__ import annotations

import atexit
import logging
import os
import threading
from typing import Dict, NamedTuple, Optional

from milvus_lite import __version__
from milvus_lite._native import NativeServer

logger = logging.getLogger(__name__)

_HOST = "127.0.0.1"
_STOP_GRACE_SECONDS = 2.0


class _Entry(NamedTuple):
    server: NativeServer
    port: int
    owner_pid: int


class ServerManager:
    """按数据目录管理进程内的 gRPC 服务。"""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._servers: Dict[str, _Entry] = {}
        atexit.register(self.release_all)
        # fork 前持有表锁，保证子进程看到一致的表，且表锁不会在子进程中永久处于加锁状态。
        if hasattr(os, "register_at_fork"):
            os.register_at_fork(
                before=self._lock.acquire,
                after_in_parent=self._lock.release,
                after_in_child=self._lock.release,
            )

    def start_and_get_uri(self, path: str, args: Optional[dict] = None) -> Optional[str]:
        """启动（或复用）``path`` 对应的服务，返回 ``http://127.0.0.1:<port>``；失败返回 None。

        ``args`` 与 3.x 一样忽略。
        """
        abs_path = os.path.abspath(path)
        with self._lock:
            entry = self._servers.get(abs_path)
            if entry is not None:
                return _uri(entry.port)
            try:
                server = NativeServer.start(abs_path, _HOST, 0, __version__)
            except Exception:
                logger.exception("Failed to start MilvusLite server for %s", abs_path)
                return None
            self._servers[abs_path] = _Entry(server, server.port, os.getpid())
        logger.info("MilvusLite server started for %s on port %d", abs_path, server.port)
        return _uri(server.port)

    def release_server(self, path: str) -> None:
        """停止 ``path`` 对应的服务并释放 LOCK。继承自父进程的服务跳过，表项保留。"""
        abs_path = os.path.abspath(path)
        with self._lock:
            self._stop_locked(abs_path)

    def release_all(self) -> None:
        """停止本进程创建的所有服务。进程退出时经 atexit 自动调用。"""
        with self._lock:
            for abs_path in list(self._servers):
                self._stop_locked(abs_path)

    def _stop_locked(self, abs_path: str) -> None:
        entry = self._servers.get(abs_path)
        if entry is None or entry.owner_pid != os.getpid():
            return
        del self._servers[abs_path]
        try:
            entry.server.stop(_STOP_GRACE_SECONDS)
        except Exception:
            # 调用方（atexit）无法接收这个错误；记录后继续停止其余服务（见 0003「进程退出」）。
            logger.exception("Failed to stop MilvusLite server for %s", abs_path)
            return
        logger.info("MilvusLite server stopped for %s (port %d)", abs_path, entry.port)


def _uri(port: int) -> str:
    return f"http://127.0.0.1:{port}"


server_manager_instance = ServerManager()
