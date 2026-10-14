"""真实 3.x 与本实现均在子进程仍持有继承 fd 时完成解锁和重开。"""

import json
import os
from pathlib import Path
import subprocess
import sys

import pytest


@pytest.mark.skipif(not hasattr(os, "fork"), reason="os.fork 在 Windows 上不存在")
def test_close_unlocks_with_a_live_fork_child(tmp_path):
    probe = Path(__file__).parents[1] / "python" / "inherited_lock_probe.py"
    results = []
    for backend, python in [
        ("rust", sys.executable),
        ("reference", os.environ["MILVUS_LITE_3X_PYTHON"]),
    ]:
        completed = subprocess.run(
            [python, str(probe), backend, str(tmp_path / f"{backend}.db")],
            capture_output=True,
            text=True,
            timeout=60,
        )
        assert completed.returncode == 0, completed.stderr
        results.append(json.loads(completed.stdout))
    assert results[0] == results[1] == {
        "held_before_close": True,
        "free_after_close": True,
        "reopen_while_child_alive": True,
        "child_stop_rejected": False,
        "child_exit": 0,
    }
