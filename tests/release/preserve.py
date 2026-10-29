"""签发前的候选保全（0022）：本机保存完整候选证据集并给出回执，工作流在签发前按回执核对审批。

完整候选证据集是候选产物的七个文件与 fi 产物的四个 FI wheel。回执是规范文本的 SHA256：第一行为候选 ID，其后每个
文件一行「SHA256␣␣规范路径」，按规范路径排序；规范路径为 release/<文件名> 与 fi/<文件名>（正式 wheel 与 FI wheel 同名，
前缀使十一条路径互不相同）。本机保存、目录复核与工作流核对都调用同一个 canonical_text。

子命令：
- save：负责人在冻结提交的干净 worktree 中运行，经本人的 gh 认证下载本运行本 attempt 的两个产物，原子地写入
  <根目录>/<模式>/<版本>/<运行 ID>.<attempt>/，最后一行输出审批备注（演练为不能用于审批的 dry-run-receipt 行）；
- verify：离线复核已保存的目录，输出与 save 相同的末行；
- digests：preserve-digests job 以本运行的 fi 产物重算四个 FI wheel 的摘要，与候选结果交叉核对后输出；
- gate：attest 的「保全回执」步骤，正式运行只在环境保护规则未变、审批备注逐字等于本次候选 ID 与回执时通过。
"""

import argparse
from datetime import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import secrets
import shutil
import stat
import subprocess
import sys
import time
import zipfile

import candidate as c
import runs

MODES = ("release", "dry-run")
# 完成标记第 2 版起记录 preserve.json 的 SHA256，任何元数据改动都能被复核发现。
COMPLETE_FORMAT = 2
RECORD_FORMAT = 1
MARKER_KEYS = {"format", "mode", "candidate_id", "receipt", "record_sha256"}
RECORD_KEYS = {"format", "mode", "version", "commit", "run", "candidate_id", "receipt", "tool_commit", "saved_at", "zips"}
ZIP_KEYS = {"artifact_id", "digest", "sha256"}
SAVED_AT = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z")
CANDIDATE_JOB = "candidate"
DIGESTS_JOB = "preserve-digests"
APPROVAL_PREFIX = "preserved"
DRY_RUN_PREFIX = "dry-run-receipt"
# rc903 的两个产物各约 22 MB；上限只防错误或损坏的产物耗尽本机资源。
MAX_ENTRIES = 16
MAX_BYTES = 512 << 20
CORE = ("COMPLETE", "preservation.txt", "receipt", "preserve.json", "release", "fi", "zips")
OPTIONAL = ("post-sign",)
NAME = re.compile(r"[A-Za-z0-9._+-]+")
CANDIDATE_ID = re.compile(rf"(?P<version>{c.VERSION.pattern})@(?P<commit>[0-9a-f]{{40}})#(?P<id>[1-9]\d*)\.(?P<attempt>[1-9]\d*)")
FI_DIR = re.compile(r"build-[a-z0-9_-]+")


def _crash_point(name):
    """测试在子进程内替换本函数以在指定时点终止进程；产品路径中什么都不做。"""


# 规范回执


def canonical_text(candidate_id, entries):
    """entries 为 {规范路径: SHA256}：恰好七个 release/ 与四个 fi/。返回规范文本的字节。"""
    c.require(isinstance(candidate_id, str) and CANDIDATE_ID.fullmatch(candidate_id), f"bad candidate id {candidate_id!r}")
    c.require(isinstance(entries, dict), "entries must be a mapping")
    for path, digest in entries.items():
        prefix, _, name = path.partition("/")
        c.require(prefix in ("release", "fi") and NAME.fullmatch(name), f"bad canonical path {path!r}")
        c.require(isinstance(digest, str) and c.HEX64.fullmatch(digest), f"bad digest for {path}")
    counts = [sum(1 for p in entries if p.startswith(f"{prefix}/")) for prefix in ("release", "fi")]
    c.require(counts == [7, 4], f"expected 7 release/ and 4 fi/ entries, got {counts}")
    # 路径只含 ASCII，字符串排序即字节序。
    return "".join([f"{candidate_id}\n"] + [f"{entries[p]}  {p}\n" for p in sorted(entries)]).encode("utf-8")


def receipt_of(text):
    return hashlib.sha256(text).hexdigest()


def output_line(mode, candidate_id, receipt):
    return f"{APPROVAL_PREFIX if mode == 'release' else DRY_RUN_PREFIX} {candidate_id} {receipt}"


def approval_comment(candidate_id, receipt):
    return f"{APPROVAL_PREFIX} {candidate_id} {receipt}"


# 候选集合


def fi_wheel_name(version, tag):
    return f"milvus_lite_rust-{version}-cp310-abi3-{tag}.whl"


def release_names(mode, version, run):
    return sorted(c.formal_names(version) + [c.candidate_name(mode, version, run), c.sums_name(mode, run)])


def check_candidate_set(mode, version, run, release_dir, fi_digests):
    """release_dir 恰为候选产物的七个文件；fi_digests 为 {FI wheel 文件名: SHA256}。返回 (候选结果, 规范条目)。"""
    release_dir = Path(release_dir)
    present = sorted(p.name for p in release_dir.iterdir())
    expected = release_names(mode, version, run)
    c.require(present == expected, f"candidate files differ: missing {sorted(set(expected) - set(present))}, "
                                   f"extra {sorted(set(present) - set(expected))}")
    data = c.load(release_dir / c.candidate_name(mode, version, run))
    c.require(data["mode"] == mode, f"candidate result mode is {data['mode']}, expected {mode}")
    c.require(data["version"] == version, f"candidate result is for {data['version']}, not {version}")
    c.require(data["run"] == run, f"candidate result belongs to run {data['run']}, not {run}")
    digests = {name: c.sha256_file(release_dir / name) for name in expected}
    formal = c.formal_names(version)
    c.require(all(data["artifacts"][name] == digests[name] for name in formal), "formal files differ from the candidate result")
    sums = c.parse_sums((release_dir / c.sums_name(mode, run)).read_text())
    c.require(sums == {name: digests[name] for name in formal + [c.candidate_name(mode, version, run)]},
              "SHA256SUMS does not match the candidate files")
    wanted = {fi_wheel_name(version, tag): data["fi_wheels"][tag] for tag in c.WHEEL_TAGS}
    c.require(isinstance(fi_digests, dict) and sorted(fi_digests) == sorted(wanted),
              f"FI wheels differ: {sorted(fi_digests) if isinstance(fi_digests, dict) else fi_digests!r}")
    c.require(fi_digests == wanted, "FI wheel digests differ from the candidate result")
    entries = {f"release/{name}": digest for name, digest in digests.items()}
    entries.update({f"fi/{name}": digest for name, digest in fi_digests.items()})
    return data, entries


def fi_artifact_digests(fi_root):
    """fi 产物的下载目录：fi.sha256 与 fi/build-<平台>/<wheel>。逐项核对清单后返回 {wheel 文件名: SHA256}。"""
    fi_root = Path(fi_root)
    top = sorted(p.name for p in fi_root.iterdir())
    c.require(top == ["fi", "fi.sha256"], f"fi artifact layout differs: {top}")
    files = {}
    for path in sorted((fi_root / "fi").rglob("*")):
        relative = path.relative_to(fi_root / "fi")
        if path.is_dir() and not path.is_symlink() and len(relative.parts) == 1:
            continue
        c.require(path.is_file() and not path.is_symlink() and len(relative.parts) == 2 and FI_DIR.fullmatch(relative.parts[0]),
                  f"unexpected entry in fi artifact: {relative}")
        files[relative.as_posix()] = path
    return check_fi_list((fi_root / "fi.sha256").read_text(), {k: c.sha256_file(p) for k, p in files.items()})


def check_fi_list(text, digests):
    """text 为 fi.sha256，digests 为 {build-<平台>/<wheel>: SHA256}；返回 {wheel 文件名: SHA256}。"""
    listed = _parse_fi_list(text)
    c.require(listed == digests, f"fi.sha256 does not match the FI wheels: listed {sorted(listed)}, present {sorted(digests)}")
    by_name = {}
    for relative, digest in digests.items():
        name = relative.split("/")[1]
        c.require(name not in by_name, f"duplicate FI wheel {name}")
        by_name[name] = digest
    return by_name


def _parse_fi_list(text):
    entries = {}
    for line in text.splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  (build-[a-z0-9_-]+/[A-Za-z0-9._+-]+)", line)
        c.require(match is not None, f"malformed fi.sha256 line {line!r}")
        c.require(match.group(2) not in entries, f"duplicate fi.sha256 entry {match.group(2)}")
        entries[match.group(2)] = match.group(1)
    c.require(text.endswith("\n") and entries, "fi.sha256 must be non-empty and end with a newline")
    return entries


def parse_fi_digests(text, version):
    """preserve-digests 的输出：恰为四条 fi/<文件名>，返回 {文件名: SHA256}。"""
    try:
        value = json.loads(text)
    except (TypeError, json.JSONDecodeError) as error:
        raise c.CandidateError(f"FI digests are not JSON: {error}") from error
    expected = sorted(f"fi/{fi_wheel_name(version, tag)}" for tag in c.WHEEL_TAGS)
    c.require(isinstance(value, dict) and sorted(value) == expected, f"FI digests must name exactly {expected}")
    c.require(all(isinstance(v, str) and c.HEX64.fullmatch(v) for v in value.values()), "FI digests must be SHA256")
    return {k.removeprefix("fi/"): v for k, v in value.items()}


# 文件系统


def fsync_dir(path):
    fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def write_file(path, data):
    with open(path, "xb") as f:
        f.write(data)
        f.flush()
        os.fsync(f.fileno())


def make_dir(path):
    """创建目录；新建的每一级都对其父目录 fsync，使目录项本身持久化。"""
    path = Path(path)
    missing = []
    while not path.exists():
        missing.append(path)
        path = path.parent
    for item in reversed(missing):
        item.mkdir()
        fsync_dir(item.parent)


def fsync_tree(root):
    """自底向上对 root 下的每个目录 fsync（文件已在写入时 fsync）。"""
    for directory, _, _ in sorted(os.walk(root), key=lambda item: -len(Path(item[0]).parts)):
        fsync_dir(directory)


def extract(zip_path, place):
    """安全解包：拒绝绝对路径、..、符号链接与特殊文件、重复条目，条目数与实际解出字节有上限。

    place(name) 返回成员应写入的路径，或返回 None 表示只读入内存（返回其字节）；目录条目忽略。
    返回 {成员名: (路径或字节, SHA256)}。
    """
    result, total = {}, 0
    with zipfile.ZipFile(zip_path) as archive:
        members = archive.infolist()
        c.require(len(members) <= MAX_ENTRIES, f"{Path(zip_path).name} has {len(members)} entries, more than {MAX_ENTRIES}")
        names = [m.filename for m in members]
        c.require(len(names) == len(set(names)), f"{Path(zip_path).name} has duplicate entries")
        for member in members:
            name = member.filename
            parts = name.rstrip("/").split("/")
            c.require(not name.startswith("/") and "\\" not in name and not any(part in ("", ".", "..") for part in parts),
                      f"unsafe entry {name!r}")
            kind = stat.S_IFMT(member.external_attr >> 16)
            if member.is_dir():
                c.require(kind in (0, stat.S_IFDIR), f"unsafe entry {name!r}")
                continue
            c.require(kind in (0, stat.S_IFREG), f"{name!r} is not a regular file")
            destination = place(name)
            digest = hashlib.sha256()
            chunks = []
            with archive.open(member) as source:
                out = open(destination, "xb") if destination is not None else None
                try:
                    for block in iter(lambda: source.read(1 << 20), b""):
                        total += len(block)
                        c.require(total <= MAX_BYTES, f"{Path(zip_path).name} expands to more than {MAX_BYTES} bytes")
                        digest.update(block)
                        if out is None:
                            chunks.append(block)
                        else:
                            out.write(block)
                    if out is not None:
                        out.flush()
                        os.fsync(out.fileno())
                finally:
                    if out is not None:
                        out.close()
            result[name] = (destination if destination is not None else b"".join(chunks), digest.hexdigest())
    return result


# 已保存目录


def parse_candidate_id(candidate_id):
    match = CANDIDATE_ID.fullmatch(candidate_id) if isinstance(candidate_id, str) else None
    c.require(match is not None, f"bad candidate id {candidate_id!r}")
    return match["version"], match["commit"], {"id": int(match["id"]), "attempt": int(match["attempt"])}


def regular_files(directory):
    found = {}
    for path in sorted(Path(directory).iterdir()):
        c.require(not path.is_symlink() and path.is_file(), f"unexpected entry {path.relative_to(path.parent.parent)}")
        found[path.name] = path
    return found


def valid_time(text):
    try:
        datetime.strptime(text, "%Y-%m-%dT%H:%M:%SZ")
    except ValueError:
        return False
    return True


def zip_names(mode, version):
    return sorted([f"{mode}-candidate-v{version}.zip", "fi.zip"])


def check_record(record, mode, candidate_id, receipt, zips_dir):
    """preserve.json：格式与键集合固定，每个值都与完成标记、候选 ID 及 zips/ 中的文件交叉核对。"""
    version, commit, run = parse_candidate_id(candidate_id)
    c.require(isinstance(record, dict) and set(record) == RECORD_KEYS,
              f"preserve.json keys differ: {sorted(record) if isinstance(record, dict) else type(record).__name__}")
    c.require(type(record["format"]) is int and record["format"] == RECORD_FORMAT, f"unknown preserve.json format {record['format']!r}")
    c.check_run_ref(record["run"], "preserve.json run")
    # 工具必须从冻结提交运行（save 已核对），因此 tool_commit 等于候选的提交。
    expected = {"mode": mode, "version": version, "commit": commit, "run": run, "candidate_id": candidate_id,
                "receipt": receipt, "tool_commit": commit}
    bad = sorted(key for key, value in expected.items() if type(record[key]) is not type(value) or record[key] != value)
    c.require(not bad, f"preserve.json does not match the preservation: {bad}")
    c.require(isinstance(record["saved_at"], str) and SAVED_AT.fullmatch(record["saved_at"]) and valid_time(record["saved_at"]),
              f"bad saved_at {record['saved_at']!r}")
    zips, names = record["zips"], zip_names(mode, version)
    c.require(isinstance(zips, dict) and sorted(zips) == names, f"preserve.json zips must name exactly {names}")
    present = regular_files(zips_dir)
    c.require(sorted(present) == names, f"zips/ differs from preserve.json: {sorted(present)}")
    ids = []
    for name in names:
        item = zips[name]
        c.require(isinstance(item, dict) and set(item) == ZIP_KEYS, f"preserve.json zips[{name}] keys differ")
        c.require(type(item["artifact_id"]) is int and item["artifact_id"] > 0, f"preserve.json zips[{name}] artifact_id is not positive")
        c.require(isinstance(item["sha256"], str) and c.HEX64.fullmatch(item["sha256"]) and item["digest"] == f"sha256:{item['sha256']}",
                  f"preserve.json zips[{name}] digest and sha256 differ")
        c.require(c.sha256_file(present[name]) == item["sha256"], f"zips/{name} differs from its artifact digest")
        ids.append(item["artifact_id"])
    c.require(len(set(ids)) == len(ids), "preserve.json names the same artifact twice")


def verify(final):
    """离线复核保存目录，返回 (模式, 候选 ID, 回执)。只严格校验回执覆盖的核心文件与 zips/，post-sign/ 不参与。"""
    final = Path(final)
    c.require(final.is_dir() and not final.is_symlink(), f"{final} is not a directory")
    c.require(re.fullmatch(r"[1-9]\d*\.[1-9]\d*", final.name), f"{final.name} is not a completed preservation directory")
    top = {p.name: p for p in final.iterdir()}
    c.require(set(CORE) <= set(top) <= set(CORE + OPTIONAL),
              f"top-level entries differ: missing {sorted(set(CORE) - set(top))}, extra {sorted(set(top) - set(CORE + OPTIONAL))}")
    for name, path in top.items():
        c.require(not path.is_symlink(), f"{name} is a symlink")
        c.require(path.is_dir() if name in ("release", "fi", "zips", "post-sign") else path.is_file(), f"{name} has the wrong type")
    record_bytes = (final / "preserve.json").read_bytes()
    try:
        marker = json.loads((final / "COMPLETE").read_text())
        record = json.loads(record_bytes.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise c.CandidateError(f"cannot read the completion marker or record: {error}") from error
    c.require(isinstance(marker, dict) and set(marker) == MARKER_KEYS and type(marker["format"]) is int
              and marker["format"] == COMPLETE_FORMAT and marker["mode"] in MODES
              and isinstance(marker["receipt"], str) and isinstance(marker["record_sha256"], str), f"bad completion marker {marker!r}")
    c.require(hashlib.sha256(record_bytes).hexdigest() == marker["record_sha256"], "preserve.json differs from the completion marker")
    mode, candidate_id = marker["mode"], marker["candidate_id"]
    version, commit, run = parse_candidate_id(candidate_id)
    c.require(final.name == f"{run['id']}.{run['attempt']}", f"{final.name} does not match {candidate_id}")
    regular_files(final / "release")
    fi = regular_files(final / "fi")
    data, entries = check_candidate_set(mode, version, run, final / "release",
                                        {name: c.sha256_file(path) for name, path in fi.items()})
    c.require(data["commit"] == commit, f"candidate result is for {data['commit']}, not {commit}")
    text = canonical_text(candidate_id, entries)
    c.require((final / "preservation.txt").read_bytes() == text, "preservation.txt differs from the files")
    receipt = receipt_of(text)
    c.require((final / "receipt").read_text() == receipt + "\n" and marker["receipt"] == receipt, "receipt differs from the files")
    check_record(record, mode, candidate_id, receipt, final / "zips")
    return mode, candidate_id, receipt


# 保存


def tool_commit(script=None):
    """脚本所在 worktree 的 HEAD；有任何未提交改动或未跟踪文件即拒绝，保证与工作流使用同一份代码。"""
    root = Path(script or __file__).resolve().parent

    def git(*args):
        result = subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True)
        c.require(result.returncode == 0, f"git {' '.join(args)} failed: {result.stderr.strip()}")
        return result.stdout

    status = git("status", "--porcelain", "--untracked-files=all")
    c.require(status == "", f"the tool's worktree has local changes:\n{status}")
    return git("rev-parse", "HEAD").strip()


class Gh(runs.Gh):
    def download(self, path, destination):
        with open(destination, "xb") as out:
            result = subprocess.run(["gh", "api", "-H", "X-GitHub-Api-Version: 2022-11-28", f"repos/{self.repo}/{path}"],
                                    stdout=out, stderr=subprocess.PIPE, text=False)
            out.flush()
            os.fsync(out.fileno())
        c.require(result.returncode == 0, f"gh api {path} failed: {result.stderr.decode(errors='replace').strip()}")


def single_job(jobs, name):
    matched = [j for j in jobs if j["name"] == name]
    c.require(len(matched) == 1, f"expected one {name} job, found {len(matched)}")
    return matched[0]


def check_run_state(mode, version, run_ref, run, jobs):
    """正式：运行进行中，candidate 与 preserve-digests 成功，attest 正在等待审批、没有执行机与步骤，未开始签发。
    演练：运行已成功完成，attest 的 sign 成功。"""
    title = f"{'release' if mode == 'release' else 'dry-run'} v{version}"
    c.require(run["path"] == runs.RELEASE_WORKFLOW and run["display_title"] == title,
              f"run {run['id']} is {run['path']} {run['display_title']!r}, not {title!r}")
    c.require(run["run_attempt"] == run_ref["attempt"],
              f"run {run['id']} is at attempt {run['run_attempt']}, not {run_ref['attempt']}; earlier artifacts are replaced")
    runs.check_job(jobs, CANDIDATE_JOB)
    runs.check_job(jobs, DIGESTS_JOB)
    attest = single_job(jobs, runs.ATTEST_JOB)
    if mode == "release":
        c.require(run["status"] != "completed", f"run {run['id']} is already {run['status']}/{run['conclusion']}")
        c.require(attest["status"] == "waiting" and not attest.get("runner_name") and not attest.get("steps"),
                  f"attest is not waiting for approval: {attest['status']}, runner {attest.get('runner_name')!r}")
        c.require(not runs.signing_started(jobs), "signing has already started")
    else:
        c.require(run["status"] == "completed" and run["conclusion"] == "success",
                  f"dry run {run['id']} is {run['status']}/{run['conclusion']}")
        runs.check_job(jobs, runs.ATTEST_JOB)
        signs = [s for s in attest.get("steps") or [] if s["name"] == runs.SIGN_STEP]
        c.require(len(signs) == 1 and (signs[0]["status"], signs[0]["conclusion"]) == ("completed", "success"),
                  "the dry run's sign step did not succeed")


def select_artifact(artifacts, name, run):
    matched = [a for a in artifacts if a["name"] == name]
    c.require(len(matched) == 1, f"expected one artifact {name}, found {len(matched)}")
    artifact = matched[0]
    c.require(type(artifact.get("id")) is int and artifact["id"] > 0, f"artifact {name} has no valid ID")
    c.require(not artifact["expired"], f"artifact {name} has expired")
    c.require(artifact["workflow_run"]["id"] == run["id"] and artifact["workflow_run"]["head_sha"] == run["head_sha"],
              f"artifact {name} belongs to another run")
    c.require(isinstance(artifact.get("digest"), str) and re.fullmatch(r"sha256:[0-9a-f]{64}", artifact["digest"]),
              f"artifact {name} has no SHA256 digest")
    return artifact


def candidate_place(names, release_dir):
    def place(name):
        c.require(name in names, f"unexpected file {name!r} in the candidate artifact")
        return release_dir / name
    return place


def fi_place(fi_dir):
    def place(name):
        if name == "fi.sha256":
            return None
        parts = PurePosixPath(name).parts
        c.require(len(parts) == 3 and parts[0] == "fi" and FI_DIR.fullmatch(parts[1]) and NAME.fullmatch(parts[2])
                  and not (fi_dir / parts[2]).exists(), f"unexpected file {name!r} in the fi artifact")
        return fi_dir / parts[2]
    return place


def fill_staging(gh, mode, version, run_ref, run, staging):
    """下载、解包、核对并写入除完成标记外的全部文件；返回 (候选 ID, 规范文本, 记录)。"""
    zips, release_dir, fi_dir = staging / "zips", staging / "release", staging / "fi"
    for directory in (zips, release_dir, fi_dir):
        directory.mkdir()
    artifacts = gh.items(f"actions/runs/{run['id']}/artifacts?per_page=100", "artifacts")
    names = {"candidate": f"{mode}-candidate-v{version}", "fi": "fi"}
    record = {"format": RECORD_FORMAT, "mode": mode, "version": version, "run": run_ref, "zips": {}}
    for key, name in names.items():
        artifact = select_artifact(artifacts, name, run)
        path = zips / f"{name}.zip"
        gh.download(f"actions/artifacts/{artifact['id']}/zip", path)
        c.require(f"sha256:{c.sha256_file(path)}" == artifact["digest"], f"artifact {name} zip differs from its API digest")
        record["zips"][path.name] = {"artifact_id": artifact["id"], "digest": artifact["digest"],
                                     "sha256": artifact["digest"].removeprefix("sha256:")}
    expected = release_names(mode, version, run_ref)
    extracted = extract(zips / f"{names['candidate']}.zip", candidate_place(set(expected), release_dir))
    c.require(sorted(extracted) == expected, f"candidate artifact files differ: {sorted(extracted)}")
    members = extract(zips / "fi.zip", fi_place(fi_dir))
    c.require("fi.sha256" in members, "fi artifact has no fi.sha256")
    listing = members.pop("fi.sha256")[0].decode("utf-8")
    fi_digests = check_fi_list(listing, {name.removeprefix("fi/"): digest for name, (_, digest) in members.items()})
    data, entries = check_candidate_set(mode, version, run_ref, release_dir, fi_digests)
    c.require(data["commit"] == run["head_sha"], f"candidate result is for {data['commit']}, not {run['head_sha']}")
    text = canonical_text(data["candidate_id"], entries)
    record.update(candidate_id=data["candidate_id"], commit=data["commit"])
    return data["candidate_id"], text, record, entries


def reread(staging, text, entries, record):
    """回读临时目录：每个文件的摘要与下载时一致，由文件重算的规范文本与写入的相同。"""
    current = {f"release/{p.name}": c.sha256_file(p) for p in (staging / "release").iterdir()}
    current.update({f"fi/{p.name}": c.sha256_file(p) for p in (staging / "fi").iterdir()})
    c.require(current == entries, "files changed while being saved")
    c.require(canonical_text(record["candidate_id"], current) == text == (staging / "preservation.txt").read_bytes(),
              "preservation.txt differs from the files")
    for name, item in record["zips"].items():
        c.require(f"sha256:{c.sha256_file(staging / 'zips' / name)}" == item["digest"], f"zips/{name} changed while being saved")


def save(gh, mode, version, run_ref, root, commit_of=tool_commit):
    """返回 save 的末行。前置条件见 check_run_state；已存在且有效的保存目录只复核并重新输出末行，不写入。"""
    c.require(mode in MODES, f"unknown mode {mode!r}")
    c.require(c.VERSION.fullmatch(version), f"bad version {version!r}")
    run = gh.get(f"actions/runs/{run_ref['id']}")
    commit = commit_of()
    c.require(run["head_sha"] == commit, f"run {run['id']} is for {run['head_sha']}; run this tool from that commit, not {commit}")
    check_run_state(mode, version, run_ref, run, gh.jobs(run_ref["id"], run_ref["attempt"]))
    expected_id = c.candidate_id(version, run["head_sha"], run_ref)
    parent = Path(root) / mode / version
    make_dir(parent)
    lock = open(parent / ".lock", "a")
    try:
        try:
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise c.CandidateError(f"another preservation holds {parent / '.lock'}") from error
        final = parent / f"{run_ref['id']}.{run_ref['attempt']}"
        if final.exists() or final.is_symlink():
            try:
                found_mode, found_id, receipt = verify(final)
            except (c.CandidateError, OSError) as error:
                raise c.CandidateError(f"{final} exists but is not a valid preservation; inspect it manually: {error}") from error
            c.require((found_mode, found_id) == (mode, expected_id), f"{final} holds {found_mode} {found_id}")
            return output_line(mode, found_id, receipt)
        staging = parent / f".incomplete-{run_ref['id']}.{run_ref['attempt']}-{secrets.token_hex(4)}"
        staging.mkdir()
        try:
            candidate_id, text, record, entries = fill_staging(gh, mode, version, run_ref, run, staging)
            c.require(candidate_id == expected_id, f"candidate id {candidate_id} is not {expected_id}")
            receipt = receipt_of(text)
            record.update(receipt=receipt, tool_commit=commit, saved_at=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()))
            write_file(staging / "preservation.txt", text)
            write_file(staging / "receipt", f"{receipt}\n".encode())
            record_bytes = (json.dumps(record, sort_keys=True, indent=2) + "\n").encode()
            write_file(staging / "preserve.json", record_bytes)
            reread(staging, text, entries, record)
            marker = {"format": COMPLETE_FORMAT, "mode": mode, "candidate_id": candidate_id, "receipt": receipt,
                      "record_sha256": hashlib.sha256(record_bytes).hexdigest()}
            write_file(staging / "COMPLETE", (json.dumps(marker, sort_keys=True) + "\n").encode())
            fsync_tree(staging)
            _crash_point("before-rename")
            os.rename(staging, final)
        except BaseException:
            shutil.rmtree(staging, ignore_errors=True)
            if staging.exists():
                # 父目录不可写时只能删掉其中的文件；原始错误照常抛出，残留目录不是证据（复核拒绝该前缀）。
                print(f"could not remove {staging}; it is incomplete and never evidence", file=sys.stderr)
            raise
        try:
            _crash_point("after-rename")
            fsync_dir(parent)
            _crash_point("after-parent-fsync")
            c.require(verify(final) == (mode, candidate_id, receipt), "the saved directory does not verify")
        except BaseException as error:
            failed = parent / f".failed-{run_ref['id']}.{run_ref['attempt']}-{secrets.token_hex(4)}"
            try:
                os.rename(final, failed)
            except OSError as move:
                raise c.CandidateError(f"saving failed after the rename ({error}) and {final} could not be moved aside "
                                       f"({move}); inspect it manually and do not approve") from error
            raise
        _crash_point("before-output")
        return output_line(mode, candidate_id, receipt)
    finally:
        lock.close()


# 工作流


def digests(mode, version, run_ref, candidate_dir, fi_root):
    fi_digests = fi_artifact_digests(fi_root)
    _, entries = check_candidate_set(mode, version, run_ref, candidate_dir, fi_digests)
    return json.dumps({k: v for k, v in sorted(entries.items()) if k.startswith("fi/")}, sort_keys=True, separators=(",", ":"))


def check_approvals(approvals, comment, environment, owner_id):
    """本 attempt 有任何拒绝即失败；备注逐字等于 comment 的记录恰好一条，且为所有者对该环境的批准。"""
    c.require(isinstance(approvals, list), "approvals must be a list")
    for item in approvals:
        c.require(isinstance(item, dict) and {"state", "comment", "user", "environments"} <= set(item)
                  and isinstance(item["user"], dict) and isinstance(item["environments"], list),
                  f"unexpected approval record {item!r}")
    c.require(not any(item["state"] == "rejected" for item in approvals), "this attempt has a rejected approval")
    matched = [item for item in approvals if item["comment"] == comment]
    c.require(len(matched) == 1, f"expected exactly one approval with comment {comment!r}, found {len(matched)}")
    item = matched[0]
    c.require(item["state"] == "approved", f"the matching approval is {item['state']}")
    names = [e.get("name") if isinstance(e, dict) else None for e in item["environments"]]
    c.require(names == [environment], f"the matching approval is for {names}, not [{environment!r}]")
    c.require(item["user"].get("id") == owner_id, f"the matching approval is by user {item['user'].get('id')}, not the owner")


def gate(gh, mode, environment, owner_id, version, run_ref, candidate_dir, fi_text):
    """attest「保全回执」：演练直接通过且不访问接口；正式运行依次核对候选、FI 摘要、环境、回执与审批。"""
    c.require(mode in MODES, f"unknown mode {mode!r}")
    if mode == "dry-run":
        return None
    c.require(environment, "the signing environment must be named")
    fi_digests = parse_fi_digests(fi_text, version)
    data, entries = check_candidate_set(mode, version, run_ref, candidate_dir, fi_digests)
    runs.check_signing_environment(gh, environment, owner_id)
    receipt = receipt_of(canonical_text(data["candidate_id"], entries))
    comment = approval_comment(data["candidate_id"], receipt)
    check_approvals(gh.get(f"actions/runs/{run_ref['id']}/approvals"), comment, environment, owner_id)
    return comment


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    s = sub.add_parser("save")
    s.add_argument("--mode", choices=MODES, required=True)
    s.add_argument("--repo", required=True)
    s.add_argument("--run", required=True, help="<run-id>.<attempt>")
    s.add_argument("--version", required=True)
    s.add_argument("--root", type=Path, required=True)
    v = sub.add_parser("verify")
    v.add_argument("directory", type=Path)
    d = sub.add_parser("digests")
    d.add_argument("--mode", choices=MODES, required=True)
    d.add_argument("--version", required=True)
    d.add_argument("--candidate-dir", type=Path, required=True)
    d.add_argument("--fi-dir", type=Path, required=True, help="the downloaded fi artifact: fi.sha256 and fi/")
    d.add_argument("--run-id", type=int, required=True)
    d.add_argument("--run-attempt", type=int, required=True)
    g = sub.add_parser("gate")
    g.add_argument("--mode", choices=MODES, required=True)
    g.add_argument("--environment", required=True)
    g.add_argument("--owner-id", type=int, required=True)
    g.add_argument("--repo", required=True)
    g.add_argument("--version", required=True)
    g.add_argument("--candidate-dir", type=Path, required=True)
    g.add_argument("--fi-digests", required=True)
    g.add_argument("--run-id", type=int, required=True)
    g.add_argument("--run-attempt", type=int, required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == "save":
            print(save(Gh(args.repo, retry=False), args.mode, args.version, runs.parse_ref(args.run), args.root), flush=True)
        elif args.command == "verify":
            print(output_line(*verify(args.directory)), flush=True)
        elif args.command == "digests":
            print(digests(args.mode, args.version, {"id": args.run_id, "attempt": args.run_attempt}, args.candidate_dir, args.fi_dir))
        else:
            comment = gate(runs.Gh(args.repo, retry=False), args.mode, args.environment, args.owner_id, args.version,
                           {"id": args.run_id, "attempt": args.run_attempt}, args.candidate_dir, args.fi_digests)
            print("dry run: no approval needed" if comment is None else f"approval matched: {comment}")
    except (c.CandidateError, OSError, ValueError, zipfile.BadZipFile) as error:
        print(f"preservation {args.command} failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
