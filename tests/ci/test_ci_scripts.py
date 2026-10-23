"""纯文档分类与汇总判定的反证（0020 公共约定 9、「判定与流程」第 3 条）：分类只在待合入的全部差异都是
文档时放行，任何异常按全量执行；汇总只接受全量全部成功或纯文档时重 job 全部跳过两种形态。"""

from pathlib import Path
import subprocess
import sys

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / ".github" / "ci"))

import changes  # noqa: E402
import summary  # noqa: E402

ZERO = "0" * 40


def sh(repo, *args):
    return subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True, text=True).stdout.strip()


def commit_files(repo, files, message):
    for name, content in files.items():
        path = repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
        sh(repo, "add", name)
    sh(repo, "commit", "-q", "-m", message)
    return sh(repo, "rev-parse", "HEAD")


@pytest.fixture
def repo(tmp_path):
    """main 上有代码与文档；origin/main 指向 main，模拟 checkout 取得的远端分支。"""
    repo = tmp_path / "repo"
    repo.mkdir()
    sh(repo, "init", "-q", "-b", "main")
    sh(repo, "config", "user.email", "ci@example.invalid")
    sh(repo, "config", "user.name", "ci")
    base = commit_files(repo, {"src.rs": "a\n", "docs/a.md": "a\n", "AGENTS.md": "a\n", "README.md": "a\n"}, "base")
    sh(repo, "update-ref", "refs/remotes/origin/main", base)
    sh(repo, "checkout", "-q", "-b", "work")
    return repo, base


def branch_push(repo, sha):
    return changes.classify(str(repo), "push", "refs/heads/work", sha, ZERO, "")


@pytest.mark.parametrize("files", [
    {"docs/a.md": "b\n"},
    {"docs/decisions/new.md": "b\n"},
    {"AGENTS.md": "b\n"},
    {"docs/a.md": "b\n", "AGENTS.md": "b\n"},
])
def test_documentation_only_branch_push(repo, files):
    repo, _ = repo
    only, reason = branch_push(repo, commit_files(repo, files, "docs"))
    assert only, reason


@pytest.mark.parametrize("files", [
    {"src.rs": "b\n"},
    {"README.md": "b\n"},
    {"docs/releases/consumed-versions.json": "{}\n"},
    {"docs/a.txt": "b\n"},
    {"sub/AGENTS.md": "b\n"},
    {"docs/a.md": "b\n", "src.rs": "b\n"},
])
def test_any_other_path_runs_in_full(repo, files):
    repo, _ = repo
    only, reason = branch_push(repo, commit_files(repo, files, "change"))
    assert not only and "not documentation" in reason


def test_earlier_unverified_commit_on_the_branch_is_not_bypassed(repo):
    # 先推代码、再推纯文档：以 merge-base 到 HEAD 的全部差异判定，最终提交仍须全量。
    repo, _ = repo
    code = commit_files(repo, {"src.rs": "b\n"}, "code")
    docs = commit_files(repo, {"docs/a.md": "b\n"}, "docs")
    only, reason = changes.classify(str(repo), "push", "refs/heads/work", docs, code, "")
    assert not only and "src.rs" in reason


def test_pull_request_uses_the_whole_pr_diff(repo):
    repo, base = repo
    commit_files(repo, {"src.rs": "b\n"}, "code")
    head = commit_files(repo, {"docs/a.md": "b\n"}, "docs")
    only, _ = changes.classify(str(repo), "pull_request", "refs/pull/1/merge", head, "", base)
    assert not only
    sh(repo, "checkout", "-q", "-b", "docs-pr", base)
    docs = commit_files(repo, {"docs/a.md": "c\n"}, "docs")
    only, reason = changes.classify(str(repo), "pull_request", "refs/pull/2/merge", docs, "", base)
    assert only, reason


def test_main_push_uses_before_after(repo):
    repo, base = repo
    docs = commit_files(repo, {"docs/a.md": "b\n"}, "docs")
    assert changes.classify(str(repo), "push", "refs/heads/main", docs, base, "")[0]
    code = commit_files(repo, {"src.rs": "b\n"}, "code")
    docs2 = commit_files(repo, {"docs/a.md": "c\n"}, "docs")
    # 一次快进带入代码与文档两个提交时，before..after 包含代码。
    assert not changes.classify(str(repo), "push", "refs/heads/main", docs2, docs, "")[0]
    assert changes.classify(str(repo), "push", "refs/heads/main", docs2, code, "")[0]


@pytest.mark.parametrize("event, ref, before, pr_base, message", [
    ("workflow_dispatch", "refs/heads/work", "", "", "always runs in full"),
    ("schedule", "refs/heads/main", "", "", "always runs in full"),
    ("push", "refs/tags/v1", "", "", "always runs in full"),
    ("push", "refs/heads/main", ZERO, "", "push before"),
    ("push", "refs/heads/main", "", "", "push before"),
    ("pull_request", "refs/pull/1/merge", "", "", "pull request base"),
    ("pull_request", "refs/pull/1/merge", "", ZERO, "pull request base"),
    ("push", "refs/heads/main", "f" * 40, "", "git diff"),
])
def test_unknown_or_missing_base_runs_in_full(repo, event, ref, before, pr_base, message):
    repo, _ = repo
    docs = commit_files(repo, {"docs/a.md": "b\n"}, "docs")
    only, reason = changes.classify(str(repo), event, ref, docs, before, pr_base)
    assert not only and message in reason


def test_missing_origin_main_and_empty_diff_run_in_full(repo):
    repo, base = repo
    only, reason = branch_push(repo, base)
    assert not only and "empty diff" in reason
    docs = commit_files(repo, {"docs/a.md": "b\n"}, "docs")
    sh(repo, "update-ref", "-d", "refs/remotes/origin/main")
    only, reason = branch_push(repo, docs)
    assert not only and "merge-base" in reason


def test_mode_changes_run_in_full(repo):
    repo, _ = repo
    (repo / "docs/link.md").symlink_to("a.md")
    sh(repo, "add", "docs/link.md")
    sh(repo, "commit", "-q", "-m", "link")
    only, reason = branch_push(repo, sh(repo, "rev-parse", "HEAD"))
    assert not only and "120000" in reason
    sh(repo, "rm", "-q", "docs/link.md")
    sh(repo, "update-index", "--add", "--cacheinfo", f"100755,{sh(repo, 'hash-object', '-w', 'docs/a.md')},docs/a.md")
    sh(repo, "commit", "-q", "-m", "exec")
    only, reason = branch_push(repo, sh(repo, "rev-parse", "HEAD"))
    assert not only and "100755" in reason


def test_deleted_and_renamed_documents(repo):
    repo, _ = repo
    sh(repo, "mv", "docs/a.md", "docs/b.md")
    sh(repo, "commit", "-q", "-m", "rename")
    assert branch_push(repo, sh(repo, "rev-parse", "HEAD"))[0]
    sh(repo, "mv", "docs/b.md", "notes.md")
    sh(repo, "commit", "-q", "-m", "move out")
    only, reason = branch_push(repo, sh(repo, "rev-parse", "HEAD"))
    assert not only and "notes.md" in reason


def test_main_writes_github_output(repo, tmp_path, monkeypatch):
    repo, _ = repo
    docs = commit_files(repo, {"docs/a.md": "b\n"}, "docs")
    out = tmp_path / "output"
    monkeypatch.chdir(repo)
    for key, value in {"EVENT": "push", "REF": "refs/heads/work", "SHA": docs, "BEFORE": ZERO, "PR_BASE": "",
                       "GITHUB_OUTPUT": str(out)}.items():
        monkeypatch.setenv(key, value)
    changes.main()
    monkeypatch.setenv("EVENT", "workflow_dispatch")
    changes.main()
    assert out.read_text() == "docs_only=true\ndocs_only=false\n"


def needs(mode, *results, classifier="success"):
    data = {"changes": {"result": classifier, "outputs": {} if mode is None else {"docs_only": mode}}}
    data.update({f"job{i}": {"result": r, "outputs": {}} for i, r in enumerate(results)})
    return data


@pytest.mark.parametrize("data", [
    needs("false", "success", "success"),
    needs("true", "skipped", "skipped"),
])
def test_summary_accepts_the_two_forms(data):
    assert summary.verdict(data)[0]


@pytest.mark.parametrize("data", [
    needs("false", "success", "skipped"),
    needs("false", "success", "failure"),
    needs("false", "cancelled", "success"),
    needs("true", "skipped", "success"),
    needs("true", "skipped", "failure"),
    needs("true", "skipped", "cancelled"),
    needs("true"),
    needs(None, "skipped", "skipped"),
    needs("", "success"),
    needs("yes", "skipped"),
    needs("false", "success", classifier="failure"),
    needs("true", "skipped", classifier="skipped"),
    {"job0": {"result": "success"}},
])
def test_summary_rejects_everything_else(data):
    assert not summary.verdict(data)[0]


def test_summary_main_exit_code(monkeypatch):
    import json
    monkeypatch.setenv("NEEDS", json.dumps(needs("true", "skipped")))
    assert summary.main() == 0
    monkeypatch.setenv("NEEDS", json.dumps(needs("true", "success")))
    assert summary.main() == 1
