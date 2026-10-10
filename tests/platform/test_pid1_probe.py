"""异常退出与正常退出不能混成同一条通过证据。"""

import importlib.util
from pathlib import Path

import pytest

spec = importlib.util.spec_from_file_location("pid1", Path(__file__).with_name("linux_pid1_probe.py"))
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


@pytest.mark.parametrize("forced,code", [(False, 0), (True, 137)])
def test_expected_exit_matches_platform_contract(forced, code):
    probe.verify_exit({"Running": False, "ExitCode": code, "OOMKilled": False}, forced)


@pytest.mark.parametrize("running,code,oom,forced", [(True, 0, False, False), (False, 137, False, False), (False, 0, False, True), (False, 137, True, True), (False, 143, False, True)])
def test_wrong_or_ambiguous_termination_is_rejected(running, code, oom, forced):
    with pytest.raises(ValueError):
        probe.verify_exit({"Running": running, "ExitCode": code, "OOMKilled": oom}, forced)


def test_docker_failure_preserves_diagnostic_output(monkeypatch):
    def failed(*args, **kwargs):
        raise probe.subprocess.CalledProcessError(1, ["docker", "build"], output="actual build failure")
    monkeypatch.setattr(probe.subprocess, "check_output", failed)
    with pytest.raises(RuntimeError, match="actual build failure"):
        probe.docker("build")


@pytest.mark.parametrize("kind,name,signal,expected", [("container", "own", "9", True), ("container", "other", "9", False), ("container", "own", "15", False), ("volume", "own", "9", False)])
def test_supervisor_event_must_identify_the_actual_container(kind, name, signal, expected):
    import json
    event = {"Type": kind, "Action": "kill", "Actor": {"Attributes": {"name": name, "signal": signal}}}
    assert probe.has_supervisor_kill(json.dumps(event), "own") is expected
