"""发行归档的内容、长描述与身份检查；不导入被检查的包。"""

import argparse
import configparser
from email.parser import Parser
import hashlib
import html
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import tarfile
import tempfile
import zipfile
from urllib.parse import urlsplit

VERSION = "0.3.0rc3"
# 已确认的公开仓库（0013 候选阶段决策 4）；按路径段匹配，同前缀的其它仓库不放行。
PROJECT = "https://github.com/GrittyChen/milvus-lite-rust"
PROJECT_URLS = {"Repository, " + PROJECT, "Issues, " + PROJECT + "/issues"}
PROJECT_ADDRESS = re.compile(re.escape(PROJECT) + r"(?=[/?#\s<>()\[\]`\"']|$)[^\s<>()\[\]`\"']*")
LOCAL_HOSTS = {"apache.org", "www.apache.org", "127.0.0.1", "localhost"}
WHEEL_PLATFORMS = ("manylinux_2_28_x86_64", "manylinux_2_28_aarch64", "macosx_11_0_arm64", "macosx_11_0_x86_64")
RELEASE_WORKFLOW = ".github/workflows/release.yml"


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


def confirmed_address(address):
    parts = urlsplit(address)
    if parts.username is not None or parts.password is not None:
        return False
    if parts.hostname in LOCAL_HOSTS:
        return True
    project = urlsplit(PROJECT)
    # `..` 段及其编码形式会被客户端整理到仓库边界之外；本项目地址不需要编码，一律拒绝。
    path = parts.path
    if "%" in path or "\\" in path or {".", ".."} & set(path.split("/")):
        return False
    return (parts.scheme, parts.netloc) == (project.scheme, project.netloc) and (path == project.path or path.startswith(project.path + "/"))


def release_install_commands(version):
    return {
        f"python -m pip install --no-index --only-binary :all: --find-links {PROJECT}/releases/expanded_assets/v{version} milvus-lite-rust=={version}",
        f"shasum -a 256 -c --ignore-missing SHA256SUMS && python -m pip install --no-index ./milvus_lite_rust-{version}-*.whl",
    }


def release_verify_commands(version):
    # 来源证明的核对命令同样按整行白名单放行：只认本仓库、发行工作流签发与完整的版本化
    # wheel 文件名，每个平台一条；不接受通配符、额外参数或其它 gh 子命令。公开下载工作流
    # 只从 release_install_commands 取安装命令，校验命令因此单列，不混入安装命令。
    repo = PROJECT.removeprefix("https://github.com/")
    return {f"gh attestation verify milvus_lite_rust-{version}-cp310-abi3-{platform}.whl --repo {repo} "
            f"--signer-workflow {repo}/{RELEASE_WORKFLOW}" for platform in WHEEL_PLATFORMS}


INDEX_INTENT_REFERENCE = "最终标记：`<data_dir>/collections/<collection>/milvus-lite-rust.index-intent.json`。"

def names_this_package(text):
    # 同一行内 milvus、lite、rust 依次出现（不区分大小写）即视为引用本包名，三段之间插入的
    # 引号、转义、Markdown 与 HTML 标记或链接拆不开这一判定。已确认仓库地址本身不是安装
    # 命令，先移除。词内拆字与运行时变换属于刻意混淆，不在范围。
    return re.search("milvus.*lite.*rust", PROJECT_ADDRESS.sub("", text), flags=re.I) is not None


def check_install_commands(body, version):
    # 名称可能被他人在 PyPI 注册（0013 候选阶段决策 6）。检查器防的是按常规写法误写的按名
    # 安装，不解析 shell 或 Markdown：除标题外，引用本包名的行只能是固定的 Release 安装
    # 命令或来源证明校验命令。`$` 与花括号是最常见的拼接手段且 README 用不到，一律拒绝；其余运行时计算
    # 不在范围，由 README 评审把关。
    installs, verifies = release_install_commands(version), release_verify_commands(version)
    commands = installs | verifies
    seen = set()
    for line in re.sub(r"\\\n", "", body).splitlines():
        line = line.strip()
        for text in (line, html.unescape(line)):
            require(not re.search(r"[${}]", text), "install command check: shell expansion")
        if line in commands:
            seen.add(line)
        # 私有协议文件名含包名；只允许完整的恢复路径说明，不能夹带任何命令。
        elif line not in {"# milvus-lite-rust", INDEX_INTENT_REFERENCE}:
            for text in (line, html.unescape(line)):
                require(not names_this_package(text), "install command check: names this package outside the release commands")
    require(installs <= seen, "install command check: missing release install command")
    require(verifies <= seen, "install command check: missing release verify command")


def check_description(body, expected, version=VERSION):
    require(body.rstrip() == expected.rstrip(), "long description differs from canonical README")
    for token in ["非官方", "无隶属关系", "pip uninstall milvus-lite", "pymilvus[milvus-lite]", "Windows", "free-threaded", "sdist", "Intel", "3.10–3.15"]:
        require(token in body, "missing user documentation: " + token)
    # 安装代码也能引入远端项目地址，不能跳过代码片段；本地 RPC 示例由精确主机名单放行。
    # 渲染器会解码链接中的字符引用，原文与解码后的文本都要核对。
    for text in (body, html.unescape(body)):
        require(not re.search(r"\b(?:href|src)\s*=", text, flags=re.I), "raw HTML link attributes in long description")
        targets = re.findall(r"\]\(([^)]+)\)", text)
        targets += re.findall(r"^\s*\[[^\]]+\]:\s*(\S+)", text, flags=re.M)
        for target in targets:
            target = target.removeprefix("<").split(">", 1)[0]
            require(target.lower().startswith(("https://", "http://", "#")), "relative file link in long description")
        for target in re.findall(r"(?:[a-z][a-z0-9+.-]*://|\bwww\.)[^\s<>\[\]()`]+", text, flags=re.I):
            address = target if "://" in target else "https://" + target
            require(confirmed_address(address.rstrip("\"'")), "unconfirmed external address in long description")
    check_install_commands(body, version)


def check_metadata(raw, readme, version):
    metadata = Parser().parsestr(raw.decode("utf-8"))
    require(metadata["Name"] == "milvus-lite-rust", "wrong distribution name")
    require(metadata["Version"] == version, "wrong artifact version")
    require(metadata["Requires-Python"] == ">=3.10", "unexpected Python bound")
    require(metadata["License-Expression"] == "Apache-2.0", "wrong license expression")
    require((metadata["Description-Content-Type"] or "").split(";", 1)[0].strip() == "text/markdown", "missing markdown description")
    require(set(metadata.get_all("License-File", [])) == {"LICENSE", "crates/grpc/proto/LICENSE"}, "missing license declaration")
    require(set(metadata.get_all("Project-URL", [])) == PROJECT_URLS and len(metadata.get_all("Project-URL", [])) == len(PROJECT_URLS) and not metadata.get_all("Home-page"), "unconfirmed project address in metadata")
    check_description(metadata.get_payload(), readme, version)


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


def check(wheel, sdist, repo, version=VERSION, kind="formal"):
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
        require(tags and all(t.startswith("cp310-abi3-") and t.removeprefix("cp310-abi3-") in WHEEL_PLATFORMS for t in tags), "unexpected wheel tag")
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
    parser.add_argument("--version", default=VERSION)
    parser.add_argument("--kind", choices=["formal", "fi"], default="formal")
    parser.add_argument("--binary", action="store_true")
    args = parser.parse_args()
    result = check(args.wheel, args.sdist, args.repo, args.version, args.kind)
    if args.binary:
        result["binary"] = check_binary(args.wheel, result["tags"])
    print(json.dumps(result))


if __name__ == "__main__":
    main()
