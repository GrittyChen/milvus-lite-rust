"""运行器不能在子步骤或最终安装验证失败后输出通过。"""

import subprocess
from pathlib import Path

import pytest
import run_acceptance as runner


def test_run_propagates_child_failure(tmp_path):
    with pytest.raises(subprocess.CalledProcessError):
        runner.run(["false"], cwd=tmp_path, log=tmp_path / "log")


def test_existing_or_inside_repository_work_is_rejected(tmp_path):
    repo = tmp_path / "repo"
    repo.mkdir()
    with pytest.raises(ValueError, match="outside"):
        runner.execute(repo, repo / "work")
    work = tmp_path / "exists"
    work.mkdir()
    with pytest.raises(FileExistsError):
        runner.execute(repo, work)


@pytest.mark.parametrize("reuse,fail_at", [(False, name) for name in ["build", "sdist", "check", "binary", "minimal-smoke", "sdk-smoke", "suite"]] + [(True, name) for name in ["check", "binary", "minimal-smoke", "sdk-smoke", "suite"]])
def test_main_failure_has_no_success_output(reuse, fail_at, tmp_path, monkeypatch, capsys):
    repo = tmp_path / "repo"
    repo.mkdir()
    work = tmp_path / "work"
    def run(command, *, cwd, log, env=None):
        name = Path(log).stem
        if name == fail_at or (fail_at == "suite" and name == "python"):
            raise RuntimeError("failed " + fail_at)
        if name == "build":
            (work / "wheels").mkdir()
            (work / "wheels/fake.whl").touch()
        if name == "sdist":
            (work / "sdist").mkdir()
            (work / "sdist/fake.tar.gz").touch()
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
    args = ["run", "--repo", str(repo), "--work-dir", str(work), "--suite"]
    if reuse:
        wheel, source = tmp_path / "candidate.whl", tmp_path / "candidate.tar.gz"
        wheel.write_bytes(b"wheel")
        source.write_bytes(b"source")
        args += ["--wheel", str(wheel), "--sdist", str(source)]
    monkeypatch.setattr("sys.argv", args)
    with pytest.raises(RuntimeError, match="failed"):
        runner.main()
    assert capsys.readouterr().out == ""


@pytest.mark.parametrize("option", ["--wheel", "--sdist"])
def test_partial_artifact_pair_rejected_before_resources(option, tmp_path, monkeypatch, capsys):
    work = tmp_path / "work"
    monkeypatch.setattr("sys.argv", ["run", "--work-dir", str(work), option, str(tmp_path / "artifact")])
    with pytest.raises(SystemExit) as error:
        runner.main()
    assert error.value.code == 2
    assert not work.exists()
    assert capsys.readouterr().out == ""


def test_missing_artifact_pair_rejected_before_resources(tmp_path):
    work = tmp_path / "work"
    with pytest.raises(ValueError, match="existing"):
        runner.execute(tmp_path / "repo", work, wheel=tmp_path / "missing.whl", sdist=tmp_path / "missing.tar.gz")
    assert not work.exists()


@pytest.mark.parametrize("python", ["python310", "python315"])
def test_existing_artifacts_are_staged_once_and_never_rebuilt(python, tmp_path, monkeypatch):
    repo, work = tmp_path / "repo", tmp_path / "work"
    wheel, source = tmp_path / "candidate.whl", tmp_path / "candidate.tar.gz"
    wheel.write_bytes(b"wheel bytes")
    source.write_bytes(b"source bytes")
    commands = []
    def gate(w, s, r):
        assert Path(w).read_bytes() == b"wheel bytes"
        assert Path(s).read_bytes() == b"source bytes"
        wheel.write_bytes(b"external mutation")
        return {"tags": ["tag"], "wheel_sha256": "frozen"}
    def run(command, *, cwd, log, env=None):
        commands.append(command)
        assert "maturin" not in command
        if "install" in command:
            assert (work / "wheels/candidate.whl").read_bytes() == b"wheel bytes"
        if "venv" in command:
            assert command[command.index("--python") + 1] == python
    monkeypatch.setattr(runner, "run", run)
    monkeypatch.setattr(runner, "check", gate)
    monkeypatch.setattr(runner, "check_binary", lambda *args: {})
    result = runner.execute(repo, work, python=python, suite=True, wheel=wheel, sdist=source)
    assert result["passed"]
    assert [Path(c[1]).name for c in commands if c[0] != "uv"] == ["install_smoke.py", "install_smoke.py", "-m"]
    assert commands[-1][1:4] == ["-m", "pytest", repo / "tests/python"]
