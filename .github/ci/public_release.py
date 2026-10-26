"""公开下载核验工作流的默认分支侧工具（0021 第 20 条）：Release 属性、匿名下载、tag 提交、公开安装与冒烟。

发行集合本身（证明、候选结果与哈希）由冻结 tag 中的 release_set.py 核对，本工具不重复实现。分工如下：
- check-version：任何 API 调用、tag 拼接或 checkout 之前，以候选版本正则校验输入；
- fetch：用 token 读取 Release 元数据，要求已发布、预发布且不可变，附件名与公开地址合法，记录 API digest；
- download：不带任何认证，从公开地址下载附件，大小与 SHA256 须与 API 记录一致；
- tag-commit：release_set 通过后，tag 解引用的提交须等于候选结果的冻结提交；
- install：干净环境中按冻结 tag 的 README 安装命令从公开地址安装，pip report 须指向本平台的公开 wheel 及核验过的哈希；
- smoke：运行冻结 tag 的 feature_smoke.py，输出的版本与 SDK 须为预期值。
"""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import urllib.error
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tests" / "release"))

import candidate as c  # noqa: E402

API = "https://api.github.com"
# 附件名会成为本地文件名，只允许不含路径分隔符、不以点开头的普通名称。
ASSET_NAME = re.compile(r"[A-Za-z0-9_-][A-Za-z0-9._-]*")
TOKEN_VARIABLES = ("GH_TOKEN", "GITHUB_TOKEN")
PACKAGE = "milvus-lite-rust"
USER_AGENT = "milvus-lite-rust-public-check"


class PublicCheckError(Exception):
    pass


def require(condition, message):
    if not condition:
        raise PublicCheckError(message)


def check_version(version):
    require(isinstance(version, str) and c.VERSION.fullmatch(version), f"bad version {version!r}")


def read(request, urlopen, timeout):
    try:
        with urlopen(request, timeout=timeout) as response:
            return response.read()
    except urllib.error.HTTPError as error:
        raise PublicCheckError(f"GET {request.full_url} returned HTTP {error.code}") from error


def api_get(path, token, urlopen=urllib.request.urlopen):
    request = urllib.request.Request(f"{API}/{path}", headers={
        "Accept": "application/vnd.github+json", "Authorization": f"Bearer {token}",
        "X-GitHub-Api-Version": "2022-11-28", "User-Agent": USER_AGENT})
    return json.loads(read(request, urlopen, 60))


def check_release(data, repo, version):
    """返回附件记录列表；不判断附件集合是否为有效发行集合，那是 release_set 的职责。"""
    tag = f"v{version}"
    require(data.get("tag_name") == tag, f"release tag is {data.get('tag_name')!r}, not {tag}")
    require(data.get("draft") is False, "release is a draft")
    require(data.get("prerelease") is True, "release is not a prerelease")
    require(data.get("immutable") is True, "release is not immutable")
    assets, names = [], set()
    for asset in data.get("assets", []):
        name = asset.get("name")
        require(isinstance(name, str) and ASSET_NAME.fullmatch(name), f"bad asset name {name!r}")
        require(name not in names, f"duplicate asset {name}")
        names.add(name)
        require(asset.get("state") == "uploaded", f"asset {name} is {asset.get('state')!r}")
        url = f"https://github.com/{repo}/releases/download/{tag}/{name}"
        require(asset.get("browser_download_url") == url, f"asset {name} is not served from {url}")
        digest = asset.get("digest")
        require(isinstance(digest, str) and digest.startswith("sha256:") and c.HEX64.fullmatch(digest[7:]),
                f"asset {name} has no sha256 digest")
        size = asset.get("size")
        require(isinstance(size, int) and size >= 0, f"asset {name} has no size")
        assets.append({"name": name, "url": url, "sha256": digest[7:], "size": size})
    return assets


def fetch(repo, version, out, token, urlopen=urllib.request.urlopen):
    data = api_get(f"repos/{repo}/releases/tags/v{version}", token, urlopen)
    assets = check_release(data, repo, version)
    record = {"repo": repo, "version": version, "release_id": data.get("id"), "html_url": data.get("html_url"),
              "assets": assets}
    Path(out).write_text(json.dumps(record, indent=2) + "\n")
    return record


def download(record, out, environ=os.environ, urlopen=urllib.request.urlopen):
    # 公开下载是这次核验要证明的事实，这一步的进程不应拿到 token。
    require(not any(environ.get(name) for name in TOKEN_VARIABLES), "download step must not receive a token")
    out = Path(out)
    out.mkdir(parents=True)
    for asset in record["assets"]:
        body = read(urllib.request.Request(asset["url"], headers={"User-Agent": USER_AGENT}), urlopen, 300)
        require(len(body) == asset["size"], f"{asset['name']} is {len(body)} bytes, API says {asset['size']}")
        require(hashlib.sha256(body).hexdigest() == asset["sha256"], f"{asset['name']} differs from the API digest")
        (out / asset["name"]).write_bytes(body)
        print(f"downloaded {asset['name']} {asset['sha256']}")


def git(repo, *args):
    result = subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True)
    require(result.returncode == 0, f"git {' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout.strip()


def tag_commit(directory, tag_dir, version):
    found = sorted(Path(directory).glob(f"candidate-{version}-*.json"))
    require(len(found) == 1, f"expected one candidate result, found {[p.name for p in found]}")
    # 候选结果已由冻结 tag 的 release_set 完整核验；这里不用默认分支的 candidate.load，
    # 它按当前阶段矩阵会拒绝较早的候选。
    commit = json.loads(found[0].read_text()).get("commit")
    require(isinstance(commit, str) and c.HEX40.fullmatch(commit), f"candidate commit is {commit!r}")
    head = git(tag_dir, "rev-parse", "HEAD")
    tagged = git(tag_dir, "rev-parse", f"refs/tags/v{version}^{{commit}}")
    require(head == tagged == commit, f"tag v{version} is {tagged} (checkout {head}), candidate commit is {commit}")
    return commit


def load_module(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def readme_install_command(tag_dir, version):
    commands = load_module(Path(tag_dir) / "tests" / "packaging" / "check_artifacts.py",
                           "frozen_check_artifacts").release_install_commands(version)
    found = [command for command in commands if command.startswith("python -m pip install ")]
    require(len(found) == 1, f"expected one pip install command, found {found}")
    lines = {line.strip() for line in (Path(tag_dir) / "README.md").read_text().splitlines()}
    require(found[0] in lines, "the frozen README does not contain the release install command")
    return found[0]


def install_argv(command, python, report):
    # 不经 shell：只替换解释器，其余参数原样保留。
    argv = shlex.split(command)
    require(argv[:4] == ["python", "-m", "pip", "install"], f"unexpected install command {command!r}")
    return [str(python), *argv[1:], "--no-cache-dir", "--report", str(report)]


def wheel_name(version, platform):
    name = f"milvus_lite_rust-{version}-cp310-abi3-{platform}.whl"
    require(name in c.formal_names(version), f"unknown wheel platform {platform}")
    return name


def check_sums(path, expected_sha256):
    require(c.sha256_file(path) == expected_sha256, "verified SHA256SUMS changed between jobs")
    return c.parse_sums(Path(path).read_text())


def check_clean(python):
    probe = ("import importlib.metadata as m, importlib.util as u, json; "
             "print(json.dumps([u.find_spec('milvus_lite') is not None, "
             "sorted(d.metadata['Name'] for d in m.distributions() "
             "if d.metadata['Name'].lower().replace('_', '-') in ('milvus-lite', 'milvus-lite-rust'))]))")
    found, dists = json.loads(subprocess.run([str(python), "-I", "-c", probe], check=True, capture_output=True,
                                             text=True).stdout)
    require(not found and not dists, f"environment already provides milvus_lite: {dists}")


def check_report(report, repo, version, platform, sums):
    name = wheel_name(version, platform)
    items = report.get("install", [])
    require(len(items) == 1, f"pip installed {len(items)} distributions, expected only {PACKAGE}")
    item = items[0]
    meta = item.get("metadata", {})
    require(str(meta.get("name", "")).lower().replace("_", "-") == PACKAGE, f"pip installed {meta.get('name')!r}")
    require(meta.get("version") == version, f"pip installed version {meta.get('version')!r}")
    info = item.get("download_info", {})
    url = f"https://github.com/{repo}/releases/download/v{version}/{name}"
    require(info.get("url") == url, f"pip downloaded {info.get('url')!r}, expected {url}")
    digest = info.get("archive_info", {}).get("hashes", {}).get("sha256")
    require(name in sums and digest == sums[name], f"pip downloaded {name} with sha256 {digest}")
    return name


def install(args):
    sums = check_sums(args.sums, args.sums_sha256)
    check_clean(args.python)
    argv = install_argv(readme_install_command(args.tag_dir, args.version), args.python, args.report)
    print("+ " + shlex.join(argv), flush=True)
    subprocess.run(argv, check=True)
    name = check_report(json.loads(Path(args.report).read_text()), args.repo, args.version, args.platform, sums)
    print(f"installed {name} {sums[name]} from the public release")


def check_smoke(output, version, sdk):
    lines = [line for line in output.splitlines() if line.strip()]
    require(lines, "feature smoke printed nothing")
    try:
        identity = json.loads(lines[-1])
    except json.JSONDecodeError as error:
        raise PublicCheckError(f"feature smoke output is not JSON: {error}") from error
    require(identity.get("passed") is True, "feature smoke did not pass")
    require(identity.get("version") == version, f"feature smoke loaded version {identity.get('version')!r}")
    require(identity.get("sdk") == sdk, f"feature smoke used pymilvus {identity.get('sdk')!r}")
    return identity


def smoke(args):
    probe = Path(args.tag_dir) / "tests" / "platform" / "feature_smoke.py"
    argv = [str(args.python), "-I", str(probe), "--repo", str(args.tag_dir), "--work-dir", str(args.work_dir)]
    result = subprocess.run(argv, capture_output=True, text=True)
    sys.stderr.write(result.stderr)
    sys.stdout.write(result.stdout)
    # 管道中的 stdout 是块缓冲，先刷出冒烟输出，拒绝信息才会排在它后面。
    sys.stdout.flush()
    require(result.returncode == 0, f"feature smoke exited with {result.returncode}")
    print(json.dumps(check_smoke(result.stdout, args.version, args.sdk)))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("check-version")
    p.add_argument("--version", required=True)
    p = sub.add_parser("fetch")
    p.add_argument("--repo", required=True)
    p.add_argument("--version", required=True)
    p.add_argument("--out", type=Path, required=True)
    p = sub.add_parser("download")
    p.add_argument("--record", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    p = sub.add_parser("tag-commit")
    p.add_argument("--dir", type=Path, required=True)
    p.add_argument("--tag-dir", type=Path, required=True)
    p.add_argument("--version", required=True)
    p = sub.add_parser("install")
    p.add_argument("--repo", required=True)
    p.add_argument("--version", required=True)
    p.add_argument("--platform", required=True)
    p.add_argument("--tag-dir", type=Path, required=True)
    p.add_argument("--python", type=Path, required=True)
    p.add_argument("--sums", type=Path, required=True)
    p.add_argument("--sums-sha256", required=True)
    p.add_argument("--report", type=Path, required=True)
    p = sub.add_parser("smoke")
    p.add_argument("--version", required=True)
    p.add_argument("--sdk", required=True)
    p.add_argument("--tag-dir", type=Path, required=True)
    p.add_argument("--python", type=Path, required=True)
    p.add_argument("--work-dir", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        if "version" in args:
            check_version(args.version)
        if args.command == "check-version":
            print(f"version {args.version} is canonical")
        elif args.command == "fetch":
            token = os.environ.get("GH_TOKEN")
            require(token, "GH_TOKEN is required to read the release")
            record = fetch(args.repo, args.version, args.out, token)
            print(f"release {record['html_url']}: {[a['name'] for a in record['assets']]}")
        elif args.command == "download":
            download(json.loads(args.record.read_text()), args.out)
        elif args.command == "tag-commit":
            print(f"tag v{args.version} is candidate commit {tag_commit(args.dir, args.tag_dir, args.version)}")
        elif args.command == "install":
            install(args)
        else:
            smoke(args)
    except (PublicCheckError, c.CandidateError) as error:
        print(f"public check rejected: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
