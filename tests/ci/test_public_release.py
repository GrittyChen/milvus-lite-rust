"""公开下载核验工具的反证（0021 第 20 条）：版本先于一切校验；Release 须已发布、预发布且不可变；附件名与公开地址
合法；下载不带认证且与 API digest 一致；tag 须指向候选冻结提交；安装命令不经 shell；pip report 与冒烟输出须
指向预期的公开 wheel、版本与 SDK。"""

import hashlib
import io
import json
import re
from pathlib import Path
import subprocess
import sys

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / ".github" / "ci"))

import public_release as pr  # noqa: E402

REPO = "owner/repo"
VERSION = "0.0.1rc903"
WHEEL = f"milvus_lite_rust-{VERSION}-cp310-abi3-macosx_11_0_arm64.whl"
INSTALL = ("python -m pip install --no-index --only-binary :all: --find-links "
           f"https://github.com/{REPO}/releases/expanded_assets/v{VERSION} milvus-lite-rust=={VERSION}")


def asset(name, body=b"x"):
    return {"name": name, "state": "uploaded", "size": len(body),
            "browser_download_url": f"https://github.com/{REPO}/releases/download/v{VERSION}/{name}",
            "digest": "sha256:" + hashlib.sha256(body).hexdigest()}


def release(**changes):
    data = {"tag_name": f"v{VERSION}", "draft": False, "prerelease": True, "immutable": True, "id": 1,
            "html_url": "https://example.invalid/r", "assets": [asset("REJECTION-TEST.txt")]}
    data.update(changes)
    return data


class Opener:
    """记录请求头的假 urlopen，按 URL 返回固定内容。"""

    def __init__(self, bodies):
        self.bodies, self.requests = bodies, []

    def __call__(self, request, timeout):
        self.requests.append(request)
        return io.BytesIO(self.bodies[request.full_url])


@pytest.mark.parametrize("version", ["0.0.1rc903", "0.4.0", "1.2.3a4"])
def test_canonical_version(version):
    pr.check_version(version)


@pytest.mark.parametrize("version", ["", "v0.0.1rc903", "0.0.1rc903\n", "0.0.1rc903;id", "../0.0.1", "0.0.1-rc.903"])
def test_bad_version_rejected(version):
    with pytest.raises(pr.PublicCheckError):
        pr.check_version(version)


def test_bad_version_rejected_before_anything_else(capsys):
    assert pr.main(["fetch", "--repo", REPO, "--version", "0.0.1rc903 x", "--out", "/nonexistent/x"]) == 1
    assert "bad version" in capsys.readouterr().err


def test_sentinel_release_passes_attribute_checks():
    # 只有哨兵附件的 Release 在这一层放行，由冻结 tag 的 release_set 拒绝。
    assert [a["name"] for a in pr.check_release(release(), REPO, VERSION)] == ["REJECTION-TEST.txt"]


@pytest.mark.parametrize("changes, message", [
    ({"draft": True}, "draft"),
    ({"prerelease": False}, "not a prerelease"),
    ({"immutable": False}, "not immutable"),
    ({"immutable": None}, "not immutable"),
    ({"tag_name": "v0.0.1rc902"}, "release tag"),
])
def test_release_attributes(changes, message):
    with pytest.raises(pr.PublicCheckError, match=message):
        pr.check_release(release(**changes), REPO, VERSION)


@pytest.mark.parametrize("bad, message", [
    ({"name": "../x"}, "bad asset name"),
    ({"name": ".hidden"}, "bad asset name"),
    ({"name": "a/b"}, "bad asset name"),
    ({"state": "starter"}, "is 'starter'"),
    ({"browser_download_url": "https://api.github.com/repos/owner/repo/releases/assets/1"}, "not served from"),
    ({"digest": None}, "no sha256 digest"),
    ({"digest": "md5:00"}, "no sha256 digest"),
])
def test_asset_records(bad, message):
    item = asset("REJECTION-TEST.txt")
    item.update(bad)
    with pytest.raises(pr.PublicCheckError, match=message):
        pr.check_release(release(assets=[item]), REPO, VERSION)


def test_duplicate_asset_rejected():
    with pytest.raises(pr.PublicCheckError, match="duplicate"):
        pr.check_release(release(assets=[asset("a"), asset("a")]), REPO, VERSION)


def test_api_uses_token_and_download_does_not(tmp_path):
    body = b"sentinel\n"
    data = release(assets=[asset("REJECTION-TEST.txt", body)])
    api = f"{pr.API}/repos/{REPO}/releases/tags/v{VERSION}"
    opener = Opener({api: json.dumps(data).encode(), data["assets"][0]["browser_download_url"]: body})
    record = pr.fetch(REPO, VERSION, tmp_path / "release.json", "secret", urlopen=opener)
    pr.download(record, tmp_path / "out", environ={}, urlopen=opener)
    assert opener.requests[0].get_header("Authorization") == "Bearer secret"
    assert opener.requests[1].get_header("Authorization") is None
    assert (tmp_path / "out" / "REJECTION-TEST.txt").read_bytes() == body


@pytest.mark.parametrize("variable", ["GH_TOKEN", "GITHUB_TOKEN"])
def test_download_refuses_token(tmp_path, variable):
    with pytest.raises(pr.PublicCheckError, match="must not receive a token"):
        pr.download({"assets": []}, tmp_path / "out", environ={variable: "secret"}, urlopen=Opener({}))


@pytest.mark.parametrize("served, message", [(b"xy", "bytes"), (b"y", "differs from the API digest")])
def test_download_must_match_api(tmp_path, served, message):
    item = pr.check_release(release(assets=[asset("f", b"x")]), REPO, VERSION)[0]
    with pytest.raises(pr.PublicCheckError, match=message):
        pr.download({"assets": [item]}, tmp_path / "out", environ={}, urlopen=Opener({item["url"]: served}))


def test_download_needs_new_directory(tmp_path):
    with pytest.raises(FileExistsError):
        pr.download({"assets": []}, tmp_path, environ={}, urlopen=Opener({}))


def git(repo, *args):
    return subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True, text=True).stdout.strip()


@pytest.fixture
def tagged(tmp_path):
    repo = tmp_path / "frozen"
    repo.mkdir()
    git(repo, "init", "-q")
    git(repo, "-c", "user.name=ci", "-c", "user.email=ci@example.invalid", "commit", "-q", "--allow-empty", "-m", "a")
    first = git(repo, "rev-parse", "HEAD")
    git(repo, "-c", "user.name=ci", "-c", "user.email=ci@example.invalid", "tag", "-a", f"v{VERSION}", "-m", "t")
    return repo, first


def candidate_dir(tmp_path, commit):
    directory = tmp_path / "release"
    directory.mkdir()
    (directory / f"candidate-{VERSION}-1-1.json").write_text(json.dumps({"commit": commit}))
    return directory


def test_tag_points_at_candidate_commit(tmp_path, tagged):
    repo, first = tagged
    assert pr.tag_commit(candidate_dir(tmp_path, first), repo, VERSION) == first


def test_tag_elsewhere_rejected(tmp_path, tagged):
    repo, _ = tagged
    with pytest.raises(pr.PublicCheckError, match="candidate commit is"):
        pr.tag_commit(candidate_dir(tmp_path, "0" * 40), repo, VERSION)


def test_checkout_must_be_the_tag(tmp_path, tagged):
    repo, first = tagged
    git(repo, "-c", "user.name=ci", "-c", "user.email=ci@example.invalid", "commit", "-q", "--allow-empty", "-m", "b")
    with pytest.raises(pr.PublicCheckError, match="checkout"):
        pr.tag_commit(candidate_dir(tmp_path, first), repo, VERSION)


def frozen_tree(tmp_path, readme=INSTALL):
    root = tmp_path / "frozen"
    (root / "tests" / "packaging").mkdir(parents=True)
    (root / "tests" / "packaging" / "check_artifacts.py").write_text(
        "def release_install_commands(version):\n"
        f"    return {{{INSTALL!r}, 'shasum -a 256 -c --ignore-missing SHA256SUMS && python -m pip install x'}}\n")
    (root / "README.md").write_text(f"# t\n\n```bash\n{readme}\n```\n")
    return root


def test_readme_install_command_and_argv(tmp_path):
    command = pr.readme_install_command(frozen_tree(tmp_path), VERSION)
    assert command == INSTALL
    argv = pr.install_argv(command, "/venv/bin/python", "/tmp/report.json")
    assert argv == ["/venv/bin/python", *INSTALL.split()[1:], "--no-cache-dir", "--report", "/tmp/report.json"]


def test_readme_must_contain_install_command(tmp_path):
    with pytest.raises(pr.PublicCheckError, match="README"):
        pr.readme_install_command(frozen_tree(tmp_path, readme="pip install something-else"), VERSION)


def test_repository_readme_and_checker_yield_exactly_one_install_command():
    # 公开下载工作流加载冻结 tag 中真实的检查器与 README；来源证明校验命令不得混入它取用的安装命令。
    root = Path(__file__).resolve().parents[2]
    version = pr.load_module(root / "tests" / "packaging" / "check_artifacts.py", "repository_check_artifacts").VERSION
    command = pr.readme_install_command(root, version)
    assert command.startswith("python -m pip install ") and command.endswith(f" milvus-lite-rust=={version}")
    assert pr.install_argv(command, "/venv/bin/python", "/r.json")[:4] == ["/venv/bin/python", "-m", "pip", "install"]


@pytest.mark.parametrize("command", ["pip install x", "python -m pip download x", "sh -c 'python -m pip install x'"])
def test_install_argv_rejects_other_commands(command):
    with pytest.raises(pr.PublicCheckError, match="unexpected install command"):
        pr.install_argv(command, "/venv/bin/python", "/r.json")


def test_sums_must_be_the_verified_file(tmp_path):
    sums = tmp_path / "SHA256SUMS"
    sums.write_text(f"{'a' * 64}  {WHEEL}\n")
    assert pr.check_sums(sums, hashlib.sha256(sums.read_bytes()).hexdigest()) == {WHEEL: "a" * 64}
    with pytest.raises(pr.PublicCheckError, match="changed between jobs"):
        pr.check_sums(sums, "b" * 64)


def report(**changes):
    item = {"metadata": {"name": "milvus-lite-rust", "version": VERSION},
            "download_info": {"url": f"https://github.com/{REPO}/releases/download/v{VERSION}/{WHEEL}",
                              "archive_info": {"hashes": {"sha256": "a" * 64}}}}
    item.update(changes)
    return {"install": [item]}


def test_report_binds_public_wheel():
    assert pr.check_report(report(), REPO, VERSION, "macosx_11_0_arm64", {WHEEL: "a" * 64}) == WHEEL


@pytest.mark.parametrize("data, message", [
    (report(metadata={"name": "milvus-lite", "version": VERSION}), "installed 'milvus-lite'"),
    (report(metadata={"name": "milvus_lite_rust", "version": "0.3.0rc2"}), "installed version"),
    (report(download_info={"url": f"https://github.com/{REPO}/releases/download/v{VERSION}/other.whl",
                           "archive_info": {"hashes": {"sha256": "a" * 64}}}), "pip downloaded"),
    (report(download_info={"url": f"file:///tmp/{WHEEL}", "archive_info": {"hashes": {"sha256": "a" * 64}}}),
     "pip downloaded"),
    (report(download_info={"url": f"https://github.com/{REPO}/releases/download/v{VERSION}/{WHEEL}",
                           "archive_info": {"hashes": {"sha256": "b" * 64}}}), "with sha256"),
    ({"install": []}, "installed 0 distributions"),
    ({"install": report()["install"] * 2}, "installed 2 distributions"),
])
def test_report_rejections(data, message):
    with pytest.raises(pr.PublicCheckError, match=message):
        pr.check_report(data, REPO, VERSION, "macosx_11_0_arm64", {WHEEL: "a" * 64})


def test_unknown_platform_rejected():
    with pytest.raises(pr.PublicCheckError, match="unknown wheel platform"):
        pr.check_report(report(), REPO, VERSION, "win_amd64", {WHEEL: "a" * 64})


def fake_python(tmp_path, answer):
    path = tmp_path / "python"
    path.write_text(f"#!/bin/sh\necho '{answer}'\n")
    path.chmod(0o755)
    return path


def test_clean_environment_passes(tmp_path):
    pr.check_clean(fake_python(tmp_path, "[false, []]"))


@pytest.mark.parametrize("answer", ['[true, []]', '[false, ["milvus-lite"]]', '[true, ["milvus-lite-rust"]]'])
def test_preinstalled_milvus_lite_rejected(tmp_path, answer):
    with pytest.raises(pr.PublicCheckError, match="already provides milvus_lite"):
        pr.check_clean(fake_python(tmp_path, answer))


def smoke_output(**changes):
    identity = {"version": VERSION, "sdk": "3.0.3", "passed": True, "stages": ["active"]}
    identity.update(changes)
    return "log line\n" + json.dumps(identity) + "\n"


def test_smoke_identity():
    assert pr.check_smoke(smoke_output(), VERSION, "3.0.3")["version"] == VERSION


@pytest.mark.parametrize("output, message", [
    (smoke_output(version="0.3.0rc2"), "loaded version"),
    (smoke_output(sdk="2.6.18"), "used pymilvus"),
    (smoke_output(passed=False), "did not pass"),
    ("", "printed nothing"),
    ("not json\n", "not JSON"),
])
def test_smoke_rejections(output, message):
    with pytest.raises(pr.PublicCheckError, match=message):
        pr.check_smoke(output, VERSION, "3.0.3")


def write_lock(tag_dir, text):
    lock = tag_dir / ".github" / "ci" / "requirements" / "sdk30.txt"
    lock.parent.mkdir(parents=True)
    lock.write_text(text)


def test_pinned_sdk_follows_the_tag_lock(tmp_path):
    # 已发布的 tag 固定的是当时的版本；main 升级 pymilvus 后重跑公开核验，期望值仍取 tag 自己的锁。
    write_lock(tmp_path, "grpcio==1.84.0\n    # via pymilvus\npymilvus==3.0.2\n    # via -r sdk30.in\n")
    assert pr.pinned_sdk(tmp_path) == "3.0.2"


@pytest.mark.parametrize("text, message", [
    (None, "is missing"),
    ("grpcio==1.84.0\n", "exactly once"),
    ("pymilvus==3.0.2\npymilvus==3.0.3\n", "exactly once"),
])
def test_pinned_sdk_rejections(tmp_path, text, message):
    if text is not None:
        write_lock(tmp_path, text)
    with pytest.raises(pr.PublicCheckError, match=message):
        pr.pinned_sdk(tmp_path)


def test_public_check_takes_sdk_from_the_tag():
    assert "--sdk" not in WORKFLOW.read_text()


WORKFLOW = Path(__file__).resolve().parents[2] / ".github" / "workflows" / "public-check.yml"
MAIN_GUARD = "    if: github.ref == 'refs/heads/main'"
SHA_REF = "          ref: ${{ github.sha }}"


def identity_problems(text):
    """公开核验的证据身份：检出默认分支检查器的每一步都绑定运行提交，verify 只在 main 上运行。

    开发环境不装 YAML 解析库，按工作流固定的缩进切分：jobs 下两格缩进为 job，六格的 "- " 开始一个步骤。
    """
    jobs, job = {}, None
    for line in text.split("\njobs:\n", 1)[1].splitlines():
        if re.match(r"^  [a-z][a-z0-9-]*:\s*$", line):
            job = line.strip()[:-1]
            jobs[job] = {"head": [], "steps": []}
        elif job and line.startswith("      - "):
            jobs[job]["steps"].append([line])
        elif job and jobs[job]["steps"]:
            jobs[job]["steps"][-1].append(line)
        elif job:
            jobs[job]["head"].append(line)
    problems = []
    defaults = [(name, step) for name, body in jobs.items() for step in body["steps"]
                if "          path: default" in step]
    if sorted(name for name, _ in defaults) != ["install", "verify"]:
        problems.append(f"default checkouts in {sorted(name for name, _ in defaults)}")
    problems += [f"{name}: default checkout not bound to github.sha" for name, step in defaults if SHA_REF not in step]
    if MAIN_GUARD not in jobs.get("verify", {}).get("head", []):
        problems.append("verify is not restricted to refs/heads/main")
    if jobs.get("install", {}).get("head", []).count("    needs: verify") != 1:
        problems.append("install does not depend on verify")
    return problems


def test_workflow_binds_checker_identity():
    assert identity_problems(WORKFLOW.read_text()) == []


def drop_line(text, line, nth):
    lines = text.split("\n")
    positions = [i for i, item in enumerate(lines) if item == line]
    del lines[positions[nth]]
    return "\n".join(lines)


@pytest.mark.parametrize("mutate", [
    lambda text: drop_line(text, SHA_REF, 0),
    lambda text: drop_line(text, SHA_REF, 1),
    lambda text: drop_line(text, MAIN_GUARD, 0),
    lambda text: text.replace(MAIN_GUARD, "    if: always()"),
    lambda text: drop_line(text, "    needs: verify", 0),
], ids=["verify-ref", "install-ref", "no-guard", "other-guard", "no-needs"])
def test_identity_mutations_detected(mutate):
    text = WORKFLOW.read_text()
    mutated = mutate(text)
    assert mutated != text
    assert identity_problems(mutated)


PYPI = f"python -m pip install --only-binary milvus-lite-rust milvus-lite-rust=={VERSION}"


def pypi_tree(tmp_path, readme=PYPI, export=True):
    root = frozen_tree(tmp_path, readme=f"{INSTALL}\n{readme}")
    if export:
        with open(root / "tests" / "packaging" / "check_artifacts.py", "a") as f:
            f.write("def pypi_install_command(version):\n"
                    "    return f'python -m pip install --only-binary milvus-lite-rust milvus-lite-rust=={version}'\n")
    return root


def test_sources_pick_their_commands(tmp_path):
    root = pypi_tree(tmp_path)
    assert pr.install_command("github", root, VERSION) == INSTALL
    assert pr.install_command("pypi", root, VERSION) == PYPI
    testpypi = pr.install_command("testpypi", root, VERSION)
    assert testpypi.startswith("python -m pip install --index-url https://test.pypi.org/simple/ --no-deps ")
    assert pr.install_argv(testpypi, "/venv/bin/python", "/r.json")[:4] == ["/venv/bin/python", "-m", "pip", "install"]


def test_pypi_source_needs_a_frozen_tag_that_publishes_to_pypi(tmp_path):
    # 已发布的旧版本没有导出 PyPI 命令，不能用 pypi 一路核验；导出了但冻结 README 没有这条命令同样拒绝。
    with pytest.raises(pr.PublicCheckError, match="do not publish to PyPI"):
        pr.install_command("pypi", pypi_tree(tmp_path / "a", export=False), VERSION)
    with pytest.raises(pr.PublicCheckError, match="PyPI install command"):
        pr.install_command("pypi", pypi_tree(tmp_path / "b", readme="pip install something-else"), VERSION)


def test_unknown_source_rejected(tmp_path):
    with pytest.raises(pr.PublicCheckError, match="unknown source"):
        pr.install_command("conda", pypi_tree(tmp_path), VERSION)


def pypi_report(url):
    return report(download_info={"url": url, "archive_info": {"hashes": {"sha256": "a" * 64}}})


@pytest.mark.parametrize("source, host", [("pypi", "files.pythonhosted.org"), ("testpypi", "test-files.pythonhosted.org")])
def test_index_report_binds_host_name_and_hash(source, host):
    url = f"https://{host}/packages/ab/cd/ef/{WHEEL}"
    assert pr.check_report(pypi_report(url), REPO, VERSION, "macosx_11_0_arm64", {WHEEL: "a" * 64}, source) == WHEEL
    bad = [f"https://github.com/{REPO}/releases/download/v{VERSION}/{WHEEL}",
           f"https://{host}.evil.example/{WHEEL}", f"https://{host}/packages/{WHEEL}.other"]
    if source == "testpypi":
        bad.append(f"https://files.pythonhosted.org/packages/ab/{WHEEL}")
    else:
        bad.append(f"https://test-files.pythonhosted.org/packages/ab/{WHEEL}")
    for address in bad:
        with pytest.raises(pr.PublicCheckError, match="pip downloaded"):
            pr.check_report(pypi_report(address), REPO, VERSION, "macosx_11_0_arm64", {WHEEL: "a" * 64}, source)
    wrong = report(download_info={"url": url, "archive_info": {"hashes": {"sha256": "b" * 64}}})
    with pytest.raises(pr.PublicCheckError, match="with sha256"):
        pr.check_report(wrong, REPO, VERSION, "macosx_11_0_arm64", {WHEEL: "a" * 64}, source)


def test_repository_checker_exports_the_pypi_command():
    root = Path(__file__).resolve().parents[2]
    checker = pr.load_module(root / "tests" / "packaging" / "check_artifacts.py", "repository_check_artifacts")
    assert checker.pypi_install_command("1.2.3") == "python -m pip install --only-binary milvus-lite-rust milvus-lite-rust==1.2.3"


def test_public_check_routes_the_source():
    # 0042 决策 5：安装步骤把 source 交给 install；只有 pypi / testpypi 才核对索引上的文件集合。
    text = WORKFLOW.read_text()
    assert '            --source "$SOURCE"' in text
    step = text.split("      - name: 索引上恰为这五个文件（pypi / testpypi）\n", 1)[1].split("\n      - ", 1)[0]
    assert "        if: inputs.source != 'github'" in step and "pypi_release.py wait-visible" in step
    assert re.search(r"options:\n\s+- github\n\s+- pypi\n\s+- testpypi\n", text)


def test_public_check_concurrency_is_per_source():
    # 同一版本连续核对 github、pypi、testpypi 时，排队的运行不能被另一来源的运行顶掉。
    assert "  group: public-check-${{ inputs.version }}-${{ inputs.source }}\n" in WORKFLOW.read_text()
