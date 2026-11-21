"""签发路径摘要与已发布基线（0039 第 1–3 条）：只改计数矩阵或兼容探针版本时判定为相同；改工作流头部的行为性键、
最后两个 job 或签发工具时判定为不同；基线只跳过没有 tag 的条目，轻量 tag、提交不符与读取失败都整体失败。

用真实的 release.yml 与 tests/release 工具在临时 git 仓库中构造提交，覆盖本地 git 读取的实际行为。
"""

import json
from pathlib import Path
import subprocess
from types import SimpleNamespace

import pytest

import candidate as c
import runs

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = (ROOT / c.RELEASE_WORKFLOW).read_text()
BASE_RUN = {"id": 5, "attempt": 1}


def git(repo, *args):
    result = subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True)
    assert result.returncode == 0, result.stderr
    return result.stdout.strip()


def ledger_text(*entries):
    return json.dumps({"format": 1, "versions": [
        {"version": v, "run_id": run_id, "run_attempt": 1, "outcome": outcome} for v, run_id, outcome in entries]})


class Repo:
    def __init__(self, path):
        self.path = path
        path.mkdir()
        git(path, "init", "-q", "-b", "main")
        git(path, "config", "user.email", "test@example.invalid")
        git(path, "config", "user.name", "test")
        git(path, "config", "tag.gpgSign", "false")
        git(path, "config", "commit.gpgSign", "false")
        files = {c.RELEASE_WORKFLOW: WORKFLOW, runs.LEDGER: ledger_text()}
        for name in (*c.SIGNING_TOOLS, *c.SIGNING_EXEMPT):
            files[name] = (ROOT / name).read_text()
        self.base = self.commit(files)

    def commit(self, files):
        for name, text in files.items():
            target = self.path / name
            if text is None:
                target.unlink()
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(text)
        git(self.path, "add", "-A")
        git(self.path, "commit", "-q", "--allow-empty", "-m", "x")
        return git(self.path, "rev-parse", "HEAD")

    def read(self, name):
        return (self.path / name).read_text()

    def edit(self, name, old, new):
        text = self.read(name)
        assert text.count(old) >= 1, old
        return self.commit({name: text.replace(old, new, 1)})

    def tag(self, name, commit, annotated=True):
        git(self.path, "tag", *(["-a", "-m", name] if annotated else []), name, commit)


class FakeGh:
    def __init__(self, heads):
        self.heads = heads

    def get(self, path):
        if path not in self.heads:
            raise c.CandidateError(f"gh api {path} failed: gh: Not Found (HTTP 404)")
        return {"head_sha": self.heads[path]}


@pytest.fixture
def repo(tmp_path):
    """基线 1.0.0 已发布并打附注 tag；之后的提交把它登记进台账。"""
    repo = Repo(tmp_path / "repo")
    repo.tag("v1.0.0", repo.base)
    repo.frozen = repo.commit({runs.LEDGER: ledger_text(("1.0.0", 5, "released"))})
    repo.gh = FakeGh({"actions/runs/5/attempts/1": repo.base})
    return repo


def basis(repo, commit):
    return runs.signing_basis(repo.gh, runs.Git(repo.path), commit)


def test_real_workflow_splits_into_header_and_signing_jobs():
    header, jobs = runs.workflow_header(WORKFLOW), runs.signing_jobs(WORKFLOW)
    assert "  MODE: " in header and "  VERSION: " in header and "PREVIOUS_VERSION" not in header
    assert "defaults:" in header and "permissions:" in header and "workflow_dispatch:" in header
    assert jobs.startswith("  preserve-digests:\n") and "\n  attest:\n" in jobs
    assert "  candidate:\n" not in jobs


def test_unchanged_signing_path_since_the_baseline(repo):
    same, recorded, changed = basis(repo, repo.frozen)
    assert same and changed == []
    assert recorded == {"baseline_version": "1.0.0", "baseline_commit": repo.base,
                        "signing_path_sha256": runs.signing_digest(runs.signing_items(runs.Git(repo.path), repo.frozen)),
                        "paths": list(c.SIGNING_PATHS)}
    c.check_basis(recorded)


@pytest.mark.parametrize("name, old, new", [
    ("tests/release/scorecard.py", "PYTHON_SUITE = 765", "PYTHON_SUITE = 766"),
    (c.RELEASE_WORKFLOW, "  PREVIOUS_VERSION: ", "  PREVIOUS_VERSION: 9.9.9\n  OLD_PREVIOUS: "),
    (c.RELEASE_WORKFLOW, "  REFERENCE_COMMIT: ", "  # 注释可以改\n  REFERENCE_COMMIT: "),
    (c.RELEASE_WORKFLOW, "--expect-group 0", "--expect-group 1"),
    (c.RELEASE_WORKFLOW, "  candidate:\n", "  candidate:\n    # 签发之前的 job\n"),
])
def test_changes_outside_the_signing_path_are_ignored(repo, name, old, new):
    commit = repo.edit(name, old, new)
    assert basis(repo, commit)[0] is True


@pytest.mark.parametrize("name, old, new, path", [
    (c.RELEASE_WORKFLOW, "  MODE: ${{", "  MODE: ${{ 'release' ||", c.WORKFLOW_HEADER),
    (c.RELEASE_WORKFLOW, "  VERSION: ${{ inputs.version }}", "  VERSION: ${{ inputs.commit }}", c.WORKFLOW_HEADER),
    (c.RELEASE_WORKFLOW, "    shell: bash", "    shell: sh", c.WORKFLOW_HEADER),
    (c.RELEASE_WORKFLOW, "permissions:\n  contents: read\n", "permissions:\n  contents: write\n", c.WORKFLOW_HEADER),
    (c.RELEASE_WORKFLOW, "      dry_run:\n", "      extra:\n        type: string\n      dry_run:\n", c.WORKFLOW_HEADER),
    (c.RELEASE_WORKFLOW, "      - name: sign\n", "      - name: sign\n        if: always()\n", c.WORKFLOW_SIGNING_JOBS),
    (c.RELEASE_WORKFLOW, "    needs: candidate\n    runs-on", "    needs: [candidate]\n    runs-on", c.WORKFLOW_SIGNING_JOBS),
    *[(name, "\n", "\n# x\n", name) for name in c.SIGNING_TOOLS],
])
def test_changes_in_the_signing_path_are_detected(repo, name, old, new, path):
    commit = repo.edit(name, old, new)
    same, _, changed = basis(repo, commit)
    assert same is False and changed == [path]


def test_missing_tool_counts_as_a_change(repo):
    commit = repo.commit({"tests/release/sums.py": None})
    assert basis(repo, commit)[2] == ["tests/release/sums.py"]
    # 两侧都缺失也算变化：判定不能因为「都没有」而放行。
    repo.tag("v1.1.0", commit)
    repo.gh.heads["actions/runs/6/attempts/1"] = commit
    final = repo.commit({runs.LEDGER: ledger_text(("1.0.0", 5, "released"), ("1.1.0", 6, "released"))})
    same, recorded, changed = basis(repo, final)
    assert recorded["baseline_version"] == "1.1.0" and same is False and changed == ["tests/release/sums.py"]


@pytest.mark.parametrize("old, new, message", [
    ("  PREVIOUS_VERSION: 0", "  PREVIOUS_VERSION: |\n    0", "single-line"),
    ("  PREVIOUS_VERSION: 0", "  PREVIOUS_VERSION:\n    0", "single-line"),
    ("\n  attest:\n", "\n  later:\n    runs-on: x\n  attest:\n", "last jobs"),
])
def test_workflow_structure_is_enforced(repo, old, new, message):
    text = WORKFLOW.replace(old, new, 1)
    assert text != WORKFLOW
    with pytest.raises(c.CandidateError, match=message):
        runs.workflow_header(text), runs.signing_jobs(text)


def test_a_job_after_attest_is_rejected():
    with pytest.raises(c.CandidateError, match="last jobs"):
        runs.signing_jobs(WORKFLOW + "  after:\n    runs-on: x\n")
    with pytest.raises(c.CandidateError, match="top level"):
        runs.signing_jobs(WORKFLOW + "on2: x\n")


def test_version_order():
    order = ["0.9.0", "0.10.0a1", "0.10.0b2", "0.10.0rc1", "0.10.0rc10", "0.10.0", "1.0.0a3", "1.0.0b1", "1.0.0rc2", "1.0.0"]
    assert sorted(reversed(order), key=runs.version_key) == order


def test_baseline_skips_only_versions_without_a_tag(repo):
    # 更高的 1.1.0 已登记为 released 但没有 tag（如测试版本）：跳过，仍取 1.0.0。
    commit = repo.commit({runs.LEDGER: ledger_text(("1.0.0", 5, "released"), ("1.1.0", 6, "released"),
                                                   ("1.2.0", 7, "abandoned"))})
    assert runs.baseline(repo.gh, runs.Git(repo.path), commit) == ("1.0.0", repo.base)


def test_baseline_takes_the_highest_tagged_version(repo):
    second = repo.commit({"README": "x"})
    repo.tag("v1.0.1", second)
    repo.gh.heads["actions/runs/6/attempts/1"] = second
    commit = repo.commit({runs.LEDGER: ledger_text(("1.0.1", 6, "released"), ("1.0.0", 5, "released"))})
    assert runs.baseline(repo.gh, runs.Git(repo.path), commit) == ("1.0.1", second)


@pytest.mark.parametrize("setup, message", [
    (lambda r: r.commit({runs.LEDGER: ledger_text()}), "no released version"),
    (lambda r: r.commit({runs.LEDGER: ledger_text(("1.0.0", 5, "abandoned"))}), "no released version"),
    (lambda r: r.commit({runs.LEDGER: None}), "missing"),
    (lambda r: r.commit({runs.LEDGER: "{"}), "not valid JSON"),
    (lambda r: (r.tag("v1.1.0", r.base, annotated=False),
                r.commit({runs.LEDGER: ledger_text(("1.0.0", 5, "released"), ("1.1.0", 6, "released"))}))[1],
     "lightweight"),
    (lambda r: (r.gh.heads.update({"actions/runs/5/attempts/1": "b" * 40}), r.frozen)[1], "built"),
    (lambda r: (r.gh.heads.clear(), r.frozen)[1], "cannot read run"),
])
def test_baseline_failures_do_not_fall_back(repo, setup, message):
    commit = setup(repo)
    with pytest.raises(c.CandidateError, match=message):
        runs.baseline(repo.gh, runs.Git(repo.path), commit)


def test_a_mismatched_newer_tag_does_not_fall_back_to_an_older_one(repo):
    other = repo.commit({"README": "x"})
    repo.tag("v1.1.0", other)
    repo.gh.heads["actions/runs/6/attempts/1"] = repo.base
    commit = repo.commit({runs.LEDGER: ledger_text(("1.0.0", 5, "released"), ("1.1.0", 6, "released"))})
    with pytest.raises(c.CandidateError, match="v1.1.0 points to"):
        runs.baseline(repo.gh, runs.Git(repo.path), commit)


def test_git_reads(repo):
    g = runs.Git(repo.path)
    assert g.show(repo.base, "nope.txt") is None
    assert g.show(repo.base, c.RELEASE_WORKFLOW).decode() == WORKFLOW
    with pytest.raises(c.CandidateError, match="not in the repository"):
        g.show("f" * 40, c.RELEASE_WORKFLOW)
    assert g.tag_kind("v1.0.0") == "tag" and g.tag_kind("v9.9.9") is None
    assert g.tag_commit("v1.0.0") == repo.base


class PrecheckGh(FakeGh):
    """前置检查用：签发环境、版本消费与继承运行都通过；记录继承运行被查询的次数，供核对判定先于它。"""

    def __init__(self, heads, sha):
        super().__init__(heads)
        self.sha, self.inherited_queries = sha, 0

    def get(self, path):
        if path.startswith("environments/"):
            from test_runs_and_release_set import signing_environment
            return signing_environment()
        return super().get(path)

    def absent(self, path):
        return True

    def items(self, path, key):
        return []

    def raw(self, path):
        return ledger_text()

    def workflow_runs(self, path, query=""):
        if path == runs.RELEASE_WORKFLOW:
            return []
        self.inherited_queries += 1
        return [{"id": 1, "run_attempt": 1, "created_at": "10:00", "status": "completed", "conclusion": "success",
                 "head_sha": self.sha, "path": path}]

    def jobs(self, run_id, attempt):
        return [{"name": name, "status": "completed", "conclusion": "success"} for name in ("ci-pass", "compare-pass")]


def precheck_args(sha):
    return SimpleNamespace(sha=sha, version="2.0.0", mode="release", run_id=100, run_attempt=1, pid1_dry_run="",
                           signing_environment="release-signing", owner_id=11266581)


def test_precheck_without_dry_run_records_the_basis(repo):
    meta = runs.precheck(PrecheckGh(repo.gh.heads, repo.frozen), runs.Git(repo.path), precheck_args(repo.frozen))
    assert meta["pid1_dry_run"] is None
    assert meta["dry_run_basis"] == basis(repo, repo.frozen)[1]
    assert meta["inherited"] == {"ci": {"id": 1, "attempt": 1}, "compare": {"id": 1, "attempt": 1}}


def test_precheck_without_dry_run_fails_on_a_changed_signing_path_before_inherited_runs(repo):
    commit = repo.edit("tests/release/runs.py", "\n", "\n# x\n")
    gh = PrecheckGh(repo.gh.heads, commit)
    with pytest.raises(c.CandidateError, match=r"signing path changed since v1\.0\.0.*tests/release/runs\.py"):
        runs.precheck(gh, runs.Git(repo.path), precheck_args(commit))
    assert gh.inherited_queries == 0


def test_precheck_without_dry_run_and_without_baseline_fails(repo):
    commit = repo.commit({runs.LEDGER: ledger_text()})
    gh = PrecheckGh(repo.gh.heads, commit)
    with pytest.raises(c.CandidateError, match="needs an unchanged signing path: no baseline"):
        runs.precheck(gh, runs.Git(repo.path), precheck_args(commit))
    assert gh.inherited_queries == 0
