# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_milvus_lite_compat.py，Apache-2.0 许可。
# 只保留 M1 范围内的用例（逐条状态见 INVENTORY.md），并删去随之不再使用的 import、helper 与分节标题。
# 改动：上游的 importorskip 改为直接导入（本仓不允许因缺依赖静默跳过）。
"""Test the milvus_lite compatibility layer.

Verifies that pymilvus can connect via MilvusClient("./demo.db") using
our ServerManager, and perform basic CRUD operations.
"""

import tempfile
import os

# 上游在此 importorskip pymilvus 与 grpc；本仓的规则不允许因缺依赖静默跳过，改为直接导入。
# milvus_lite.server_manager 是 pymilvus 接入点的对外契约，本仓同样提供。
from pymilvus import MilvusClient
from milvus_lite.server_manager import ServerManager


class TestServerManager:
    """Unit tests for ServerManager lifecycle."""

    def test_start_and_get_uri(self):
        mgr = ServerManager()
        with tempfile.TemporaryDirectory() as d:
            db_path = os.path.join(d, "test.db")
            uri = mgr.start_and_get_uri(db_path)
            try:
                assert uri is not None
                assert uri.startswith("http://127.0.0.1:")
                port = int(uri.split(":")[-1])
                assert port > 0
            finally:
                mgr.release_all()

    def test_reuse_same_path(self):
        mgr = ServerManager()
        with tempfile.TemporaryDirectory() as d:
            db_path = os.path.join(d, "test.db")
            uri1 = mgr.start_and_get_uri(db_path)
            uri2 = mgr.start_and_get_uri(db_path)
            try:
                assert uri1 == uri2
            finally:
                mgr.release_all()

    def test_different_paths(self):
        mgr = ServerManager()
        with tempfile.TemporaryDirectory() as d:
            uri1 = mgr.start_and_get_uri(os.path.join(d, "a.db"))
            uri2 = mgr.start_and_get_uri(os.path.join(d, "b.db"))
            try:
                assert uri1 != uri2
            finally:
                mgr.release_all()


class TestPymilvusIntegration:
    """End-to-end: pymilvus MilvusClient → .db path → our engine."""

    def test_shorthand_create_collection(self):
        """Test pymilvus shorthand: create_collection("name", dimension=N)."""
        mgr = ServerManager()
        with tempfile.TemporaryDirectory() as d:
            db_path = os.path.join(d, "short.db")
            uri = mgr.start_and_get_uri(db_path)
            assert uri is not None

            try:
                client = MilvusClient(uri=uri)
                client.create_collection("quick", dimension=8)
                client.insert("quick", [
                    {"id": i, "vector": [float(i)] * 8}
                    for i in range(10)
                ])
                results = client.search("quick", data=[[1.0] * 8], limit=3)
                assert len(results[0]) == 3
                client.drop_collection("quick")
            finally:
                mgr.release_all()
