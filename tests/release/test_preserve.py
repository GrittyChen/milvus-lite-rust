"""签发前保全（0022）：规范回执的字节形式，本机保存的前置条件、原子完成与崩溃恢复，目录复核，以及工作流的
FI 摘要与审批核对。任何偏差都必须拒绝；保存失败不得留下有效的最终目录或输出审批行。"""

import copy
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import stat
import subprocess
import sys
import zipfile

import pytest

import candidate as c
import preserve as p
import runs
from test_candidate import COMMIT, RUN, VERSION, build_env, identity, meta, stages
from test_runs_and_release_set import OWNER, SIGNING, probe, signing_environment

BUILD_DIRS = {"macosx_11_0_arm64": "build-macos-arm64", "macosx_11_0_x86_64": "build-macos-x86_64",
              "manylinux_2_28_aarch64": "build-linux-aarch64", "manylinux_2_28_x86_64": "build-linux-x86_64"}
CANDIDATE_ID = f"{VERSION}@{COMMIT}#{RUN['id']}.{RUN['attempt']}"


def sha(data):
    return hashlib.sha256(data).hexdigest()


# 规范回执

VECTOR = {
    **{f"release/{name}": digit * 64 for name, digit in zip(c.formal_names(VERSION), "12345")},
    f"release/candidate-{VERSION}-100-2.json": "6" * 64,
    "release/SHA256SUMS": "7" * 64,
    **{f"fi/milvus_lite_rust-{VERSION}-cp310-abi3-{tag}.whl": digit * 64 for tag, digit in zip(c.WHEEL_TAGS, "89ab")},
}
VECTOR_TEXT = (
    f"{CANDIDATE_ID}\n"
    f"{'8' * 64}  fi/milvus_lite_rust-1.2.0rc3-cp310-abi3-macosx_11_0_arm64.whl\n"
    f"{'9' * 64}  fi/milvus_lite_rust-1.2.0rc3-cp310-abi3-macosx_11_0_x86_64.whl\n"
    f"{'a' * 64}  fi/milvus_lite_rust-1.2.0rc3-cp310-abi3-manylinux_2_28_aarch64.whl\n"
    f"{'b' * 64}  fi/milvus_lite_rust-1.2.0rc3-cp310-abi3-manylinux_2_28_x86_64.whl\n"
    f"{'7' * 64}  release/SHA256SUMS\n"
    f"{'6' * 64}  release/candidate-1.2.0rc3-100-2.json\n"
    f"{'1' * 64}  release/milvus_lite_rust-1.2.0rc3-cp310-abi3-macosx_11_0_arm64.whl\n"
    f"{'2' * 64}  release/milvus_lite_rust-1.2.0rc3-cp310-abi3-macosx_11_0_x86_64.whl\n"
    f"{'3' * 64}  release/milvus_lite_rust-1.2.0rc3-cp310-abi3-manylinux_2_28_aarch64.whl\n"
    f"{'4' * 64}  release/milvus_lite_rust-1.2.0rc3-cp310-abi3-manylinux_2_28_x86_64.whl\n"
    f"{'5' * 64}  release/milvus_lite_rust-1.2.0rc3.tar.gz\n"
).encode()
# 由 shasum -a 256 对上面的字节独立计算，不经被测代码。
VECTOR_RECEIPT = "8c3b4e85ec873c468c9992446ee79ea85000e3d03bc4ea9061fc6addb5640764"


def test_canonical_text_vector():
    text = p.canonical_text(CANDIDATE_ID, VECTOR)
    assert text == VECTOR_TEXT
    assert not text.startswith(b"\xef\xbb\xbf") and b"\r" not in text and text.endswith(b"\n") and not text.endswith(b"\n\n")
    assert p.receipt_of(text) == VECTOR_RECEIPT
    assert p.canonical_text(CANDIDATE_ID, dict(reversed(list(VECTOR.items())))) == text


def test_same_name_formal_and_fi_wheels_stay_distinct():
    name = c.formal_names(VERSION)[0]
    assert f"release/{name}" in VECTOR and f"fi/{name}" in VECTOR
    lines = p.canonical_text(CANDIDATE_ID, VECTOR).decode().splitlines()
    assert len(lines) == 12 and len(set(lines)) == 12


def vector(**changes):
    entries = dict(VECTOR)
    for key, value in changes.items():
        if value is None:
            entries.pop(key)
        else:
            entries[key] = value
    return entries


FIRST = f"release/{c.formal_names(VERSION)[0]}"


@pytest.mark.parametrize("candidate_id, entries, message", [
    (CANDIDATE_ID, vector(**{FIRST: "A" * 64}), "bad digest"),
    (CANDIDATE_ID, vector(**{FIRST: "1" * 63}), "bad digest"),
    (CANDIDATE_ID, vector(**{FIRST: None}), "expected 7 release/ and 4 fi/"),
    (CANDIDATE_ID, {**vector(), "fi/extra.whl": "c" * 64}, "expected 7 release/ and 4 fi/"),
    (CANDIDATE_ID, {**vector(**{FIRST: None}), "release/a b": "1" * 64}, "bad canonical path"),
    (CANDIDATE_ID, {**vector(**{FIRST: None}), "release/a\nb": "1" * 64}, "bad canonical path"),
    (CANDIDATE_ID, {**vector(**{FIRST: None}), "release/sub/x": "1" * 64}, "bad canonical path"),
    (CANDIDATE_ID, {**vector(**{FIRST: None}), "other/x": "1" * 64}, "bad canonical path"),
    (CANDIDATE_ID + "\n", vector(), "bad candidate id"),
    (f"{VERSION}@{COMMIT}#100", vector(), "bad candidate id"),
    (f"{VERSION}@{COMMIT.upper()}#100.2", vector(), "bad candidate id"),
])
def test_canonical_text_rejects(candidate_id, entries, message):
    with pytest.raises(c.CandidateError, match=message):
        p.canonical_text(candidate_id, entries)


# 候选产物与假 Gh


def make_candidate(tmp_path, mode="release", run=RUN):
    """模拟 candidate job 的产物：release/ 七个文件，fi/ 为 fi 产物的下载布局（fi.sha256 与 fi/build-<平台>/）。"""
    base = tmp_path / f"src-{mode}"
    artifacts, fi_root = base / "formal", base / "fi-artifact"
    artifacts.mkdir(parents=True)
    for name in c.formal_names(VERSION):
        (artifacts / name).write_bytes(f"{mode} {name}".encode())
    listing = {}
    for tag in c.WHEEL_TAGS:
        wheel = fi_root / "fi" / BUILD_DIRS[tag] / p.fi_wheel_name(VERSION, tag)
        wheel.parent.mkdir(parents=True)
        wheel.write_bytes(f"fi {tag}".encode())
        listing[f"{BUILD_DIRS[tag]}/{wheel.name}"] = c.sha256_file(wheel)
    (fi_root / "fi.sha256").write_text(c.sums_text(listing))
    m = dict(meta(mode), run=dict(run))
    out = c.write(m, identity(), artifacts, fi_root / "fi", stages(), build_env(), base / "release")
    for name in c.formal_names(VERSION):
        shutil.copy2(artifacts / name, out.parent / name)
    return out.parent, fi_root


def make_zip(path, members):
    """members 为 {成员名: 字节}；值为 (字节, 外部属性) 时写入指定的 Unix 文件类型。"""
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, value in members.items():
            data, mode = value if isinstance(value, tuple) else (value, stat.S_IFREG | 0o644)
            info = zipfile.ZipInfo(name)
            info.external_attr = mode << 16
            archive.writestr(info, data)
    return path


def candidate_members(release_dir):
    return {path.name: path.read_bytes() for path in sorted(release_dir.iterdir())}


def fi_members(fi_root):
    members = {"fi.sha256": (fi_root / "fi.sha256").read_bytes()}
    for path in sorted((fi_root / "fi").rglob("*.whl")):
        members[path.relative_to(fi_root).as_posix()] = path.read_bytes()
    return members


def run_object(mode="release", **changes):
    title = f"{'release' if mode == 'release' else 'dry-run'} v{VERSION}"
    base = {"id": RUN["id"], "run_attempt": RUN["attempt"], "path": runs.RELEASE_WORKFLOW, "display_title": title,
            "head_sha": COMMIT, "status": "in_progress" if mode == "release" else "completed",
            "conclusion": None if mode == "release" else "success"}
    return {**base, **changes}


def job(name, status="completed", conclusion="success", steps=None, runner="runner"):
    return {"name": name, "status": status, "conclusion": conclusion, "steps": steps, "runner_name": runner}


def sign_step(status="completed", conclusion="success"):
    return {"name": runs.SIGN_STEP, "status": status, "conclusion": conclusion}


def jobs_for(mode="release"):
    if mode == "release":
        attest = job(runs.ATTEST_JOB, "waiting", None, steps=[], runner=None)
    else:
        attest = job(runs.ATTEST_JOB, steps=[sign_step()])
    return [job(p.CANDIDATE_JOB), job(p.DIGESTS_JOB), attest]


class SaveGh:
    """save 用的假 Gh：运行、jobs 与产物列表为可序列化的数据，下载从本地 zip 复制，便于在子进程中重建。"""

    def __init__(self, mode, root, run, jobs, artifacts, blobs):
        self.mode, self.root, self.run, self.jobs_list, self.artifacts, self.blobs = mode, root, run, jobs, artifacts, blobs
        self.calls = []

    def spec(self):
        return {"mode": self.mode, "root": str(self.root), "run": self.run, "jobs": self.jobs_list,
                "artifacts": self.artifacts, "blobs": {str(k): str(v) for k, v in self.blobs.items()}}

    @classmethod
    def from_spec(cls, spec):
        return cls(spec["mode"], Path(spec["root"]), spec["run"], spec["jobs"], spec["artifacts"],
                   {int(k): Path(v) for k, v in spec["blobs"].items()})

    def get(self, path):
        self.calls.append(path)
        c.require(path == f"actions/runs/{RUN['id']}", f"unexpected path {path}")
        return self.run

    def jobs(self, run_id, attempt):
        assert (run_id, attempt) == (RUN["id"], RUN["attempt"])
        return self.jobs_list

    def items(self, path, key):
        assert key == "artifacts" and path.startswith(f"actions/runs/{RUN['id']}/artifacts")
        return self.artifacts

    def download(self, path, destination):
        artifact_id = int(path.split("/")[2])
        with open(destination, "xb") as out:
            out.write(self.blobs[artifact_id].read_bytes())


def build_gh(tmp_path, mode="release", candidate_zip=None, fi_zip=None):
    release_dir, fi_root = make_candidate(tmp_path, mode)
    zips = tmp_path / f"zips-{mode}"
    zips.mkdir(exist_ok=True)
    candidate_zip = candidate_zip or make_zip(zips / "candidate.zip", candidate_members(release_dir))
    fi_zip = fi_zip or make_zip(zips / "fi.zip", fi_members(fi_root))
    artifacts = [
        {"id": 11, "name": f"{mode}-candidate-v{VERSION}", "expired": False, "digest": f"sha256:{c.sha256_file(candidate_zip)}",
         "workflow_run": {"id": RUN["id"], "head_sha": COMMIT}},
        {"id": 12, "name": "fi", "expired": False, "digest": f"sha256:{c.sha256_file(fi_zip)}",
         "workflow_run": {"id": RUN["id"], "head_sha": COMMIT}},
    ]
    return SaveGh(mode, tmp_path / "root", run_object(mode), jobs_for(mode), artifacts, {11: candidate_zip, 12: fi_zip})


def save(gh, mode=None):
    return p.save(gh, mode or gh.mode, VERSION, RUN, gh.root, commit_of=lambda: COMMIT)


def final_dir(gh, mode=None):
    return gh.root / (mode or gh.mode) / VERSION / f"{RUN['id']}.{RUN['attempt']}"


def leftovers(gh):
    parent = gh.root / gh.mode / VERSION
    return sorted(x.name for x in parent.iterdir() if x.name != ".lock") if parent.exists() else []


# save 正例


@pytest.mark.parametrize("mode, prefix", [("release", "preserved"), ("dry-run", "dry-run-receipt")])
def test_save_writes_the_frozen_layout(tmp_path, mode, prefix):
    gh = build_gh(tmp_path, mode)
    line = save(gh)
    final = final_dir(gh)
    assert leftovers(gh) == [final.name]
    assert sorted(x.name for x in final.iterdir()) == sorted(p.CORE)
    assert sorted(x.name for x in (final / "release").iterdir()) == p.release_names(mode, VERSION, RUN)
    assert sorted(x.name for x in (final / "fi").iterdir()) == sorted(p.fi_wheel_name(VERSION, t) for t in c.WHEEL_TAGS)
    receipt = (final / "receipt").read_text().strip()
    assert line == f"{prefix} {CANDIDATE_ID} {receipt}"
    assert p.receipt_of((final / "preservation.txt").read_bytes()) == receipt
    marker = json.loads((final / "COMPLETE").read_text())
    assert marker == {"format": 2, "mode": mode, "candidate_id": CANDIDATE_ID, "receipt": receipt,
                      "record_sha256": c.sha256_file(final / "preserve.json")}
    record = json.loads((final / "preserve.json").read_text())
    assert set(record) == p.RECORD_KEYS and record["format"] == 1
    assert (record["mode"], record["version"], record["commit"], record["run"], record["candidate_id"], record["receipt"],
            record["tool_commit"]) == (mode, VERSION, COMMIT, RUN, CANDIDATE_ID, receipt, COMMIT)
    assert record["zips"] == {
        f"{mode}-candidate-v{VERSION}.zip": {"artifact_id": 11, "digest": gh.artifacts[0]["digest"],
                                             "sha256": gh.artifacts[0]["digest"].removeprefix("sha256:")},
        "fi.zip": {"artifact_id": 12, "digest": gh.artifacts[1]["digest"], "sha256": gh.artifacts[1]["digest"].removeprefix("sha256:")}}
    assert p.verify(final) == (mode, CANDIDATE_ID, receipt)


def test_release_and_dry_run_use_separate_roots(tmp_path):
    release, dry = build_gh(tmp_path / "a", "release"), build_gh(tmp_path / "b", "dry-run")
    dry.root = release.root
    save(release)
    save(dry)
    assert final_dir(release).exists() and final_dir(dry).exists() and final_dir(release) != final_dir(dry)


# save 反证


def mutate_jobs(fn):
    def apply(gh):
        fn(gh.jobs_list)
    return apply


def attest_of(jobs):
    return [j for j in jobs if j["name"] == runs.ATTEST_JOB][0]


@pytest.mark.parametrize("mutate, message", [
    (mutate_jobs(lambda j: attest_of(j).update(runner_name="GitHub Actions 1")), "not waiting"),
    (mutate_jobs(lambda j: attest_of(j).update(steps=[{"name": "Set up job", "status": "completed", "conclusion": "success"}])),
     "not waiting"),
    (mutate_jobs(lambda j: attest_of(j).update(status="in_progress", runner_name="r", steps=[sign_step("in_progress", None)])),
     "not waiting"),
    (mutate_jobs(lambda j: attest_of(j).update(status="completed", conclusion="success", runner_name="r", steps=[sign_step()])),
     "not waiting"),
    (mutate_jobs(lambda j: attest_of(j).update(status="completed", conclusion="skipped")), "not waiting"),
    (mutate_jobs(lambda j: attest_of(j).update(status="completed", conclusion="failure", runner_name="")), "not waiting"),
    (mutate_jobs(lambda j: j.remove(attest_of(j))), "expected one attest"),
    (mutate_jobs(lambda j: j.append(job(runs.ATTEST_JOB, "waiting", None, steps=[], runner=None))), "expected one attest"),
    (mutate_jobs(lambda j: j[1].update(conclusion="failure")), "preserve-digests"),
    (mutate_jobs(lambda j: j.pop(1)), "preserve-digests"),
    (mutate_jobs(lambda j: j[0].update(conclusion="failure")), "candidate"),
    (lambda gh: gh.run.update(status="completed", conclusion="failure"), "already completed"),
    (lambda gh: gh.run.update(display_title=f"dry-run v{VERSION}"), "not 'release"),
    (lambda gh: gh.run.update(path=".github/workflows/ci.yml"), "not 'release"),
    (lambda gh: gh.run.update(run_attempt=3), "attempt 3, not 2"),
    (lambda gh: gh.run.update(head_sha="b" * 40), "run this tool from that commit"),
])
def test_release_save_requires_a_run_waiting_for_approval(tmp_path, mutate, message):
    gh = build_gh(tmp_path)
    mutate(gh)
    with pytest.raises(c.CandidateError, match=message):
        save(gh)
    assert leftovers(gh) == []


@pytest.mark.parametrize("mutate, message", [
    (lambda gh: gh.run.update(status="in_progress", conclusion=None), "dry run 100 is in_progress"),
    (lambda gh: gh.run.update(conclusion="failure"), "completed/failure"),
    (mutate_jobs(lambda j: attest_of(j).update(steps=[sign_step(conclusion="failure")], conclusion="failure")), "attest"),
    (mutate_jobs(lambda j: attest_of(j).update(steps=[sign_step(conclusion="skipped")])), "sign step did not succeed"),
    (mutate_jobs(lambda j: attest_of(j).update(steps=[])), "sign step did not succeed"),
    (lambda gh: gh.run.update(display_title=f"release v{VERSION}"), "not 'dry-run"),
])
def test_dry_run_save_requires_a_successful_dry_run(tmp_path, mutate, message):
    gh = build_gh(tmp_path, "dry-run")
    mutate(gh)
    with pytest.raises(c.CandidateError, match=message):
        save(gh)
    assert leftovers(gh) == []


def test_release_and_dry_run_candidates_cannot_be_mixed(tmp_path):
    # 正式模式拿到演练的运行与产物、演练模式拿到正式的运行与产物，都在运行核对处拒绝。
    with pytest.raises(c.CandidateError, match="not 'release"):
        save(build_gh(tmp_path / "a", "dry-run"), "release")
    with pytest.raises(c.CandidateError, match="not 'dry-run"):
        save(build_gh(tmp_path / "b", "release"), "dry-run")
    # 运行状态正确但产物是另一种模式：正式产物名找不到；同名产物中装着演练文件则文件集合不符。
    gh = build_gh(tmp_path / "c", "release")
    gh.artifacts[0]["name"] = f"dry-run-candidate-v{VERSION}"
    with pytest.raises(c.CandidateError, match="expected one artifact release-candidate"):
        save(gh)
    dry_dir, _ = make_candidate(tmp_path / "d", "dry-run")
    zip_path = make_zip(tmp_path / "d" / "mixed.zip", candidate_members(dry_dir))
    gh = build_gh(tmp_path / "e", "release", candidate_zip=zip_path)
    with pytest.raises(c.CandidateError, match="unexpected file 'dry-run"):
        save(gh)
    assert leftovers(gh) == []


def artifact(index, **changes):
    def apply(gh):
        gh.artifacts[index].update(changes)
    return apply


@pytest.mark.parametrize("mutate, message", [
    (lambda gh: gh.artifacts.pop(0), "expected one artifact release-candidate"),
    (lambda gh: gh.artifacts.pop(1), "expected one artifact fi"),
    (lambda gh: gh.artifacts.append(dict(gh.artifacts[1], id=13)), "expected one artifact fi"),
    (artifact(0, expired=True), "expired"),
    (artifact(1, workflow_run={"id": 99, "head_sha": COMMIT}), "another run"),
    (artifact(1, workflow_run={"id": RUN["id"], "head_sha": "b" * 40}), "another run"),
    (artifact(0, digest=None), "no SHA256 digest"),
    (artifact(0, id="11"), "no valid ID"),
    (artifact(0, id=0), "no valid ID"),
    (artifact(0, digest="md5:00"), "no SHA256 digest"),
    (artifact(1, digest="sha256:" + "0" * 64), "differs from its API digest"),
])
def test_save_rejects_artifact_metadata(tmp_path, mutate, message):
    gh = build_gh(tmp_path)
    mutate(gh)
    with pytest.raises(c.CandidateError, match=message):
        save(gh)
    assert leftovers(gh) == []


def edit_members(which, fn):
    """改写 zip 成员后重建 zip 并同步其 API digest，使拒绝只来自内容核对。"""
    def apply(tmp_path):
        release_dir, fi_root = make_candidate(tmp_path / "edit")
        members = candidate_members(release_dir) if which == "candidate" else fi_members(fi_root)
        fn(members)
        path = make_zip(tmp_path / f"edited-{which}.zip", members)
        return build_gh(tmp_path, **{f"{which}_zip": path})
    return apply


def rewrite_candidate(members, change):
    name = c.candidate_name("release", VERSION, RUN)
    data = json.loads(members[name])
    change(data)
    members[name] = (json.dumps(data, sort_keys=True, indent=2) + "\n").encode()


FORMAL = c.formal_names(VERSION)[0]
FI_WHEEL = f"fi/{BUILD_DIRS[c.WHEEL_TAGS[0]]}/{p.fi_wheel_name(VERSION, c.WHEEL_TAGS[0])}"


def relist(members):
    listing = {k.removeprefix("fi/"): sha(v) for k, v in members.items() if k.startswith("fi/")}
    members["fi.sha256"] = c.sums_text(listing).encode()


@pytest.mark.parametrize("build, message", [
    (edit_members("candidate", lambda m: m.update({"/abs": b"x"})), "unsafe entry"),
    (edit_members("candidate", lambda m: m.update({"../up": b"x"})), "unsafe entry"),
    (edit_members("candidate", lambda m: m.update({"a\\b": b"x"})), "unsafe entry"),
    (edit_members("candidate", lambda m: m.update({"link": (b"SHA256SUMS", stat.S_IFLNK | 0o777)})), "not a regular file"),
    (edit_members("candidate", lambda m: m.update({"extra.txt": b"x"})), "unexpected file 'extra.txt'"),
    (edit_members("candidate", lambda m: m.pop(FORMAL)), "candidate artifact files differ"),
    (edit_members("candidate", lambda m: m.update({f"pad{i}": b"" for i in range(10)})), "more than 16"),
    (edit_members("candidate", lambda m: m.update({FORMAL: b"other bytes"})), "formal files differ"),
    (edit_members("candidate", lambda m: rewrite_candidate(m, lambda d: d.update(extra=1))), "keys differ"),
    (edit_members("candidate", lambda m: m.update({"SHA256SUMS": b"0" * 64 + b"  x\n"})), "SHA256SUMS does not match"),
    (edit_members("fi", lambda m: m.update({"fi.sha256": b"0" * 64 + b"  build-x/y.whl\n"})), "fi.sha256 does not match"),
    (edit_members("fi", lambda m: (m.update({FI_WHEEL: b"other"}), relist(m))), "FI wheel digests differ"),
    (edit_members("fi", lambda m: (m.pop(FI_WHEEL), relist(m))), "FI wheels differ"),
    (edit_members("fi", lambda m: (m.update({"fi/build-x/milvus_lite_rust-9.9.9-cp310-abi3-x.whl": b"x"}), relist(m))),
     "FI wheels differ"),
    (edit_members("fi", lambda m: m.update({"fi/flat.whl": b"x"})), "unexpected file 'fi/flat.whl'"),
    (edit_members("fi", lambda m: m.pop("fi.sha256")), "no fi.sha256"),
])
def test_save_rejects_artifact_contents(tmp_path, build, message):
    gh = build(tmp_path)
    with pytest.raises(c.CandidateError, match=message):
        save(gh)
    assert leftovers(gh) == []


def test_save_rejects_a_candidate_from_another_attempt(tmp_path):
    other = {"id": RUN["id"], "attempt": 1}
    release_dir, fi_root = make_candidate(tmp_path / "old", run=other)
    members = candidate_members(release_dir)
    # 同名产物中是 attempt 1 的候选结果：文件名不在本 attempt 的期望集合内。
    gh = build_gh(tmp_path, candidate_zip=make_zip(tmp_path / "old.zip", members))
    with pytest.raises(c.CandidateError, match="unexpected file 'candidate-1.2.0rc3-100-1.json'"):
        save(gh)


def test_save_limits_expanded_bytes(tmp_path, monkeypatch):
    gh = build_gh(tmp_path)
    monkeypatch.setattr(p, "MAX_BYTES", 64)
    with pytest.raises(c.CandidateError, match="expands to more than 64 bytes"):
        save(gh)
    assert leftovers(gh) == []


def test_save_rejects_duplicate_zip_entries(tmp_path):
    release_dir, _ = make_candidate(tmp_path / "dup")
    path = tmp_path / "dup.zip"
    with pytest.warns(UserWarning):
        with zipfile.ZipFile(path, "w") as archive:
            for name, data in candidate_members(release_dir).items():
                archive.writestr(name, data)
            archive.writestr(FORMAL, b"again")
    with pytest.raises(c.CandidateError, match="duplicate entries"):
        save(build_gh(tmp_path, candidate_zip=path))


def git(root, *args):
    subprocess.run(["git", "-C", str(root), *args], check=True, capture_output=True)


def test_tool_commit_requires_a_clean_worktree(tmp_path):
    root = tmp_path / "repo"
    (root / "tests" / "release").mkdir(parents=True)
    script = root / "tests" / "release" / "preserve.py"
    script.write_text("# copy\n")
    git(root, "init", "-q")
    git(root, "-c", "user.email=t@example.com", "-c", "user.name=t", "add", ".")
    git(root, "-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "-m", "x")
    head = subprocess.run(["git", "-C", str(root), "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
    assert p.tool_commit(script) == head
    (root / "untracked.txt").write_text("x")
    with pytest.raises(c.CandidateError, match="local changes"):
        p.tool_commit(script)
    (root / "untracked.txt").unlink()
    script.write_text("# changed\n")
    with pytest.raises(c.CandidateError, match="local changes"):
        p.tool_commit(script)


# 原子保存与恢复


def test_existing_valid_directory_is_only_reverified(tmp_path):
    gh = build_gh(tmp_path)
    line = save(gh)
    final = final_dir(gh)
    before = {x: x.stat().st_mtime_ns for x in final.rglob("*")}
    assert save(gh) == line
    assert {x: x.stat().st_mtime_ns for x in final.rglob("*")} == before
    # 恢复也要求运行仍可审批：attest 已开始后不再输出审批行。
    attest_of(gh.jobs_list).update(status="in_progress", runner_name="r", steps=[sign_step("in_progress", None)])
    with pytest.raises(c.CandidateError, match="not waiting"):
        save(gh)


@pytest.mark.parametrize("damage", [
    lambda d: (d / "COMPLETE").unlink(),
    lambda d: (d / "release" / FORMAL).write_bytes(b"changed"),
    lambda d: (d / "fi" / p.fi_wheel_name(VERSION, c.WHEEL_TAGS[0])).unlink(),
])
def test_existing_invalid_directory_is_refused_and_left_alone(tmp_path, damage):
    gh = build_gh(tmp_path)
    save(gh)
    final = final_dir(gh)
    damage(final)
    snapshot = sorted((x.relative_to(final), x.read_bytes() if x.is_file() else None) for x in final.rglob("*"))
    with pytest.raises(c.CandidateError, match="not a valid preservation; inspect it manually"):
        save(gh)
    assert sorted((x.relative_to(final), x.read_bytes() if x.is_file() else None) for x in final.rglob("*")) == snapshot


def test_lock_held_by_another_process_is_refused(tmp_path):
    gh = build_gh(tmp_path)
    parent = gh.root / "release" / VERSION
    parent.mkdir(parents=True)
    with open(parent / ".lock", "a") as lock:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
        with pytest.raises(c.CandidateError, match="another preservation holds"):
            save(gh)
    assert leftovers(gh) == []


class Points:
    def __init__(self):
        self.seen = []

    def __call__(self, name):
        self.seen.append(name)


def count_fsyncs(monkeypatch, gh):
    calls, points = [], Points()
    real = os.fsync

    def counting(fd):
        calls.append(len(points.seen))
        return real(fd)
    monkeypatch.setattr(p.os, "fsync", counting)
    monkeypatch.setattr(p, "_crash_point", points)
    save(gh)
    monkeypatch.setattr(p.os, "fsync", real)
    return calls


def test_every_fsync_failure_leaves_no_valid_directory(tmp_path, monkeypatch):
    calls = count_fsyncs(monkeypatch, build_gh(tmp_path / "count"))
    # calls[k] 为第 k 次 fsync 之前经过的崩溃点数：0 表示 rename 之前，之后为 rename 后的父目录 fsync。
    assert calls.count(0) > 20 and calls[-1] >= 1
    real = os.fsync
    for k in range(len(calls)):
        gh = build_gh(tmp_path / f"k{k}")
        state = {"n": 0}

        def failing(fd, k=k):
            state["n"] += 1
            if state["n"] == k + 1:
                raise OSError(5, "injected fsync failure")
            return real(fd)
        monkeypatch.setattr(p.os, "fsync", failing)
        with pytest.raises(OSError, match="injected"):
            save(gh)
        monkeypatch.setattr(p.os, "fsync", real)
        left = leftovers(gh)
        assert not final_dir(gh).exists(), k
        if calls[k] == 0:
            assert left == [], (k, left)
        else:
            assert len(left) == 1 and left[0].startswith(".failed-100.2-"), (k, left)


def test_write_and_rename_failures_leave_nothing(tmp_path, monkeypatch):
    real_write, real_rename = p.write_file, os.rename
    for name in ("preservation.txt", "receipt", "preserve.json", "COMPLETE"):
        gh = build_gh(tmp_path / name)

        def failing(path, data, name=name):
            if Path(path).name == name:
                raise OSError(28, "injected write failure")
            return real_write(path, data)
        monkeypatch.setattr(p, "write_file", failing)
        with pytest.raises(OSError, match="injected"):
            save(gh)
        monkeypatch.setattr(p, "write_file", real_write)
        assert leftovers(gh) == [], name
    gh = build_gh(tmp_path / "rename")

    def failing_rename(src, dst):
        raise OSError(18, "injected rename failure")
    monkeypatch.setattr(p.os, "rename", failing_rename)
    with pytest.raises(OSError, match="injected"):
        save(gh)
    monkeypatch.setattr(p.os, "rename", real_rename)
    assert leftovers(gh) == []


def test_leftover_staging_is_reported(tmp_path, monkeypatch, capsys):
    # 真实情形是父目录在 rename 前变为不可写（本机真实文件系统故障注入已验证），这里以清理失败模拟。
    gh = build_gh(tmp_path)
    monkeypatch.setattr(p.shutil, "rmtree", lambda path, ignore_errors=False: None)
    monkeypatch.setattr(p.os, "rename", lambda src, dst: (_ for _ in ()).throw(OSError(13, "injected rename failure")))
    with pytest.raises(OSError, match="injected"):
        save(gh)
    left = leftovers(gh)
    assert len(left) == 1 and left[0].startswith(".incomplete-") and not final_dir(gh).exists()
    assert "could not remove" in capsys.readouterr().err
    with pytest.raises(c.CandidateError, match="not a completed preservation directory"):
        p.verify(gh.root / "release" / VERSION / left[0])


def test_files_changed_before_the_marker_are_detected(tmp_path, monkeypatch):
    gh = build_gh(tmp_path)
    real = p.reread

    def corrupting(staging, *args):
        (staging / "release" / FORMAL).write_bytes(b"changed on disk")
        return real(staging, *args)
    monkeypatch.setattr(p, "reread", corrupting)
    with pytest.raises(c.CandidateError, match="files changed while being saved"):
        save(gh)
    assert leftovers(gh) == []


def test_failure_after_rename_moves_the_directory_aside(tmp_path, monkeypatch):
    gh = build_gh(tmp_path / "verify")
    real_verify = p.verify

    def failing_verify(final):
        raise c.CandidateError("injected verify failure")
    monkeypatch.setattr(p, "verify", failing_verify)
    with pytest.raises(c.CandidateError, match="injected"):
        save(gh)
    monkeypatch.setattr(p, "verify", real_verify)
    left = leftovers(gh)
    assert not final_dir(gh).exists() and len(left) == 1 and left[0].startswith(".failed-")
    with pytest.raises(c.CandidateError, match="not a completed preservation directory"):
        p.verify(gh.root / "release" / VERSION / left[0])
    # 移出也失败：报错并要求人工检查，仍不输出审批行。
    gh = build_gh(tmp_path / "stuck")
    real_rename = os.rename
    renames = {"n": 0}

    def second_rename_fails(src, dst):
        renames["n"] += 1
        if renames["n"] == 2:
            raise OSError(13, "injected")
        return real_rename(src, dst)
    monkeypatch.setattr(p, "verify", failing_verify)
    monkeypatch.setattr(p.os, "rename", second_rename_fails)
    with pytest.raises(c.CandidateError, match="could not be moved aside.*do not approve"):
        save(gh)


CHILD = """
import json, os, signal, sys
sys.path.insert(0, sys.argv[1])
import preserve, test_preserve as t
point = sys.argv[3]
preserve._crash_point = lambda name: os.kill(os.getpid(), signal.SIGKILL) if name == point else None
gh = t.SaveGh.from_spec(json.loads(open(sys.argv[2]).read()))
print(t.save(gh), flush=True)
"""


@pytest.mark.parametrize("point", ["before-rename", "after-rename", "after-parent-fsync", "before-output"])
def test_sigkill_at_each_point_is_recoverable(tmp_path, point):
    gh = build_gh(tmp_path)
    spec = tmp_path / "spec.json"
    spec.write_text(json.dumps(gh.spec()))
    child = subprocess.run([sys.executable, "-c", CHILD, str(Path(__file__).parent), str(spec), point],
                           capture_output=True, text=True, timeout=120)
    assert child.returncode == -signal.SIGKILL, child.stderr
    assert child.stdout == ""
    final = final_dir(gh)
    if point == "before-rename":
        left = leftovers(gh)
        assert not final.exists() and len(left) == 1 and left[0].startswith(".incomplete-100.2-")
        with pytest.raises(c.CandidateError):
            p.verify(gh.root / "release" / VERSION / left[0])
        line = save(gh)  # 锁已随进程释放，重新保存正常完成。
        assert p.verify(final)[2] in line
    else:
        assert leftovers(gh) == [final.name]
        mode, candidate_id, receipt = p.verify(final)
        before = {x: x.stat().st_mtime_ns for x in final.rglob("*")}
        assert save(gh) == f"preserved {candidate_id} {receipt}"
        assert {x: x.stat().st_mtime_ns for x in final.rglob("*")} == before


# verify 与目录布局


@pytest.fixture
def saved(tmp_path):
    gh = build_gh(tmp_path)
    save(gh)
    return final_dir(gh)


def test_post_sign_material_does_not_change_the_receipt(saved):
    before = p.verify(saved)
    (saved / "post-sign" / "rekor").mkdir(parents=True)
    (saved / "post-sign" / "attestation.jsonl").write_text("{}\n")
    (saved / "post-sign" / "rekor" / "entry.json").write_text("{}\n")
    assert p.verify(saved) == before
    (saved / "post-sign" / "attestation.jsonl").write_text("corrupted")
    (saved / "post-sign" / "rekor" / "entry.json").unlink()
    assert p.verify(saved) == before


@pytest.mark.parametrize("damage, message", [
    (lambda d: (d / "notes.txt").write_text("x"), "extra"),
    (lambda d: (d / "release" / "extra.txt").write_text("x"), "candidate files differ"),
    (lambda d: (d / "fi" / "extra.whl").write_text("x"), "FI wheels differ"),
    (lambda d: (d / "zips" / "extra.zip").write_text("x"), "zips/ differs from preserve.json"),
    (lambda d: next((d / "zips").iterdir()).write_bytes(b"changed"), "differs from its artifact digest"),
    (lambda d: (d / "release" / FORMAL).write_bytes(b"changed"), "formal files differ"),
    (lambda d: (d / "release" / FORMAL).unlink(), "candidate files differ"),
    (lambda d: (d / "preservation.txt").write_text("x\n"), "preservation.txt differs"),
    (lambda d: (d / "receipt").write_text("0" * 64 + "\n"), "receipt differs"),
    (lambda d: (d / "COMPLETE").write_text(json.dumps({**json.loads((d / "COMPLETE").read_text()), "mode": "dry-run"})),
     "candidate files differ"),
    (lambda d: (d / "preserve.json").write_text((d / "preserve.json").read_text() + " "), "differs from the completion marker"),
    (lambda d: (d / "COMPLETE").write_text("{"), "cannot read"),
    (lambda d: (d / "COMPLETE").unlink(), "missing"),
    (lambda d: (d / "post-sign").write_text("x"), "wrong type"),
    (lambda d: ((d / "release" / FORMAL).unlink(), (d / "release" / FORMAL).symlink_to(d / "receipt")), "unexpected entry"),
])
def test_verify_rejects(saved, damage, message):
    damage(saved)
    with pytest.raises(c.CandidateError, match=message):
        p.verify(saved)


def rebind(final, change):
    """改写 preserve.json 后同步完成标记中的 record_sha256：拒绝只能来自字段本身的校验。"""
    record = json.loads((final / "preserve.json").read_text())
    change(record)
    data = (json.dumps(record, sort_keys=True, indent=2) + "\n").encode()
    (final / "preserve.json").write_bytes(data)
    marker = json.loads((final / "COMPLETE").read_text())
    marker["record_sha256"] = sha(data)
    (final / "COMPLETE").write_text(json.dumps(marker, sort_keys=True) + "\n")


CANDIDATE_ZIP = f"release-candidate-v{VERSION}.zip"


def set_zip(name, **changes):
    return lambda r: r["zips"][name].update(changes)


@pytest.mark.parametrize("change, message", [
    (lambda r: r.update(tool_commit="0" * 40), r"does not match the preservation: \['tool_commit'\]"),
    (lambda r: r.update(version="9.9.9"), r"\['version'\]"),
    (lambda r: r.update(run={"id": 999, "attempt": 7}), r"\['run'\]"),
    (lambda r: r.update(run={"id": "100", "attempt": 2}), "run id and attempt must be positive integers"),
    (lambda r: r.update(run=[100, 2]), "run must be"),
    (lambda r: r.update(commit="0" * 40), r"\['commit', 'tool_commit'\]|\['commit'\]"),
    (lambda r: r.update(mode="dry-run"), r"\['mode'\]"),
    (lambda r: r.update(candidate_id=f"{VERSION}@{COMMIT}#100.1"), r"\['candidate_id'\]"),
    (lambda r: r.update(receipt="0" * 64), r"\['receipt'\]"),
    (lambda r: r.pop("saved_at"), "keys differ"),
    (lambda r: r.update(saved_at="yesterday"), "bad saved_at"),
    (lambda r: r.update(saved_at=None), "bad saved_at"),
    (lambda r: r.update(saved_at="9999-99-99T99:99:99Z"), "bad saved_at"),
    (lambda r: r.update(saved_at="2026-02-30T12:00:00Z"), "bad saved_at"),
    (lambda r: r.update(extra=1), "keys differ"),
    (lambda r: r.update(format=2), "unknown preserve.json format"),
    (lambda r: r.update(format="1"), "unknown preserve.json format"),
    (lambda r: r.update(zips=[]), "must name exactly"),
    (lambda r: r.update(zips="x"), "must name exactly"),
    (lambda r: r["zips"].pop("fi.zip"), "must name exactly"),
    (lambda r: r["zips"].update({"other.zip": r["zips"]["fi.zip"]}), "must name exactly"),
    (lambda r: r["zips"].update({"fi.zip": "x"}), "keys differ"),
    (lambda r: r["zips"]["fi.zip"].pop("sha256"), "keys differ"),
    (lambda r: r["zips"]["fi.zip"].update(extra=1), "keys differ"),
    (set_zip("fi.zip", artifact_id="12"), "artifact_id is not positive"),
    (set_zip("fi.zip", artifact_id=0), "artifact_id is not positive"),
    (lambda r: r["zips"]["fi.zip"].update(artifact_id=r["zips"][CANDIDATE_ZIP]["artifact_id"]), "same artifact twice"),
    (set_zip("fi.zip", digest="md5:00"), "digest and sha256 differ"),
    (set_zip("fi.zip", sha256="0" * 64, digest="sha256:" + "0" * 64), "differs from its artifact digest"),
    (set_zip("fi.zip", sha256="0" * 64), "digest and sha256 differ"),
    (set_zip("fi.zip", sha256=None), "digest and sha256 differ"),
])
def test_verify_rejects_record_tampering(saved, change, message):
    before = (saved / "receipt").read_text()
    rebind(saved, change)
    with pytest.raises(c.CandidateError, match=message):
        p.verify(saved)
    assert (saved / "receipt").read_text() == before


def test_verify_rejects_a_record_that_is_not_an_object(saved):
    data = b"[]\n"
    (saved / "preserve.json").write_bytes(data)
    marker = json.loads((saved / "COMPLETE").read_text())
    marker["record_sha256"] = sha(data)
    (saved / "COMPLETE").write_text(json.dumps(marker))
    with pytest.raises(c.CandidateError, match="keys differ: list"):
        p.verify(saved)


def test_artifact_id_change_alone_is_detected(saved):
    # 只改 artifact ID（摘要不变）也须被发现：未同步完成标记时由 record_sha256 拒绝。
    record = json.loads((saved / "preserve.json").read_text())
    record["zips"]["fi.zip"]["artifact_id"] = 99
    (saved / "preserve.json").write_text(json.dumps(record, sort_keys=True, indent=2) + "\n")
    with pytest.raises(c.CandidateError, match="differs from the completion marker"):
        p.verify(saved)


@pytest.mark.parametrize("change, message", [
    (lambda m: m.update(format=1), "bad completion marker"),
    (lambda m: m.pop("record_sha256"), "bad completion marker"),
    (lambda m: m.update(record_sha256=7), "bad completion marker"),
    (lambda m: m.update(candidate_id=7), "bad candidate id"),
    (lambda m: m.update(receipt=None), "bad completion marker"),
    (lambda m: m.update(extra=1), "bad completion marker"),
])
def test_verify_rejects_malformed_markers(saved, change, message):
    marker = json.loads((saved / "COMPLETE").read_text())
    change(marker)
    (saved / "COMPLETE").write_text(json.dumps(marker))
    with pytest.raises(c.CandidateError, match=message):
        p.verify(saved)


@pytest.mark.parametrize("name", [".incomplete-100.2-abcd", ".failed-100.2-abcd", "100.1", "x"])
def test_verify_rejects_directories_that_are_not_completed_preservations(saved, name):
    other = saved.parent / name
    shutil.copytree(saved, other)
    with pytest.raises(c.CandidateError):
        p.verify(other)


# digests


@pytest.mark.parametrize("mode", ["release", "dry-run"])
def test_digests(tmp_path, mode):
    release_dir, fi_root = make_candidate(tmp_path, mode)
    out = json.loads(p.digests(mode, VERSION, RUN, release_dir, fi_root))
    assert out == {f"fi/{path.name}": c.sha256_file(path) for path in (fi_root / "fi").rglob("*.whl")}
    assert p.parse_fi_digests(json.dumps(out), VERSION) == {k.removeprefix("fi/"): v for k, v in out.items()}


def first_wheel(fi_root):
    return sorted((fi_root / "fi").rglob("*.whl"))[0]


def rewrite_listing(fi_root):
    listing = {x.relative_to(fi_root / "fi").as_posix(): c.sha256_file(x) for x in (fi_root / "fi").rglob("*.whl")}
    (fi_root / "fi.sha256").write_text(c.sums_text(listing))


@pytest.mark.parametrize("damage, message", [
    (lambda r, f: first_wheel(f).unlink(), "fi.sha256 does not match"),
    (lambda r, f: (first_wheel(f).unlink(), rewrite_listing(f)), "FI wheels differ"),
    (lambda r, f: ((f / "fi" / "build-extra").mkdir(), (f / "fi" / "build-extra" / "x.whl").write_bytes(b"x"), rewrite_listing(f)),
     "FI wheels differ"),
    (lambda r, f: first_wheel(f).write_bytes(b"changed"), "fi.sha256 does not match"),
    (lambda r, f: (first_wheel(f).write_bytes(b"changed"), rewrite_listing(f)), "FI wheel digests differ"),
    (lambda r, f: (f / "fi" / "loose.whl").write_bytes(b"x"), "unexpected entry"),
    (lambda r, f: (f / "fi.sha256").unlink(), "layout differs"),
    (lambda r, f: (r / FORMAL).write_bytes(b"changed"), "formal files differ"),
])
def test_digests_rejects(tmp_path, damage, message):
    release_dir, fi_root = make_candidate(tmp_path)
    damage(release_dir, fi_root)
    with pytest.raises(c.CandidateError, match=message):
        p.digests("release", VERSION, RUN, release_dir, fi_root)


def test_digests_rejects_a_candidate_from_another_attempt(tmp_path):
    release_dir, fi_root = make_candidate(tmp_path)
    with pytest.raises(c.CandidateError, match="candidate files differ"):
        p.digests("release", VERSION, {"id": RUN["id"], "attempt": 3}, release_dir, fi_root)


# gate


class GateGh:
    def __init__(self, approvals, env=None):
        self.responses = {f"environments/{SIGNING}": env or signing_environment(),
                          f"actions/runs/{RUN['id']}/approvals": approvals}
        self.calls = []

    def get(self, path):
        self.calls.append(path)
        if path not in self.responses:
            raise c.CandidateError(f"gh api {path} failed: gh: Not Found (HTTP 404)")
        value = self.responses[path]
        if isinstance(value, Exception):
            raise value
        return copy.deepcopy(value)


@pytest.fixture
def gate_inputs(tmp_path):
    release_dir, fi_root = make_candidate(tmp_path)
    return release_dir, p.digests("release", VERSION, RUN, release_dir, fi_root)


def comment_for(release_dir, fi_text):
    _, entries = p.check_candidate_set("release", VERSION, RUN, release_dir, p.parse_fi_digests(fi_text, VERSION))
    return p.approval_comment(CANDIDATE_ID, p.receipt_of(p.canonical_text(CANDIDATE_ID, entries)))


def approval(comment, state="approved", env=SIGNING, user=OWNER):
    """以探针的批准记录为基础，换掉备注、状态、环境与用户。"""
    item = probe("approvals-approved.json")[0]
    item.update(comment=comment, state=state)
    item["environments"] = [dict(item["environments"][0], name=name) for name in ([env] if isinstance(env, str) else env)]
    item["user"]["id"] = user
    return item


def run_gate(gh, release_dir, fi_text, run=RUN, mode="release"):
    return p.gate(gh, mode, SIGNING, OWNER, VERSION, run, release_dir, fi_text)


def test_gate_accepts_the_matching_approval(gate_inputs):
    release_dir, fi_text = gate_inputs
    comment = comment_for(release_dir, fi_text)
    gh = GateGh([approval(comment)])
    assert run_gate(gh, release_dir, fi_text) == comment
    assert gh.calls == [f"environments/{SIGNING}", f"actions/runs/{RUN['id']}/approvals"]


def test_gate_matches_the_line_printed_by_save(tmp_path):
    gh = build_gh(tmp_path)
    line = save(gh)
    final = final_dir(gh)
    fi_text = json.dumps({f"fi/{x.name}": c.sha256_file(x) for x in (final / "fi").iterdir()})
    assert run_gate(GateGh([approval(line)]), final / "release", fi_text) == line


def test_dry_run_gate_makes_no_api_calls(gate_inputs):
    release_dir, fi_text = gate_inputs
    gh = GateGh([])
    assert run_gate(gh, release_dir, "not even json", mode="dry-run") is None
    assert gh.calls == []


def replace_receipt(comment):
    return comment[:-64] + "0" * 64


@pytest.mark.parametrize("approvals, message", [
    (lambda m: [], "found 0"),
    (lambda m: [approval(replace_receipt(m))], "found 0"),
    (lambda m: [approval(m.replace(f"#{RUN['id']}.{RUN['attempt']}", f"#{RUN['id']}.1"))], "found 0"),
    (lambda m: [approval(m.replace("preserved ", "dry-run-receipt ", 1))], "found 0"),
    (lambda m: [approval(m.replace(" ", "  ", 1))], "found 0"),
    (lambda m: [approval(m + "\n")], "found 0"),
    (lambda m: [approval(m.upper())], "found 0"),
    (lambda m: [approval(m), approval(m)], "found 2"),
    (lambda m: [approval(m, state="rejected")], "rejected approval"),
    (lambda m: [approval(m), probe("approvals-rejected.json")[0]], "rejected approval"),
    (lambda m: [approval(m, env="dry-run-signing")], "not \\['release-signing'\\]"),
    (lambda m: [approval(m, env=[])], "not \\['release-signing'\\]"),
    (lambda m: [approval(m, env=[SIGNING, "other"])], "not \\['release-signing'\\]"),
    (lambda m: [approval(m, user=42)], "not the owner"),
    (lambda m: [{"comment": m, "state": "approved"}], "unexpected approval record"),
    (lambda m: {"comment": m}, "must be a list"),
    (lambda m: c.CandidateError("gh api approvals failed: HTTP 502"), "HTTP 502"),
])
def test_gate_rejects_approvals(gate_inputs, approvals, message):
    release_dir, fi_text = gate_inputs
    with pytest.raises(c.CandidateError, match=message):
        run_gate(GateGh(approvals(comment_for(release_dir, fi_text))), release_dir, fi_text)


@pytest.mark.parametrize("env, message", [
    (dict(signing_environment(), can_admins_bypass=True), "bypass"),
    (dict(signing_environment(), protection_rules=[]), "exactly one required reviewers rule"),
])
def test_gate_rechecks_the_environment(gate_inputs, env, message):
    release_dir, fi_text = gate_inputs
    gh = GateGh([approval(comment_for(release_dir, fi_text))], env)
    with pytest.raises(c.CandidateError, match=message):
        run_gate(gh, release_dir, fi_text)
    assert gh.calls == [f"environments/{SIGNING}"]
    gh = GateGh([approval(comment_for(release_dir, fi_text))])
    del gh.responses[f"environments/{SIGNING}"]
    with pytest.raises(c.CandidateError, match="HTTP 404"):
        run_gate(gh, release_dir, fi_text)


def test_gate_rejects_candidate_and_fi_digest_mismatches(gate_inputs):
    release_dir, fi_text = gate_inputs
    gh = GateGh([approval(comment_for(release_dir, fi_text))])
    with pytest.raises(c.CandidateError, match="candidate files differ"):
        run_gate(gh, release_dir, fi_text, run={"id": RUN["id"], "attempt": 3})
    digests = json.loads(fi_text)
    first = sorted(digests)[0]
    for bad, message in [({k: v for k, v in digests.items() if k != first}, "must name exactly"),
                         ({**digests, first: "0" * 64}, "FI wheel digests differ"),
                         ({**digests, first: "X"}, "must be SHA256"), ("[]", "must name exactly"), ("{", "not JSON")]:
        text = bad if isinstance(bad, str) else json.dumps(bad)
        with pytest.raises(c.CandidateError, match=message):
            run_gate(gh, release_dir, text)
    assert gh.calls == []


# 命令行


def test_cli_verify_and_digests(saved, tmp_path, capsys):
    assert p.main(["verify", str(saved)]) == 0
    assert capsys.readouterr().out.startswith(f"preserved {CANDIDATE_ID} ")
    (saved / "receipt").write_text("x\n")
    assert p.main(["verify", str(saved)]) == 1
    assert "preservation verify failed" in capsys.readouterr().err
    release_dir, fi_root = make_candidate(tmp_path / "cli")
    assert p.main(["digests", "--mode", "release", "--version", VERSION, "--candidate-dir", str(release_dir),
                   "--fi-dir", str(fi_root), "--run-id", "100", "--run-attempt", "2"]) == 0
    assert len(json.loads(capsys.readouterr().out)) == 4
