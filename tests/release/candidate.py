"""候选结果与 SHA256SUMS 的生成与严格校验（0021 第 12 条）。

候选结果带格式版本、键集合固定，读取方缺键、多键、格式版本不认识或取值不合规都拒绝；阶段按
[scorecard.py](scorecard.py) 的固定矩阵逐项核对，缺平台、缺阶段、计数键或数量不符都拒绝。
它不记录 SHA256SUMS 的哈希；SHA256SUMS 在它之后生成，覆盖五个正式附件与候选结果。
正式运行未引用演练时，`dry_run_basis` 记录签发路径相对已发布基线未变的依据（0039）。
"""

import argparse
import hashlib
import json
from pathlib import Path
import re
import sys

import scorecard

FORMAT = 2
MODES = ("release", "dry-run")
# 平台到 wheel 标签：工作流测试矩阵的 platform.tag 须与此一致（test_release_workflow 核对）。
SCOPE_TAGS = {"linux-x86_64": "manylinux_2_28_x86_64", "linux-aarch64": "manylinux_2_28_aarch64",
              "macos-arm64": "macosx_11_0_arm64", "macos-x86_64": "macosx_11_0_x86_64"}
# 字面量：tests/packaging 以 ast.literal_eval 读取（不导入本模块）；与 SCOPE_TAGS 的一致由 tests/release 核对。
WHEEL_TAGS = ("macosx_11_0_arm64", "macosx_11_0_x86_64", "manylinux_2_28_aarch64", "manylinux_2_28_x86_64")
# 测试 job 的 part；每个平台 × part 交出一份 tested-<平台>-<part>.sha256（0039 第 8 条）。
TEST_PARTS = ("diff-2.6.17", "diff-3.0.2", "interop", "python", "static", "fi")
KEYS = {"format", "scorecard", "mode", "candidate_id", "version", "commit", "run", "source", "inherited",
        "pid1_dry_run", "dry_run_basis", "artifacts", "fi_wheels", "build_environment", "stages"}
# 签发路径（0039 第 1 条）：工作流头部（env 只留 MODE 与 VERSION）、最后两个 job，以及签发入口工具在
# tests/release 内的 import 闭包；scorecard.py 显式豁免。闭包由工具自检从入口重算并与此核对。
RELEASE_WORKFLOW = ".github/workflows/release.yml"
WORKFLOW_HEADER = RELEASE_WORKFLOW + "#header"
WORKFLOW_SIGNING_JOBS = RELEASE_WORKFLOW + "#signing-jobs"
SIGNING_ENTRIES = ("tests/release/preserve.py", "tests/release/release_set.py", "tests/release/runs.py", "tests/release/sums.py")
SIGNING_EXEMPT = ("tests/release/scorecard.py",)
SIGNING_TOOLS = ("tests/release/candidate.py", "tests/release/ledger.py", "tests/release/preserve.py",
                 "tests/release/release_set.py", "tests/release/runs.py", "tests/release/sums.py")
SIGNING_PATHS = (WORKFLOW_HEADER, WORKFLOW_SIGNING_JOBS, *SIGNING_TOOLS)
BASIS_KEYS = {"baseline_version", "baseline_commit", "signing_path_sha256", "paths"}
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


def check_basis(value):
    require(isinstance(value, dict) and set(value) == BASIS_KEYS,
            f"a release run without a dry run must record dry_run_basis with keys {sorted(BASIS_KEYS)}")
    require(isinstance(value["baseline_version"], str) and VERSION.fullmatch(value["baseline_version"]),
            f"bad baseline version {value['baseline_version']!r}")
    require(isinstance(value["baseline_commit"], str) and HEX40.fullmatch(value["baseline_commit"]), "bad baseline commit")
    require(isinstance(value["signing_path_sha256"], str) and HEX64.fullmatch(value["signing_path_sha256"]),
            "bad signing path digest")
    require(value["paths"] == list(SIGNING_PATHS), f"dry_run_basis paths must be exactly {list(SIGNING_PATHS)}")


def wheel_name(version, scope):
    return f"milvus_lite_rust-{version}-cp310-abi3-{SCOPE_TAGS[scope]}.whl"


def tested_name(scope, part):
    return f"tested-{scope}-{part}.sha256"


def check_tested(directory, formal_sums, fi_sums, version):
    """每个测试 job 实际使用的三个文件须与装配清单成对一致（0039 第 8 条）：24 份恰好齐全，每份恰为本平台的
    正式 wheel、本平台的 FI wheel 与 sdist。build.sha256 与产物同包，只能发现传输损坏；这里把测试用的字节
    与将要签发的字节绑在一起。"""
    directory = Path(directory)
    formal, fi = parse_manifest(formal_sums), parse_manifest(fi_sums)
    expected = {tested_name(scope, part) for scope in SCOPE_TAGS for part in TEST_PARTS}
    present = {p.name for p in directory.glob("tested-*.sha256")}
    require(present == expected, f"tested digests differ: missing {sorted(expected - present)}, extra {sorted(present - expected)}")
    for scope in SCOPE_TAGS:
        wheel = wheel_name(version, scope)
        want = {f"formal/{wheel}": formal.get(wheel), f"formal/milvus_lite_rust-{version}.tar.gz":
                formal.get(f"milvus_lite_rust-{version}.tar.gz"), f"fi/build-{scope}/{wheel}": fi.get(f"build-{scope}/{wheel}")}
        require(all(want.values()), f"assembled manifests lack the files for {scope}")
        for part in TEST_PARTS:
            got = parse_manifest((directory / tested_name(scope, part)).read_text())
            require(got == want, f"{tested_name(scope, part)} does not match the assembled manifests: {got} != {want}")


def parse_manifest(text):
    """sums.py 写出的清单：「摘要  相对路径」，路径可含子目录。"""
    entries = {}
    for line in text.splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  ([^/\\\s][^\\]*)", line)
        require(match is not None and ".." not in match.group(2).split("/"), f"malformed manifest line {line!r}")
        require(match.group(2) not in entries, f"duplicate manifest entry {match.group(2)}")
        entries[match.group(2)] = match.group(1)
    require(text.endswith("\n") and entries, "manifest must be non-empty and end with a newline")
    return entries


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
        # 正式运行二选一：引用演练，或记录签发路径未变的依据（0039 第 4 条）。
        if data["pid1_dry_run"] is not None:
            check_run_ref(data["pid1_dry_run"], "pid1_dry_run")
            require(data["dry_run_basis"] is None, "a release run that references a dry run records no dry_run_basis")
        else:
            check_basis(data["dry_run_basis"])
    else:
        require(data["pid1_dry_run"] is None, "a dry run does not reference another dry run")
        require(data["dry_run_basis"] is None, "a dry run records no dry_run_basis")

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
    """meta 来自前置检查：mode、version、commit、run、inherited、pid1_dry_run、dry_run_basis。"""
    version = meta["version"]
    present = sorted(p.name for p in Path(artifacts_dir).iterdir())
    require(present == formal_names(version), f"formal artifact set differs: {present}")
    artifacts = {name: sha256_file(Path(artifacts_dir) / name) for name in present}
    fi_wheels = {}
    for tag in WHEEL_TAGS:
        found = sorted(Path(fi_dir).glob(f"*/milvus_lite_rust-{version}-cp310-abi3-{tag}.whl"))
        require(len(found) == 1, f"expected one FI wheel for {tag}, found {len(found)}")
        fi_wheels[tag] = sha256_file(found[0])
    data = {**{k: meta[k] for k in ("mode", "version", "commit", "run", "inherited", "pid1_dry_run", "dry_run_basis")},
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
    t = sub.add_parser("tested", help="cross-check the files each test job used against the assembled manifests")
    t.add_argument("--dir", type=Path, required=True)
    t.add_argument("--formal-sums", type=Path, required=True)
    t.add_argument("--fi-sums", type=Path, required=True)
    t.add_argument("--version", required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == "write":
            meta = json.loads(args.meta.read_text())
            # 只重跑失败 job 时前置检查沿用旧 attempt；候选结果必须属于当前 attempt，否则与之后的证明对不上。
            require(meta["run"] == {"id": args.run_id, "attempt": args.run_attempt},
                    f"precheck ran in {meta['run']}, not in run {args.run_id} attempt {args.run_attempt}; re-run all jobs")
            print(write(meta, json.loads(args.identity.read_text()), args.artifacts, args.fi, merge_stages(args.stage),
                        read_build_environment(args.build_env), args.out))
        elif args.command == "tested":
            check_tested(args.dir, args.formal_sums.read_text(), args.fi_sums.read_text(), args.version)
            print(f"tested digests match the assembled manifests: {len(SCOPE_TAGS) * len(TEST_PARTS)} files")
        else:
            data = load(args.path)
            print(f"candidate result valid: {data['candidate_id']} ({data['mode']})")
    except CandidateError as error:
        print(f"candidate result rejected: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
