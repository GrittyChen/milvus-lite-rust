"""发行工作流的前置检查与签发前复核（0021 第 2、3、10 条），以及签发环境的保护规则检查（0022）。

判定逻辑是纯函数，输入为 GitHub REST API 返回的运行与 job 对象；`Gh` 只负责经 gh api 取数。
"""

import argparse
import json
from pathlib import Path
import subprocess
import sys

import ledger
from candidate import VERSION, HEX40, CandidateError, require

INHERITED = {"ci": (".github/workflows/ci.yml", "ci-pass"), "compare": (".github/workflows/compare.yml", "compare-pass")}
RELEASE_WORKFLOW = ".github/workflows/release.yml"
ATTEST_JOB = "attest"
SIGN_STEP = "sign"
PID1_JOBS = ("pid1 (ubuntu-24.04)", "pid1 (ubuntu-24.04-arm)")
LEDGER = "docs/releases/consumed-versions.json"


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


class Gh:
    def __init__(self, repo):
        self.repo = repo

    def _api(self, path, *extra):
        return subprocess.run(["gh", "api", "-H", "X-GitHub-Api-Version: 2022-11-28", *extra, f"repos/{self.repo}/{path}"],
                              capture_output=True, text=True)

    def get(self, path):
        result = self._api(path)
        require(result.returncode == 0, f"gh api {path} failed: {result.stderr.strip()}")
        return json.loads(result.stdout)

    def items(self, path, key):
        result = self._api(path, "--paginate", "--jq", f".{key}[]")
        require(result.returncode == 0, f"gh api {path} failed: {result.stderr.strip()}")
        return [json.loads(line) for line in result.stdout.splitlines() if line]

    def absent(self, path):
        result = self._api(path)
        if result.returncode == 0:
            return False
        require("HTTP 404" in result.stderr, f"gh api {path} failed: {result.stderr.strip()}")
        return True

    def raw(self, path):
        result = self._api(path, "-H", "Accept: application/vnd.github.raw+json")
        require(result.returncode == 0, f"gh api {path} failed: {result.stderr.strip()}")
        return result.stdout

    def workflow_runs(self, path, query=""):
        return self.items(f"actions/workflows/{Path(path).name}/runs?per_page=100{query}", "workflow_runs")

    def jobs(self, run_id, attempt):
        return self.items(f"actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100", "jobs")


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


def precheck(gh, args):
    require(HEX40.fullmatch(args.sha) and VERSION.fullmatch(args.version), "sha and version must be canonical")
    inherited, snapshot = inherited_runs(gh, args.sha)
    meta = {"mode": args.mode, "version": args.version, "commit": args.sha,
            "run": {"id": args.run_id, "attempt": args.run_attempt}, "inherited": inherited,
            "inherited_runs": snapshot, "pid1_dry_run": None}
    if args.mode == "release":
        require(args.signing_environment and args.owner_id, "a release run must name its signing environment and owner")
        check_signing_environment(gh, args.signing_environment, args.owner_id)
        hit = consumed_versions(gh, args.version, meta["run"])
        require(hit is None, f"version already consumed: {hit}")
        # 每个正式运行都引用同一冻结提交上包含真实 PID 1 的成功演练（0021 第 10 条），不从台账或其他状态推断。
        require(args.pid1_dry_run, "a release run must reference a dry run of the same commit that included PID 1")
        ref = parse_ref(args.pid1_dry_run)
        check_dry_run(gh.get(f"actions/runs/{ref['id']}/attempts/{ref['attempt']}"), gh.jobs(ref["id"], ref["attempt"]),
                      args.sha, args.version)
        meta["pid1_dry_run"] = ref
    else:
        require(not args.pid1_dry_run, "a dry run does not reference another dry run")
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
    r = sub.add_parser("recheck")
    r.add_argument("--meta", type=Path, required=True)
    r.add_argument("--run-id", type=int, required=True)
    r.add_argument("--run-attempt", type=int, required=True)
    args = parser.parse_args(argv)
    gh = Gh(args.repo)
    try:
        if args.command == "precheck":
            meta = precheck(gh, args)
            args.out.write_text(json.dumps(meta, sort_keys=True, indent=2) + "\n")
            print(json.dumps(meta, sort_keys=True))
        else:
            recheck(gh, json.loads(args.meta.read_text()), {"id": args.run_id, "attempt": args.run_attempt})
            print("recheck passed")
    except CandidateError as error:
        print(f"release precheck failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
