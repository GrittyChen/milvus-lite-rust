"""固定测试连接的所有权；SDK 默认共享池的业务语义仍由端到端套件覆盖。"""


def test_differential_fixture_owns_dedicated_connections(diff):
    for client in (diff.rust, diff.ref):
        handler = client._get_connection()
        # 两条受支持的 SDK 都提供这些统计；不只核对构造参数，以免 fixture
        # 回退到共享池后用例仍然通过。
        entries = client._manager.get_stats()["connections"]
        assert any(entry["dedicated"] and entry["key"] == str(id(handler)) for entry in entries)
