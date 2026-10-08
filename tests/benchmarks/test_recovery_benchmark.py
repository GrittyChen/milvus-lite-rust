"""恢复测量必须先拒绝错误结果，且不能悄悄创建不存在的基准库。"""
from pathlib import Path
import hashlib
import runpy
import shutil
import subprocess
import sys

import pytest

from history import copy_baseline_snapshot

SCRIPT = Path(__file__).resolve().parents[2] / "benchmarks/recovery/run.py"


def test_recovery_rejects_wrong_count_candidates_and_distances():
    helpers = runpy.run_path(str(SCRIPT))
    count, search = helpers["check_count"], helpers["check_search"]
    count([{"count(*)": 2000}], 2000)
    for got in [[], [{"count(*)": 1999}], [{"count(*)": 2001}]]:
        with pytest.raises(RuntimeError, match="行数不一致"):
            count(got, 2000)
    search([[{"id": 3, "distance": 0.0}]], [3], [0.0])
    for got in [[], [[{"id": 4, "distance": 0.0}]],
                [[{"id": 3, "distance": 2e-6}]],
                [[{"id": 3, "distance": float("nan")}]]]:
        with pytest.raises(RuntimeError, match="Search"):
            search(got, [3], [0.0])


def test_recovery_refuses_to_create_a_missing_database(tmp_path):
    database = tmp_path / "absent.db"
    result = subprocess.run([sys.executable, str(SCRIPT), str(database), "--rows", "2000",
                             "--round", "1", "--build", "test"],
                            capture_output=True, text=True, timeout=10)
    assert result.returncode != 0
    assert "不新建数据目录" in result.stderr
    assert result.stdout == ""
    assert not database.exists()


@pytest.mark.parametrize("names,description", [
    ([], {}),
    (["vec", "other"], {}),
    (["vec"], {"index_type": "HNSW", "metric_type": "L2"}),
    (["vec"], {"index_type": "FLAT", "metric_type": "IP"}),
    (["vec"], {"index_type": "FLAT"}),
    (["vec"], RuntimeError("describe RPC failure")),
])
def test_recovery_main_rejects_wrong_indexes_before_output_and_stops_service(
        tmp_path, monkeypatch, capsys, names, description):
    import numpy as np
    from types import SimpleNamespace
    events = []

    class Client:
        def __init__(self, *args, **kwargs):
            pass

        def load_collection(self, collection):
            events.append("load")

        def list_indexes(self, collection):
            events.append("list")
            return names

        def describe_index(self, collection, name):
            events.append("describe")
            if isinstance(description, Exception):
                raise description
            return description

        def query(self, *args, **kwargs):
            pytest.fail("索引未通过校验不得查询或输出测量结果")

        search = query

        def close(self):
            events.append("close")

    base = SimpleNamespace(np=np, queries=np.zeros((200, 2)), C="bench", MilvusClient=Client,
                           start_server=lambda: (SimpleNamespace(pid=123), "unused"),
                           rss_mb=lambda: 0,
                           stop_server=lambda proc, graceful: events.append(("stop", graceful)))
    main = runpy.run_path(str(SCRIPT))["main"]
    monkeypatch.setitem(main.__globals__, "load_baseline", lambda *args: base)
    monkeypatch.setattr(sys, "argv", [str(SCRIPT), str(tmp_path), "--rows", "10",
                                     "--dim", "2", "--round", "1", "--build", "test"])
    expected = "describe RPC failure" if isinstance(description, Exception) else "FLAT L2"
    with pytest.raises(RuntimeError, match=expected):
        main()
    assert events[:2] == ["load", "list"]
    assert events[-2:] == ["close", ("stop", False)]
    assert capsys.readouterr().out == ""


def test_recovery_accepts_exact_flat_l2_definition():
    from types import SimpleNamespace
    read = runpy.run_path(str(SCRIPT))["read_index"]
    client = SimpleNamespace(list_indexes=lambda collection: ["vec"],
                             describe_index=lambda collection, name: {
                                 "index_type": "FLAT", "metric_type": "L2", "other": "ignored"})
    assert read(client, "bench") == {"index_type": "FLAT", "metric_type": "L2"}


def test_recovery_historical_script_fingerprint_uses_fixed_baseline_inputs(tmp_path):
    copy_baseline_snapshot(SCRIPT.parents[2], tmp_path / "baseline")
    (tmp_path / "recovery").mkdir()
    shutil.copy2(SCRIPT, tmp_path / "recovery/run.py")
    subprocess.run(["git", "apply", str(SCRIPT.with_name("measured-20261008.patch"))],
                   cwd=tmp_path / "recovery", check=True, capture_output=True, text=True)
    digest = hashlib.sha256()
    for name in ["baseline", "recovery"]:
        for path in sorted((tmp_path / name).iterdir()):
            digest.update(f"{name}/{path.name}".encode())
            digest.update(path.read_bytes())
    assert digest.hexdigest() == "5e17c0ec3f56fbd380e828e887b6e612f841bfe0dcdac6dc9e15f58fda08d369"
