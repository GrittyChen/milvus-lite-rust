"""缺失或错误的发行内容必须被静态门槛拒绝。"""

import io
from pathlib import Path
import tarfile
import zipfile

import pytest
from check_artifacts import VERSION, check, check_binary


@pytest.fixture
def artifacts(tmp_path):
    repo = Path(__file__).resolve().parents[2]
    root = tmp_path / "source"
    for name in ["README.md", "LICENSE", "crates/grpc/proto/LICENSE"]:
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes((repo / name).read_bytes())
    metadata = ("Metadata-Version: 2.4\nName: milvus-lite-rust\nVersion: 0.3.0rc3\nRequires-Python: >=3.10\nLicense-Expression: Apache-2.0\nLicense-File: LICENSE\nLicense-File: crates/grpc/proto/LICENSE\nProject-URL: Repository, https://github.com/GrittyChen/milvus-lite-rust\nProject-URL: Issues, https://github.com/GrittyChen/milvus-lite-rust/issues\nDescription-Content-Type: text/markdown; charset=UTF-8\n\n" + (root / "README.md").read_text()).encode()
    info = "milvus_lite_rust-0.3.0rc3.dist-info"
    wheel_files = {
        info + "/METADATA": metadata,
        info + "/WHEEL": b"Wheel-Version: 1.0\nTag: cp310-abi3-macosx_11_0_arm64\n",
        info + "/entry_points.txt": b"[console_scripts]\nmilvus-lite = milvus_lite.cmdline:main\n",
        "milvus_lite/_native.abi3.so": b"not loaded by content inspector",
    }
    for name in ["LICENSE", "crates/grpc/proto/LICENSE"]:
        wheel_files[info + "/licenses/" + name] = (root / name).read_bytes()
    for name in ["__init__.py", "cmdline.py", "__main__.py", "server_manager.py"]:
        wheel_files["milvus_lite/" + name] = b"# module\n"
    source_files = {"PKG-INFO": metadata}
    for name in ["README.md", "LICENSE", "crates/grpc/proto/LICENSE"]:
        source_files[name] = (root / name).read_bytes()
    for name in ["Cargo.toml", "Cargo.lock", "pyproject.toml", "crates/core/Cargo.toml", "crates/grpc/Cargo.toml", "crates/py/Cargo.toml", "crates/py/build.rs", "crates/py/python/milvus_lite/cmdline.py"]:
        source_files[name] = b"source\n"
    wheel, sdist = tmp_path / "artifact.whl", tmp_path / "source.tar.gz"
    def write():
        with zipfile.ZipFile(wheel, "w") as z:
            for name, content in wheel_files.items():
                z.writestr(name, content)
        with tarfile.open(sdist, "w:gz") as t:
            for name, content in source_files.items():
                member = tarfile.TarInfo("package/" + name)
                member.size = len(content)
                t.addfile(member, io.BytesIO(content))
    return root, wheel, sdist, wheel_files, source_files, info, write


def test_valid_content_and_mime_parameters(artifacts):
    root, wheel, sdist, _, _, _, write = artifacts
    write()
    assert check(wheel, sdist, root)["tags"] == ["cp310-abi3-macosx_11_0_arm64"]


@pytest.mark.parametrize("case", ["license_missing", "license_wrong", "source_license", "version", "python_cap", "tag", "cli", "native_count", "fi", "private", "unsafe", "description", "duplicate_config"])
def test_invalid_artifact_never_passes(case, artifacts):
    root, wheel, sdist, w, s, info, write = artifacts
    if case == "license_missing":
        del w[info + "/licenses/LICENSE"]
    elif case == "license_wrong":
        w[info + "/licenses/LICENSE"] = b"wrong"
    elif case == "source_license":
        del s["LICENSE"]
    elif case == "version":
        w[info + "/METADATA"] = w[info + "/METADATA"].replace(b"Version: 0.3.0rc3", b"Version: 9.0")
    elif case == "python_cap":
        w[info + "/METADATA"] = w[info + "/METADATA"].replace(b">=3.10", b">=3.10,<3.16")
    elif case == "tag":
        w[info + "/WHEEL"] = b"Tag: cp315-cp315-win_amd64\n"
    elif case == "cli":
        w[info + "/entry_points.txt"] = b"[console_scripts]\nmilvus-lite = other:main\n"
    elif case == "native_count":
        w["milvus_lite/_native.other.so"] = b"another"
    elif case == "fi":
        w["milvus_lite/_native.abi3.so"] = b"_pause_compaction"
    elif case == "private":
        w["references/secret"] = b"private"
    elif case == "unsafe":
        s["../escape"] = b"bad"
    elif case == "description":
        w[info + "/METADATA"] = w[info + "/METADATA"].split(b"\n\n")[0] + b"\n\n"
    else:
        s["crates/py/pyproject.toml"] = b"duplicate"
    write()
    with pytest.raises((ValueError, KeyError)):
        check(wheel, sdist, root)


def test_relative_link_rejected_even_when_all_copies_match(artifacts):
    root, wheel, sdist, w, s, info, write = artifacts
    addition = b"\n[details](docs/private.md)\n"
    (root / "README.md").write_bytes((root / "README.md").read_bytes() + addition)
    w[info + "/METADATA"] += addition
    s["PKG-INFO"] += addition
    s["README.md"] += addition
    write()
    with pytest.raises(ValueError, match="relative file link"):
        check(wheel, sdist, root)


def test_binary_wrong_arch_or_deployment_rejected(artifacts, monkeypatch):
    _, wheel, _, _, _, _, write = artifacts
    write()
    monkeypatch.setattr("check_artifacts.subprocess.check_output", lambda args, **_: "x86_64" if args[0] == "lipo" else "")
    with pytest.raises(ValueError, match="architecture"):
        check_binary(wheel, ["cp310-abi3-macosx_11_0_arm64"])
    monkeypatch.setattr("check_artifacts.subprocess.check_output", lambda args, **_: "arm64" if args[0] == "lipo" else "Load command 1\ncmd LC_BUILD_VERSION\nminos 12.0\n")
    with pytest.raises(ValueError, match="deployment"):
        check_binary(wheel, ["cp310-abi3-macosx_11_0_arm64"])


@pytest.mark.parametrize("versions", ["GLIBC_2.29", "GLIBC_2.28 GLIBC_PRIVATE", "no version table"])
def test_linux_binary_rejects_unverified_or_newer_glibc(versions, artifacts, monkeypatch):
    _, wheel, _, _, _, _, write = artifacts
    write()
    monkeypatch.setattr("check_artifacts.subprocess.check_output", lambda args, **_: "Machine: AArch64" if args[1] == "-h" else versions)
    with pytest.raises(ValueError):
        check_binary(wheel, ["cp310-abi3-manylinux_2_28_aarch64"])


@pytest.mark.parametrize("link", [
    "[project](https://github.com/placeholder/milvus-lite-rust)",
    "[project][source]\n[source]: https://github.com/placeholder/milvus-lite-rust",
    "<https://github.com/placeholder/milvus-lite-rust>",
    "https://apache.org.example.invalid/project",
    "https://apache.org@github.com/placeholder/project",
    "<HTTPS://GITHUB.COM/placeholder/project>",
    "`https://github.com/placeholder/project`",
    "```bash\npip install git+https://github.com/placeholder/x\n```",
    "~~~bash\npip install git+ssh://git@github.com/placeholder/x\n~~~",
    "www.github.com/placeholder/project",
])
def test_unconfirmed_external_link_rejected_in_matching_descriptions(link, artifacts):
    root, wheel, sdist, w, s, info, write = artifacts
    addition = ("\n" + link + "\n").encode()
    (root / "README.md").write_bytes((root / "README.md").read_bytes() + addition)
    w[info + "/METADATA"] += addition
    s["PKG-INFO"] += addition
    s["README.md"] += addition
    write()
    with pytest.raises(ValueError, match="unconfirmed external"):
        check(wheel, sdist, root)


@pytest.mark.parametrize("header", ["Project-URL", "Home-page"])
@pytest.mark.parametrize("destination", ["wheel", "sdist"])
def test_unconfirmed_project_metadata_is_rejected(header, destination, artifacts):
    root, wheel, sdist, w, s, info, write = artifacts
    files, key = (w, info + "/METADATA") if destination == "wheel" else (s, "PKG-INFO")
    files[key] = (header + ": https://github.com/placeholder/project\n").encode() + files[key]
    write()
    with pytest.raises(ValueError, match="unconfirmed project address"):
        check(wheel, sdist, root)


@pytest.mark.parametrize("change", ["missing", "duplicate", "wrong_issues", "home_page"])
@pytest.mark.parametrize("destination", ["wheel", "sdist"])
def test_project_metadata_must_be_exactly_the_confirmed_addresses(change, destination, artifacts):
    root, wheel, sdist, w, s, info, write = artifacts
    files, key = (w, info + "/METADATA") if destination == "wheel" else (s, "PKG-INFO")
    issues = b"Project-URL: Issues, https://github.com/GrittyChen/milvus-lite-rust/issues\n"
    if change == "missing":
        files[key] = files[key].replace(issues, b"")
    elif change == "duplicate":
        files[key] = issues + files[key]
    elif change == "wrong_issues":
        files[key] = files[key].replace(issues, issues.replace(b"/issues", b"-issues"))
    else:
        files[key] = b"Home-page: https://github.com/GrittyChen/milvus-lite-rust\n" + files[key]
    write()
    with pytest.raises(ValueError, match="unconfirmed project address"):
        check(wheel, sdist, root)


def test_source_distribution_cannot_pin_user_toolchain(artifacts):
    root, wheel, sdist, _, s, _, write = artifacts
    s["rust-toolchain.toml"] = b'[toolchain]\nchannel = "1.98.1"\n'
    write()
    with pytest.raises(ValueError, match="development toolchain"):
        check(wheel, sdist, root)


@pytest.mark.parametrize("addition,reason", [
    ("[docs][r]\n[r]: docs/private.md", "relative file link"),
    ("[docs][r]\n  [r]: <docs/private.md>", "relative file link"),
    ('<a href="docs/private.md">docs</a>', "raw HTML"),
    ('<img SRC = "docs/private.png">', "raw HTML"),
])
def test_other_relative_links_are_rejected(addition, reason):
    from check_artifacts import check_description
    readme = (Path(__file__).resolve().parents[2] / "README.md").read_text()
    body = readme + "\n" + addition
    with pytest.raises(ValueError, match=reason):
        check_description(body, body)


@pytest.mark.parametrize("addition", [
    "https://github.com/GrittyChen/milvus-lite-rust-evil",
    "https://github.com/GrittyChen/other",
    "https://github.com/grittychen/milvus-lite-rust",
    "http://github.com/GrittyChen/milvus-lite-rust",
    "https://www.github.com/GrittyChen/milvus-lite-rust",
    "https://github.com:443/GrittyChen/milvus-lite-rust",
    "https://x@github.com/GrittyChen/milvus-lite-rust",
    "https://github.com.evil.example/GrittyChen/milvus-lite-rust",
    "[source](https://github.com/GrittyChen/milvus-lite-rust/../../other-owner/other)",
    "[source][r]\n[r]: https://github.com/GrittyChen/milvus-lite-rust/../../other-owner/other",
    "<https://github.com/GrittyChen/milvus-lite-rust/../../other-owner/other>",
    "https://github.com/GrittyChen/milvus-lite-rust/%2e%2e/%2E%2E/other-owner/other",
    "https://github.com/GrittyChen/milvus-lite-rust%2F..%2F..%2Fother-owner%2Fother",
    "https://github.com/GrittyChen/milvus-lite-rust/./issues",
    "https://github.com/GrittyChen/milvus-lite-rust/%252e%252e/other",
    "https://github.com/GrittyChen/milvus-lite-rust/..\\..\\other-owner/other",
    "https://x@apache.org/licenses/",
])
def test_lookalike_project_addresses_are_rejected(addition):
    from check_artifacts import check_description
    readme = (Path(__file__).resolve().parents[2] / "README.md").read_text()
    body = readme + "\n" + addition
    with pytest.raises(ValueError, match="unconfirmed external"):
        check_description(body, body)


def described(addition):
    readme = (Path(__file__).resolve().parents[2] / "README.md").read_text()
    return readme + "\n" + addition + "\n"


@pytest.mark.parametrize("addition", [
    "```bash\npython -m pip install pymilvus milvus-lite-rust\n```",
    "```bash\npip install milvus-lite-rust==0.3.0rc3 --find-links https://github.com/GrittyChen/milvus-lite-rust/releases/expanded_assets/v0.3.0rc3\n```",
    '`uv pip install "milvus_lite_rust==0.3.0rc3"`',
    "```bash\npython -m pip install \\\n  Milvus.Lite.Rust\n```",
    "```bash\npython -m pip install milvus-lite-\\\nrust==0.3.0rc3\n```",
    "`pip3 install milvus-lite-rust[extra]>=0.1`",
    "```bash\npip install milvus-lite-rust==0.3.0rc3 # --no-index\n```",
    "```bash\npip install milvus-lite-rust==0.3.0rc3 && echo --no-index\n```",
    "```bash\npip install --no-index ./x.whl; pip install milvus-lite-rust\n```",
    "```bash\npython -m pip --quiet install milvus-lite-rust==0.3.0rc3\n```",
    "```bash\npip install \"milvus--lite__rust==0.3.0rc3\"\n```",
    "```bash\npip install milvus-lite-rust --prefix --no-index\n```",
    "```bash\npip install milvus\\-lite\\-rust\n```",
    "```bash\npip install \"milvus-\"\"lite-rust\"\n```",
    '`pip install "" milvus-lite-rust`',
    "`uv add milvus-lite-rust`",
    "`pip download milvus-lite-rust`",
    "`uvx --from milvus-lite-rust milvus-lite server --data-dir x`",
    "pip install milvus&#45;lite&#x2D;rust",
    "pip install &#109;ilvus-lite-rust",
    "## pip install milvus-lite-rust",
    "#  milvus-lite-rust",
    "pip install milvus-lite-&shy;rust",
    "pip install milvus-lite-​rust",
    "pip install milvus-lite-**rust**",
    "pip install milvus-lite-<b>rust</b>",
    "pip install milvus-lite-<!-- x -->rust",
    "安装：pip install `milvus-lite-rust`",
    "pip install `milvus-lite-rust`==0.3.0rc3",
    "pip install ``milvus-lite-rust``",
    "pip install [milvus-lite-](https://www.apache.org/ \"a)b\")rust",
    "pip install [milvus-lite-](<https://www.apache.org/a)b>)rust",
    "pip install [milvus-lite-](https://www.apache.org/(a)b)rust",
    "pip install [milvus-lite-][a\\]b]rust",
    "`printf '%s' milvus-lite- rust | tr -d ' ' | xargs pip install`",
    "`python -c \"import subprocess; subprocess.run(['pip', 'install', 'milvus-lite-' 'rust'])\"`",
    "不要用 `pip install milvus-lite-rust --no-index` 以外的写法安装",
    "`pip install --no-index --find-links https://github.com/GrittyChen/milvus-lite-rust/releases/expanded_assets/v0.3.0rc3 milvus-lite-rust==0.3.0rc3`",
    "`pip install ./milvus_lite_rust-0.3.0rc3-cp310-abi3-macosx_11_0_arm64.whl`",
    "python -m pip install --no-index --only-binary :all: --find-links https://github.com/GrittyChen/milvus-lite-rust/releases/expanded_assets/v0.1.0rc2 milvus-lite-rust==0.1.0rc2",
    "python -m pip install --no-index --only-binary :all: --find-links https://github.com/GrittyChen/milvus-lite-rust/releases/expanded_assets/v0.3.0rc3 milvus-lite-rust==0.3.0rc3; pip install milvus-lite-rust",
])
def test_naming_this_package_outside_release_commands_is_rejected(addition):
    from check_artifacts import check_description
    body = described(addition)
    with pytest.raises(ValueError, match="names this package outside the release commands"):
        check_description(body, body)


@pytest.mark.parametrize("addition", [
    "```bash\nN=milvus-lite; pip install $N-rust\n```",
    "```bash\npip install milvus-lite-{rust,x}\n```",
    "```bash\npip install {milvus-lite-r,x}ust\n```",
    "`pip install milvus-lite-ru{s,}t`",
    "    ```python\npip install {milvus-lite-r,x}ust\n    ```",
    "```python\n# pip install {milvus-lite-r,x}ust\n```",
    "pip install milvus-lite-ru&#123;s,&#125;t",
    "价格 &#36;5",
])
def test_shell_expansion_is_rejected(addition):
    from check_artifacts import check_description
    body = described(addition)
    with pytest.raises(ValueError, match="shell expansion"):
        check_description(body, body)


def test_only_the_confirmed_address_itself_is_removed_before_name_detection():
    from check_artifacts import names_this_package
    assert not names_this_package("<https://github.com/GrittyChen/milvus-lite-rust/issues>")
    assert names_this_package("see https://github.com/GrittyChen/milvus-lite-rust-evil")


@pytest.mark.parametrize("version,change,reason", [
    ("0.3.0rc3", "missing", "missing release install command"),
    ("0.1.0rc2", None, "names this package outside the release commands"),
])
def test_release_commands_must_match_this_version(version, change, reason):
    from check_artifacts import check_description, release_install_commands
    body = (Path(__file__).resolve().parents[2] / "README.md").read_text()
    if change == "missing":
        body = body.replace(sorted(release_install_commands(version))[1], "python -m pip install .")
    with pytest.raises(ValueError, match=reason):
        check_description(body, body, version)


REPO_ARG = "--repo GrittyChen/milvus-lite-rust"
WORKFLOW_ARG = "--signer-workflow GrittyChen/milvus-lite-rust/.github/workflows/release.yml"
X86_WHEEL = f"milvus_lite_rust-{VERSION}-cp310-abi3-manylinux_2_28_x86_64.whl"
X86_VERIFY = f"gh attestation verify {X86_WHEEL} {REPO_ARG} {WORKFLOW_ARG}"


def test_verify_commands_cover_exactly_the_four_wheel_platforms():
    from check_artifacts import WHEEL_PLATFORMS, release_verify_commands
    platforms = ["manylinux_2_28_x86_64", "manylinux_2_28_aarch64", "macosx_11_0_arm64", "macosx_11_0_x86_64"]
    assert list(WHEEL_PLATFORMS) == platforms
    assert release_verify_commands(VERSION) == {
        f"gh attestation verify milvus_lite_rust-{VERSION}-cp310-abi3-{p}.whl {REPO_ARG} {WORKFLOW_ARG}" for p in platforms}


def verify_lines(text):
    return [line.strip() for line in text.splitlines() if line.strip().startswith("gh attestation")]


def expected_verify_lines():
    # 正文按平台表的顺序写四条命令；顺序、条数与内容都须一致。
    from check_artifacts import WHEEL_PLATFORMS
    return [f"gh attestation verify milvus_lite_rust-{VERSION}-cp310-abi3-{p}.whl {REPO_ARG} {WORKFLOW_ARG}"
            for p in WHEEL_PLATFORMS]


def assert_verify_commands(text, label):
    # 发行说明不经过长描述检查器；两处的校验命令必须逐字、按序相同，重复或多出的变体同样失败。
    assert verify_lines(text) == expected_verify_lines(), f"{label}: verify commands differ in content, order or count"


def test_readme_and_release_notes_carry_the_same_verify_commands():
    from check_artifacts import release_verify_commands
    root = Path(__file__).resolve().parents[2]
    assert set(expected_verify_lines()) == release_verify_commands(VERSION)
    for path in [root / "README.md", root / "docs" / "releases" / f"{VERSION}.md"]:
        assert_verify_commands(path.read_text(), path.name)


@pytest.mark.parametrize("document", ["README.md", f"docs/releases/{VERSION}.md"])
@pytest.mark.parametrize("mutation", ["duplicate", "swap"])
def test_verify_command_order_and_count_are_enforced(document, mutation):
    text = (Path(__file__).resolve().parents[2] / document).read_text()
    first, second = expected_verify_lines()[:2]
    if mutation == "duplicate":
        changed = text.replace(first + "\n", first + "\n" + first + "\n", 1)
    else:
        changed = text.replace(first + "\n" + second + "\n", second + "\n" + first + "\n", 1)
    assert changed != text
    with pytest.raises(AssertionError, match="differ in content, order or count"):
        assert_verify_commands(changed, document)


def test_wheel_platforms_match_the_candidate_result_tags():
    # 两个模块各自维护平台集合；读取候选结果工具的源码常量，不导入它（避免牵入发行工具的依赖）。
    import ast
    from check_artifacts import WHEEL_PLATFORMS
    source = (Path(__file__).resolve().parents[2] / "tests" / "release" / "candidate.py").read_text()
    tags, = [ast.literal_eval(node.value) for node in ast.parse(source).body
             if isinstance(node, ast.Assign) and any(getattr(t, "id", None) == "WHEEL_TAGS" for t in node.targets)]
    assert set(WHEEL_PLATFORMS) == set(tags) and len(WHEEL_PLATFORMS) == len(tags) == 4


@pytest.mark.parametrize("addition", [
    X86_VERIFY.replace(f"-{VERSION}-", "-0.1.0rc2-"),
    X86_VERIFY.replace("manylinux_2_28_x86_64", "*"),
    X86_VERIFY.replace("cp310-abi3-manylinux_2_28_x86_64", "*"),
    X86_VERIFY.replace("verify milvus", "verify ./milvus"),
    X86_VERIFY.replace("-cp310-abi3-manylinux_2_28_x86_64.whl", ".tar.gz"),
    X86_VERIFY.replace("manylinux_2_28_x86_64", "musllinux_1_2_x86_64"),
    X86_VERIFY.replace("milvus_lite_rust-", "Milvus_Lite_Rust-"),
    X86_VERIFY.replace(REPO_ARG, REPO_ARG + "-evil"),
    X86_VERIFY.replace(REPO_ARG, "--repo other/milvus-lite-rust"),
    X86_VERIFY.replace(REPO_ARG, "--owner GrittyChen"),
    X86_VERIFY.replace("release.yml", "ci.yml"),
    f"gh attestation verify {X86_WHEEL} {REPO_ARG}",
    X86_VERIFY.replace(WORKFLOW_ARG, "--signer-repo GrittyChen/milvus-lite-rust"),
    X86_VERIFY + " --format json",
    X86_VERIFY + f" --source-ref refs/tags/v{VERSION}",
    X86_VERIFY + " --bundle x.jsonl",
    X86_VERIFY + " --help",
    f"gh attestation verify {X86_WHEEL} --deny-self-hosted-runners {REPO_ARG} {WORKFLOW_ARG}",
    f"gh attestation verify {X86_WHEEL} {WORKFLOW_ARG} {REPO_ARG}",
    X86_VERIFY + " || true",
    X86_VERIFY + "; pip install milvus-lite-rust",
    X86_VERIFY + " && pip install milvus-lite-rust",
    X86_VERIFY + " | tee log",
    X86_VERIFY + " # 注释",
    f"gh attestation verify `echo {X86_WHEEL}` {REPO_ARG} {WORKFLOW_ARG}",
    f"`{X86_VERIFY}`",
    X86_VERIFY.replace("attestation verify", "attestation download"),
    f"gh release download v{VERSION} {REPO_ARG}",
    f"gh attestation verify {X86_WHEEL} \\\n  {REPO_ARG} {WORKFLOW_ARG}",
    X86_VERIFY.replace(REPO_ARG, "&#45;" + REPO_ARG[1:]),
])
def test_verify_command_variants_are_rejected(addition):
    from check_artifacts import check_description
    body = described(addition)
    with pytest.raises(ValueError, match="names this package outside the release commands"):
        check_description(body, body)


def test_verify_command_on_an_oci_image_is_rejected():
    # 任何带协议的地址先由地址白名单拒绝，早于按名安装的判定。
    from check_artifacts import check_description
    body = described(f"gh attestation verify oci://ghcr.io/grittychen/milvus-lite-rust {REPO_ARG} {WORKFLOW_ARG}")
    with pytest.raises(ValueError, match="unconfirmed external address"):
        check_description(body, body)


@pytest.mark.parametrize("addition", [
    f"gh attestation verify $WHEEL {REPO_ARG} {WORKFLOW_ARG}",
    f"gh attestation verify $(ls *.whl) {REPO_ARG} {WORKFLOW_ARG}",
    X86_VERIFY.replace(VERSION, "${VERSION}"),
])
def test_verify_command_expansion_is_rejected(addition):
    from check_artifacts import check_description
    body = described(addition)
    with pytest.raises(ValueError, match="shell expansion"):
        check_description(body, body)


@pytest.mark.parametrize("platform", ["manylinux_2_28_x86_64", "manylinux_2_28_aarch64", "macosx_11_0_arm64", "macosx_11_0_x86_64"])
def test_each_verify_command_is_required(platform):
    from check_artifacts import check_description
    readme = (Path(__file__).resolve().parents[2] / "README.md").read_text()
    command = X86_VERIFY.replace("manylinux_2_28_x86_64", platform)
    assert readme.count(command + "\n") == 1
    body = readme.replace(command + "\n", "")
    with pytest.raises(ValueError, match="missing release verify command"):
        check_description(body, body)


@pytest.mark.parametrize("addition", [
    "`pip install https://github.com/GrittyChen/milvus-lite-rust/releases/download/v0.3.0rc3/milvus_lite_rust-0.3.0rc3-cp310-abi3-macosx_11_0_arm64.whl`",
    "`pip install .`",
    "`pip install pymilvus`",
    "`pip uninstall milvus-lite`",
    "[issues](https://github.com/GrittyChen/milvus-lite-rust/issues)",
    "<https://github.com/GrittyChen/milvus-lite-rust>",
    "以 Rust 重写的 milvus-lite，与官方 milvus-lite 互通。",
    "The project's wheels don't need a Rust toolchain.",
    "```python\nclient.insert(\"docs\", [dict(id=2)])\n```",
])
def test_release_installs_and_project_mentions_are_accepted(addition):
    from check_artifacts import check_description
    body = described(addition)
    check_description(body, body)


def test_same_line_mention_of_official_package_and_rust_is_rejected_by_design():
    # 判定按片段顺序而非渲染结果，宁可多拒：这种句子需要调整语序或分行。
    from check_artifacts import check_description
    body = described("本项目是 milvus-lite 的 Rust 实现。")
    with pytest.raises(ValueError, match="names this package"):
        check_description(body, body)


@pytest.mark.parametrize("addition", [
    '`MilvusClient(uri="http://127.0.0.1:19530")`',
    '```python\nuri="http://localhost:19530"\n```',
    '[license][r]\n[r]: <https://www.apache.org/licenses/LICENSE-2.0>',
    '[section](#platform)',
    '[license](HTTPS://WWW.APACHE.ORG/licenses/LICENSE-2.0)',
    '`uri="http://localhost"`',
    'www.apache.org/licenses/LICENSE-2.0',
])
def test_confirmed_links_and_local_rpc_examples_remain_accepted(addition):
    from check_artifacts import check_description
    readme = (Path(__file__).resolve().parents[2] / "README.md").read_text()
    body = readme + "\n" + addition
    check_description(body, body)


@pytest.mark.parametrize(
    "addition",
    [
        "python -m pip install milvus-lite-rust.index-intent.json",
        "最终标记：`<data_dir>/collections/<collection>/milvus-lite-rust.index-intent.json`。; python -m pip install foo",
        "python -m pip install foo # 最终标记：`<data_dir>/collections/<collection>/milvus-lite-rust.index-intent.json`。",
        "最终标记：`<data_dir>/collections/<collection>/milvus-lite-rust.index-intent.json`。\npython -m pip install milvus-lite-rust",
    ],
)
def test_intent_file_reference_cannot_hide_install_commands(addition):
    from check_artifacts import (
        check_install_commands,
        release_install_commands,
        VERSION,
    )

    body = "\n".join(sorted(release_install_commands(VERSION))) + "\n" + addition
    with pytest.raises(ValueError, match="install command check"):
        check_install_commands(body, VERSION)


def test_exact_intent_reference_is_allowed_with_release_commands():
    from check_artifacts import (
        check_install_commands,
        release_install_commands,
        release_verify_commands,
        INDEX_INTENT_REFERENCE,
        VERSION,
    )

    body = (
        "\n".join(sorted(release_install_commands(VERSION) | release_verify_commands(VERSION)))
        + "\n"
        + INDEX_INTENT_REFERENCE
    )
    check_install_commands(body, VERSION)
