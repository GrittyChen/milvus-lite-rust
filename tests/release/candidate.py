"""候选结果与 SHA256SUMS 的生成与严格校验（0021 第 12 条）。

候选结果带格式版本、键集合固定，读取方缺键、多键、格式版本不认识或取值不合规都拒绝；阶段按
[scorecard.py](scorecard.py) 的固定矩阵逐项核对，缺平台、缺阶段、计数键或数量不符都拒绝。
它不记录 SHA256SUMS 的哈希；SHA256SUMS 在它之后生成，覆盖五个正式附件与候选结果。
"""

import argparse
import hashlib
import json
from pathlib import Path
import re
import sys

import scorecard

FORMAT = 1
MODES = ("release", "dry-run")
WHEEL_TAGS = ("macosx_11_0_arm64", "macosx_11_0_x86_64", "manylinux_2_28_aarch64", "manylinux_2_28_x86_64")
KEYS = {"format", "scorecard", "mode", "candidate_id", "version", "commit", "run", "source", "inherited",
        "pid1_dry_run", "artifacts", "fi_wheels", "build_environment", "stages"}
SOURCE_KEYS = {"commit", "version", "archive_sha256", "source_entries", "regular_files", "symlinks", "product_inputs",
               "source_manifest_sha256", "product_manifest_sha256", "tool_versions"}
SOURCE_TOOL_KEYS = {"git", "python"}
STAGE_FILE_KEYS = {"scope", "stage", "returncode", "counts", "environment"}
VERSION = re.compile(r"\d+\.\d+\.\d+(?:(?:a|b|rc)\d+)?")
HEX40 = re.compile(r"[0-9a-f]{40}")
HEX64 = re.compile(r"[0-9a-f]{64}")


class CandidateError(Exception):
    pass


def require(condition, message):
    if not condition:
        raise CandidateError(message)


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def formal_names(version):
    return sorted([f"milvus_lite_rust-{version}-cp310-abi3-{tag}.whl" for tag in WHEEL_TAGS]
                  + [f"milvus_lite_rust-{version}.tar.gz"])


def candidate_id(version, commit, run):
    return f"{version}@{commit}#{run['id']}.{run['attempt']}"


def candidate_name(mode, version, run):
    return ("dry-run-" if mode == "dry-run" else "") + f"candidate-{version}-{run['id']}-{run['attempt']}.json"


def sums_name(mode, run):
    return f"dry-run-SHA256SUMS-{run['id']}-{run['attempt']}" if mode == "dry-run" else "SHA256SUMS"


def is_int(value, minimum):
    return type(value) is int and value >= minimum


def check_run_ref(value, what):
    require(isinstance(value, dict) and set(value) == {"id", "attempt"}, f"{what} must be {{id, attempt}}")
    require(is_int(value["id"], 1) and is_int(value["attempt"], 1), f"{what} id and attempt must be positive integers")


def check_digests(value, keys, what):
    require(isinstance(value, dict) and sorted(value) == sorted(keys), f"{what} must cover exactly {sorted(keys)}")
    require(all(isinstance(v, str) and HEX64.fullmatch(v) for v in value.values()), f"{what} must be SHA256 digests")


def check_stage(scope, stage, result):
    where = f"stages[{scope}][{stage}]"
    require(isinstance(result, dict) and set(result) == {"returncode", "counts", "environment"}, f"{where} keys differ")
    require(type(result["returncode"]) is int and result["returncode"] == 0, f"{where} did not pass")
    rules, counts = scorecard.MATRIX[scope][stage], result["counts"]
    require(isinstance(counts, dict) and set(counts) == set(rules), f"{where} count keys must be {sorted(rules)}")
    bad = {key: counts[key] for key in rules if not scorecard.count_ok(rules[key], counts[key])}
    require(not bad, f"{where} counts {bad} do not match {rules}")
    environment, keys = result["environment"], scorecard.environment_keys(scope)
    require(isinstance(environment, dict) and set(environment) == set(keys), f"{where} environment keys must be {sorted(keys)}")
    require(all(isinstance(v, str) and v.strip() for v in environment.values()), f"{where} environment values must be non-empty")
    if "pid1_image" in environment:
        # PID 1 阶段须记录本平台固定的基础镜像摘要及实际镜像 ID；其他阶段不使用该镜像。
        image = environment["pid1_image"]
        if stage == "pid1":
            require(re.fullmatch(re.escape(scorecard.PID1_IMAGES[scope]) + r" id sha256:[0-9a-f]{64}", image),
                    f"{where} pid1_image {image!r} is not the pinned {scorecard.PID1_IMAGES[scope]} with an image id")
        else:
            require(image == scorecard.NOT_USED, f"{where} pid1_image must be {scorecard.NOT_USED}")


def validate(data, filename=None):
    require(isinstance(data, dict), "candidate result must be an object")
    require(type(data.get("format")) is int and data.get("format") == FORMAT, f"unknown format {data.get('format')!r}")
    require(set(data) == KEYS, f"keys differ: missing {sorted(KEYS - set(data))}, extra {sorted(set(data) - KEYS)}")
    require(type(data["scorecard"]) is int and data["scorecard"] == scorecard.FORMAT, f"unknown scorecard {data['scorecard']!r}")
    mode, version, commit, run = data["mode"], data["version"], data["commit"], data["run"]
    require(mode in MODES, f"unknown mode {mode!r}")
    require(isinstance(version, str) and VERSION.fullmatch(version), f"bad version {version!r}")
    require(isinstance(commit, str) and HEX40.fullmatch(commit), f"bad commit {commit!r}")
    check_run_ref(run, "run")
    require(data["candidate_id"] == candidate_id(version, commit, run), "candidate_id does not match version, commit and run")
    if filename is not None:
        require(filename == candidate_name(mode, version, run), f"file name {filename} does not match content")

    source = data["source"]
    require(isinstance(source, dict) and set(source) == SOURCE_KEYS, "source identity keys differ")
    require(source["commit"] == commit and source["version"] == version, "source identity is for another commit or version")
    require(all(HEX64.fullmatch(str(source[k])) for k in ("archive_sha256", "source_manifest_sha256", "product_manifest_sha256")),
            "source identity digests must be SHA256")
    require(all(is_int(source[k], 1) for k in ("source_entries", "regular_files", "product_inputs"))
            and is_int(source["symlinks"], 0) and source["regular_files"] + source["symlinks"] == source["source_entries"],
            "source identity counts are inconsistent")
    tools = source["tool_versions"]
    require(isinstance(tools, dict) and set(tools) == SOURCE_TOOL_KEYS
            and all(isinstance(v, str) and v.strip() for v in tools.values()) and tools["git"].startswith("git version "),
            f"source identity tool versions must name git and python: {tools!r}")

    inherited = data["inherited"]
    require(isinstance(inherited, dict) and set(inherited) == {"ci", "compare"}, "inherited must name ci and compare runs")
    for name, ref in inherited.items():
        check_run_ref(ref, f"inherited {name}")
    if mode == "release":
        check_run_ref(data["pid1_dry_run"], "pid1_dry_run")
    else:
        require(data["pid1_dry_run"] is None, "a dry run does not reference another dry run")

    check_digests(data["artifacts"], formal_names(version), "artifacts")
    check_digests(data["fi_wheels"], WHEEL_TAGS, "fi_wheels")

    build = data["build_environment"]
    require(isinstance(build, dict) and set(build) == set(scorecard.PLATFORMS),
            f"build_environment must cover {sorted(scorecard.PLATFORMS)}")
    for platform, values in build.items():
        keys = scorecard.build_environment_keys(platform)
        require(isinstance(values, dict) and set(values) == set(keys), f"build_environment[{platform}] keys must be {sorted(keys)}")
        require(all(isinstance(v, str) and v.strip() for v in values.values()), f"build_environment[{platform}] values must be non-empty")

    stages = data["stages"]
    require(isinstance(stages, dict) and set(stages) == set(scorecard.MATRIX),
            f"stage scopes must be {sorted(scorecard.MATRIX)}")
    for scope, entries in stages.items():
        expected = scorecard.MATRIX[scope]
        require(isinstance(entries, dict) and set(entries) == set(expected),
                f"stages[{scope}] missing {sorted(set(expected) - set(entries or {}))}, extra {sorted(set(entries or {}) - set(expected))}")
        for stage, result in entries.items():
            check_stage(scope, stage, result)
    return data


def load(path):
    path = Path(path)
    try:
        data = json.loads(path.read_text())
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise CandidateError(f"cannot read {path.name}: {error}") from error
    return validate(data, path.name)


def build(meta, identity, artifacts_dir, fi_dir, stages, build_environment):
    """meta 来自前置检查：mode、version、commit、run、inherited、pid1_dry_run。"""
    version = meta["version"]
    present = sorted(p.name for p in Path(artifacts_dir).iterdir())
    require(present == formal_names(version), f"formal artifact set differs: {present}")
    artifacts = {name: sha256_file(Path(artifacts_dir) / name) for name in present}
    fi_wheels = {}
    for tag in WHEEL_TAGS:
        found = sorted(Path(fi_dir).glob(f"*/milvus_lite_rust-{version}-cp310-abi3-{tag}.whl"))
        require(len(found) == 1, f"expected one FI wheel for {tag}, found {len(found)}")
        fi_wheels[tag] = sha256_file(found[0])
    data = {**{k: meta[k] for k in ("mode", "version", "commit", "run", "inherited", "pid1_dry_run")},
            "format": FORMAT, "scorecard": scorecard.FORMAT, "candidate_id": candidate_id(version, meta["commit"], meta["run"]),
            "source": identity, "artifacts": artifacts, "fi_wheels": fi_wheels, "build_environment": build_environment,
            "stages": stages}
    return validate(data)


def sums_text(entries):
    return "".join(f"{digest}  {name}\n" for name, digest in sorted(entries.items()))


def parse_sums(text):
    entries = {}
    for line in text.splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  ([^/\\\s][^/\\]*)", line)
        require(match is not None, f"malformed SHA256SUMS line {line!r}")
        require(match.group(2) not in entries, f"duplicate SHA256SUMS entry {match.group(2)}")
        entries[match.group(2)] = match.group(1)
    require(text.endswith("\n") and entries, "SHA256SUMS must be non-empty and end with a newline")
    return entries


def read_build_environment(directory):
    """装配交出的 build-env-<平台>.json，每个平台恰好一份。"""
    found = {p.name.removeprefix("build-env-").removesuffix(".json"): p for p in Path(directory).glob("build-env-*.json")}
    require(set(found) == set(scorecard.PLATFORMS), f"build environment records differ: {sorted(found)}")
    return {platform: json.loads(path.read_text()) for platform, path in found.items()}


def write(meta, identity, artifacts_dir, fi_dir, stages, build_environment, out):
    data = build(meta, identity, artifacts_dir, fi_dir, stages, build_environment)
    out = Path(out)
    out.mkdir(parents=True)
    name = candidate_name(data["mode"], data["version"], data["run"])
    (out / name).write_text(json.dumps(data, sort_keys=True, indent=2) + "\n")
    entries = {**data["artifacts"], name: sha256_file(out / name)}
    (out / sums_name(data["mode"], data["run"])).write_text(sums_text(entries))
    return out / name


def merge_stages(paths):
    """每个验收步骤写一份 {scope, stage, returncode, counts, environment}；同一 scope / stage 不得重复。"""
    stages = {}
    for path in sorted(paths):
        item = json.loads(Path(path).read_text())
        require(isinstance(item, dict) and set(item) == STAGE_FILE_KEYS, f"stage file keys differ: {path}")
        require(item["stage"] not in stages.setdefault(item["scope"], {}), f"duplicate stage {item['scope']}/{item['stage']}")
        stages[item["scope"]][item["stage"]] = {k: item[k] for k in ("returncode", "counts", "environment")}
    return stages


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    w = sub.add_parser("write")
    w.add_argument("--meta", type=Path, required=True)
    w.add_argument("--identity", type=Path, required=True)
    w.add_argument("--artifacts", type=Path, required=True)
    w.add_argument("--fi", type=Path, required=True)
    w.add_argument("--stage", type=Path, action="append", required=True)
    w.add_argument("--build-env", type=Path, required=True, help="directory with build-env-<platform>.json")
    w.add_argument("--out", type=Path, required=True)
    w.add_argument("--run-id", type=int, required=True, help="the run that writes the result; must match the precheck")
    w.add_argument("--run-attempt", type=int, required=True)
    c = sub.add_parser("check")
    c.add_argument("path", type=Path)
    args = parser.parse_args(argv)
    try:
        if args.command == "write":
            meta = json.loads(args.meta.read_text())
            # 只重跑失败 job 时前置检查沿用旧 attempt；候选结果必须属于当前 attempt，否则与之后的证明对不上。
            require(meta["run"] == {"id": args.run_id, "attempt": args.run_attempt},
                    f"precheck ran in {meta['run']}, not in run {args.run_id} attempt {args.run_attempt}; re-run all jobs")
            print(write(meta, json.loads(args.identity.read_text()), args.artifacts, args.fi, merge_stages(args.stage),
                        read_build_environment(args.build_env), args.out))
        else:
            data = load(args.path)
            print(f"candidate result valid: {data['candidate_id']} ({data['mode']})")
    except CandidateError as error:
        print(f"candidate result rejected: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
