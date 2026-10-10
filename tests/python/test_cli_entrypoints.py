"""复制自 milvus-lite v3.2.1（43d1257）tests/test_dump_tool.py，Apache-2.0。

仅取两个 server/模块入口用例，测试主体原样保留。
"""

import runpy
import sys
import pytest
from milvus_lite import cmdline

def test_main_dispatches_server(monkeypatch, tmp_path):
    calls = []

    def fake_run_server(data_dir, host, port, max_workers):
        calls.append({
            "data_dir": data_dir,
            "host": host,
            "port": port,
            "max_workers": max_workers,
        })

    monkeypatch.setattr(cmdline, "run_server", fake_run_server)

    rc = cmdline.main([
        "server",
        "--data-dir", str(tmp_path / "data"),
        "--host", "127.0.0.1",
        "--port", "19531",
        "--max-workers", "3",
    ])

    assert rc == 0
    assert calls == [{
        "data_dir": str(tmp_path / "data"),
        "host": "127.0.0.1",
        "port": 19531,
        "max_workers": 3,
    }]

def test_package_module_main_delegates_to_cmdline(monkeypatch):
    calls = []

    def fake_main():
        calls.append(True)
        return 23

    monkeypatch.setattr(cmdline, "main", fake_main)
    monkeypatch.setitem(sys.modules, "milvus_lite.cmdline", cmdline)

    with pytest.raises(SystemExit) as exc_info:
        runpy.run_module("milvus_lite.__main__", run_name="__main__")

    assert exc_info.value.code == 23
    assert calls == [True]
