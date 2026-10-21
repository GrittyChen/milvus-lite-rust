"""固定测试连接的所有权；SDK 默认共享池的业务语义仍由端到端套件覆盖。"""

import errno
import socket
from urllib.parse import urlparse

import pytest


@pytest.mark.skipif(not hasattr(socket, "SO_REUSEPORT"), reason="平台没有 SO_REUSEPORT")
def test_reference_server_does_not_share_its_port(ref_uri):
    # 参照服务若开启 SO_REUSEPORT，同一用户的另一个 SO_REUSEPORT socket 就能绑到同一端口，
    # 老内核的端口 0 自动分配也会把它分给别的参照服务（见 server3x.py）。
    with socket.socket() as other:
        other.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
        with pytest.raises(OSError) as error:
            other.bind(("127.0.0.1", urlparse(ref_uri).port))
    assert error.value.errno == errno.EADDRINUSE


def test_differential_fixture_owns_dedicated_connections(diff):
    for client in (diff.rust, diff.ref):
        handler = client._get_connection()
        # 两条受支持的 SDK 都提供这些统计；不只核对构造参数，以免 fixture
        # 回退到共享池后用例仍然通过。
        entries = client._manager.get_stats()["connections"]
        assert any(entry["dedicated"] and entry["key"] == str(id(handler)) for entry in entries)
