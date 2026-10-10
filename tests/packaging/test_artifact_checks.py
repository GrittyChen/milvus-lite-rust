"""缺失或错误的发行内容必须被静态门槛拒绝。"""

import io
from pathlib import Path
import tarfile
import zipfile

import pytest
from check_artifacts import check, check_binary


@pytest.fixture
def artifacts(tmp_path):
    repo = Path(__file__).resolve().parents[2]
    root = tmp_path / "source"
    for name in ["README.md", "LICENSE", "crates/grpc/proto/LICENSE"]:
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes((repo / name).read_bytes())
    metadata = ("Metadata-Version: 2.4\nName: milvus-lite-rust\nVersion: 0.1.0.dev0\nRequires-Python: >=3.10\nLicense-Expression: Apache-2.0\nLicense-File: LICENSE\nLicense-File: crates/grpc/proto/LICENSE\nDescription-Content-Type: text/markdown; charset=UTF-8\n\n" + (root / "README.md").read_text()).encode()
    info = "milvus_lite_rust-0.1.0.dev0.dist-info"
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
        w[info + "/METADATA"] = w[info + "/METADATA"].replace(b"Version: 0.1.0.dev0", b"Version: 9.0")
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
