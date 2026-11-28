"""PyPI 上传工具的反证（0042）：版本守卫（台账、流程测试版本、冻结 tag 是否纳入 PyPI）、索引上的现有文件
只能属于本组且同名同哈希、上传 job 按 verify job 的摘要核对文件、wheel 在前 sdist 最后、可见性等待的上限与
立即失败条件、工作流的身份与权限约束。"""

import hashlib
import io
import json
from pathlib import Path
import re
import sys
import urllib.error

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / ".github" / "ci"))

import pypi_release as pp  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
VERSION = "0.9.0rc2"
NAMES = pp.c.formal_names(VERSION)
PYPI = f"python -m pip install --only-binary milvus-lite-rust milvus-lite-rust=={VERSION}"


def release_dir(tmp_path):
    root = tmp_path / "release"
    root.mkdir(parents=True)
    sums = {}
    for name in NAMES:
        body = name.encode()
        (root / name).write_bytes(body)
        sums[name] = hashlib.sha256(body).hexdigest()
    # 真实 Release 的 SHA256SUMS 还覆盖候选结果 JSON（共六行），它不上传，也不应出现在待传或等待的集合中。
    candidate = f"candidate-{VERSION}-1-1.json"
    (root / candidate).write_text("{}")
    (root / "SHA256SUMS").write_text(pp.c.sums_text({**sums, candidate: hashlib.sha256(b"{}").hexdigest()}))
    return root, sums


def frozen_tree(tmp_path, export=True, readme=PYPI):
    root = tmp_path / "frozen"
    (root / "tests" / "packaging").mkdir(parents=True)
    body = "def release_install_commands(version):\n    return set()\n"
    if export:
        body += ("def pypi_install_command(version):\n"
                 "    return f'python -m pip install --only-binary milvus-lite-rust milvus-lite-rust=={version}'\n")
    (root / "tests" / "packaging" / "check_artifacts.py").write_text(body)
    (root / "README.md").write_text(f"# t\n\n```bash\n{readme}\n```\n")
    return root


def ledger_file(tmp_path, outcome="released", version=VERSION):
    tmp_path.mkdir(parents=True, exist_ok=True)
    path = tmp_path / "ledger.json"
    path.write_text(json.dumps({"format": 1, "versions": [
        {"version": version, "run_id": 1, "run_attempt": 1, "outcome": outcome}]}))
    return path


class Index:
    """假的索引 JSON API：files 为 None 时返回 404。"""

    def __init__(self, target, files=None, status=None):
        self.target, self.files, self.status, self.urls = target, files, status, []

    def __call__(self, request, timeout):
        self.urls.append(request.full_url)
        if self.status is not None:
            raise urllib.error.HTTPError(request.full_url, self.status, "x", {}, None)
        if self.files is None:
            raise urllib.error.HTTPError(request.full_url, 404, "not found", {}, None)
        host = pp.INDEXES[self.target]["files"]
        urls = [{"filename": n, "digests": {"sha256": d}, "url": f"{host}packages/aa/bb/{n}"}
                for n, d in self.files.items()]
        return io.BytesIO(json.dumps({"urls": urls}).encode())


class Args:
    def __init__(self, **kw):
        self.__dict__.update(kw)


def guard_args(tmp_path, target="pypi", **kw):
    values = dict(version=VERSION, target=target)
    values.update(kw)
    if "tag_dir" not in values:
        values["tag_dir"] = frozen_tree(tmp_path)
    if "ledger" not in values:
        values["ledger"] = ledger_file(tmp_path)
    values["dir"] = release_dir(tmp_path)[0]
    return Args(**values)


@pytest.mark.parametrize("version", ["0.0.1rc903", "0.0.1rc901", "0.0.2"])
def test_flow_test_versions_rejected(version):
    with pytest.raises(pp.PublishError, match="release-flow test version"):
        pp.check_version(version)


@pytest.mark.parametrize("version", ["v0.9.0rc2", "0.9.0rc2 ", "0.9.0-rc.2", ""])
def test_bad_version_rejected(version):
    with pytest.raises(pp.PublishError, match="bad version"):
        pp.check_version(version)


def test_unknown_target_rejected():
    with pytest.raises(pp.PublishError, match="unknown target"):
        pp.check_target("pypi.org")


def test_guard_passes_and_lists_pending_files(tmp_path):
    index = Index("pypi")
    assert pp.guard(guard_args(tmp_path), index) == NAMES
    assert index.urls == [f"https://pypi.org/pypi/milvus-lite-rust/{VERSION}/json"]


@pytest.mark.parametrize("outcome", ["candidate", "abandoned", "signing-failed"])
def test_guard_requires_released(tmp_path, outcome):
    with pytest.raises(pp.PublishError, match="ledger outcome"):
        pp.guard(guard_args(tmp_path, ledger=ledger_file(tmp_path, outcome)), Index("pypi"))


def test_guard_requires_ledger_entry(tmp_path):
    with pytest.raises(pp.PublishError, match="not in the ledger"):
        pp.guard(guard_args(tmp_path, ledger=ledger_file(tmp_path, version="0.9.0rc1")), Index("pypi"))


def test_pypi_target_requires_a_frozen_tag_that_publishes_to_pypi(tmp_path):
    with pytest.raises(pp.PublishError, match="do not publish to PyPI"):
        pp.guard(guard_args(tmp_path / "a", tag_dir=frozen_tree(tmp_path / "fa", export=False)), Index("pypi"))
    with pytest.raises(pp.PublishError, match="PyPI install command"):
        pp.guard(guard_args(tmp_path / "b", tag_dir=frozen_tree(tmp_path / "fb", readme="x")), Index("pypi"))


def test_testpypi_target_accepts_an_old_frozen_tag(tmp_path):
    # TestPyPI 演练用已发布的旧版本，它们的冻结 tag 没有 PyPI 命令（0042 决策 11）。
    args = guard_args(tmp_path, target="testpypi", tag_dir=frozen_tree(tmp_path / "old", export=False))
    index = Index("testpypi")
    assert pp.guard(args, index) == NAMES
    assert index.urls == [f"https://test.pypi.org/pypi/milvus-lite-rust/{VERSION}/json"]


def test_guard_resumes_with_identical_files(tmp_path):
    args = guard_args(tmp_path)
    _, sums = release_dir(tmp_path / "x")
    uploaded = {NAMES[0]: sums[NAMES[0]]}
    assert pp.guard(args, Index("pypi", uploaded)) == NAMES[1:]


def test_guard_rejects_foreign_or_different_files(tmp_path):
    args = guard_args(tmp_path)
    _, sums = release_dir(tmp_path / "x")
    with pytest.raises(pp.PublishError, match="outside this release"):
        pp.guard(args, Index("pypi", {**sums, "milvus_lite_rust-0.9.0rc2-cp310-abi3-win_amd64.whl": "a" * 64}))
    with pytest.raises(pp.PublishError, match="with sha256"):
        pp.guard(args, Index("pypi", {NAMES[0]: "b" * 64}))


def test_guard_rejects_files_that_differ_from_sums(tmp_path):
    args = guard_args(tmp_path)
    (args.dir / NAMES[0]).write_bytes(b"tampered")
    with pytest.raises(pp.PublishError, match="differs from SHA256SUMS"):
        pp.guard(args, Index("pypi"))


def test_guard_requires_all_five_files(tmp_path):
    args = guard_args(tmp_path)
    sums = pp.c.parse_sums((args.dir / "SHA256SUMS").read_text())
    del sums[NAMES[-1]]
    (args.dir / "SHA256SUMS").write_text(pp.c.sums_text(sums))
    with pytest.raises(pp.PublishError, match="lacks"):
        pp.guard(args, Index("pypi"))


@pytest.mark.parametrize("status", [403, 500])
def test_index_errors_other_than_404_fail(tmp_path, status):
    with pytest.raises(pp.PublishError, match=f"HTTP {status}"):
        pp.guard(guard_args(tmp_path), Index("pypi", status=status))


def test_index_must_serve_files_from_its_own_host(tmp_path):
    class Elsewhere(Index):
        def __call__(self, request, timeout):
            urls = [{"filename": NAMES[0], "digests": {"sha256": "a" * 64}, "url": f"https://evil.example/{NAMES[0]}"}]
            return io.BytesIO(json.dumps({"urls": urls}).encode())
    with pytest.raises(pp.PublishError, match="serves"):
        pp.index_files("pypi", VERSION, Elsewhere("pypi"))


def test_upload_order_puts_sdist_last():
    order = pp.upload_order(NAMES)
    assert order[-1] == f"milvus_lite_rust-{VERSION}.tar.gz"
    assert all(name.endswith(".whl") for name in order[:-1]) and len(order) == 5


def test_files_checks_the_verified_sums(tmp_path, capsys):
    directory, _ = release_dir(tmp_path)
    sums = directory / "SHA256SUMS"
    digest = hashlib.sha256(sums.read_bytes()).hexdigest()
    pp.files(Args(version=VERSION, dir=directory, sums=sums, sums_sha256=digest))
    printed = capsys.readouterr().out.split()
    assert [Path(p).name for p in printed] == pp.upload_order(NAMES)
    with pytest.raises(pp.PublishError, match="changed between jobs"):
        pp.files(Args(version=VERSION, dir=directory, sums=sums, sums_sha256="c" * 64))
    (directory / NAMES[0]).write_bytes(b"tampered")
    with pytest.raises(pp.PublishError, match="verified SHA256SUMS"):
        pp.files(Args(version=VERSION, dir=directory, sums=sums, sums_sha256=digest))


class Clock:
    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds


def wait_args(tmp_path, target="pypi", timeout=60, interval=15):
    directory, sums = release_dir(tmp_path)
    path = directory / "SHA256SUMS"
    return Args(version=VERSION, target=target, sums=path, sums_sha256=hashlib.sha256(path.read_bytes()).hexdigest(),
                timeout=timeout, interval=interval), sums


class Sequence:
    """依次返回给定的索引状态；None 表示 404，"down" 表示连接失败。"""

    def __init__(self, target, states):
        self.target, self.states, self.calls = target, list(states), 0

    def __call__(self, request, timeout):
        state = self.states[min(self.calls, len(self.states) - 1)]
        self.calls += 1
        if state == "down":
            raise urllib.error.URLError("timed out")
        return Index(self.target, state)(request, timeout)


def test_wait_visible_waits_until_exactly_the_five_files(tmp_path):
    args, sums = wait_args(tmp_path)
    clock = Clock()
    opener = Sequence("pypi", [None, "down", {NAMES[0]: sums[NAMES[0]]}, sums])
    pp.wait_visible(args, opener, clock, clock.sleep)
    assert opener.calls == 4 and clock.now == 45


def test_wait_visible_gives_up_at_the_deadline(tmp_path):
    args, _ = wait_args(tmp_path, timeout=60)
    clock = Clock()
    with pytest.raises(pp.PublishError, match="after 60 s: not yet listed"):
        pp.wait_visible(args, Sequence("pypi", [None]), clock, clock.sleep)
    assert clock.now <= 60


def test_wait_visible_fails_at_once_on_foreign_or_different_files(tmp_path):
    args, sums = wait_args(tmp_path)
    clock = Clock()
    with pytest.raises(pp.PublishError, match="outside this release"):
        pp.wait_visible(args, Sequence("pypi", [{**sums, "x.whl": "a" * 64}]), clock, clock.sleep)
    with pytest.raises(pp.PublishError, match="with sha256"):
        pp.wait_visible(args, Sequence("pypi", [{NAMES[0]: "d" * 64}]), clock, clock.sleep)
    assert clock.now == 0


def test_main_rejects_flow_test_version_before_anything(capsys, tmp_path):
    assert pp.main(["guard", "--version", "0.0.1rc903", "--target", "pypi", "--tag-dir", str(tmp_path),
                    "--ledger", str(tmp_path / "none"), "--dir", str(tmp_path)]) == 1
    assert "release-flow test version" in capsys.readouterr().err


def test_candidate_result_in_sums_is_never_uploaded(tmp_path, capsys):
    args = guard_args(tmp_path)
    assert len(pp.c.parse_sums((args.dir / "SHA256SUMS").read_text())) == 6
    assert pp.guard(args, Index("pypi")) == NAMES
    sums = args.dir / "SHA256SUMS"
    capsys.readouterr()
    pp.files(Args(version=VERSION, dir=args.dir, sums=sums, sums_sha256=hashlib.sha256(sums.read_bytes()).hexdigest()))
    assert [Path(p).name for p in capsys.readouterr().out.split()] == pp.upload_order(NAMES)


class Listing:
    """直接给出索引 JSON 的 urls 列表。"""

    def __init__(self, urls):
        self.urls = urls

    def __call__(self, request, timeout):
        return io.BytesIO(json.dumps({"urls": self.urls}).encode())


def entry(name, host="https://files.pythonhosted.org/", path="packages/aa/bb/", digest="a" * 64):
    return {"filename": name, "digests": {"sha256": digest}, "url": f"{host}{path}{name}"}


def test_index_lists_each_file_once():
    with pytest.raises(pp.PublishError, match="twice"):
        pp.index_files("pypi", VERSION, Listing([entry(NAMES[0]), entry(NAMES[0])]))


@pytest.mark.parametrize("target, item", [
    ("pypi", entry(NAMES[0], path="packages/aa/bb/x-")),
    ("pypi", entry(NAMES[0], host="https://test-files.pythonhosted.org/")),
    ("testpypi", entry(NAMES[0])),
    ("pypi", entry(NAMES[0], host="https://files.pythonhosted.org.evil.example/")),
], ids=["name-suffix", "pypi-from-test-host", "testpypi-from-pypi-host", "lookalike-host"])
def test_index_file_address_must_match_host_and_name(target, item):
    with pytest.raises(pp.PublishError, match="serves"):
        pp.index_files(target, VERSION, Listing([item]))


def test_index_file_addresses_accepted_per_target():
    assert pp.index_files("pypi", VERSION, Listing([entry(NAMES[0])])) == {NAMES[0]: "a" * 64}
    test = entry(NAMES[0], host="https://test-files.pythonhosted.org/")
    assert pp.index_files("testpypi", VERSION, Listing([test])) == {NAMES[0]: "a" * 64}


def test_wait_visible_keeps_waiting_through_read_timeouts(tmp_path):
    args, sums = wait_args(tmp_path)
    clock = Clock()
    calls = []

    def opener(request, timeout):
        calls.append(1)
        if len(calls) == 1:
            raise TimeoutError("read timed out")
        return Index("pypi", sums)(request, timeout)
    pp.wait_visible(args, opener, clock, clock.sleep)
    assert len(calls) == 2


# ---- 工作流约束 ----

WORKFLOW = ROOT / ".github" / "workflows" / "pypi-publish.yml"
SIGNING_TOOLS = ("candidate.py", "ledger.py", "preserve.py", "release_set.py", "runs.py", "sums.py")
MAIN_GUARD = "    if: github.ref == 'refs/heads/main'"


def jobs(text):
    found, job = {}, None
    for line in text.split("\njobs:\n", 1)[1].splitlines():
        if re.match(r"^  [a-z][a-z0-9-]*:\s*$", line):
            job = line.strip()[:-1]
            found[job] = []
        elif job:
            found[job].append(line)
    return {name: "\n".join(lines) for name, lines in found.items()}


def steps(body):
    found = []
    for line in body.splitlines():
        if line.startswith("      - "):
            found.append(line)
        elif found:
            found[-1] += "\n" + line
    return found


def step(body, marker):
    matched = [s for s in steps(body) if marker in s]
    return matched[0] if len(matched) == 1 else ""


def case_branch(body, label):
    match = re.search(rf"^\s+{label}\) (.*?) ;;$", body, re.M)
    return match.group(1) if match else ""


def workflow_problems(text):
    problems = []
    found = jobs(text)
    if sorted(found) != ["pypi-pass", "upload", "verify"]:
        return [f"jobs are {sorted(found)}"]
    verify, upload, summary = found["verify"], found["upload"], found["pypi-pass"]
    for name in ("verify", "upload"):
        body = found[name]
        if MAIN_GUARD not in body.splitlines():
            problems.append(f"{name} is not restricted to refs/heads/main")
        if body.count("          ref: ${{ github.sha }}") != body.count("path: default") or "path: default" not in body:
            problems.append(f"{name}: default checkout not bound to github.sha")
    checkouts = [s for s in steps(text) if "uses: actions/checkout@" in s]
    if not checkouts or any("persist-credentials: false" not in s for s in checkouts):
        problems.append("a checkout persists credentials")
    header = "\n".join(line for line in text.split("\njobs:\n", 1)[0].splitlines() if not line.lstrip().startswith("#"))
    if "id-token" in verify or "id-token" in header or "id-token" in summary:
        problems.append("id-token outside the upload job")
    if "      id-token: write" not in upload.splitlines():
        problems.append("upload has no id-token")
    if "      attestations: read" not in verify.splitlines():
        problems.append("verify cannot read attestations")
    if "    environment: ${{ inputs.target }}" not in upload.splitlines():
        problems.append("upload does not run in the target environment")
    if "    needs: verify" not in upload.splitlines():
        problems.append("upload does not depend on verify")
    if "frozen/tests/release/release_set.py" not in step(verify, "release_set"):
        problems.append("verify does not use the frozen release_set")
    if "default/.github/ci/pypi_release.py guard " not in step(verify, "pypi_release.py guard"):
        problems.append("verify does not run the upload guard")
    if "path: ${{ runner.temp }}/release\n" not in step(verify, "upload-artifact"):
        problems.append("verify does not hand over the verified release directory")
    if "SUMS_SHA256: ${{ needs.verify.outputs.sums_sha256 }}" not in upload:
        problems.append("upload does not take the digest from verify")
    for marker in ("pypi_release.py files ", "pypi_release.py wait-visible "):
        if '--sums-sha256 "$SUMS_SHA256"' not in step(upload, marker):
            problems.append(f"{marker.strip()} does not use the verified digest")
    upload_step = step(upload, "uv publish")
    if "--trusted-publishing always" not in upload_step or "< /dev/null" not in upload_step:
        problems.append("upload does not force trusted publishing with closed stdin")
    if '< "$RUNNER_TEMP/upload-order.txt"' not in upload_step:
        problems.append("upload does not follow the order from files")
    if "--publish-url https://test.pypi.org/legacy/" not in case_branch(upload_step, "testpypi"):
        problems.append("testpypi does not use the TestPyPI upload endpoint")
    if "--publish-url" in case_branch(upload_step, "pypi") or "https://pypi.org/simple/" not in case_branch(upload_step, "pypi"):
        problems.append("pypi branch is wrong")
    if any(not re.search(r"\bbash \.github/ci/install_uv\.sh\b", line) for line in text.splitlines()
           if "install_uv.sh" in line and not line.lstrip().startswith("#")):
        problems.append("install_uv.sh is not run with bash")
    for block in re.findall(r"run: \|?\n?((?:.*\n)*?)(?=\s+- |\s*\n  [a-z]|\Z)", text):
        if "${{ inputs." in block or "${{ github.event" in block:
            problems.append("inputs interpolated into a run block")
            break
    if "    if: always()" not in summary.splitlines() or "    needs: [verify, upload]" not in summary.splitlines() \
            or 'test "$VERIFY" = success && test "$UPLOAD" = success' not in summary:
        problems.append("pypi-pass does not require both jobs")
    if re.search(r"secrets\.|PYPI_TOKEN|password", text, re.I):
        problems.append("workflow references a secret or password")
    if "continue-on-error" in text:
        problems.append("a step or job may fail without failing the run")
    if any(re.match(r"^        if:", line) for line in (verify + "\n" + upload).splitlines()):
        problems.append("a verify or upload step is conditional")
    frozen = [s for s in checkouts if "path: frozen" in s]
    if len(frozen) != 1 or "          ref: refs/tags/v${{ inputs.version }}\n" not in frozen[0] + "\n" \
            or text.count("refs/tags/v${{ inputs.version }}") != 1:
        problems.append("the frozen checkout is not the version tag")
    perms = re.search(r"^    permissions:\n((?:      .*\n)+)", upload + "\n", re.M)
    if not perms or perms.group(1) != "      contents: read\n      id-token: write\n":
        problems.append("upload permissions are not exactly contents: read and id-token: write")
    return problems


def test_workflow_constraints():
    assert workflow_problems(WORKFLOW.read_text()) == []


def replace_once(old, new):
    def mutate(text):
        assert old in text, old
        return text.replace(old, new, 1)
    return mutate


@pytest.mark.parametrize("mutate", [
    replace_once(MAIN_GUARD, "    if: always()"),
    replace_once("      id-token: write", "      contents: read"),
    replace_once("    environment: ${{ inputs.target }}", ""),
    replace_once("--trusted-publishing always", "--trusted-publishing automatic"),
    replace_once("permissions:\n  contents: read\n", "permissions:\n  contents: read\n  id-token: write\n"),
    replace_once("frozen/tests/release/release_set.py", "default/tests/release/release_set.py"),
    lambda t: t + "\n# uses ${{ secrets.PYPI_TOKEN }}\n",
    replace_once("default/.github/ci/pypi_release.py guard ", "echo guard "),
    replace_once("pypi_release.py wait-visible ", "echo wait-visible "),
    replace_once(" --publish-url https://test.pypi.org/legacy/", ""),
    replace_once('--sums-sha256 "$SUMS_SHA256" > "$RUNNER_TEMP/upload-order.txt"',
                 '--sums-sha256 "$(sha256sum "$RUNNER_TEMP/release/SHA256SUMS" | cut -c1-64)" > "$RUNNER_TEMP/upload-order.txt"'),
    replace_once("bash .github/ci/install_uv.sh", ".github/ci/install_uv.sh"),
    replace_once("          persist-credentials: false\n", ""),
    replace_once("      attestations: read\n", ""),
    replace_once("path: ${{ runner.temp }}/release\n", "path: ${{ runner.temp }}/release/SHA256SUMS\n"),
    replace_once(' "$file" < /dev/null', ' "$file"'),
    replace_once('run: python3 default/.github/ci/public_release.py tag-commit --dir "$RUNNER_TEMP/release" --tag-dir frozen --version "$VERSION"',
                 'run: python3 default/.github/ci/public_release.py tag-commit --dir "$RUNNER_TEMP/release" --tag-dir frozen --version "${{ inputs.version }}"'),
    replace_once("    needs: [verify, upload]", "    needs: [verify]"),
    replace_once("    if: always()", "    if: success()"),
    replace_once("      - name: 上传守卫\n", "      - name: 上传守卫\n        continue-on-error: true\n"),
    replace_once("      - name: 索引上恰为这五个文件\n", "      - name: 索引上恰为这五个文件\n        if: false\n"),
    replace_once("ref: refs/tags/v${{ inputs.version }}", "ref: refs/heads/main"),
    replace_once("      id-token: write\n", "      id-token: write\n      attestations: write\n"),
], ids=["guard", "id-token", "environment", "automatic", "top-id-token", "default-release-set", "secret",
        "no-upload-guard", "no-wait-visible", "no-testpypi-endpoint", "own-digest", "install-uv-exec", "credentials",
        "no-attestations", "artifact-path", "stdin", "inputs-in-run", "summary-needs", "summary-if",
        "continue-on-error", "step-if", "frozen-ref", "extra-permission"])
def test_workflow_mutations_detected(mutate):
    text = WORKFLOW.read_text()
    mutated = mutate(text)
    assert mutated != text
    assert workflow_problems(mutated)


def test_tool_does_not_live_in_the_signing_closure():
    # 0042 决策 4：新增的上传工具不改 0039 的签发路径，签发工具仍只有这六个且不引用本工具。
    release = ROOT / "tests" / "release"
    for name in SIGNING_TOOLS:
        assert "pypi_release" not in (release / name).read_text(), name
    assert not (release / "pypi_release.py").exists()
