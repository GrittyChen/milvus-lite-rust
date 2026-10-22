"""核对普通 pytest 的末行汇总：passed 恰为预期数量，不含其他结果（warnings 与耗时除外）。

只用于分片工具自身的自检；其余 pytest 运行经分片工具核对，避免用工具验证工具（0020 公共约定 4）。
非严格 xpass 的退出码为 0、JUnit 也只记为普通用例，末行汇总是唯一会列出它的输出。
"""
import re
import sys

ALLOWED = {"passed", "warning", "warnings"}


def main(path, expected):
    lines = [line for line in open(path, encoding="utf-8", errors="replace").read().splitlines() if line.strip()]
    summary = re.sub(r"\s+in [\d.]+s.*$", "", lines[-1].strip("= ")) if lines else ""
    counts = dict((kind, int(n)) for n, kind in re.findall(r"(\d+) (\w+)", summary))
    if counts.get("passed") != int(expected) or set(counts) - ALLOWED:
        raise SystemExit(f"末行汇总 {summary!r} 与预期的 {expected} passed 不符")
    print(summary)


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
