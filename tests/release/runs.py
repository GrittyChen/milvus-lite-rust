"""发行工作流的前置检查与签发前复核（0021 第 2、3、10 条），签发环境的保护规则检查（0022），以及签发路径
相对已发布基线是否变化的判定（0039）。

判定逻辑是纯函数，输入为 GitHub REST API 返回的运行与 job 对象；`Gh` 只负责经 gh api 取数，`Git` 只负责读本地仓库。
"""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
import time

import ledger
from candidate import (HEX40, RELEASE_WORKFLOW, SIGNING_PATHS, VERSION, WORKFLOW_HEADER, WORKFLOW_SIGNING_JOBS,
                       CandidateError, require)

INHERITED = {"ci": (".github/workflows/ci.yml", "ci-pass"), "compare": (".github/workflows/compare.yml", "compare-pass")}
ATTEST_JOB = "attest"
SIGN_STEP = "sign"
PID1_JOBS = ("pid1 (ubuntu-24.04)", "pid1 (ubuntu-24.04-arm)")
LEDGER = "docs/releases/consumed-versions.json"
# 签发路径中工作流头部的 env 只保留这两个键：attest 与 preserve-digests 只读它们（0039 第 1 条）。
SIGNING_ENV_KEYS = ("MODE", "VERSION")
SIGNING_JOBS = ("preserve-digests", "attest")


def run_ref(run):
    return {"id": run["id"], "attempt": run["run_attempt"]}


def run_snapshot(runs, sha, path):
    """同一提交该工作流全部运行的 (ID, 最新 attempt)。重跑会增加 attempt 而不改创建时间，排队中的重跑也已计入。"""
    return sorted([r["id"], r["run_attempt"]] for r in runs if r["head_sha"] == sha and r["path"] == path)


def select_latest(runs, sha, path):
    """同一提交创建时间最新的运行，不区分事件；它本身必须已完成且成功，旧的成功不能替代。"""
    runs = [r for r in runs if r["head_sha"] == sha and r["path"] == path]
    require(runs, f"no {path} run for {sha}")
    latest = max(runs, key=lambda r: (r["created_at"], r["id"]))
    require(latest["status"] == "completed" and latest["conclusion"] == "success",
            f"latest {path} run {latest['id']} attempt {latest['run_attempt']} is {latest['status']}/{latest['conclusion']}")
    return latest


def check_job(jobs, name):
    matched = [j for j in jobs if j["name"] == name]
    require(len(matched) == 1 and matched[0]["status"] == "completed" and matched[0]["conclusion"] == "success",
            f"job {name} is not a single success: {[(j['status'], j['conclusion']) for j in matched]}")


def check_full_run(jobs, path):
    """继承的运行须为全量且每个 job 都成功：纯文档提交的运行会合法地跳过测试 job，汇总 job 仍为 success，
    只看汇总会继承一次没有执行任何测试的运行（0020 公共约定 9）。"""
    bad = [(j["name"], j["status"], j["conclusion"]) for j in jobs if (j["status"], j["conclusion"]) != ("completed", "success")]
    require(jobs and not bad, f"{path} run is not a full run with every job successful (dispatch full CI and Compare runs): {bad}")


def signing_started(jobs):
    """签发步骤一旦开始即消费版本。跳过或排队的步骤不算；job 已分配执行机而步骤信息缺失时按已开始处理。"""
    for job in jobs:
        if job["name"] != ATTEST_JOB or job["conclusion"] == "skipped":
            continue
        steps = [s for s in job.get("steps") or [] if s["name"] == SIGN_STEP]
        if not steps:
            if job.get("runner_name"):
                return True
            continue
        if any(s["status"] != "queued" and s["conclusion"] != "skipped" for s in steps):
            return True
    return False


def release_attempts(runs, version, current):
    """同版本正式运行需要检查的 (运行 ID, attempt)：其他运行的全部 attempt，以及本运行此前的 attempt。

    本运行的较早 attempt 也可能已开始签发；只排除正在执行的这一个 attempt。
    """
    pairs = []
    for run in runs:
        if run["display_title"] != f"release v{version}" or run["path"] != RELEASE_WORKFLOW:
            continue
        last = current["attempt"] - 1 if run["id"] == current["id"] else run["run_attempt"]
        pairs += [(run["id"], attempt) for attempt in range(1, last + 1)]
    return pairs


def check_dry_run(run, jobs, sha, version):
    require(run["path"] == RELEASE_WORKFLOW and run["display_title"] == f"dry-run v{version}",
            f"run {run['id']} is not a dry run of {version}")
    require(run["head_sha"] == sha, f"dry run {run['id']} is for {run['head_sha']}, not {sha}")
    require(run["status"] == "completed" and run["conclusion"] == "success",
            f"dry run {run['id']} attempt {run['run_attempt']} is {run['status']}/{run['conclusion']}")
    for name in PID1_JOBS:
        check_job(jobs, name)


def parse_ref(text):
    try:
        run_id, attempt = (int(x) for x in text.split("."))
    except ValueError as error:
        raise CandidateError(f"run reference must be <id>.<attempt>, got {text!r}") from error
    require(run_id > 0 and attempt > 0, f"run reference must be positive, got {text!r}")
    return {"id": run_id, "attempt": attempt}


# 只读接口的有限重试（0021 第 11 条传输层）：只对可判定为暂时性的传输错误，以及 HTTP 成功但响应体不是合法 JSON
# 的情形重试。任何 HTTP 4xx 都是确定的回答，不重试；429 也不重试，因为 gh api 的错误输出不含 Retry-After。
RETRY_DELAYS = (5, 15, 45)
# 文本取自 gh 的实际输出：DNS 等连接失败为「error connecting to <host>」，服务端断开为「…: EOF」。
TRANSIENT = re.compile(r"HTTP 50[0234]|stream error|CANCEL|timeout|timed out|connection reset|connection refused"
                       r"|unexpected EOF|: EOF$|error connecting to|TLS handshake", re.IGNORECASE | re.MULTILINE)
DEFINITE = re.compile(r"HTTP 4\d\d")


def transient(stderr):
    return not DEFINITE.search(stderr) and bool(TRANSIENT.search(stderr))


def retry_notice(operation, attempt, reason, delay):
    # 写到 stderr：precheck 把结果打印到 stdout，runner 两路都解析工作流命令。
    print(f"::warning title=transport-retry::{operation}: attempt {attempt} failed ({reason}); retrying in {delay}s",
          file=sys.stderr, flush=True)


class Gh:
    """经 gh api 取数。默认不重试；只读且失败可安全重试的调用方显式传 retry=True。0022 的保全路径（门闩与 save；
    verify 不访问接口）保持不重试：门闩是放行签发的唯一判据，失败发生在签发前、不消费版本。"""

    def __init__(self, repo, retry=False, sleep=time.sleep):
        self.repo = repo
        self.retry = retry
        self.sleep = sleep

    def _api(self, path, *extra):
        return subprocess.run(["gh", "api", "-H", "X-GitHub-Api-Version: 2022-11-28", *extra, f"repos/{self.repo}/{path}"],
                              capture_output=True, text=True)

    def _call(self, path, extra=(), parse=None):
        """返回 (result, value)。失败的 result 交给调用方判定；不重试的实例与逐次调用 _api 相同。"""
        attempts = len(RETRY_DELAYS) + 1 if self.retry else 1
        for attempt in range(attempts):
            result = self._api(path, *extra)
            if result.returncode != 0:
                if attempt + 1 == attempts or not transient(result.stderr):
                    return result, None
                reason = result.stderr.strip().splitlines()[-1]
            else:
                try:
                    return result, parse(result.stdout) if parse else result.stdout
                except json.JSONDecodeError as error:
                    if attempt + 1 == attempts:
                        raise
                    reason = f"invalid JSON in a successful response: {error}"
            retry_notice(f"gh api {path}", attempt + 1, reason, RETRY_DELAYS[attempt])
            self.sleep(RETRY_DELAYS[attempt])

    def get(self, path):
        result, value = self._call(path, parse=json.loads)
        require(result.returncode == 0, f"gh api {path} failed: {result.stderr.strip()}")
        return value

    def items(self, path, key):
        result, value = self._call(path, ("--paginate", "--jq", f".{key}[]"),
                                   parse=lambda out: [json.loads(line) for line in out.splitlines() if line])
        require(result.returncode == 0, f"gh api {path} failed: {result.stderr.strip()}")
        return value

    def absent(self, path):
        result, _ = self._call(path)
        if result.returncode == 0:
            return False
        require("HTTP 404" in result.stderr, f"gh api {path} failed: {result.stderr.strip()}")
        return True

    def raw(self, path):
        result, value = self._call(path, ("-H", "Accept: application/vnd.github.raw+json"))
        require(result.returncode == 0, f"gh api {path} failed: {result.stderr.strip()}")
        return value

    def workflow_runs(self, path, query=""):
        return self.items(f"actions/workflows/{Path(path).name}/runs?per_page=100{query}", "workflow_runs")

    def jobs(self, run_id, attempt):
        return self.items(f"actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100", "jobs")


class Git:
    """读本地仓库中某个提交的文件与 tag。工作流的 checkout 带完整历史与 tag（fetch-depth: 0）。"""

    def __init__(self, directory="."):
        self.directory = directory

    def _run(self, *args):
        return subprocess.run(["git", "-C", str(self.directory), *args], capture_output=True)

    def show(self, commit, path):
        """文件内容（bytes）；提交中没有该文件时为 None，提交本身不存在时失败。"""
        require(self._run("cat-file", "-e", f"{commit}^{{commit}}").returncode == 0, f"commit {commit} is not in the repository")
        if self._run("cat-file", "-e", f"{commit}:{path}").returncode != 0:
            return None
        result = self._run("show", f"{commit}:{path}")
        require(result.returncode == 0, f"git show {commit}:{path} failed: {result.stderr.decode(errors='replace').strip()}")
        return result.stdout

    def tag_kind(self, name):
        """refs/tags/<name> 指向的对象类型：附注 tag 为 tag，轻量 tag 为 commit；不存在时为 None。"""
        if self._run("show-ref", "--verify", "--quiet", f"refs/tags/{name}").returncode != 0:
            return None
        result = self._run("cat-file", "-t", f"refs/tags/{name}")
        require(result.returncode == 0, f"cannot read tag {name}")
        return result.stdout.decode().strip()

    def tag_commit(self, name):
        result = self._run("rev-parse", "--verify", f"refs/tags/{name}^{{commit}}")
        require(result.returncode == 0, f"tag {name} does not resolve to a commit")
        return result.stdout.decode().strip()


def workflow_header(text):
    """jobs: 之前的全部行，env 块只留 MODE 与 VERSION；env 中每个键都须是单行，注释与空行丢弃。"""
    lines = text.splitlines(keepends=True)
    require(lines.count("jobs:\n") == 1, "release workflow must have exactly one top-level jobs: line")
    out, in_env = [], False
    for line in lines[:lines.index("jobs:\n")]:
        if line[:1] not in (" ", "\n", "#", ""):
            in_env = line == "env:\n"
            out.append(line)
        elif in_env:
            if not line.strip() or line.strip().startswith("#"):
                continue
            match = re.fullmatch(r"  ([A-Z][A-Z0-9_]*): ([^|>\s].*)\n", line)
            require(match is not None, f"release workflow env entries must be single-line KEY: value, got {line!r}")
            if match.group(1) in SIGNING_ENV_KEYS:
                out.append(line)
        else:
            out.append(line)
    return "".join(out)


def signing_jobs(text):
    """jobs 中最后两个 job（preserve-digests 与 attest）的全部文本；它们须正是最后两个 job。"""
    lines = text.splitlines(keepends=True)
    require(lines.count("jobs:\n") == 1, "release workflow must have exactly one top-level jobs: line")
    start = lines.index("jobs:\n") + 1
    keys = [(i, m.group(1)) for i, line in enumerate(lines[start:], start) if (m := re.fullmatch(r"  ([A-Za-z0-9_-]+):\n", line))]
    require([name for _, name in keys[-len(SIGNING_JOBS):]] == list(SIGNING_JOBS),
            f"the last jobs of the release workflow must be {list(SIGNING_JOBS)}, got {[name for _, name in keys[-2:]]}")
    tail = lines[keys[-len(SIGNING_JOBS)][0]:]
    require(not any(re.match(r"\S", line) for line in tail), "nothing may follow the signing jobs at the top level")
    return "".join(tail)


def signing_items(git, commit):
    """签发路径各项的内容（bytes）；文件缺失为 None。"""
    workflow = git.show(commit, RELEASE_WORKFLOW)
    items = {}
    for name in SIGNING_PATHS:
        if name == WORKFLOW_HEADER:
            items[name] = None if workflow is None else workflow_header(workflow.decode()).encode()
        elif name == WORKFLOW_SIGNING_JOBS:
            items[name] = None if workflow is None else signing_jobs(workflow.decode()).encode()
        else:
            items[name] = git.show(commit, name)
    return items


def signing_digest(items):
    """逐项「路径 + 内容」的摘要，缺失的项记为 missing。"""
    text = "".join(f"{hashlib.sha256(v).hexdigest() if v is not None else 'missing'}  {k}\n" for k, v in sorted(items.items()))
    return hashlib.sha256(text.encode()).hexdigest()


def version_key(version):
    """X.Y.Z[(a|b|rc)N] 的顺序：a < b < rc < 无后缀。"""
    match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)(?:(a|b|rc)(\d+))?", version)
    require(match is not None, f"bad version {version!r}")
    major, minor, patch, stage, number = match.groups()
    return int(major), int(minor), int(patch), ("a", "b", "rc", None).index(stage), int(number or 0)


def baseline(gh, git, sha):
    """冻结提交中的台账里，tag 存在的、版本号最高的 released 条目，返回 (版本, 提交)。

    只跳过 tag 不存在的条目（例如没有公开 tag 的测试版本）；tag 为轻量 tag、指向的提交与台账运行不符或运行记录
    读取失败，都整体失败而不回退到更低的版本，以免掩盖 tag 被移动或台账有误（0039 第 2 条）。
    """
    text = git.show(sha, LEDGER)
    require(text is not None, f"no baseline: {LEDGER} is missing at {sha}")
    entries = sorted((e for e in ledger.loads(text.decode()).values() if e["outcome"] == "released"),
                     key=lambda e: version_key(e["version"]), reverse=True)
    for entry in entries:
        tag = f"v{entry['version']}"
        kind = git.tag_kind(tag)
        if kind is None:
            continue
        require(kind == "tag", f"no baseline: {tag} is a lightweight tag")
        commit = git.tag_commit(tag)
        try:
            run = gh.get(f"actions/runs/{entry['run_id']}/attempts/{entry['run_attempt']}")
        except CandidateError as error:
            raise CandidateError(f"no baseline: cannot read run {entry['run_id']}.{entry['run_attempt']} of {tag}: {error}") from error
        require(run.get("head_sha") == commit,
                f"no baseline: {tag} points to {commit}, but ledger run {entry['run_id']} built {run.get('head_sha')}")
        return entry["version"], commit
    raise CandidateError("no baseline: no released version in the ledger has a tag")


def signing_basis(gh, git, sha):
    """返回 (判定, dry_run_basis 或 None, 变化的路径)。判定为 True 表示签发路径与基线相同，可以不引用演练。"""
    version, commit = baseline(gh, git, sha)
    ours, theirs = signing_items(git, sha), signing_items(git, commit)
    # 任一侧缺失都算变化（0039 第 1 条「文件缺失视为不同」），两侧都缺失也不例外。
    changed = sorted(k for k in SIGNING_PATHS if ours[k] is None or theirs[k] is None or ours[k] != theirs[k])
    basis = {"baseline_version": version, "baseline_commit": commit, "signing_path_sha256": signing_digest(ours),
             "paths": list(SIGNING_PATHS)}
    return not changed, basis, changed


def inherited_runs(gh, sha):
    """继承的 CI / Compare 运行及同一提交全部运行的快照。快照供签发前复核：较旧的运行被重跑时，按创建时间选出的
    最新运行不变，只有快照能看出 attempt 增加。"""
    refs, snapshot = {}, {}
    for name, (path, summary) in INHERITED.items():
        rows = gh.workflow_runs(path, f"&head_sha={sha}")
        latest = select_latest(rows, sha, path)
        jobs = gh.jobs(latest["id"], latest["run_attempt"])
        check_job(jobs, summary)
        check_full_run(jobs, path)
        refs[name] = run_ref(latest)
        snapshot[name] = run_snapshot(rows, sha, path)
    return refs, snapshot


def consumed_versions(gh, version, current):
    """返回已消费判据中命中的那一项；None 表示未消费。"""
    if not gh.absent(f"git/ref/tags/v{version}"):
        return f"tag v{version} exists"
    if any(r["tag_name"] == f"v{version}" or r["name"] == f"v{version}" for r in gh.items("releases?per_page=100", "")):
        return f"release v{version} exists"
    if version in ledger.loads(gh.raw(f"contents/{LEDGER}?ref=main")):
        return f"version {version} is in the ledger"
    # 运行列表只给出最新 attempt；较早的 attempt 可能已开始签发，逐个检查。
    for run_id, attempt in release_attempts(gh.workflow_runs(RELEASE_WORKFLOW), version, current):
        if signing_started(gh.jobs(run_id, attempt)):
            return f"run {run_id} attempt {attempt} started signing {version}"
    return None


def check_signing_environment(gh, name, owner_id):
    """签发环境须恰有一条必需审批人规则：审批人只有仓库所有者、允许自审、管理员不能绕过、部署分支不限。

    引用不存在的环境时 GitHub 会自动建出无保护的同名环境（0022 第 1 节），因此缺失与任何偏差都拒绝。
    prevent_self_review 只在 required_reviewers 规则对象内，环境对象顶层没有该字段（门闩探针实测）。
    """
    env = gh.get(f"environments/{name}")
    require(isinstance(env, dict) and env.get("name") == name, f"environment {name} was not returned")
    rules = env.get("protection_rules")
    require(isinstance(rules, list) and len(rules) == 1 and isinstance(rules[0], dict)
            and rules[0].get("type") == "required_reviewers",
            f"environment {name} must have exactly one required reviewers rule: "
            f"{[r.get('type') if isinstance(r, dict) else r for r in rules] if isinstance(rules, list) else rules!r}")
    rule = rules[0]
    require(rule.get("prevent_self_review") is False, f"environment {name} must allow self-review (the owner triggers and approves)")
    reviewers = rule.get("reviewers")
    require(isinstance(reviewers, list) and len(reviewers) == 1 and isinstance(reviewers[0], dict)
            and reviewers[0].get("type") == "User" and isinstance(reviewers[0].get("reviewer"), dict)
            and type(reviewers[0]["reviewer"].get("id")) is int and reviewers[0]["reviewer"]["id"] == owner_id,
            f"environment {name} must have the repository owner as its only reviewer")
    require(env.get("can_admins_bypass") is False, f"environment {name} lets administrators bypass its protection rules")
    require("deployment_branch_policy" in env and env["deployment_branch_policy"] is None,
            f"environment {name} must not restrict deployment branches")


def precheck(gh, git, args):
    """顺序：参数 → 签发环境 → 版本消费 → 演练引用或签发路径判定 → 继承的 CI 与 Compare（0039 第 3 条）。
    判定放在继承运行之前，不满足时在第一分钟内失败，不必先等同一提交的 CI 与 Compare。"""
    require(HEX40.fullmatch(args.sha) and VERSION.fullmatch(args.version), "sha and version must be canonical")
    meta = {"mode": args.mode, "version": args.version, "commit": args.sha,
            "run": {"id": args.run_id, "attempt": args.run_attempt}, "pid1_dry_run": None, "dry_run_basis": None}
    if args.mode == "release":
        require(args.signing_environment and args.owner_id, "a release run must name its signing environment and owner")
        check_signing_environment(gh, args.signing_environment, args.owner_id)
        hit = consumed_versions(gh, args.version, meta["run"])
        require(hit is None, f"version already consumed: {hit}")
        if args.pid1_dry_run:
            # 引用演练时仍按第 10 条核对：同一提交、演练模式、成功且两个 PID 1 job 成功。
            ref = parse_ref(args.pid1_dry_run)
            check_dry_run(gh.get(f"actions/runs/{ref['id']}/attempts/{ref['attempt']}"), gh.jobs(ref["id"], ref["attempt"]),
                          args.sha, args.version)
            meta["pid1_dry_run"] = ref
        else:
            try:
                same, basis, changed = signing_basis(gh, git, args.sha)
            except CandidateError as error:
                raise CandidateError(f"a release run without a dry run needs an unchanged signing path: {error}") from error
            require(same, f"the signing path changed since v{basis['baseline_version']} ({basis['baseline_commit']}); "
                          f"reference a dry run of this commit: {changed}")
            meta["dry_run_basis"] = basis
    else:
        require(not args.pid1_dry_run, "a dry run does not reference another dry run")
    meta["inherited"], meta["inherited_runs"] = inherited_runs(gh, args.sha)
    return meta


def recheck(gh, meta, run):
    # 只重跑失败 job 时前置检查不会重跑，meta 仍是旧 attempt；签发会落在新 attempt，证明与候选结果对不上，因此拒绝。
    require(meta["run"] == run, f"precheck ran in attempt {meta['run']}, signing would run in {run}; re-run all jobs")
    # 前置检查之后，同一提交上任何新运行或任何旧运行的重跑都是新证据，无论其进行中、失败还是成功都拒绝签发。
    inherited, snapshot = inherited_runs(gh, meta["commit"])
    require(inherited == meta["inherited"] and snapshot == meta["inherited_runs"],
            f"a newer CI / Compare run or re-run appeared since the precheck: {meta['inherited_runs']} -> {snapshot}")
    if meta["mode"] == "release":
        hit = consumed_versions(gh, meta["version"], run)
        require(hit is None, f"version already consumed: {hit}")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", required=True)
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("precheck")
    p.add_argument("--sha", required=True)
    p.add_argument("--version", required=True)
    p.add_argument("--mode", choices=("release", "dry-run"), required=True)
    p.add_argument("--run-id", type=int, required=True)
    p.add_argument("--run-attempt", type=int, required=True)
    p.add_argument("--pid1-dry-run", default="")
    p.add_argument("--signing-environment", default="")
    p.add_argument("--owner-id", type=int, default=0)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--git-dir", type=Path, default=Path("."))
    r = sub.add_parser("recheck")
    r.add_argument("--meta", type=Path, required=True)
    r.add_argument("--run-id", type=int, required=True)
    r.add_argument("--run-attempt", type=int, required=True)
    a = sub.add_parser("absent", help="exit 0 only when the API answers 404 for the path")
    a.add_argument("--path", required=True)
    b = sub.add_parser("basis", help="read-only: the baseline and whether the signing path of a commit is unchanged")
    b.add_argument("--sha", required=True)
    b.add_argument("--git-dir", type=Path, default=Path("."))
    args = parser.parse_args(argv)
    gh = Gh(args.repo, retry=True)
    try:
        if args.command == "basis":
            same, basis, changed = signing_basis(gh, Git(args.git_dir), args.sha)
            print(json.dumps({"commit": args.sha, "unchanged": same, "changed": changed, "basis": basis}, sort_keys=True))
        elif args.command == "absent":
            if not gh.absent(args.path):
                print(f"{args.path} exists", file=sys.stderr)
                return 1
            print(f"{args.path} is absent")
        elif args.command == "precheck":
            meta = precheck(gh, Git(args.git_dir), args)
            args.out.write_text(json.dumps(meta, sort_keys=True, indent=2) + "\n")
            print(json.dumps(meta, sort_keys=True))
        else:
            recheck(gh, json.loads(args.meta.read_text()), {"id": args.run_id, "attempt": args.run_attempt})
            print("recheck passed")
    except CandidateError as error:
        print(f"release {args.command} failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
