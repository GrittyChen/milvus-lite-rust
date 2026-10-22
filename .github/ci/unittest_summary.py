"""核对 unittest 的输出：恰好运行预期数量的用例，末行为不带任何附注的 OK。

用于以 subtests 组织、无法经分片工具按用例计数的自检；`OK (skipped=N)` 的退出码也为 0，必须从输出拒绝。
"""
import re
import sys


def main(path, expected):
    lines = [line.strip() for line in open(path, encoding="utf-8", errors="replace").read().splitlines() if line.strip()]
    ran = [int(n) for n in re.findall(r"^Ran (\d+) tests? in ", "\n".join(lines), re.M)]
    if ran != [int(expected)] or not lines or lines[-1] != "OK":
        raise SystemExit(f"{path}：运行记录 {ran}、末行 {lines[-1] if lines else None!r}，预期运行 {expected} 项且末行为 OK")
    print(f"Ran {expected} tests, OK")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
