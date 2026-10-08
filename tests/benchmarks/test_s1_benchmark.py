"""专项基准的正确性校验不能把错误结果当作更低延迟。"""
from pathlib import Path
import hashlib
import importlib.metadata
import runpy
import subprocess
import shutil
import sys

import pytest

from history import copy_baseline_snapshot

SCRIPT = Path(__file__).resolve().parents[2] / "benchmarks/s1/run.py"


def test_s1_results_reject_missing_rows_wrong_order_and_invalid_distances():
    helpers = runpy.run_path(str(SCRIPT))
    check_query, check_search = helpers["check_query"], helpers["check_search"]
    expected = [{"id": 2, "tag": "t2"}, {"id": 1, "tag": "t1"}]
    check_query(expected, expected)
    for result in [[], expected[::-1], [{"id": 2, "tag": "wrong"}, expected[1]]]:
        with pytest.raises(RuntimeError, match="Query"):
            check_query(result, expected)
    correct = [[{"id": 2, "distance": 1.0}, {"id": 1, "distance": 2.0}]]
    check_search(correct, [2, 1], [1.0, 2.0])
    for result in [[], [correct[0][::-1]], [[{"id": 2, "distance": float("nan")}, correct[0][1]]],
                   [[{"id": 2, "distance": 1.1}, correct[0][1]]]]:
        with pytest.raises(RuntimeError, match="Search"):
            check_search(result, [2, 1], [1.0, 2.0])


def test_s1_preparation_never_overwrites_an_existing_database(tmp_path):
    database = tmp_path / "existing.db"
    database.mkdir()
    marker = database / "untouched"
    marker.write_text("preserve")
    result = subprocess.run([sys.executable, str(SCRIPT), str(database), "--rows", "2000",
                             "--prepare", "--round", "1", "--build", "test"],
                            capture_output=True, text=True, timeout=10)
    assert result.returncode != 0
    assert "不覆盖已存在的数据目录" in result.stderr
    assert result.stdout == ""
    assert marker.read_text() == "preserve"


@pytest.mark.parametrize("expected,within,outside", [
    (0.0, 1e-6, 2e-6),
    (1.0, 1.0 + 9e-6, 1.0 + 1.5e-5),
    (100.0, 100.0 + 9e-4, 100.0 + 1.005e-3),
])
def test_s1_l2_tolerance_uses_the_maximum_instead_of_an_additive_allowance(expected, within, outside):
    check = runpy.run_path(str(SCRIPT))["check_search"]
    check([[{"id": 1, "distance": within}]], [1], [expected])
    with pytest.raises(RuntimeError, match="距离超出"):
        check([[{"id": 1, "distance": outside}]], [1], [expected])


def test_s1_search_rejects_nonfinite_truth_and_mismatched_shapes():
    check = runpy.run_path(str(SCRIPT))["check_search"]
    for value in [float("inf"), float("-inf"), float("nan")]:
        with pytest.raises(RuntimeError, match="距离超出"):
            check([[{"id": 1, "distance": value}]], [1], [value])
    with pytest.raises(RuntimeError, match="距离超出"):
        check([[{"id": 1, "distance": 1.0}, {"id": 2, "distance": 1.0}]], [1, 2], [1.0])


def test_s1_dependency_versions_record_required_packages_and_only_tolerate_missing_pyarrow(monkeypatch):
    collect = runpy.run_path(str(SCRIPT))["dependency_versions"]
    versions = {"numpy": "numpy-version", "pymilvus": "sdk-version", "pyarrow": "arrow-version"}
    monkeypatch.setattr(importlib.metadata, "version", versions.__getitem__)
    got = collect("engine-version")
    assert {name: got[name] for name in versions} == versions
    assert got["impl"] == "engine-version"
    assert got["python"] == sys.version.split()[0]

    def unavailable(name):
        if name == "pyarrow":
            raise importlib.metadata.PackageNotFoundError(name)
        return versions[name]

    monkeypatch.setattr(importlib.metadata, "version", unavailable)
    got = collect("engine-version")
    assert "pyarrow" not in got
    assert got["numpy"] == versions["numpy"]

    def damaged(name):
        if name == "pyarrow":
            raise OSError("metadata is unreadable")
        return versions[name]

    monkeypatch.setattr(importlib.metadata, "version", damaged)
    with pytest.raises(OSError, match="metadata is unreadable"):
        collect("engine-version")


def assert_s1_historical_fingerprint(repo, destination):
    copy_baseline_snapshot(repo, destination / "baseline")
    (destination / "s1").mkdir()
    source = repo / "benchmarks/s1"
    shutil.copy2(source / "run.py", destination / "s1/run.py")
    subprocess.run(["git", "apply", str(source / "measured-20261008.patch")],
                   cwd=destination / "s1", check=True, capture_output=True, text=True)
    digest = hashlib.sha256()
    for name in ["baseline", "s1"]:
        for path in sorted((destination / name).iterdir()):
            digest.update(f"{name}/{path.name}".encode())
            digest.update(path.read_bytes())
    assert digest.hexdigest() == "cce9abebc190c27a38ad9b1c3f14397d7a259b07331c1d8b19efd14376e06766"


def test_s1_measured_source_patch_restores_both_historical_rounds(tmp_path):
    assert_s1_historical_fingerprint(SCRIPT.parents[2], tmp_path)
