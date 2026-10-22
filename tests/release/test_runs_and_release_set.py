"""前置检查、签发前复核与发布附件核对的反证：进行中或失败的新运行、已消费的版本（含较早 attempt）、
缺少 PID 1 演练、他次运行的证明、演练结果混入与篡改都必须被拒绝。"""

import json
import shutil
from types import SimpleNamespace

import pytest

import candidate as c
import release_set
import runs
from test_candidate import COMMIT, VERSION, build_env, build_inputs, identity, meta, stages

CI = ".github/workflows/ci.yml"
COMPARE = ".github/workflows/compare.yml"


def run(id, created, status="completed", conclusion="success", sha=COMMIT, path=CI, attempt=1, title="CI"):
    return {"id": id, "run_attempt": attempt, "created_at": created, "status": status, "conclusion": conclusion,
            "head_sha": sha, "path": path, "display_title": title}


def test_latest_run_is_by_creation_not_completion():
    assert runs.select_latest([run(1, "10:00"), run(2, "11:00")], COMMIT, CI)["id"] == 2
    with pytest.raises(c.CandidateError, match="in_progress"):
        runs.select_latest([run(1, "10:00"), run(2, "11:00", "in_progress", None)], COMMIT, CI)
    with pytest.raises(c.CandidateError, match="failure"):
        runs.select_latest([run(1, "10:00"), run(2, "11:00", conclusion="failure")], COMMIT, CI)
    for conclusion in ("cancelled", "skipped"):
        with pytest.raises(c.CandidateError):
            runs.select_latest([run(1, "10:00"), run(2, "11:00", conclusion=conclusion)], COMMIT, CI)
    with pytest.raises(c.CandidateError, match="no .* run"):
        runs.select_latest([run(1, "10:00", sha="b" * 40)], COMMIT, CI)


def test_other_workflows_and_commits_do_not_count():
    rows = [run(1, "10:00"), run(2, "12:00", sha="b" * 40, conclusion="failure"),
            run(3, "12:00", path=".github/workflows/x.yml", conclusion="failure")]
    assert runs.select_latest(rows, COMMIT, CI)["id"] == 1


def job(name, status="completed", conclusion="success", steps=None, runner="runner"):
    return {"name": name, "status": status, "conclusion": conclusion, "steps": steps, "runner_name": runner}


def step(status="completed", conclusion="success"):
    return {"name": runs.SIGN_STEP, "status": status, "conclusion": conclusion}


def test_summary_job_must_be_single_success():
    runs.check_job([job("ci-pass")], "ci-pass")
    for jobs in ([], [job("ci-pass", conclusion="skipped")], [job("ci-pass"), job("ci-pass")]):
        with pytest.raises(c.CandidateError):
            runs.check_job(jobs, "ci-pass")


@pytest.mark.parametrize("jobs, started", [
    ([job("attest", steps=[step()])], True),
    ([job("attest", conclusion="failure", steps=[step(conclusion="failure")])], True),
    ([job("attest", "in_progress", None, steps=[step("in_progress", None)])], True),
    ([job("attest", conclusion="failure", steps=[])], True),
    ([job("attest", conclusion="failure", steps=[step(conclusion="skipped")])], False),
    ([job("attest", "in_progress", None, steps=[step("queued", None)])], False),
    ([job("attest", conclusion="skipped", steps=[])], False),
    ([job("attest", conclusion="cancelled", steps=[], runner=None)], False),
    ([job("candidate", steps=[step()])], False),
])
def test_signing_started(jobs, started):
    assert runs.signing_started(jobs) is started


def dry_run(**changes):
    return {**run(7, "09:00", path=runs.RELEASE_WORKFLOW, title=f"dry-run v{VERSION}"), **changes}


def test_dry_run_reference():
    pid1 = [job(name) for name in runs.PID1_JOBS]
    runs.check_dry_run(dry_run(), pid1, COMMIT, VERSION)
    for bad in (dry_run(head_sha="b" * 40), dry_run(conclusion="failure"), dry_run(display_title=f"release v{VERSION}"),
                dry_run(path=CI)):
        with pytest.raises(c.CandidateError):
            runs.check_dry_run(bad, pid1, COMMIT, VERSION)
    for jobs in (pid1[:1], [], [job(runs.PID1_JOBS[0]), job(runs.PID1_JOBS[1], conclusion="failure")]):
        with pytest.raises(c.CandidateError, match="pid1"):
            runs.check_dry_run(dry_run(), jobs, COMMIT, VERSION)


class FakeGh:
    """按 REST 路径返回固定数据；jobs 以 (运行 ID, attempt) 为键，未登记的运行返回成功的汇总 job。"""

    def __init__(self):
        self.ci = [run(1, "10:00")]
        self.compare = [run(2, "10:00", path=COMPARE, title="Compare")]
        self.release_runs, self.jobs_by_attempt = [], {}
        self.tags, self.releases, self.ledger = set(), [], {"format": 1, "versions": []}
        self.attempts = {"actions/runs/7/attempts/1": dry_run()}
        self.jobs_by_attempt[(7, 1)] = [job(name) for name in runs.PID1_JOBS]

    def workflow_runs(self, path, query=""):
        return {CI: self.ci, COMPARE: self.compare, runs.RELEASE_WORKFLOW: self.release_runs}[path]

    def jobs(self, run_id, attempt):
        return self.jobs_by_attempt.get((run_id, attempt), [job("ci-pass"), job("compare-pass")])

    def absent(self, path):
        return path.removeprefix("git/ref/tags/") not in self.tags

    def items(self, path, key):
        return self.releases

    def raw(self, path):
        return json.dumps(self.ledger)

    def get(self, path):
        return self.attempts[path]


def args(**changes):
    return SimpleNamespace(**{**dict(sha=COMMIT, version=VERSION, mode="release", run_id=100, run_attempt=1,
                                     pid1_dry_run="7.1"), **changes})


@pytest.fixture
def gh():
    return FakeGh()


def release_run(id, attempt):
    return run(id, "08:00", conclusion="failure", path=runs.RELEASE_WORKFLOW, attempt=attempt, title=f"release v{VERSION}")


def test_precheck_passes_and_records_runs(gh):
    out = runs.precheck(gh, args())
    assert out["inherited"] == {"ci": {"id": 1, "attempt": 1}, "compare": {"id": 2, "attempt": 1}}
    assert out["pid1_dry_run"] == {"id": 7, "attempt": 1}
    assert out["inherited_runs"] == {"ci": [[1, 1]], "compare": [[2, 1]]}
    assert set(out) == {"mode", "version", "commit", "run", "inherited", "inherited_runs", "pid1_dry_run"}


@pytest.mark.parametrize("mutate, message", [
    (lambda gh: gh.ci.append(run(3, "11:00", "in_progress", None)), "in_progress"),
    (lambda gh: gh.ci.append(run(3, "11:00", conclusion="failure")), "failure"),
    (lambda gh: gh.jobs_by_attempt.update({(1, 1): [job("ci-pass", conclusion="failure")]}), "ci-pass"),
    (lambda gh: gh.tags.add(f"v{VERSION}"), "tag"),
    (lambda gh: gh.releases.append({"tag_name": f"v{VERSION}", "name": "x"}), "release"),
    (lambda gh: gh.ledger["versions"].append({"version": VERSION, "run_id": 5, "run_attempt": 1, "outcome": "abandoned"}), "ledger"),
    (lambda gh: gh.ledger.update(format=9), "ledger format"),
    (lambda gh: (gh.release_runs.append(release_run(50, 1)),
                 gh.jobs_by_attempt.update({(50, 1): [job("attest", conclusion="failure", steps=[step(conclusion="failure")])]})),
     "started signing"),
    (lambda gh: gh.attempts.update({"actions/runs/7/attempts/1": dry_run(head_sha="b" * 40)}), "dry run"),
    (lambda gh: gh.jobs_by_attempt.update({(7, 1): []}), "pid1"),
])
def test_precheck_rejects(gh, mutate, message):
    mutate(gh)
    with pytest.raises(c.CandidateError, match=message):
        runs.precheck(gh, args())


def test_earlier_attempt_that_started_signing_consumes_the_version(gh):
    gh.release_runs.append(release_run(50, 2))
    gh.jobs_by_attempt[(50, 1)] = [job("attest", conclusion="failure", steps=[step(conclusion="failure")])]
    gh.jobs_by_attempt[(50, 2)] = [job("attest", conclusion="skipped", steps=[])]
    with pytest.raises(c.CandidateError, match="attempt 1 started signing"):
        runs.precheck(gh, args())


def test_current_runs_earlier_attempt_that_started_signing_consumes_the_version(gh):
    # 本运行 attempt 1 已开始签发后失败，负责人重跑全部 job 进入 attempt 2：仍须拒绝。
    gh.release_runs.append(release_run(100, 2))
    gh.jobs_by_attempt[(100, 1)] = [job("attest", conclusion="failure", steps=[step(conclusion="failure")])]
    with pytest.raises(c.CandidateError, match="run 100 attempt 1 started signing"):
        runs.precheck(gh, args(run_attempt=2))


def test_current_attempt_itself_is_not_counted(gh):
    gh.release_runs.append(release_run(100, 1))
    gh.jobs_by_attempt[(100, 1)] = [job("attest", "in_progress", None, steps=[step("in_progress", None)])]
    runs.precheck(gh, args(run_attempt=1))


def test_recheck_rejects_partial_rerun(gh):
    recorded = runs.precheck(gh, args())
    with pytest.raises(c.CandidateError, match="re-run all jobs"):
        runs.recheck(gh, recorded, {"id": 100, "attempt": 2})


def test_failures_before_signing_and_dry_runs_do_not_consume(gh):
    gh.release_runs.append(release_run(50, 2))
    gh.jobs_by_attempt[(50, 1)] = [job("attest", conclusion="skipped", steps=[])]
    gh.jobs_by_attempt[(50, 2)] = [job("attest", conclusion="failure", steps=[step(conclusion="skipped")])]
    gh.release_runs.append(dry_run(id=60))
    gh.jobs_by_attempt[(60, 1)] = [job("attest", steps=[step()])]
    runs.precheck(gh, args())


def test_every_release_run_needs_its_own_pid1_dry_run(gh):
    gh.ledger["versions"].append({"version": "0.9.0", "run_id": 5, "run_attempt": 1, "outcome": "released"})
    with pytest.raises(c.CandidateError, match="must reference a dry run"):
        runs.precheck(gh, args(pid1_dry_run=""))
    with pytest.raises(c.CandidateError, match="run reference"):
        runs.precheck(gh, args(pid1_dry_run="7"))


def test_dry_run_mode(gh):
    gh.tags.add(f"v{VERSION}")
    assert runs.precheck(gh, args(mode="dry-run", pid1_dry_run=""))["pid1_dry_run"] is None
    with pytest.raises(c.CandidateError, match="does not reference"):
        runs.precheck(gh, args(mode="dry-run"))


def test_recheck_rejects_newer_inherited_run_and_consumed_version(gh):
    recorded = runs.precheck(gh, args())
    runs.recheck(gh, recorded, {"id": 100, "attempt": 1})
    gh.compare.append(run(9, "12:00", path=COMPARE, title="Compare"))
    with pytest.raises(c.CandidateError, match="newer"):
        runs.recheck(gh, recorded, {"id": 100, "attempt": 1})
    gh.compare.pop()
    gh.ci[0] = dict(gh.ci[0], run_attempt=2)
    with pytest.raises(c.CandidateError, match="newer"):
        runs.recheck(gh, recorded, {"id": 100, "attempt": 1})
    gh.ci[0] = dict(gh.ci[0], run_attempt=1)
    gh.tags.add(f"v{VERSION}")
    with pytest.raises(c.CandidateError, match="consumed"):
        runs.recheck(gh, recorded, {"id": 100, "attempt": 1})


@pytest.mark.parametrize("status, conclusion", [("queued", None), ("in_progress", None), ("completed", "failure"),
                                                ("completed", "success")])
def test_recheck_rejects_rerun_of_an_older_run(gh, status, conclusion):
    # 前置检查时较旧的运行 A 已成功、较新的 B 被选中；构建期间重跑 A，A 的创建时间不变，按创建时间仍选 B。
    gh.ci.insert(0, run(3, "09:00"))
    recorded = runs.precheck(gh, args())
    assert recorded["inherited"]["ci"] == {"id": 1, "attempt": 1}
    gh.ci[0] = run(3, "09:00", status, conclusion, attempt=2)
    with pytest.raises(c.CandidateError, match="re-run appeared"):
        runs.recheck(gh, recorded, {"id": 100, "attempt": 1})


REPO = "owner/repo"


@pytest.fixture
def release_dir(tmp_path):
    artifacts, fi = build_inputs(tmp_path)
    out = c.write(meta(), identity(), artifacts, fi, stages(), build_env(), tmp_path / "out")
    directory = tmp_path / "release"
    directory.mkdir()
    for name in c.formal_names(VERSION):
        shutil.copy2(artifacts / name, directory / name)
    shutil.copy2(out, directory / out.name)
    shutil.copy2(out.parent / "SHA256SUMS", directory / "SHA256SUMS")
    return directory


def statement(subjects, run_id=100, attempt=2, commit=COMMIT, workflow=runs.RELEASE_WORKFLOW):
    invocation = f"https://github.com/{REPO}/actions/runs/{run_id}/attempts/{attempt}"
    return {"subject": [{"name": n, "digest": {"sha256": d}} for n, d in subjects.items()],
            "predicate": {"runDetails": {"metadata": {"invocationId": invocation}},
                          "buildDefinition": {"externalParameters": {"workflow": {"path": workflow}},
                                              "resolvedDependencies": [{"digest": {"gitCommit": commit}}]}}}


def duplicated_subject(signed):
    signed["subject"].append(dict(signed["subject"][0]))
    return signed


def transparency_log(directory, *extra):
    """模拟透明日志：签发时的七个文件共享一个多主体证明；extra 为同摘要上的其他证明。"""
    log = [statement({p.name: c.sha256_file(p) for p in directory.iterdir()}), *extra]
    calls = []

    def statements_for(path, repo, commit=None):
        calls.append((path.name, commit))
        digest = c.sha256_file(path)
        found = [s for s in log if digest in {x["digest"]["sha256"] for x in s["subject"]}]
        c.require(repo == REPO and found, f"no attestation for {path.name}")
        return found
    return statements_for, calls


def test_release_set_verifies(release_dir):
    statements_for, calls = transparency_log(release_dir)
    data = release_set.verify(release_dir, VERSION, REPO, statements_for, prepare=lambda d: None)
    assert data["mode"] == "release"
    assert calls[0] == ("candidate-1.2.0rc3-100-2.json", None)
    assert sorted(name for name, _ in calls[1:]) == sorted(p.name for p in release_dir.iterdir())
    assert all(commit == COMMIT for _, commit in calls[1:])


def tamper_mode(directory):
    path = next(directory.glob("candidate-*.json"))
    data = json.loads(path.read_text())
    data["mode"], data["pid1_dry_run"] = "dry-run", None
    path.write_text(json.dumps(data, sort_keys=True, indent=2) + "\n")


@pytest.mark.parametrize("mutate, message", [
    (lambda d: next(d.glob("candidate-*.json")).unlink(), "expected one candidate result"),
    (tamper_mode, "no attestation"),
    (lambda d: (d / "dry-run-candidate-1.2.0rc3-101-1.json").write_text("{}"), "extra"),
    (lambda d: (d / "milvus_lite_rust-1.2.0rc3.tar.gz").write_bytes(b"other"), "exactly the seven"),
    (lambda d: (d / "SHA256SUMS").unlink(), "missing"),
    (lambda d: (d / "extra.whl").write_bytes(b""), "extra"),
])
def test_release_set_rejects(release_dir, mutate, message):
    statements_for, _ = transparency_log(release_dir)
    mutate(release_dir)
    with pytest.raises(c.CandidateError, match=message):
        release_set.verify(release_dir, VERSION, REPO, statements_for)


@pytest.mark.parametrize("other, message", [
    (lambda s: statement(s, run_id=101), "attested by .*/runs/101/"),
    (lambda s: statement(s, attempt=1), "attested by .*/attempts/1"),
    (lambda s: statement(s, commit="b" * 40), "attested for"),
    (lambda s: statement(s, workflow=".github/workflows/other.yml"), "attested for"),
    (lambda s: statement({k: v for k, v in s.items() if k != "SHA256SUMS"}), "exactly the seven"),
    (lambda s: duplicated_subject(statement(s)), "exactly the seven"),
])
def test_release_set_rejects_attestations_from_other_invocations(release_dir, other, message):
    signed = {p.name: c.sha256_file(p) for p in release_dir.iterdir()}
    statements_for, _ = transparency_log(release_dir, other(signed))
    with pytest.raises(c.CandidateError, match=message):
        release_set.verify(release_dir, VERSION, REPO, statements_for)


def test_release_set_rejects_when_only_another_run_attested(release_dir):
    signed = {p.name: c.sha256_file(p) for p in release_dir.iterdir()}

    def statements_for(path, repo, commit=None):
        return [statement(signed, run_id=101)]
    with pytest.raises(c.CandidateError, match="runs/101/"):
        release_set.verify(release_dir, VERSION, REPO, statements_for)


def test_release_set_rejects_dry_run_result_even_if_attested(release_dir):
    tamper_mode(release_dir)
    path = next(release_dir.glob("candidate-*.json"))
    sums = c.parse_sums((release_dir / "SHA256SUMS").read_text())
    sums[path.name] = c.sha256_file(path)
    (release_dir / "SHA256SUMS").write_text(c.sums_text(sums))
    statements_for, _ = transparency_log(release_dir)
    with pytest.raises(c.CandidateError, match="does not match content"):
        release_set.verify(release_dir, VERSION, REPO, statements_for)


def test_release_set_rejects_inconsistent_signed_set(release_dir):
    # 即使七个文件一起签发，SHA256SUMS 与附件不符仍须拒绝。
    sums = c.parse_sums((release_dir / "SHA256SUMS").read_text())
    sums[c.formal_names(VERSION)[0]] = "0" * 64
    (release_dir / "SHA256SUMS").write_text(c.sums_text(sums))
    statements_for, _ = transparency_log(release_dir)
    with pytest.raises(c.CandidateError, match="SHA256SUMS does not match"):
        release_set.verify(release_dir, VERSION, REPO, statements_for)


def test_release_set_prepare_hook_and_version(release_dir):
    statements_for, _ = transparency_log(release_dir)

    def reject(data):
        raise c.CandidateError("tag exists")
    with pytest.raises(c.CandidateError, match="tag exists"):
        release_set.verify(release_dir, VERSION, REPO, statements_for, prepare=reject)
    with pytest.raises(c.CandidateError, match="expected one candidate result"):
        release_set.verify(release_dir, "1.2.0rc4", REPO, statements_for)


@pytest.mark.parametrize("mutate, message", [
    (lambda gh: gh.tags.add(f"v{VERSION}"), "tag"),
    (lambda gh: gh.releases.append({"tag_name": f"v{VERSION}", "name": "x"}), "release"),
    (lambda gh: gh.ledger["versions"].append({"version": VERSION, "run_id": 99, "run_attempt": 1, "outcome": "candidate"}), "ledger records run"),
    (lambda gh: gh.ledger["versions"].append({"version": VERSION, "run_id": 100, "run_attempt": 2, "outcome": "abandoned"}), "outcome"),
    (lambda gh: gh.ledger.update(versions={}), "list"),
])
def test_prepare_checks(gh, mutate, message):
    data = meta()
    release_set.prepare_checks(gh, data)
    gh.ledger["versions"].append({"version": VERSION, "run_id": 100, "run_attempt": 2, "outcome": "candidate"})
    release_set.prepare_checks(gh, data)
    gh.ledger["versions"].clear()
    mutate(gh)
    with pytest.raises(c.CandidateError, match=message):
        release_set.prepare_checks(gh, data)
