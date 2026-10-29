"""只读接口有限重试（0021 第 11 条传输层）的反证：只重试暂时性传输错误与成功响应中的非法 JSON，确定的回答
原样失败；默认不重试；0022 的保全路径（门闩、save）不重试。"""

import json
import subprocess

import pytest

import candidate as c
import preserve as p
import release_set
import runs
from test_candidate import RUN, VERSION
from test_preserve import approval, comment_for, gate_inputs  # noqa: F401
from test_runs_and_release_set import OWNER, SIGNING, signing_environment

def ok(value):
    return (0, json.dumps(value) if not isinstance(value, str) else value, "")


def fail(message):
    return (1, "", f"gh: {message}")


class Scripted(runs.Gh):
    """按路径逐次给出 gh api 的结果；最后一个结果在用完后重复。"""

    def __init__(self, responses, retry=False):
        self.slept = []
        super().__init__("owner/repo", retry=retry, sleep=self.slept.append)
        self.responses = {path: list(items) for path, items in responses.items()}
        self.calls = []

    def _api(self, path, *extra):
        self.calls.append(path)
        items = self.responses[path]
        code, out, err = items.pop(0) if len(items) > 1 else items[0]
        return subprocess.CompletedProcess(["gh", "api", path], code, out, err)


def notices(capsys):
    return [line for line in capsys.readouterr().err.splitlines() if "transport-retry" in line]


@pytest.mark.parametrize("message", ["Server Error (HTTP 502)", "HTTP 503: Service Unavailable", "HTTP 504",
                                     "HTTP 500", "stream error: stream ID 1; CANCEL; received from peer",
                                     "dial tcp: i/o timeout", "read: connection reset by peer", "unexpected EOF",
                                     "net/http: TLS handshake timeout", "dial tcp 1.2.3.4:443: connect: connection timed out",
                                     'Get "https://api.github.com/repos/o/r": EOF',
                                     "error connecting to api.github.com\ncheck your internet connection or https://githubstatus.com",
                                     "dial tcp [::1]:443: connect: connection refused"])
def test_transient_errors_are_retried(message, capsys):
    gh = Scripted({"x": [fail(message), ok({"a": 1})]}, retry=True)
    assert gh.get("x") == {"a": 1}
    assert gh.calls == ["x", "x"] and gh.slept == [5]
    assert len(notices(capsys)) == 1


@pytest.mark.parametrize("message", ["Not Found (HTTP 404)", "Bad credentials (HTTP 401)", "Forbidden (HTTP 403)",
                                     "Validation Failed (HTTP 422)", "API rate limit exceeded (HTTP 429)",
                                     "HTTP 403: secondary rate limit; timeout", "unknown failure"])
def test_definite_answers_are_not_retried(message, capsys):
    gh = Scripted({"x": [fail(message), ok({"a": 1})]}, retry=True)
    with pytest.raises(c.CandidateError, match="failed"):
        gh.get("x")
    assert gh.calls == ["x"] and gh.slept == [] and notices(capsys) == []


def test_retry_gives_up_after_the_limit():
    gh = Scripted({"x": [fail("HTTP 502")]}, retry=True)
    with pytest.raises(c.CandidateError, match="HTTP 502"):
        gh.get("x")
    assert len(gh.calls) == 4 and gh.slept == [5, 15, 45]


def test_default_instance_does_not_retry():
    gh = Scripted({"x": [fail("HTTP 502"), ok({"a": 1})]})
    with pytest.raises(c.CandidateError, match="HTTP 502"):
        gh.get("x")
    assert gh.calls == ["x"] and gh.slept == []


def test_invalid_json_in_a_successful_response():
    gh = Scripted({"x": [ok('{"a": '), ok('{"a": 1}\n{"a": 2}')]}, retry=True)
    assert gh.items("x", "k") == [{"a": 1}, {"a": 2}]
    assert gh.slept == [5]
    with pytest.raises(json.JSONDecodeError):
        Scripted({"x": [ok('{"a": '), ok({"a": 1})]}).get("x")


def test_absent_keeps_404_as_a_definite_answer():
    assert Scripted({"x": [fail("Not Found (HTTP 404)")]}, retry=True).absent("x") is True
    gh = Scripted({"x": [fail("stream error: CANCEL"), fail("Not Found (HTTP 404)")]}, retry=True)
    assert gh.absent("x") is True and gh.slept == [5]
    assert Scripted({"x": [ok({})]}, retry=True).absent("x") is False
    with pytest.raises(c.CandidateError, match="HTTP 403"):
        Scripted({"x": [fail("HTTP 403")]}, retry=True).absent("x")


# 各调用方的实例：前置检查、签发前复核、absent 与发布准备重试；门闩与 save 不重试。


class Recording(Scripted):
    created = []

    def __init__(self, repo, retry=False, **_):
        Recording.created.append(retry)
        super().__init__({}, retry=retry)


@pytest.fixture
def recording(monkeypatch):
    Recording.created = []
    return Recording


def test_runs_cli_uses_retrying_instances(monkeypatch, recording, tmp_path):
    # main 为全部子命令构造同一个实例：precheck、recheck 与 absent 都经它取数。
    monkeypatch.setattr(runs, "Gh", recording)
    monkeypatch.setattr(recording, "absent", lambda self, path: True)
    assert runs.main(["--repo", "owner/repo", "absent", "--path", "attestations/sha256:" + "0" * 64]) == 0
    seen = []
    monkeypatch.setattr(runs, "recheck", lambda gh, meta, ref: seen.append(gh.retry))
    meta = tmp_path / "meta.json"
    meta.write_text("{}")
    assert runs.main(["--repo", "owner/repo", "recheck", "--meta", str(meta), "--run-id", "1", "--run-attempt", "1"]) == 0
    assert recording.created == [True, True] and seen == [True]


def test_absent_cli_rejects_an_existing_attestation(monkeypatch, recording):
    monkeypatch.setattr(runs, "Gh", recording)
    monkeypatch.setattr(recording, "absent", lambda self, path: False)
    assert runs.main(["--repo", "owner/repo", "absent", "--path", "attestations/sha256:" + "0" * 64]) == 1


def test_release_set_prepare_uses_a_retrying_instance(monkeypatch, tmp_path):
    seen = []
    monkeypatch.setattr(release_set, "prepare_checks", lambda gh, data: seen.append(gh.retry))
    monkeypatch.setattr(release_set, "verify", lambda directory, version, repo, prepare=None: (prepare({}), {"candidate_id": "x"})[1])
    assert release_set.main(["--dir", str(tmp_path), "--version", VERSION, "--repo", "owner/repo", "--prepare"]) == 0
    assert seen == [True]


def gate_args(release_dir, fi_text):
    return ["gate", "--mode", "release", "--environment", SIGNING, "--owner-id", str(OWNER), "--repo", "owner/repo",
            "--version", VERSION, "--candidate-dir", str(release_dir), "--fi-digests", fi_text,
            "--run-id", str(RUN["id"]), "--run-attempt", str(RUN["attempt"])]


@pytest.mark.parametrize("flaky", [f"environments/{SIGNING}", f"actions/runs/{RUN['id']}/approvals"])
def test_gate_does_not_retry(gate_inputs, monkeypatch, flaky):  # noqa: F811
    release_dir, fi_text = gate_inputs
    responses = {f"environments/{SIGNING}": [ok(signing_environment())],
                 f"actions/runs/{RUN['id']}/approvals": [ok([approval(comment_for(release_dir, fi_text))])]}
    responses[flaky] = [fail("Server Error (HTTP 502)")] + responses[flaky]
    instances = []

    class GateGh(Scripted):
        def __init__(self, repo, retry=False, **_):
            super().__init__(responses, retry=retry)
            instances.append(self)

    monkeypatch.setattr(p.runs, "Gh", GateGh)
    assert p.main(gate_args(release_dir, fi_text)) == 1
    assert [gh.retry for gh in instances] == [False] and instances[0].calls.count(flaky) == 1
    # 同一组响应在重试实例上会通过：测试确实能区分重试与不重试。
    assert p.gate(GateGh("owner/repo", retry=True), "release", SIGNING, OWNER, VERSION, RUN, release_dir, fi_text)


def test_save_does_not_retry(monkeypatch, tmp_path):
    created = []

    class SaveGh(p.Gh):
        def __init__(self, repo, retry=False, **kwargs):
            created.append(retry)
            super().__init__(repo, retry=retry, **kwargs)

        def _api(self, path, *extra):
            return subprocess.CompletedProcess(["gh"], 1, "", "gh: Server Error (HTTP 502)")

    monkeypatch.setattr(p, "Gh", SaveGh)
    assert p.main(["save", "--mode", "release", "--repo", "owner/repo", "--run", f"{RUN['id']}.{RUN['attempt']}",
                   "--version", VERSION, "--root", str(tmp_path)]) == 1
    assert created == [False]
