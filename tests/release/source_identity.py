"""从冻结提交生成候选的源码身份，并核对工作树中的产品 / 构建输入与之一致（0021 第 5 条）。

规则与 0.3.0rc1 / rc2 候选一致：归档为 `git archive --format=tar <commit>`；源树清单覆盖归档中的全部文件与软链，
记录 Git 模式与 SHA256（软链取目标字符串的 SHA256）；产品 / 构建输入为 `crates/` 下全部文件及根目录的
五个构建配置。两份清单以键排序、缩进 2、末尾换行的 JSON 序列化后取 SHA256。身份另记生成归档与清单所用的
Git 与 Python 版本，临时工作流产物过期后仍能知道这份字节流由什么工具产生。
"""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import stat
import subprocess
import sys
import tarfile

ROOT_INPUTS = ("Cargo.lock", "Cargo.toml", "clippy.toml", "pyproject.toml", "rust-toolchain.toml")
PRODUCT_DIR = "crates/"


class IdentityError(Exception):
    pass


def is_product(path):
    return path in ROOT_INPUTS or path.startswith(PRODUCT_DIR)


def canonical(data):
    return (json.dumps(data, sort_keys=True, indent=2) + "\n").encode()


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def cargo_version_to_python(version):
    """Cargo 的 `0.3.0-rc.2` 对应 Python 的 `0.3.0rc2`；不认识的预发布形式直接拒绝，不猜测。"""
    match = re.fullmatch(r"(\d+\.\d+\.\d+)(?:-(a|b|rc)\.(\d+))?", version)
    if not match:
        raise IdentityError(f"unsupported Cargo version {version!r}")
    base, kind, number = match.groups()
    return base + (f"{kind}{number}" if kind else "")


def manifests(archive):
    source = {}
    with tarfile.open(fileobj=io.BytesIO(archive)) as bundle:
        for member in bundle.getmembers():
            if member.isdir():
                continue
            if member.name in source:
                raise IdentityError(f"duplicate archive member {member.name}")
            if member.issym():
                source[member.name] = {"mode": "120000", "sha256": sha256(member.linkname.encode())}
            elif member.isfile():
                mode = "100755" if member.mode & 0o111 else "100644"
                source[member.name] = {"mode": mode, "sha256": sha256(bundle.extractfile(member).read())}
            else:
                raise IdentityError(f"unexpected archive member type {member.name}")
    product = {name: entry["sha256"] for name, entry in source.items() if is_product(name)}
    missing = [name for name in ROOT_INPUTS if name not in product]
    if missing:
        raise IdentityError(f"missing root build inputs {missing}")
    return source, product


def tool_versions():
    git = subprocess.run(["git", "--version"], check=True, capture_output=True, text=True).stdout.strip()
    return {"git": git, "python": f"{platform.python_implementation()} {platform.python_version()}"}


def generate(repo, commit, version, out):
    resolved = subprocess.run(["git", "-C", repo, "rev-parse", "--verify", commit + "^{commit}"],
                              check=True, capture_output=True, text=True).stdout.strip()
    if resolved != commit:
        raise IdentityError(f"commit must be a full SHA, got {commit!r} resolving to {resolved}")
    archive = subprocess.run(["git", "-C", repo, "archive", "--format=tar", commit], check=True, capture_output=True).stdout
    source, product = manifests(archive)
    cargo = subprocess.run(["git", "-C", repo, "show", f"{commit}:Cargo.toml"], check=True, capture_output=True, text=True).stdout
    found = re.search(r'(?m)^version = "([^"]+)"$', cargo)
    if not found or cargo_version_to_python(found.group(1)) != version:
        raise IdentityError(f"Cargo workspace version {found and found.group(1)!r} does not match {version}")
    out.mkdir(parents=True)
    (out / "source.tar").write_bytes(archive)
    (out / "source-inputs.json").write_bytes(canonical(source))
    (out / "product-inputs.json").write_bytes(canonical(product))
    identity = {
        "commit": commit,
        "version": version,
        "archive_sha256": sha256(archive),
        "source_entries": len(source),
        "regular_files": sum(entry["mode"] != "120000" for entry in source.values()),
        "symlinks": sum(entry["mode"] == "120000" for entry in source.values()),
        "product_inputs": len(product),
        "source_manifest_sha256": sha256(canonical(source)),
        "product_manifest_sha256": sha256(canonical(product)),
        "tool_versions": tool_versions(),
    }
    (out / "source-identity.json").write_bytes(canonical(identity))
    return identity


def worktree_entries(repo):
    """工作树中产品 / 构建输入路径下的全部条目，包括被忽略的文件：构建只读取工作树，忽略规则不改变构建输入。"""
    found = {}
    for name in ROOT_INPUTS:
        if os.path.lexists(repo / name):
            found[name] = repo / name
    for directory, dirnames, filenames in os.walk(repo / PRODUCT_DIR):
        base = Path(directory)
        for name in list(dirnames):
            if (base / name).is_symlink():
                dirnames.remove(name)
                filenames.append(name)
        for name in filenames:
            found[(base / name).relative_to(repo).as_posix()] = base / name
    return found


def check_worktree(repo, source):
    expected = {name: entry for name, entry in source.items() if is_product(name)}
    present = worktree_entries(repo)
    problems = [f"untracked build input {name}" for name in sorted(set(present) - set(expected))]
    problems += [f"missing build input {name}" for name in sorted(set(expected) - set(present))]
    for name in sorted(set(expected) & set(present)):
        path, entry = present[name], expected[name]
        mode = os.lstat(path).st_mode
        if stat.S_ISLNK(mode):
            actual = {"mode": "120000", "sha256": sha256(os.readlink(path).encode())}
        elif stat.S_ISREG(mode):
            actual = {"mode": "100755" if mode & 0o111 else "100644", "sha256": sha256(path.read_bytes())}
        else:
            actual = {"mode": oct(stat.S_IFMT(mode)), "sha256": None}
        if actual != entry:
            problems.append(f"build input differs from frozen commit {name}")
    if problems:
        raise IdentityError("; ".join(problems))
    return len(expected)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    gen = sub.add_parser("generate")
    gen.add_argument("--repo", type=Path, default=Path("."))
    gen.add_argument("--commit", required=True)
    gen.add_argument("--version", required=True)
    gen.add_argument("--out", type=Path, required=True)
    check = sub.add_parser("check-worktree")
    check.add_argument("--repo", type=Path, default=Path("."))
    check.add_argument("--source-inputs", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == "generate":
            print(json.dumps(generate(args.repo, args.commit, args.version, args.out), sort_keys=True))
        else:
            count = check_worktree(args.repo.resolve(), json.loads(args.source_inputs.read_text()))
            print(f"worktree build inputs match frozen commit: {count}")
    except IdentityError as error:
        print(f"source identity failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
