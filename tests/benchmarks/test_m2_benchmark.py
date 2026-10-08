"""保护基准的失败传播、实际耗时分母与多轮汇总；不把吞吐目标写成断言。"""

import json
import hashlib
from pathlib import Path
import runpy
import subprocess
import shutil
import sys

import pytest


ROOT = Path(__file__).resolve().parents[2]
BENCH = ROOT / "benchmarks/baseline/bench.py"


@pytest.fixture
def bench(monkeypatch, tmp_path):
    monkeypatch.setattr(sys, "argv", [str(BENCH), str(tmp_path / "a.db"), "2000", "4", "FLAT", "m2"])
    return runpy.run_path(str(BENCH))


def test_concurrency_uses_actual_elapsed_and_propagates_worker_failure(bench, monkeypatch):
    globals_ = bench["concurrent"].__globals__
    monkeypatch.setitem(globals_, "search_loop", lambda *args: ([1.0, 2.0], 10.0))
    got = bench["concurrent"]("unused", 2, False)
    assert got["completed_requests"] == 4
    assert got["elapsed_s"] == 10.0
    assert got["qps"] == 0.4

    def fail(*args):
        raise RuntimeError("worker failed")

    monkeypatch.setitem(globals_, "search_loop", fail)
    with pytest.raises(RuntimeError, match="客户端线程并发检索失败") as error:
        bench["concurrent"]("unused", 2, False)
    assert str(error.value.__cause__) == "worker failed"


def test_server_startup_failure_exits_without_metrics(tmp_path):
    path = tmp_path / "file.db"
    path.write_text("not a directory")
    got = subprocess.run([sys.executable, str(BENCH), str(path), "2000", "4", "FLAT", "memory", "server"],
                         capture_output=True, text=True, timeout=30)
    assert got.returncode != 0
    assert not any(line.startswith("{") for line in got.stdout.splitlines())
    assert "服务端握手失败" in got.stderr


def test_m1_small_dataset_passes_validation_before_opening_server(tmp_path):
    path = tmp_path / "file.db"
    path.write_text("not a directory")
    got = subprocess.run([sys.executable, str(BENCH), str(path), "1000", "4", "FLAT", "m1", "server"],
                         capture_output=True, text=True, timeout=30)
    assert got.returncode != 0
    assert "服务端握手失败" in got.stderr
    assert "N >= 2000" not in got.stderr


def test_mixed_writer_partial_success_is_an_error_not_a_measurement(bench, monkeypatch):
    sent, closed = [], []

    class Client:
        def __init__(self, **kwargs):
            pass

        def upsert(self, *args):
            return {"upsert_count": 0}

        def close(self):
            closed.append("client")

    class Start:
        def wait(self, **kwargs):
            pass

        def abort(self):
            closed.append("barrier")

    class Connection:
        def send(self, result):
            sent.append(result)

        def close(self):
            closed.append("connection")

    monkeypatch.setitem(bench["_mixed_worker"].__globals__, "MilvusClient", Client)
    with pytest.raises(RuntimeError, match="部分 upsert"):
        bench["_mixed_worker"]("unused", Start(), Connection(), [{"id": 1}])
    assert len(sent) == 1 and sent[0][0] == "error"
    assert closed == ["barrier", "client", "connection"]


def test_summary_keeps_three_rounds_and_separates_script_identity(tmp_path):
    path = tmp_path / "measurements.jsonl"
    rows = [{"ver": "rs", "index": "FLAT", "mode": "inproc", "N": 100000, "dim": 128,
             "phase": "m2", "versions": {"build_commit": "abc"}, "benchmark_sha256": "same",
             "concurrency_duration_s": 8, "insert_rows_per_s": value} for value in (10, 30, 20)]
    rows.append({**rows[0], "benchmark_sha256": "different", "insert_rows_per_s": 100})
    path.write_text("".join(json.dumps(row) + "\n" for row in rows))
    got = subprocess.run([sys.executable, str(ROOT / "benchmarks/baseline/summarize.py"), str(path)],
                         capture_output=True, text=True, check=True)
    assert "×3" in got.stdout and "×1" in got.stdout
    assert "20 (10–30)" in got.stdout and "100" in got.stdout


def test_m2_keeps_build_database_and_uses_its_own_path(tmp_path):
    fake = tmp_path / "v3/bin/python"
    fake.parent.mkdir(parents=True)
    fake.write_text(f"#!{sys.executable}\nimport json,sys\nfrom pathlib import Path\nPath(sys.argv[2]).mkdir(exist_ok=True)\nprint(json.dumps({{'db':sys.argv[2],'phase':sys.argv[6]}}))\n")
    fake.chmod(0o755)
    build = tmp_path / "r_v3_FLAT_inproc_2000_4.db"
    build.mkdir()
    marker = build / "original"
    marker.write_text("build state")
    subprocess.run([str(ROOT / "benchmarks/baseline/run.sh"), str(tmp_path), "2000", "4", "out.jsonl", "v3:FLAT:inproc:m2"],
                   capture_output=True, text=True, check=True)
    assert marker.read_text() == "build state"
    rows = [json.loads(line) for line in (tmp_path / "out.jsonl").read_text().splitlines()]
    assert [row["phase"] for row in rows] == ["m2", "reopen"]
    assert all(Path(row["db"]).name == "r_v3_FLAT_inproc_2000_4_m2.db" for row in rows)


def test_measured_source_patch_restores_historical_fingerprint(tmp_path):
    source = ROOT / "benchmarks/baseline"
    for path in source.iterdir():
        if path.suffix in (".py", ".sh"):
            shutil.copy2(path, tmp_path / path.name)
    subprocess.run(["git", "apply", str(source / "measured-20261008.patch")], cwd=tmp_path, check=True,
                   capture_output=True, text=True)
    digest = hashlib.sha256()
    for path in sorted(tmp_path.iterdir()):
        digest.update(path.name.encode())
        digest.update(path.read_bytes())
    assert digest.hexdigest() == "00947bbcd41dcdce36ef8ddea529309e11b60386b3411a7662bb6b39d0893d5a"
