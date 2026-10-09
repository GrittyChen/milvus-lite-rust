"""合并测量不能用错行数、维度或无真实轮次的记录通过。"""

import importlib.util
import json
from pathlib import Path

import pytest


def module():
    path = Path(__file__).resolve().parents[2] / "benchmarks/compaction/run.py"
    spec = importlib.util.spec_from_file_location("compaction_benchmark", path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


@pytest.mark.parametrize("log", ["", "compaction completed: input_rows=199999 elapsed_seconds=0.1", "compaction completed: input_rows=200000 elapsed_seconds=0.1\ncompaction completed: input_rows=200000 elapsed_seconds=0.2"])
def test_single_round_validation_rejects_missing_wrong_budget_or_multiple_rounds(log):
    with pytest.raises(ValueError):
        module().checked_rounds(log)


def test_single_round_validation_reads_actual_diagnostic():
    assert module().checked_rounds("other diagnostic\ncompaction completed: input_rows=200000 elapsed_seconds=0.545436\n") == 0.545436


@pytest.mark.parametrize("dim,physical,live", [(64,1000,1000),(128,999,999),(128,1000,999)])
def test_fixture_identity_rejects_wrong_dimension_or_dead_rows(tmp_path,monkeypatch,dim,physical,live):
    target=tmp_path / "collections/c"
    target.mkdir(parents=True)
    (target / "schema.json").write_text(json.dumps({"fields":[{"name":"id","dtype":"int64","is_primary":True},{"name":"vector","dtype":"float_vector","dim":dim}]}))
    benchmark=module()
    monkeypatch.setattr(benchmark,"physical",lambda _: {"physical_rows":physical,"live_rows":live})
    with pytest.raises(ValueError):
        benchmark.validate_fixture(tmp_path,1000,128)


def test_prepare_refuses_existing_database_before_loading_reference_dependencies(tmp_path):
    with pytest.raises(ValueError,match="fixture already exists"):
        # 参考环境依赖在这个测试中不可假定可用，提前拒绝必须发生在导入之前。
        module().prepare(tmp_path,1_000_000,128)


def publication_module():
    path = Path(__file__).resolve().parents[2] / "benchmarks/compaction/publication.py"
    spec = importlib.util.spec_from_file_location("compaction_publication", path)
    target = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(target)
    return target


@pytest.mark.parametrize("log", ["", "compaction publication: input_rows=199999 wait_seconds=0.1 lock_seconds=0.2", "compaction publication: input_rows=200000 wait_seconds=0.1 lock_seconds=0.2\ncompaction publication: input_rows=200000 wait_seconds=0.1 lock_seconds=0.2"])
def test_publication_diagnostic_requires_one_complete_expected_round(log):
    with pytest.raises(ValueError):
        publication_module().checked_publication(log)


def test_publication_diagnostic_and_query_percentile_are_independent():
    target = publication_module()
    result = target.checked_publication("compaction publication: input_rows=200000 wait_seconds=0.1 lock_seconds=0.2")
    assert result == {"publication_wait_seconds": 0.1, "publication_lock_seconds": 0.2}
    assert target.statistics([1.0, 0.0]) == {"samples": 2, "p99_seconds": 0.99, "max_seconds": 1.0}


@pytest.mark.parametrize("samples", [[], [float("nan")], [-1]])
def test_query_latency_rejects_invalid_samples(samples):
    with pytest.raises(ValueError):
        publication_module().statistics(samples)


def test_publication_rejects_wrong_primary_type_before_starting_process(tmp_path, monkeypatch):
    import sys
    collection = tmp_path / "collections/c"
    collection.mkdir(parents=True)
    (collection / "schema.json").write_text(json.dumps({"fields": [{"name": "id", "dtype": "int64", "is_primary": True}]}))
    target = publication_module()
    monkeypatch.setattr(sys, "argv", ["publication", "--database", str(tmp_path), "--pk-kind", "varchar", "--python", "absent", "--work-dir", str(tmp_path / "rounds")])
    with pytest.raises(ValueError, match="primary type"):
        target.main()
    assert not (tmp_path / "rounds").exists()


def test_round_process_enables_diagnostics_without_changing_parent_environment(tmp_path, monkeypatch, capsys):
    import os
    import sys
    from types import SimpleNamespace
    seed = tmp_path / "seed.db"
    seed.mkdir()
    (seed / "marker").write_text("same fixture")
    target = module()
    layout = {"physical_rows": 1_000_000, "live_rows": 1_000_000, "dead_rows": 0}
    monkeypatch.setattr(target, "validate_fixture", lambda *args: layout)
    monkeypatch.setattr(target, "physical", lambda *args: layout)
    monkeypatch.setenv("MILVUS_LITE_COMPACTION_DIAGNOSTICS", "0")
    def child(*args, **kwargs):
        assert kwargs["env"]["MILVUS_LITE_COMPACTION_DIAGNOSTICS"] == "1"
        return SimpleNamespace(stdout=json.dumps({"stop_seconds": 0.1}), stderr="compaction completed: input_rows=200000 elapsed_seconds=0.2")
    monkeypatch.setattr(target.subprocess, "run", child)
    monkeypatch.setattr(target.importlib.metadata, "version", lambda name: "test-version")
    monkeypatch.setattr(sys, "argv", ["run", "--database", str(seed), "--work-dir", str(tmp_path / "rounds"), "--python", "unused", "--rounds", "1"])
    target.main()
    assert json.loads(capsys.readouterr().out)["records"][0]["round_seconds"] == 0.2
    assert os.environ["MILVUS_LITE_COMPACTION_DIAGNOSTICS"] == "0"
    assert (seed / "marker").read_text() == "same fixture"
