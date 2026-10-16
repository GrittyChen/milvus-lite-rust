"""只有 pytest 分区调用观察启用时才导入，独立 Arrow 真值工具不依赖 pytest。"""

import pytest
import audit_plugin


@pytest.hookimpl(tryfirst=True)
def pytest_runtest_setup(item):
    audit_plugin._phase = "setup"


@pytest.hookimpl(tryfirst=True)
def pytest_runtest_call(item):
    audit_plugin._phase = "call"


@pytest.hookimpl(tryfirst=True)
def pytest_runtest_teardown(item):
    audit_plugin._phase = "teardown"
