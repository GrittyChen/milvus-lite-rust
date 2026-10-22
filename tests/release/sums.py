"""发行工作流内部交接用的 SHA256 清单：写出目录下全部文件（相对路径）的摘要，或按清单逐项核对。

manylinux 容器不一定带 shasum，因此用 Python 实现；格式与 `shasum -a 256` 相同。发布用的 SHA256SUMS 由
candidate.py 生成，不用本工具。
"""

import hashlib
from pathlib import Path
import sys


def digest(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def write(directory, out):
    directory = Path(directory)
    files = sorted(p for p in directory.rglob("*") if p.is_file() and p.resolve() != Path(out).resolve())
    if not files:
        raise SystemExit(f"{directory} has no files")
    Path(out).write_text("".join(f"{digest(p)}  {p.relative_to(directory).as_posix()}\n" for p in files))


def check(sums, directory):
    directory = Path(directory)
    entries = {}
    for line in Path(sums).read_text().splitlines():
        value, _, name = line.partition("  ")
        if len(value) != 64 or not name or name.startswith("/") or ".." in Path(name).parts or name in entries:
            raise SystemExit(f"malformed checksum line {line!r}")
        entries[name] = value
    present = {p.relative_to(directory).as_posix() for p in directory.rglob("*") if p.is_file()} - {Path(sums).name}
    if set(entries) != present:
        raise SystemExit(f"file set differs: missing {sorted(set(entries) - present)}, extra {sorted(present - set(entries))}")
    bad = [name for name, value in entries.items() if digest(directory / name) != value]
    if bad:
        raise SystemExit(f"checksum mismatch: {bad}")
    print(f"{len(entries)} files match {Path(sums).name}")


if __name__ == "__main__":
    if sys.argv[1:2] == ["digest"] and len(sys.argv) == 3:
        print(digest(sys.argv[2]))
    elif len(sys.argv) == 4 and sys.argv[1] in ("write", "check"):
        (write if sys.argv[1] == "write" else check)(*sys.argv[2:])
    else:
        raise SystemExit("usage: sums.py write <dir> <out> | check <sums> <dir> | digest <file>")
