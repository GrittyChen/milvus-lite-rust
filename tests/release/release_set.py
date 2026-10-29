"""核对一组待发布或已公开的 Release 附件（0021 第 20 条）。

发布准备在创建草稿前执行，公开下载工作流对公开地址下载的同一组文件重复执行：
- 七个文件恰好齐全；
- 候选结果的来源证明先于其内容被信任，且为正式运行；
- 每个文件的全部来源证明都来自候选结果声称的那次运行（invocation 的运行 ID 与 attempt）、
  冻结提交与发行工作流，主体恰为这七个文件，即同一次多主体签发；
- 五个正式附件与候选结果的哈希、SHA256SUMS 一致。
发布准备另外要求版本没有 tag / Release，且版本台账没有登记其他运行。
"""

import argparse
import json
from pathlib import Path
import re
import subprocess
import sys

import candidate as c
import ledger
from runs import LEDGER, RELEASE_WORKFLOW, Gh


def attestation_statements(path, repo, commit=None):
    command = ["gh", "attestation", "verify", str(path), "--repo", repo, "--signer-workflow", f"{repo}/{RELEASE_WORKFLOW}",
               "--format", "json"]
    if commit:
        command += ["--source-digest", commit]
    result = subprocess.run(command, capture_output=True, text=True)
    c.require(result.returncode == 0, f"attestation verify failed for {Path(path).name}: {result.stderr.strip()}")
    try:
        return [item["verificationResult"]["statement"] for item in json.loads(result.stdout)]
    except (json.JSONDecodeError, KeyError, TypeError) as error:
        raise c.CandidateError(f"unexpected attestation output for {Path(path).name}: {error}") from error


def check_statements(name, statements, repo, data, subjects):
    invocation = f"https://github.com/{repo}/actions/runs/{data['run']['id']}/attempts/{data['run']['attempt']}"
    c.require(statements, f"no attestation for {name}")
    for statement in statements:
        try:
            predicate = statement["predicate"]
            got = {
                "invocation": predicate["runDetails"]["metadata"]["invocationId"],
                "workflow": predicate["buildDefinition"]["externalParameters"]["workflow"]["path"],
                "commit": predicate["buildDefinition"]["resolvedDependencies"][0]["digest"]["gitCommit"],
                "subjects": [(s["name"], s["digest"]["sha256"]) for s in statement["subject"]],
            }
        except (KeyError, IndexError, TypeError) as error:
            raise c.CandidateError(f"attestation for {name} lacks {error}") from error
        c.require(got["invocation"] == invocation, f"{name} is attested by {got['invocation']}, not {invocation}")
        c.require(got["workflow"] == RELEASE_WORKFLOW and got["commit"] == data["commit"],
                  f"{name} is attested for {got['workflow']} at {got['commit']}")
        # 按列表比较：转成字典会把重复的主体折叠掉，七个条目必须各出现一次。
        c.require(len(got["subjects"]) == len(subjects) and dict(got["subjects"]) == subjects,
                  f"attestation for {name} does not cover exactly the seven release files once each")


def verify(directory, version, repo, statements_for=attestation_statements, prepare=None):
    directory = Path(directory)
    names = sorted(p.name for p in directory.iterdir())
    formal = c.formal_names(version)
    candidates = [n for n in names if re.fullmatch(rf"candidate-{re.escape(version)}-\d+-\d+\.json", n)]
    c.require(len(candidates) == 1, f"expected one candidate result for {version}, found {candidates}")
    expected = sorted(formal + candidates + ["SHA256SUMS"])
    c.require(names == expected, f"attachment set differs: missing {sorted(set(expected) - set(names))}, "
                                 f"extra {sorted(set(names) - set(expected))}")
    result = directory / candidates[0]
    first = statements_for(result, repo)
    data = c.load(result)
    c.require(data["mode"] == "release", f"candidate result mode is {data['mode']}")
    c.require(data["version"] == version, f"candidate result is for {data['version']}")
    subjects = {name: c.sha256_file(directory / name) for name in names}
    check_statements(result.name, first, repo, data, subjects)
    for name in names:
        check_statements(name, statements_for(directory / name, repo, data["commit"]), repo, data, subjects)
    sums = c.parse_sums((directory / "SHA256SUMS").read_text())
    c.require(sums == {name: subjects[name] for name in formal + candidates}, "SHA256SUMS does not match the attachments")
    c.require(all(data["artifacts"][name] == subjects[name] for name in formal), "candidate result hashes differ from attachments")
    if prepare is not None:
        prepare(data)
    return data


def prepare_checks(gh, data):
    version = data["version"]
    c.require(gh.absent(f"git/ref/tags/v{version}"), f"tag v{version} exists")
    c.require(not any(r["tag_name"] == f"v{version}" for r in gh.items("releases?per_page=100", "")),
              f"release v{version} exists")
    ledger.check_preparable(ledger.loads(gh.raw(f"contents/{LEDGER}?ref=main")), version, data["run"])


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--dir", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--prepare", action="store_true", help="also require no tag / Release and no other run in the ledger")
    args = parser.parse_args(argv)
    try:
        prepare = (lambda data: prepare_checks(Gh(args.repo, retry=True), data)) if args.prepare else None
        data = verify(args.dir, args.version, args.repo, prepare=prepare)
    except c.CandidateError as error:
        print(f"release set rejected: {error}", file=sys.stderr)
        return 1
    print(f"release set verified: {data['candidate_id']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
