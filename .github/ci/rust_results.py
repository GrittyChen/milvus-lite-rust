"""核对 cargo test 的输出：至少有一份汇总，且没有失败与 ignored 用例（0020 零跳过）。

可选的第二个参数要求 passed 总数恰为该值，用于按名称筛选的运行，避免筛选落空时以 0 项通过。
"""
import re
import sys

PATTERN = re.compile(r"^test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored;", re.M)


def main(path, expected=None):
    results = PATTERN.findall(open(path, encoding="utf-8", errors="replace").read())
    if not results:
        raise SystemExit(f"{path} 中没有 cargo test 汇总")
    bad = [r for r in results if r[0] != "ok" or r[2] != "0" or r[3] != "0"]
    if bad:
        raise SystemExit(f"{path} 有失败或 ignored 的汇总：{bad}")
    passed = sum(int(r[1]) for r in results)
    if expected is not None and passed != int(expected):
        raise SystemExit(f"{path} 共 {passed} passed，预期 {expected}")
    print(f"{len(results)} 份汇总，共 {passed} passed，0 failed，0 ignored")


if __name__ == "__main__":
    main(*sys.argv[1:3])
