"""候选结果、阶段矩阵、SHA256SUMS 与版本台账：生成结果可被读回，任何缺键、多键、缺阶段、计数或格式偏差都被拒绝。"""

import copy
import json
from pathlib import Path

import pytest

import candidate as c
import ledger
import scorecard

VERSION = "1.2.0rc3"
COMMIT = "a" * 40
RUN = {"id": 100, "attempt": 2}


def identity():
    return {"commit": COMMIT, "version": VERSION, "archive_sha256": "1" * 64, "source_entries": 10, "regular_files": 9,
            "symlinks": 1, "product_inputs": 5, "source_manifest_sha256": "2" * 64, "product_manifest_sha256": "3" * 64,
            "tool_versions": {"git": "git version 2.53.0", "python": "CPython 3.12.3"}}


def meta(mode="release"):
    return {"mode": mode, "version": VERSION, "commit": COMMIT, "run": dict(RUN),
            "inherited": {"ci": {"id": 1, "attempt": 1}, "compare": {"id": 2, "attempt": 3}},
            "pid1_dry_run": {"id": 90, "attempt": 1} if mode == "release" else None}


IMAGE_ID = "sha256:" + "e" * 64


def stage_result(scope, stage):
    counts = {k: 1 if rule == scorecard.POSITIVE else rule for k, rule in scorecard.MATRIX[scope][stage].items()}
    environment = {k: "v" for k in scorecard.environment_keys(scope)}
    if "pid1_image" in environment:
        environment["pid1_image"] = f"{scorecard.PID1_IMAGES[scope]} id {IMAGE_ID}" if stage == "pid1" else scorecard.NOT_USED
    return {"returncode": 0, "counts": counts, "environment": environment}


def pid1_env(scope):
    return lambda d: d["stages"][scope]["pid1"]["environment"]


def stages():
    return {scope: {stage: stage_result(scope, stage) for stage in entries} for scope, entries in scorecard.MATRIX.items()}


def build_env():
    return {p: {k: "v" for k in scorecard.build_environment_keys(p)} for p in scorecard.PLATFORMS}


def build_inputs(tmp_path):
    artifacts, fi = tmp_path / "artifacts", tmp_path / "fi"
    artifacts.mkdir()
    for name in c.formal_names(VERSION):
        (artifacts / name).write_bytes(name.encode())
    for tag in c.WHEEL_TAGS:
        (fi / tag).mkdir(parents=True)
        (fi / tag / f"milvus_lite_rust-{VERSION}-cp310-abi3-{tag}.whl").write_bytes(b"fi " + tag.encode())
    return artifacts, fi


@pytest.fixture
def inputs(tmp_path):
    return build_inputs(tmp_path)


def written(inputs, tmp_path, mode="release"):
    return c.write(meta(mode), identity(), *inputs, stages(), build_env(), tmp_path / f"out-{mode}")


def test_matrix_shape():
    assert set(scorecard.MATRIX) == {*scorecard.PLATFORMS, "sdist"}
    for platform in scorecard.PLATFORMS:
        stages_ = scorecard.MATRIX[platform]
        assert stages_["python-3.15-2.6.17"] == {"collected": 473, "executed": 473, "group": 0}
        assert stages_["differential-3.0.2"] == {"collected": 421, "executed": 421, "group": 118}
        assert ("pid1" in stages_) == platform.startswith("linux-")
    assert "previous-reader" in scorecard.MATRIX["linux-x86_64"] and "previous-reader" not in scorecard.MATRIX["linux-aarch64"]


def test_scorecard_expect_cli(capsys):
    scorecard.main(["expect", "macos-x86_64", "interop", "collected"])
    assert capsys.readouterr().out == "211\n"
    with pytest.raises(SystemExit):
        scorecard.main(["expect", "linux-x86_64", "rust-default", "passed"])


def test_write_and_reload(inputs, tmp_path):
    path = written(inputs, tmp_path)
    assert path.name == "candidate-1.2.0rc3-100-2.json"
    data = c.load(path)
    assert data["candidate_id"] == f"{VERSION}@{COMMIT}#100.2"
    assert data["artifacts"] == {n: c.sha256_file(inputs[0] / n) for n in c.formal_names(VERSION)}
    sums = c.parse_sums((path.parent / "SHA256SUMS").read_text())
    assert sorted(sums) == sorted(c.formal_names(VERSION) + [path.name])
    assert sums[path.name] == c.sha256_file(path)
    assert "SHA256SUMS" not in path.read_text()


def test_dry_run_names_are_unique_to_the_run(inputs, tmp_path):
    path = written(inputs, tmp_path, "dry-run")
    assert path.name == "dry-run-candidate-1.2.0rc3-100-2.json"
    assert sorted(p.name for p in path.parent.iterdir()) == ["dry-run-SHA256SUMS-100-2", path.name]
    assert c.load(path)["mode"] == "dry-run"


def first(d):
    return next(iter(d))


MUTATIONS = {
    "missing key": lambda d: d.pop("stages"),
    "extra key": lambda d: d.update(note="x"),
    "unknown format": lambda d: d.update(format=2),
    "boolean format": lambda d: d.update(format=True),
    "unknown scorecard": lambda d: d.update(scorecard=2),
    "unknown mode": lambda d: d.update(mode="test"),
    "short commit": lambda d: d.update(commit="a" * 7),
    "candidate id": lambda d: d.update(candidate_id="x"),
    "run attempt zero": lambda d: d["run"].update(attempt=0),
    "source commit": lambda d: d["source"].update(commit="b" * 40),
    "source counts": lambda d: d["source"].update(symlinks=2),
    "source extra key": lambda d: d["source"].update(extra=1),
    "source tools missing": lambda d: d["source"].pop("tool_versions"),
    "source tool missing": lambda d: d["source"]["tool_versions"].pop("git"),
    "source tool extra": lambda d: d["source"]["tool_versions"].update(tar="bsdtar 3.7"),
    "source tool empty": lambda d: d["source"]["tool_versions"].update(python=" "),
    "source git not a version": lambda d: d["source"]["tool_versions"].update(git="unknown"),
    "inherited missing": lambda d: d["inherited"].pop("compare"),
    "inherited string id": lambda d: d["inherited"]["ci"].update(id="1"),
    "release without pid1 dry run": lambda d: d.update(pid1_dry_run=None),
    "pid1 ref keys": lambda d: d.update(pid1_dry_run={"id": 1}),
    "artifact missing": lambda d: d["artifacts"].pop(first(d["artifacts"])),
    "artifact renamed": lambda d: d["artifacts"].update({"other.whl": d["artifacts"].pop(first(d["artifacts"]))}),
    "artifact digest": lambda d: d["artifacts"].update({first(d["artifacts"]): "Z" * 64}),
    "fi missing": lambda d: d["fi_wheels"].pop("macosx_11_0_x86_64"),
    "platform missing": lambda d: d["stages"].pop("macos-x86_64"),
    "extra scope": lambda d: d["stages"].update({"only-scope": {"only-stage": {"returncode": 0, "counts": {"foo": 0}, "environment": {}}}}),
    "only one stage": lambda d: d.update(stages={"only-scope": {"only-stage": {"returncode": 0, "counts": {"foo": 0}, "environment": {}}}}),
    "stage missing": lambda d: d["stages"]["linux-aarch64"].pop("pid1"),
    "pid1 image not recorded": lambda d: pid1_env("linux-x86_64")(d).update(pid1_image=scorecard.NOT_USED),
    "pid1 image wrong digest": lambda d: pid1_env("linux-x86_64")(d).update(pid1_image=f"python@sha256:{'0' * 64} id {IMAGE_ID}"),
    "pid1 image of other platform": lambda d: pid1_env("linux-aarch64")(d).update(
        pid1_image=pid1_env("linux-x86_64")(d)["pid1_image"]),
    "pid1 image without id": lambda d: pid1_env("linux-x86_64")(d).update(pid1_image=scorecard.PID1_IMAGES["linux-x86_64"]),
    "pid1 image on other stage": lambda d: d["stages"]["linux-x86_64"]["lock"]["environment"].update(
        pid1_image=pid1_env("linux-x86_64")(d)["pid1_image"]),
    "extra stage": lambda d: d["stages"]["macos-arm64"].update(pid1=d["stages"]["linux-x86_64"]["pid1"]),
    "failed stage": lambda d: d["stages"]["linux-x86_64"]["interop"].update(returncode=1),
    "count short": lambda d: d["stages"]["macos-arm64"]["python-3.10-3.0.2"]["counts"].update(executed=472),
    "group differs": lambda d: d["stages"]["macos-arm64"]["differential-2.6.17"]["counts"].update(group=117),
    "positive count zero": lambda d: d["stages"]["linux-x86_64"]["rust-fi"]["counts"].update(passed=0),
    "ignored rust": lambda d: d["stages"]["linux-x86_64"]["rust-default"]["counts"].update(ignored=1),
    "boolean count": lambda d: d["stages"]["linux-x86_64"]["rust-default"]["counts"].update(failed=False),
    "extra count key": lambda d: d["stages"]["linux-x86_64"]["interop"]["counts"].update(skipped=0),
    "missing count key": lambda d: d["stages"]["linux-x86_64"]["interop"]["counts"].pop("group"),
    "stage extra key": lambda d: d["stages"]["linux-x86_64"]["interop"].update(skipped=0),
    "environment missing key": lambda d: d["stages"]["macos-arm64"]["interop"]["environment"].pop("xcode"),
    "environment linux key on mac": lambda d: d["stages"]["macos-arm64"]["interop"]["environment"].update(docker="x"),
    "environment empty value": lambda d: d["stages"]["sdist"]["inputs"]["environment"].update(rustc=" "),
    "build env platform missing": lambda d: d["build_environment"].pop("macos-x86_64"),
    "build env key missing": lambda d: d["build_environment"]["linux-aarch64"].pop("maturin"),
    "build env mac key on linux": lambda d: d["build_environment"]["linux-x86_64"].update(xcode="x"),
    "build env empty": lambda d: d["build_environment"]["macos-arm64"].update(rustc=""),
}


@pytest.mark.parametrize("name", MUTATIONS)
def test_validate_rejects(inputs, tmp_path, name):
    data = json.loads(written(inputs, tmp_path).read_text())
    c.validate(copy.deepcopy(data))
    MUTATIONS[name](data)
    with pytest.raises(c.CandidateError):
        c.validate(data)


def test_dry_run_must_not_reference_dry_run(inputs, tmp_path):
    data = json.loads(written(inputs, tmp_path, "dry-run").read_text())
    data["pid1_dry_run"] = {"id": 1, "attempt": 1}
    with pytest.raises(c.CandidateError, match="does not reference"):
        c.validate(data)


def test_load_rejects_name_content_mismatch(inputs, tmp_path):
    path = written(inputs, tmp_path)
    data = json.loads(path.read_text())
    data["mode"], data["pid1_dry_run"] = "dry-run", None
    path.write_text(json.dumps(data))
    with pytest.raises(c.CandidateError, match="does not match content"):
        c.load(path)
    path.write_text("{")
    with pytest.raises(c.CandidateError, match="cannot read"):
        c.load(path)


def test_build_rejects_artifact_set_changes(inputs):
    artifacts, fi = inputs
    (artifacts / "extra.whl").write_bytes(b"")
    with pytest.raises(c.CandidateError, match="formal artifact set differs"):
        c.build(meta(), identity(), artifacts, fi, stages(), build_env())
    (artifacts / "extra.whl").unlink()
    (artifacts / c.formal_names(VERSION)[0]).unlink()
    with pytest.raises(c.CandidateError, match="formal artifact set differs"):
        c.build(meta(), identity(), artifacts, fi, stages(), build_env())


def test_build_rejects_duplicate_fi(inputs):
    artifacts, fi = inputs
    (fi / "copy").mkdir()
    (fi / "copy" / f"milvus_lite_rust-{VERSION}-cp310-abi3-macosx_11_0_arm64.whl").write_bytes(b"")
    with pytest.raises(c.CandidateError, match="expected one FI wheel"):
        c.build(meta(), identity(), artifacts, fi, stages(), build_env())


def test_merge_stages(tmp_path):
    def stage(name, scope, stage_name, **extra):
        path = tmp_path / name
        path.write_text(json.dumps({"scope": scope, "stage": stage_name, "returncode": 0, "counts": {"passed": 1},
                                    "environment": {"uv": "0.12.23"}, **extra}))
        return path
    assert c.merge_stages([stage("a", "mac", "x"), stage("b", "mac", "y")]) == {
        "mac": {name: {"returncode": 0, "counts": {"passed": 1}, "environment": {"uv": "0.12.23"}} for name in "xy"}}
    with pytest.raises(c.CandidateError, match="duplicate stage"):
        c.merge_stages([stage("a", "mac", "x"), stage("c", "mac", "x")])
    with pytest.raises(c.CandidateError, match="keys differ"):
        c.merge_stages([stage("d", "mac", "z", note="x")])


@pytest.mark.parametrize("text", ["", "abc  x\n", f"{'a' * 64}  x", f"{'a' * 64} x\n", f"{'a' * 64}  dir/x\n",
                                  f"{'a' * 64}  x\n{'b' * 64}  x\n"])
def test_parse_sums_rejects(text):
    with pytest.raises(c.CandidateError):
        c.parse_sums(text)


def entry(version="1.0.0rc1", **changes):
    return {"version": version, "run_id": 5, "run_attempt": 1, "outcome": "released", **changes}


def test_ledger_reads_consumed_versions():
    entries = ledger.loads(json.dumps({"format": 1, "versions": [entry(), entry("1.0.0rc2", outcome="signing-failed")]}))
    assert set(entries) == {"1.0.0rc1", "1.0.0rc2"}
    assert ledger.loads(json.dumps({"format": 1, "versions": []})) == {}


@pytest.mark.parametrize("data", [
    "not json",
    json.dumps([]),
    json.dumps({"format": 2, "versions": []}),
    json.dumps({"format": 1}),
    json.dumps({"format": 1, "versions": {}}),
    json.dumps({"format": 1, "versions": [entry(), entry()]}),
    json.dumps({"format": 1, "versions": [entry(outcome="published")]}),
    json.dumps({"format": 1, "versions": [entry(run_id=0)]}),
    json.dumps({"format": 1, "versions": [entry(version="v1.0.0")]}),
    json.dumps({"format": 1, "versions": [{**entry(), "note": "x"}]}),
])
def test_ledger_fails_closed(data):
    with pytest.raises(c.CandidateError):
        ledger.loads(data)


def test_ledger_preparable():
    entries = {"1.0.0": entry("1.0.0", outcome="candidate")}
    ledger.check_preparable({}, "1.0.0", {"id": 5, "attempt": 1})
    ledger.check_preparable(entries, "1.0.0", {"id": 5, "attempt": 1})
    for run in ({"id": 6, "attempt": 1}, {"id": 5, "attempt": 2}):
        with pytest.raises(c.CandidateError, match="ledger records run"):
            ledger.check_preparable(entries, "1.0.0", run)
    for outcome in ("released", "abandoned", "signing-failed"):
        with pytest.raises(c.CandidateError, match="outcome"):
            ledger.check_preparable({"1.0.0": entry("1.0.0", outcome=outcome)}, "1.0.0", {"id": 5, "attempt": 1})


def test_repository_ledger_is_valid():
    root = Path(__file__).resolve().parents[2]
    assert ledger.main([str(root / "docs/releases/consumed-versions.json")]) == 0


def test_write_cli_rejects_result_from_another_attempt(inputs, tmp_path, capsys):
    meta_path, identity_path = tmp_path / "meta.json", tmp_path / "identity.json"
    meta_path.write_text(json.dumps(meta()))
    identity_path.write_text(json.dumps(identity()))
    stage_files = []
    for scope, entries in stages().items():
        for name, result in entries.items():
            path = tmp_path / f"{scope}--{name}.json"
            path.write_text(json.dumps({"scope": scope, "stage": name, **result}))
            stage_files += ["--stage", str(path)]
    envs = tmp_path / "envs"
    envs.mkdir()
    for platform, values in build_env().items():
        (envs / f"build-env-{platform}.json").write_text(json.dumps(values))
    base = ["write", "--meta", str(meta_path), "--identity", str(identity_path), "--artifacts", str(inputs[0]),
            "--fi", str(inputs[1]), "--build-env", str(envs), *stage_files]
    assert c.main(base + ["--out", str(tmp_path / "a"), "--run-id", "100", "--run-attempt", "3"]) == 1
    assert "re-run all jobs" in capsys.readouterr().err and not (tmp_path / "a").exists()
    assert c.main(base + ["--out", str(tmp_path / "b"), "--run-id", "100", "--run-attempt", "2"]) == 0


def test_read_build_environment_requires_every_platform(tmp_path):
    for platform, values in build_env().items():
        (tmp_path / f"build-env-{platform}.json").write_text(json.dumps(values))
    assert set(c.read_build_environment(tmp_path)) == set(scorecard.PLATFORMS)
    (tmp_path / "build-env-macos-x86_64.json").unlink()
    with pytest.raises(c.CandidateError, match="build environment records differ"):
        c.read_build_environment(tmp_path)
