"""发行归档的内容、长描述与身份检查；不导入被检查的包。"""

import argparse
import configparser
from email.parser import Parser
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import tarfile
import tempfile
import zipfile
from urllib.parse import urlsplit


def require(condition, reason):
    if not condition:
        raise ValueError(reason)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def safe_names(names):
    require(len(names) == len(set(names)), "duplicate archive entry")
    for name in names:
        parts = PurePosixPath(name).parts
        require(not name.startswith("/") and ".." not in parts and "\\" not in name, "unsafe archive path")
        require(not any(p in {".workbuddy", "references", ".git", "__pycache__"} or p.endswith(".db") for p in parts), "private or runtime content in artifact")


def check_description(body, expected):
    require(body.rstrip() == expected.rstrip(), "long description differs from canonical README")
    for token in ["非官方", "无隶属关系", "pip uninstall milvus-lite", "pymilvus[milvus-lite]", "Windows", "free-threaded", "sdist", "Intel", "3.10–3.15"]:
        require(token in body, "missing user documentation: " + token)
    # 安装代码也能引入远端项目地址，不能跳过代码片段；本地 RPC 示例由精确主机名单放行。
    require(not re.search(r"\b(?:href|src)\s*=", body, flags=re.I), "raw HTML link attributes in long description")
    targets = re.findall(r"\]\(([^)]+)\)", body)
    targets += re.findall(r"^\s*\[[^\]]+\]:\s*(\S+)", body, flags=re.M)
    for target in targets:
        target = target.removeprefix("<").split(">", 1)[0]
        require(target.lower().startswith(("https://", "http://", "#")), "relative file link in long description")
    for target in re.findall(r"(?:[a-z][a-z0-9+.-]*://|\bwww\.)[^\s<>\[\]()`]+", body, flags=re.I):
        address = target if "://" in target else "https://" + target
        require(urlsplit(address.rstrip("\"'")).hostname in {"apache.org", "www.apache.org", "127.0.0.1", "localhost"}, "unconfirmed external address in long description")


def check_metadata(raw, readme, version):
    metadata = Parser().parsestr(raw.decode("utf-8"))
    require(metadata["Name"] == "milvus-lite-rust", "wrong distribution name")
    require(metadata["Version"] == version, "wrong artifact version")
    require(metadata["Requires-Python"] == ">=3.10", "unexpected Python bound")
    require(metadata["License-Expression"] == "Apache-2.0", "wrong license expression")
    require((metadata["Description-Content-Type"] or "").split(";", 1)[0].strip() == "text/markdown", "missing markdown description")
    require(set(metadata.get_all("License-File", [])) == {"LICENSE", "crates/grpc/proto/LICENSE"}, "missing license declaration")
    require(not metadata.get_all("Project-URL") and not metadata.get_all("Home-page"), "unconfirmed project address in metadata")
    check_description(metadata.get_payload(), readme)


def check_binary(wheel, tags):
    with zipfile.ZipFile(wheel) as archive, tempfile.TemporaryDirectory() as directory:
        name, = [n for n in archive.namelist() if n.startswith("milvus_lite/_native") and n.endswith(".so")]
        binary = Path(directory) / "native.so"
        binary.write_bytes(archive.read(name))
        if "macosx" in tags[0]:
            arch = subprocess.check_output(["lipo", "-archs", str(binary)], text=True).strip()
            expected = "arm64" if tags[0].endswith("arm64") else "x86_64"
            require(arch == expected, "Mach-O architecture differs from wheel tag")
            output = subprocess.check_output(["otool", "-l", str(binary)], text=True)
            # 只解释部署版本所在的 load command，避免把其它库版本误当系统下限。
            blocks = re.split(r"Load command \d+", output)
            minimum = [re.search(r"(?:minos|version)\s+([\d.]+)", b).group(1) for b in blocks if "LC_BUILD_VERSION" in b or "LC_VERSION_MIN_MACOSX" in b]
            require(minimum and all(tuple(map(int, v.split(".")[:2])) <= (11, 0) for v in minimum), "Mach-O deployment target exceeds macOS 11")
            return {"architecture": arch, "minimum_os": minimum}
        header = subprocess.check_output(["readelf", "-h", str(binary)], text=True)
        require(("AArch64" if tags[0].endswith("aarch64") else "X86-64") in header, "ELF architecture differs from wheel tag")
        output = subprocess.check_output(["readelf", "--version-info", str(binary)], text=True)
        versions = sorted({tuple(map(int, v.split("."))) for v in re.findall(r"GLIBC_(\d+\.\d+(?:\.\d+)?)", output)})
        require(versions and all(v[:2] <= (2, 28) for v in versions), "ELF requires glibc newer than 2.28")
        require("GLIBC_PRIVATE" not in output, "private glibc ABI")
        return {"glibc_versions": versions}


def check(wheel, sdist, repo, version="0.1.0.dev0", kind="formal"):
    repo = Path(repo)
    readme = (repo / "README.md").read_text()
    licenses = {p: (repo / p).read_bytes() for p in ["LICENSE", "crates/grpc/proto/LICENSE"]}
    with zipfile.ZipFile(wheel) as archive:
        names = archive.namelist()
        safe_names(names)
        infos = [n.removesuffix("/METADATA") for n in names if n.endswith(".dist-info/METADATA")]
        require(len(infos) == 1, "one dist-info directory required")
        info = infos[0]
        check_metadata(archive.read(info + "/METADATA"), readme, version)
        for name, content in licenses.items():
            require(archive.read(info + "/licenses/" + name) == content, "wrong license content: " + name)
        entry = configparser.ConfigParser()
        entry.read_string(archive.read(info + "/entry_points.txt").decode())
        require(entry["console_scripts"]["milvus-lite"] == "milvus_lite.cmdline:main", "missing CLI entrypoint")
        tags = [line[5:] for line in archive.read(info + "/WHEEL").decode().splitlines() if line.startswith("Tag: ")]
        allowed = {"manylinux_2_28_x86_64", "manylinux_2_28_aarch64", "macosx_11_0_arm64", "macosx_11_0_x86_64"}
        require(tags and all(t.startswith("cp310-abi3-") and t.removeprefix("cp310-abi3-") in allowed for t in tags), "unexpected wheel tag")
        native = [n for n in names if n.startswith("milvus_lite/_native") and n.endswith((".so", ".pyd"))]
        require(len(native) == 1, "one native module required")
        data = archive.read(native[0])
        hooks = [marker in data for marker in [b"_pause_compaction", b"_inject_compaction_fault"]]
        require(all(hooks) if kind == "fi" else not any(hooks), "formal/FI artifact mismatch")
        for name in ["milvus_lite/__init__.py", "milvus_lite/cmdline.py", "milvus_lite/__main__.py", "milvus_lite/server_manager.py"]:
            require(name in names, "missing module " + name)
    with tarfile.open(sdist) as archive:
        members = archive.getmembers()
        names = [m.name for m in members]
        safe_names(names)
        require(all(not m.issym() and not m.islnk() for m in members), "symlink in sdist")
        roots = {PurePosixPath(n).parts[0] for n in names}
        require(len(roots) == 1, "one sdist root required")
        root = roots.pop()
        require(all(PurePosixPath(n).name != "rust-toolchain.toml" for n in names), "development toolchain must not be included in sdist")
        def read(name):
            file = archive.extractfile(root + "/" + name)
            require(file is not None, "missing source file " + name)
            return file.read()
        check_metadata(read("PKG-INFO"), readme, version)
        require(read("README.md").decode() == readme, "wrong sdist README")
        for name, content in licenses.items():
            require(read(name) == content, "wrong sdist license " + name)
        for name in ["Cargo.toml", "Cargo.lock", "pyproject.toml", "crates/core/Cargo.toml", "crates/grpc/Cargo.toml", "crates/py/Cargo.toml", "crates/py/build.rs", "crates/py/python/milvus_lite/cmdline.py"]:
            read(name)
        require(root + "/crates/py/pyproject.toml" not in names, "duplicate packaging configuration")
    return {"version": version, "kind": kind, "wheel_sha256": digest(Path(wheel).read_bytes()), "sdist_sha256": digest(Path(sdist).read_bytes()), "native_sha256": digest(data), "tags": tags}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wheel", required=True, type=Path)
    parser.add_argument("--sdist", required=True, type=Path)
    parser.add_argument("--repo", required=True, type=Path)
    parser.add_argument("--version", default="0.1.0.dev0")
    parser.add_argument("--kind", choices=["formal", "fi"], default="formal")
    parser.add_argument("--binary", action="store_true")
    args = parser.parse_args()
    result = check(args.wheel, args.sdist, args.repo, args.version, args.kind)
    if args.binary:
        result["binary"] = check_binary(args.wheel, result["tags"])
    print(json.dumps(result))


if __name__ == "__main__":
    main()
