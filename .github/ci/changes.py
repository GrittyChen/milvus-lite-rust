"""判定本次运行能否按纯文档跳过测试 job（0020 公共约定 9），结果写入 GITHUB_OUTPUT 的 docs_only。

分类以待合入的全部差异为准，不只看本次推送：工作分支上未经验证的代码提交之后再推一个纯文档提交，
只比较 before..after 会让最终提交绿色放行。PR 取 base 与 PR 提交的 merge-base；main 以外分支的推送取
origin/main 与 HEAD 的 merge-base；main 的推送取 before..after。手动触发与定时运行、base 缺失或为全零、
git 失败、差异为空或出现任何不认识的情形，一律按全量执行。
"""

import os
import re
import subprocess
import sys

HEX40 = re.compile(r"[0-9a-f]{40}")
ZERO = "0" * 40
# 只认 docs/ 下的 Markdown 与根 AGENTS.md：README.md 进入 sdist 与长描述，docs/releases/consumed-versions.json 是
# 发行台账（tests/release 校验它），都不算纯文档。
DOC_PATH = re.compile(r"docs/(?:[^/]+/)*[^/]+\.md")
ROOT_DOCS = {"AGENTS.md"}
REGULAR, ABSENT = "100644", "000000"


class Full(Exception):
    """按全量执行的原因。"""


def git(repo, *args):
    result = subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True)
    if result.returncode != 0:
        raise Full(f"git {' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout


def commit(value, what):
    if not HEX40.fullmatch(value or "") or value == ZERO:
        raise Full(f"{what} is missing or not a commit: {value!r}")
    return value


def base_of(repo, event, ref, sha, before, pr_base):
    sha = commit(sha, "head")
    if event == "pull_request":
        return git(repo, "merge-base", commit(pr_base, "pull request base"), sha).strip()
    if event == "push" and ref == "refs/heads/main":
        return commit(before, "push before")
    if event == "push" and ref.startswith("refs/heads/"):
        return git(repo, "merge-base", "refs/remotes/origin/main", sha).strip()
    raise Full(f"event {event} on {ref} always runs in full")


def changed_entries(repo, base, sha):
    """`git diff --raw` 的 (旧模式, 新模式, 路径)。不合并改名，删除与新增两侧路径都参与判定。"""
    out = git(repo, "diff", "--raw", "--no-renames", "--no-ext-diff", "-z", base, sha)
    fields = out.split("\0")
    if fields and fields[-1] == "":
        fields.pop()
    if len(fields) % 2:
        raise Full("unexpected git diff --raw output")
    entries = []
    for meta, path in zip(fields[::2], fields[1::2]):
        parts = meta.lstrip(":").split()
        if len(parts) != 5:
            raise Full(f"unexpected git diff --raw record: {meta!r}")
        entries.append((parts[0], parts[1], path))
    return entries


def docs_only(entries):
    if not entries:
        raise Full("empty diff")
    for old, new, path in entries:
        if not (DOC_PATH.fullmatch(path) or path in ROOT_DOCS):
            raise Full(f"{path} is not documentation")
        # 只接受普通文件的增删改：软链或可执行位的变化可能改变其他文件的含义。
        if {old, new} - {REGULAR, ABSENT} or new == old == ABSENT:
            raise Full(f"{path} changes mode {old} -> {new}")
    return True


def classify(repo, event, ref, sha, before, pr_base):
    try:
        base = base_of(repo, event, ref, sha, before, pr_base)
        docs_only(changed_entries(repo, base, sha))
    except Full as reason:
        return False, str(reason)
    return True, f"only documentation changed in {base}..{sha}"


def main():
    env = os.environ
    only, reason = classify(".", env.get("EVENT", ""), env.get("REF", ""), env.get("SHA", ""),
                            env.get("BEFORE", ""), env.get("PR_BASE", ""))
    print(("docs-only: " if only else "full: ") + reason)
    with open(env["GITHUB_OUTPUT"], "a", encoding="utf-8") as out:
        out.write(f"docs_only={'true' if only else 'false'}\n")


if __name__ == "__main__":
    sys.exit(main())
