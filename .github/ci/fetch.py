"""内容由 sha256 固定的下载（0021 第 11 条的传输层有限重试），以及按平台取固定清单中的 uv wheel。

下载失败（curl 非零退出，含 DNS、连接重置、截断）按递增间隔重试，每次输出 transport-retry 警告注解；
下载完成后核对 sha256，不符立即失败、不重试——内容由哈希判定，重试只能得到同一份字节或失败。
uv 不经 PyPI 索引获取：索引响应可能被截断（0021 试点记录「偶发故障」），而固定地址的 wheel 可以核对哈希。
"""

import argparse
import hashlib
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import time

DELAYS = (5, 15, 45)
UV_KEYS = {("Linux", "x86_64"): "linux-x86_64", ("Linux", "aarch64"): "linux-aarch64",
           ("Darwin", "arm64"): "macos-arm64", ("Darwin", "x86_64"): "macos-x86_64"}


class FetchError(Exception):
    pass


def retry_notice(operation, attempt, reason, delay):
    # 写到 stderr：调用方常把 stdout 当作结果（例如 uv 子命令输出的路径），runner 两路都解析工作流命令。
    print(f"::warning title=transport-retry::{operation}: attempt {attempt} failed ({reason}); retrying in {delay}s",
          file=sys.stderr, flush=True)


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as file:
        for block in iter(lambda: file.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def download(url, expected, out, run=subprocess.run, sleep=time.sleep):
    out = Path(out)
    # 任何 curl 非零退出都会重试；目标目录缺失是本地的确定性错误，先建好，免得把它当作传输失败重试。
    out.parent.mkdir(parents=True, exist_ok=True)
    part = out.with_name(out.name + ".part")
    for attempt in range(len(DELAYS) + 1):
        result = run(["curl", "-sSfL", "--max-time", "600", "-o", str(part), url], capture_output=True, text=True)
        if result.returncode == 0:
            break
        reason = result.stderr.strip().splitlines()[-1] if result.stderr.strip() else f"curl exit {result.returncode}"
        if attempt == len(DELAYS):
            part.unlink(missing_ok=True)
            raise FetchError(f"download {url} failed after {attempt + 1} attempts: {reason}")
        retry_notice(f"download {url}", attempt + 1, reason, DELAYS[attempt])
        sleep(DELAYS[attempt])
    actual = sha256(part)
    if actual != expected:
        part.unlink()
        raise FetchError(f"{url} has sha256 {actual}, expected {expected}")
    os.replace(part, out)
    return out


def uv_wheels(path):
    """解析固定清单：每行为平台键、文件名、地址与 sha256，# 开头为注释。"""
    entries = {}
    for number, line in enumerate(Path(path).read_text().splitlines(), 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        fields = line.split()
        if len(fields) != 4 or fields[0] in entries:
            raise FetchError(f"{path}:{number}: expected one line per platform: key filename url sha256")
        key, filename, url, digest = fields
        if not url.endswith("/" + filename) or not re.fullmatch(r"[0-9a-f]{64}", digest):
            raise FetchError(f"{path}:{number}: url must end with the filename and sha256 must be 64 hex digits")
        entries[key] = (filename, url, digest)
    if set(entries) != set(UV_KEYS.values()):
        raise FetchError(f"{path}: platforms {sorted(entries)} differ from {sorted(UV_KEYS.values())}")
    return entries


def uv_entry(path, version, system=None, machine=None):
    entries = uv_wheels(path)
    for filename, _, _ in entries.values():
        if not filename.startswith(f"uv-{version}-"):
            raise FetchError(f"{path}: {filename} is not uv {version}")
    key = UV_KEYS.get((system or platform.system(), machine or platform.machine()))
    if key is None:
        raise FetchError(f"no uv wheel for {system or platform.system()} {machine or platform.machine()}")
    return entries[key]


def main(argv=None):
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    f = sub.add_parser("file")
    f.add_argument("--url", required=True)
    f.add_argument("--sha256", required=True)
    f.add_argument("--out", required=True)
    u = sub.add_parser("uv")
    u.add_argument("--list", required=True)
    u.add_argument("--version", required=True)
    u.add_argument("--out-dir", required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == "file":
            download(args.url, args.sha256, args.out)
        else:
            filename, url, digest = uv_entry(args.list, args.version)
            print(download(url, digest, Path(args.out_dir) / filename))
    except FetchError as error:
        print(f"fetch failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
