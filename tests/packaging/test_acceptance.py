"""运行器不能在子步骤或最终安装验证失败后输出通过；依赖须按约束安装，全套须经分片工具核对收集数。"""

import json
import subprocess
from pathlib import Path

import pytest
import run_acceptance as runner

NATIVE = "a" * 64


def test_run_propagates_child_failure(tmp_path):
    with pytest.raises(subprocess.CalledProcessError):
        runner.run(["false"], cwd=tmp_path, log=tmp_path / "log")


@pytest.fixture
def constraints(tmp_path):
    path = tmp_path / "constraints.txt"
    path.write_text("pymilvus==3.0.2\n")
    return path


def test_existing_or_inside_repository_work_is_rejected(tmp_path, constraints):
    repo = tmp_path / "repo"
    repo.mkdir()
    with pytest.raises(ValueError, match="outside"):
        runner.execute(repo, repo / "work", constraints=constraints)
    work = tmp_path / "exists"
    work.mkdir()
    with pytest.raises(FileExistsError):
        runner.execute(repo, work, constraints=constraints)


def test_constraints_and_suite_count_are_required_before_resources(tmp_path, constraints):
    work = tmp_path / "work"
    for kwargs, message in [({}, "constraints"), ({"constraints": tmp_path / "missing.txt"}, "constraints"),
                            ({"constraints": constraints, "suite": True}, "expected test count"),
                            ({"constraints": constraints, "suite": True, "suite_expect": 0}, "expected test count")]:
        with pytest.raises(ValueError, match=message):
            runner.execute(tmp_path / "repo", work, **kwargs)
    assert not work.exists()


def fake_steps(work, fail_at=None, summary=None):
    """模拟子步骤；suite-run 写出分片工具的 summary.json。"""
    commands = []

    def run(command, *, cwd, log, env=None):
        commands.append(command)
        name = Path(log).stem
        if name == fail_at:
            raise RuntimeError("failed " + fail_at)
        if name == "build":
            (work / "wheels").mkdir()
            (work / "wheels/fake.whl").touch()
        if name == "sdist":
            (work / "sdist").mkdir()
            (work / "sdist/fake.tar.gz").touch()
        if name == "suite-run":
            out = Path(command[command.index("--out") + 1])
            out.mkdir()
            (out / "summary.json").write_text(json.dumps(summary or {"passed": True, "counts": {"collected": 473, "executed": 473, "group": 0}}))
    return run, commands


def patch(monkeypatch, run, fail_at=None, installed=NATIVE):
    def gate(*args):
        if fail_at == "check":
            raise RuntimeError("failed check")
        return {"tags": ["tag"]}

    def binary(*args):
        if fail_at == "binary":
            raise RuntimeError("failed binary")
        return {}
    monkeypatch.setattr(runner, "run", run)
    monkeypatch.setattr(runner, "check", gate)
    monkeypatch.setattr(runner, "check_binary", binary)
    monkeypatch.setattr(runner, "wheel_native", lambda wheel: NATIVE)
    monkeypatch.setattr(runner, "installed_native", lambda interpreter, cwd: installed)


@pytest.mark.parametrize("reuse,fail_at", [(False, name) for name in ["build", "sdist", "check", "binary", "minimal-smoke", "sdk-smoke", "suite-plan", "suite-run"]]
                         + [(True, name) for name in ["check", "binary", "minimal-smoke", "sdk-smoke", "suite-plan", "suite-run"]])
def test_main_failure_has_no_success_output(reuse, fail_at, tmp_path, monkeypatch, capsys, constraints):
    repo = tmp_path / "repo"
    repo.mkdir()
    work = tmp_path / "work"
    run, _ = fake_steps(work, fail_at)
    patch(monkeypatch, run, fail_at)
    args = ["run", "--repo", str(repo), "--work-dir", str(work), "--constraints", str(constraints), "--suite", "--suite-expect", "473"]
    if reuse:
        wheel, source = tmp_path / "candidate.whl", tmp_path / "candidate.tar.gz"
        wheel.write_bytes(b"wheel")
        source.write_bytes(b"source")
        args += ["--wheel", str(wheel), "--sdist", str(source)]
    monkeypatch.setattr("sys.argv", args)
    with pytest.raises(RuntimeError, match="failed"):
        runner.main()
    assert capsys.readouterr().out == ""


@pytest.mark.parametrize("summary", [
    {"passed": False, "counts": {"collected": 473, "executed": 473, "group": 0}},
    {"passed": True, "counts": {"collected": 472, "executed": 472, "group": 0}},
    {"passed": True, "counts": {"collected": 473, "executed": 470, "group": 0}},
    {"passed": True, "counts": {"collected": 473, "executed": 473, "group": 1}},
    {"passed": True},
])
def test_suite_summary_must_show_exact_count(summary, tmp_path, monkeypatch, constraints):
    work = tmp_path / "work"
    run, _ = fake_steps(work, summary=summary)
    patch(monkeypatch, run)
    with pytest.raises(ValueError, match="suite summary"):
        runner.execute(tmp_path / "repo", work, suite=True, suite_expect=473, constraints=constraints)


def test_native_mismatch_fails(tmp_path, monkeypatch, constraints):
    work = tmp_path / "work"
    run, _ = fake_steps(work)
    patch(monkeypatch, run, installed="b" * 64)
    with pytest.raises(ValueError, match="native module"):
        runner.execute(tmp_path / "repo", work, constraints=constraints)


def test_wheel_native_requires_single_module(tmp_path):
    import zipfile
    wheel = tmp_path / "w.whl"
    with zipfile.ZipFile(wheel, "w") as bundle:
        bundle.writestr("milvus_lite/_native.abi3.so", b"native")
        bundle.writestr("milvus_lite/_native.pyi", b"stub")
    assert runner.wheel_native(wheel) == __import__("hashlib").sha256(b"native").hexdigest()
    with zipfile.ZipFile(wheel, "a") as bundle:
        bundle.writestr("milvus_lite/_native.cpython-310.so", b"other")
    with pytest.raises(ValueError, match="one native module"):
        runner.wheel_native(wheel)


@pytest.mark.parametrize("option", ["--wheel", "--sdist"])
def test_partial_artifact_pair_rejected_before_resources(option, tmp_path, monkeypatch, capsys, constraints):
    work = tmp_path / "work"
    monkeypatch.setattr("sys.argv", ["run", "--work-dir", str(work), "--constraints", str(constraints), option, str(tmp_path / "artifact")])
    with pytest.raises(SystemExit) as error:
        runner.main()
    assert error.value.code == 2
    assert not work.exists()
    assert capsys.readouterr().out == ""


def test_missing_artifact_pair_rejected_before_resources(tmp_path, constraints):
    work = tmp_path / "work"
    with pytest.raises(ValueError, match="existing"):
        runner.execute(tmp_path / "repo", work, wheel=tmp_path / "missing.whl", sdist=tmp_path / "missing.tar.gz", constraints=constraints)
    assert not work.exists()


@pytest.mark.parametrize("python", ["python310", "python315"])
def test_existing_artifacts_are_staged_once_and_never_rebuilt(python, tmp_path, monkeypatch, constraints):
    repo, work = tmp_path / "repo", tmp_path / "work"
    wheel, source = tmp_path / "candidate.whl", tmp_path / "candidate.tar.gz"
    wheel.write_bytes(b"wheel bytes")
    source.write_bytes(b"source bytes")
    run, commands = fake_steps(work)

    def checked(command, *, cwd, log, env=None):
        assert "maturin" not in command
        if "install" in command:
            assert (work / "wheels/candidate.whl").read_bytes() == b"wheel bytes"
            assert command[command.index("--constraints") + 1] == constraints
        if "venv" in command:
            assert command[command.index("--python") + 1] == python
        run(command, cwd=cwd, log=log, env=env)

    def gate(w, s, r):
        assert Path(w).read_bytes() == b"wheel bytes"
        assert Path(s).read_bytes() == b"source bytes"
        wheel.write_bytes(b"external mutation")
        return {"tags": ["tag"], "wheel_sha256": "frozen"}
    patch(monkeypatch, checked)
    monkeypatch.setattr(runner, "check", gate)
    result = runner.execute(repo, work, python=python, suite=True, suite_expect=473, wheel=wheel, sdist=source, constraints=constraints)
    assert result["passed"] and result["native_sha256"] == NATIVE
    assert result["suite"] == {"collected": 473, "executed": 473, "group": 0}
    assert result["constraints_sha256"] == __import__("hashlib").sha256(constraints.read_bytes()).hexdigest()
    assert [Path(c[1]).name for c in commands if c[0] != "uv"] == ["install_smoke.py", "install_smoke.py", "shard.py", "shard.py"]
    plan = commands[-2]
    assert plan[2] == "plan" and plan[plan.index("--suite") + 1] == repo.resolve() / "tests/python"
    for option, value in [("--shards", "1"), ("--order", "round-robin"), ("--expect", 473), ("--expect-group", "0"), ("--cwd", work / "outside")]:
        assert plan[plan.index(option) + 1] == value
    assert commands[-1][2] == "run"
