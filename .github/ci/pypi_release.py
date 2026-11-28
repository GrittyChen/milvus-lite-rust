"""PyPI / TestPyPI 上传与上传后可见性核对的默认分支侧工具（0042）。

发行集合、候选结果与来源证明仍由冻结 tag 中的 release_set.py 核对，Release 属性、匿名下载与 tag 提交由
public_release.py 核对，本工具不重复实现。分工如下：
- guard：上传前的版本守卫：台账为 released、不是流程测试版本、目标为 pypi 时冻结 tag 已纳入 PyPI，
  目标索引上的现有文件都属于本组且同名同哈希；
- files：上传 job 中按 verify job 输出的摘要核对 SHA256SUMS 与五个文件，按 wheel 在前、sdist 最后的顺序列出；
- wait-visible：上传后在固定上限内等待目标索引恰好列出这五个文件，哈希与 SHA256SUMS 一致。

本工具放在 .github/ci/，不改 tests/release/ 下的签发工具：那六个文件属于 0039 的签发路径。
"""

import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import time
import urllib.error
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tests" / "release"))

import candidate as c  # noqa: E402
import ledger  # noqa: E402

PACKAGE = "milvus-lite-rust"
USER_AGENT = "milvus-lite-rust-pypi-publish"
INDEXES = {
    "pypi": {"json": "https://pypi.org/pypi", "files": "https://files.pythonhosted.org/"},
    "testpypi": {"json": "https://test.pypi.org/pypi", "files": "https://test-files.pythonhosted.org/"},
}
# 0.0.1rcNNN 是发行流程的测试版本（台账中 0.0.1rc903 为 released），永远不上传。
FLOW_TEST_PREFIX = "0.0."


class PublishError(Exception):
    pass


def require(condition, message):
    if not condition:
        raise PublishError(message)


def check_version(version):
    require(isinstance(version, str) and c.VERSION.fullmatch(version), f"bad version {version!r}")
    require(not version.startswith(FLOW_TEST_PREFIX), f"{version} is a release-flow test version")


def check_target(target):
    require(target in INDEXES, f"unknown target {target!r}")


def check_ledger(text, version):
    entry = ledger.loads(text).get(version)
    require(entry is not None, f"{version} is not in the ledger")
    require(entry["outcome"] == "released", f"ledger outcome for {version} is {entry['outcome']}")


def load_module(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def frozen_pypi_command(tag_dir, version):
    """冻结 tag 纳入 PyPI 的判据：它的检查器导出 PyPI 安装命令，且冻结 README 含这条命令。

    已发布的旧版本没有这个导出，按“不含”处理：它们的 README 写着没有发布到 PyPI，永远不上传。"""
    checker = load_module(Path(tag_dir) / "tests" / "packaging" / "check_artifacts.py", "frozen_check_artifacts")
    make = getattr(checker, "pypi_install_command", None)
    require(callable(make), f"the frozen tools of v{version} do not publish to PyPI")
    command = make(version)
    lines = {line.strip() for line in (Path(tag_dir) / "README.md").read_text().splitlines()}
    require(command in lines, f"the frozen README of v{version} does not contain the PyPI install command")
    return command


def read_sums(path, expected_sha256=None):
    if expected_sha256 is not None:
        require(c.sha256_file(path) == expected_sha256, "verified SHA256SUMS changed between jobs")
    sums = c.parse_sums(Path(path).read_text())
    return sums


def formal_sums(sums, version):
    names = c.formal_names(version)
    missing = [name for name in names if name not in sums]
    require(not missing, f"SHA256SUMS lacks {missing}")
    return {name: sums[name] for name in names}


def index_files(target, version, urlopen=urllib.request.urlopen):
    """返回 {文件名: sha256}；版本或项目不存在（HTTP 404）时为空。其余 HTTP 错误原样失败。"""
    url = f"{INDEXES[target]['json']}/{PACKAGE}/{version}/json"
    request = urllib.request.Request(url, headers={"Accept": "application/json", "User-Agent": USER_AGENT})
    try:
        with urlopen(request, timeout=60) as response:
            data = json.loads(response.read())
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return {}
        raise PublishError(f"GET {url} returned HTTP {error.code}") from error
    except json.JSONDecodeError as error:
        raise PublishError(f"GET {url} returned invalid JSON: {error}") from error
    files = {}
    for item in data.get("urls", []):
        name = item.get("filename")
        digest = item.get("digests", {}).get("sha256")
        require(isinstance(name, str) and isinstance(digest, str) and c.HEX64.fullmatch(digest),
                f"index lists a malformed file {item!r}")
        require(name not in files, f"index lists {name} twice")
        address = item.get("url")
        require(isinstance(address, str) and address.startswith(INDEXES[target]["files"])
                and address.endswith("/" + name), f"index serves {name} from {address!r}")
        files[name] = digest
    return files


def check_existing(existing, expected):
    """索引上的文件必须全部属于本组且同名同哈希；返回尚未上传的文件名。"""
    foreign = sorted(set(existing) - set(expected))
    require(not foreign, f"index already has files outside this release: {foreign}")
    for name, digest in existing.items():
        require(digest == expected[name], f"index has {name} with sha256 {digest}, release has {expected[name]}")
    return [name for name in expected if name not in existing]


def upload_order(names):
    # wheel 在前、sdist 最后：中途失败时不会出现只有 sdist 的状态，让缺 wheel 的平台落到本机构建。
    return sorted(n for n in names if n.endswith(".whl")) + sorted(n for n in names if not n.endswith(".whl"))


def guard(args, urlopen=urllib.request.urlopen):
    check_ledger(Path(args.ledger).read_text(), args.version)
    if args.target == "pypi":
        frozen_pypi_command(args.tag_dir, args.version)
    expected = formal_sums(read_sums(Path(args.dir) / "SHA256SUMS"), args.version)
    for name, digest in expected.items():
        require(c.sha256_file(Path(args.dir) / name) == digest, f"{name} differs from SHA256SUMS")
    pending = check_existing(index_files(args.target, args.version, urlopen), expected)
    print(f"{args.target}: {len(expected) - len(pending)} of {len(expected)} files already uploaded; pending {pending}")
    return pending


def files(args):
    expected = formal_sums(read_sums(args.sums, args.sums_sha256), args.version)
    for name, digest in expected.items():
        require(c.sha256_file(Path(args.dir) / name) == digest, f"{name} differs from the verified SHA256SUMS")
    for name in upload_order(expected):
        print(Path(args.dir) / name)


def wait_visible(args, urlopen=urllib.request.urlopen, clock=time.monotonic, sleep=time.sleep):
    """固定上限内等待索引恰好列出这五个文件（0042 决策 5）：这是等待可见，不是重试判定。

    索引上出现本组以外的文件或哈希不同，立即失败，不再等待；只有“还没看到”与连接失败会继续等待。"""
    expected = formal_sums(read_sums(args.sums, args.sums_sha256), args.version)
    deadline = clock() + args.timeout
    while True:
        try:
            pending = check_existing(index_files(args.target, args.version, urlopen), expected)
            reason = f"not yet listed: {pending}"
        except OSError as error:
            # 连接失败、读取响应体时超时或被重置（URLError、TimeoutError、ConnectionError 都是 OSError）；
            # 非 404 的 HTTP 错误已在 index_files 中转成 PublishError，不在这里继续等。
            pending, reason = None, f"connection failed: {error}"
        if pending == []:
            print(f"{args.target} lists exactly the {len(expected)} release files with matching sha256")
            return
        require(clock() + args.interval <= deadline, f"{args.target} after {args.timeout} s: {reason}")
        print(f"waiting for {args.target}: {reason}", flush=True)
        sleep(args.interval)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("guard")
    p.add_argument("--version", required=True)
    p.add_argument("--target", required=True)
    p.add_argument("--tag-dir", type=Path, required=True)
    p.add_argument("--ledger", type=Path, required=True)
    p.add_argument("--dir", type=Path, required=True)
    p = sub.add_parser("files")
    p.add_argument("--version", required=True)
    p.add_argument("--dir", type=Path, required=True)
    p.add_argument("--sums", type=Path, required=True)
    p.add_argument("--sums-sha256", required=True)
    p = sub.add_parser("wait-visible")
    p.add_argument("--version", required=True)
    p.add_argument("--target", required=True)
    p.add_argument("--sums", type=Path, required=True)
    p.add_argument("--sums-sha256", required=True)
    p.add_argument("--timeout", type=int, default=600)
    p.add_argument("--interval", type=int, default=15)
    args = parser.parse_args(argv)
    try:
        check_version(args.version)
        if "target" in args:
            check_target(args.target)
        if args.command == "guard":
            guard(args)
        elif args.command == "files":
            files(args)
        else:
            wait_visible(args)
    except (PublishError, c.CandidateError, OSError) as error:
        print(f"pypi publish rejected: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
