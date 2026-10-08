"""批量距离测量不能以错误结果、错误真值或较松的截断判据换取更低耗时。"""
from pathlib import Path
import importlib.metadata
import json
import runpy
import sys
from types import SimpleNamespace

import numpy as np
import pytest

SCRIPT = Path(__file__).resolve().parents[2] / "benchmarks/batch-search/run.py"


def test_batch_validation_checks_every_query_distance_and_cutoff():
    check = runpy.run_path(str(SCRIPT))["check_search"]
    vectors = np.arange(12, dtype=np.float32)[:, None]
    queries = np.zeros((2, 1), dtype=np.float32)
    scores = [float(i * i) for i in range(10)]
    truth = {"ids": [list(range(10))] * 2, "scores": [scores] * 2}
    correct = [[{"id": i, "distance": scores[i]} for i in range(10)] for _ in queries]
    check(correct, queries, vectors, truth)
    for bad in [correct[:1], [correct[0], correct[1][::-1]],
                [correct[0], correct[1][:-1] + [{"id": 10, "distance": 100.0}]],
                [correct[0], correct[1][:-1] + [{"id": 9, "distance": 81.01}]],
                [correct[0], correct[1][:-1] + [{"id": 9, "distance": float("nan")}]],
                [correct[0], correct[1][:-1] + [correct[1][0]]]]:
        with pytest.raises(RuntimeError):
            check(bad, queries, vectors, truth)
    zero = np.zeros_like(vectors)
    ties = {"ids": [list(range(10))], "scores": [[0.0] * 10]}
    check([[{"id": i, "distance": 0.0} for i in list(range(9)) + [10]]], queries[:1], zero, ties)
    with pytest.raises(RuntimeError, match="距离"):
        check([[{"id": i, "distance": 0.0} for i in range(10)]], queries[:1],
              np.full_like(vectors, np.inf), ties)


def test_truth_preparation_matches_direct_float64_order_and_never_overwrites(tmp_path):
    prepare = runpy.run_path(str(SCRIPT))["prepare_truth"]
    destination = tmp_path / "truth"
    prepare(destination, 20, 17)
    vectors = np.load(destination / "vectors.npy")
    queries = np.load(destination / "queries.npy")
    truth = json.loads((destination / "truth.json").read_text())
    for qi, query in enumerate(queries):
        scores = ((vectors.astype(np.float64) - query.astype(np.float64)) ** 2).sum(axis=1)
        ids = np.lexsort((np.arange(20), scores))[:10]
        assert truth["ids"][qi] == ids.tolist()
        assert np.allclose(truth["scores"][qi], scores[ids], rtol=1e-14, atol=1e-14)
    original = (destination / "truth.json").read_bytes()
    with pytest.raises(RuntimeError, match="不覆盖"):
        prepare(destination, 20, 17)
    assert (destination / "truth.json").read_bytes() == original


def test_main_rejects_missing_database_before_loading_backend_or_creating_directory(
        tmp_path, monkeypatch, capsys):
    main = runpy.run_path(str(SCRIPT))["main"]
    monkeypatch.setitem(main.__globals__, "load_baseline",
                        lambda *args: pytest.fail("不存在的数据库不得加载后端"))
    database = tmp_path / "missing.db"
    monkeypatch.setattr(sys, "argv", [str(SCRIPT), "--database", str(database),
                                     "--truth", str(tmp_path / "missing-truth"), "--rows", "10",
                                     "--dim", "1", "--round", "1", "--build", "test"])
    with pytest.raises(SystemExit) as error:
        main()
    assert error.value.code == 2
    assert not database.exists()
    assert not (tmp_path / "missing-truth").exists()
    assert capsys.readouterr().out == ""


@pytest.fixture
def main_case(tmp_path, monkeypatch):
    helpers = runpy.run_path(str(SCRIPT))
    truth_dir = tmp_path / "truth"
    helpers["prepare_truth"](truth_dir, 10, 1)
    truth = json.loads((truth_dir / "truth.json").read_text())
    database = tmp_path / "healthy.db"
    database.mkdir()
    marker = database / "preserve"
    marker.write_text("unchanged")
    baseline = tmp_path / "baseline"
    baseline.mkdir()
    (baseline / "bench.py").write_text("# fake baseline for main-flow test\n")
    events = []
    state = {"scenario": "valid", "searches": 0}

    class Client:
        def __init__(self, *args, **kwargs):
            events.append("client")

        def load_collection(self, collection):
            events.append("load")

        def list_indexes(self, collection):
            events.append("list")
            return {"missing_index": [], "multiple_indexes": ["vec", "other"]}.get(
                state["scenario"], ["vec"])

        def describe_index(self, collection, name):
            events.append("describe")
            if state["scenario"] == "describe_rpc_failure":
                raise RuntimeError("describe RPC failed")
            return {"index_type": "HNSW" if state["scenario"] == "wrong_index" else "FLAT",
                    "metric_type": "IP" if state["scenario"] == "wrong_metric" else "L2"}

        def query(self, *args, **kwargs):
            events.append("count")
            return [{"count(*)": 9 if state["scenario"] == "wrong_count" else 10}]

        def search(self, collection, data, **kwargs):
            state["searches"] += 1
            events.append(f"search:{len(data)}")
            if state["scenario"] == "timed_rpc_failure" and state["searches"] > 1:
                raise RuntimeError("search RPC failed")
            if (state["scenario"] == "invalid_precheck"
                    or state["scenario"] == "invalid_timed_result" and state["searches"] > 1):
                return []
            return [[{"id": pk, "distance": distance} for pk, distance in zip(ids, scores)]
                    for ids, scores in zip(truth["ids"][:len(data)], truth["scores"][:len(data)])]

        def close(self):
            events.append("close")
            if state["scenario"] == "close_failure":
                raise RuntimeError("close failed")

    def lat(operation, samples):
        events.append(f"lat:{samples}")
        operation()
        return {"n": samples}

    base = SimpleNamespace(np=np, C="bench", MilvusClient=Client, SEARCH_PARAMS={"metric_type": "L2"},
                           lat=lat, __file__=str(baseline / "bench.py"), VER="fake", PKG_VERSION="test")
    main = helpers["main"]
    monkeypatch.setitem(main.__globals__, "load_baseline", lambda *args: base)
    monkeypatch.setattr(importlib.metadata, "version", lambda name: f"test-{name}")
    monkeypatch.setattr(sys, "argv", [str(SCRIPT), "--database", str(database),
                                     "--truth", str(truth_dir), "--rows", "10", "--dim", "1",
                                     "--round", "1", "--build", "test"])
    return main, events, state, marker


@pytest.mark.parametrize("scenario,message", [
    ("missing_index", "唯一 FLAT L2"),
    ("multiple_indexes", "唯一 FLAT L2"),
    ("wrong_index", "实际索引"),
    ("wrong_metric", "实际索引"),
    ("wrong_count", "行数不符"),
    ("describe_rpc_failure", "describe RPC failed"),
    ("invalid_precheck", "查询条数不符"),
    ("invalid_timed_result", "查询条数不符"),
    ("timed_rpc_failure", "search RPC failed"),
    ("close_failure", "close failed"),
])
def test_main_failure_never_outputs_measurement_and_closes_client(main_case, capsys, scenario, message):
    main, events, state, marker = main_case
    state["scenario"] = scenario
    with pytest.raises(RuntimeError, match=message):
        main()
    assert events[:3] == ["client", "load", "list"]
    assert events[-1] == "close"
    if scenario in {"missing_index", "multiple_indexes", "wrong_index", "wrong_metric",
                    "wrong_count", "describe_rpc_failure"}:
        assert state["searches"] == 0
        assert not any(event.startswith("lat:") for event in events)
    elif scenario == "invalid_precheck":
        assert state["searches"] == 1
        assert "lat:100" not in events
    elif scenario == "close_failure":
        assert state["searches"] == 4
        assert "lat:10" in events
    else:
        assert state["searches"] == 2
        assert "lat:100" in events
    assert capsys.readouterr().out == ""
    assert marker.read_text() == "unchanged"


def test_main_success_checks_both_query_sizes_and_emits_one_record_after_close(main_case, capsys):
    main, events, state, marker = main_case
    main()
    output = capsys.readouterr().out.splitlines()
    assert len(output) == 1
    record = json.loads(output[0])
    assert record["metrics"] == {"search_nq1_top10": {"n": 100}, "search_nq100_top10": {"n": 10}}
    assert events == ["client", "load", "list", "describe", "count", "search:1", "lat:100",
                      "search:1", "search:100", "lat:10", "search:100", "close"]
    assert state["searches"] == 4
    assert marker.read_text() == "unchanged"
