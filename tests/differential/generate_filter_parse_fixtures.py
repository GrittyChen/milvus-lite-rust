"""以真实 v3.2.1 parser 生成语法诊断真值；--verify 只核对，不覆盖。

用例种子部分取自参照实现 tests/search/filter/test_parser.py（Apache-2.0，43d1257）。
运行：<3.x python> tests/differential/generate_filter_parse_fixtures.py [--verify]
"""
import ast
import json
import random
import sys
import subprocess
from pathlib import Path
import milvus_lite
from milvus_lite.search.filter.parser import parse_expr

assert milvus_lite.__version__ == "3.2.1"
root = Path(__file__).resolve().parents[2]
assert subprocess.check_output(['git', '-C', str(root / 'references/milvus-lite'), 'rev-parse', '--short=7', 'HEAD'], text=True).strip() == '43d1257'
assert Path(milvus_lite.__file__).resolve().is_relative_to(root / 'references/milvus-lite')
reference = root / "references/milvus-lite/tests/search/filter/test_parser.py"
sources = {
    "", "id >= 1 and (age < 30)", "id in [1, - 2,]", "id in [bad]",
    "color == tRuE", "编号 == １２", "id == ²", "id == 1e999", "id == 1e-999",
    "id == 1 # comment", "id == 1\n and age < 2", "--7", "-id", "id == 7.0",
    "1 + 1 is null", "exists 1 + 1", "array_contains(id, -1)", "unknown_fn(id)",
    "a[1][\"x\"] == 3", "$meta[0]", "geometry_dwithin(v, 'POINT(0 0)', -1)",
    "INTERVAL 'P0D'", "INTERVAL 'P'", "INTERVAL 'PT0S'", "INTERVAL 'P2W3DT2H4M5.1S'",
    "ISO '2025-01-01'", "ISO '20250229T00:00:00Z'", "ISO '2020-W53-7T12:00:00+08:30'",
    "ISO '2025-01-01🐈12:34:56+08:30'", "ISO '2025-01-01Té+00:00'",
    "ISO '2025-01-01T00:00:00+00:99'", "ISO '2025-01-01T00:00:00+00:00:99'",
    "ARRAY_LENGTH(id", "TEXT_MATCH(id,'x'", "array_contains(id, 1)", "JSON_CONTAINS_ALL(id,[1,2])",
    "ISO '0001-01-01T00:00:00+01:00'", "ISO '9999-12-31T23:00:00-01:00'",
    "ISO '9999-W52-7T00:00:00Z'", "ISO '2025-W01-12:00+00:00'",
    "ISO '2025W0112:00+00:00'", "ISO '2025-01-01T00:00:00+00:99'",
    "INTERVAL 'P１D'", "INTERVAL 'PT١.٥S'", "INTERVAL 'P1D\\n'", "INTERVAL 'P0D\\n'",
    "age == \u0378", "'x' '\u0378'", "age = '\U0001ffff'", "text_match('\u0378', 'x')",
}
# 新增回归不改变原 800 次变异的种子集合，保留此前核验过的全部语料。
regressions = {
    "ISO '2025-01-01T00:05:00Z08:30'", "ISO '2025-01-01T00:00:08:30Z'",
    "ISO '2025-01-01T00:00:08:30'", "ISO '2025-01-01T00:00:08:30+08:30'",
    "ISO '2025-01-01T00:00:08:123456789+08:30:08:30'",
    "ISO '2025-01-01T00:00:08:30@Z'", "ISO '2025-01-01T000008:30Z'",
    "ISO '2025-01-01T00:00:08:Z'", "ISO '2025-01-01T00:00:08:'",
    "ISO '2025-01-01T00:00:08::30Z'", "ISO '2025-01-01T00:00:08:3.0Z'",
    "ISO '2025-01-01T00:00:08:1234567.89+08:30:08:30'",
    "ISO '2025-01-01T00:00:08:12345678,9+08:30:08:30'",
    "ISO '2025-01-01T00:00:08.123456💡+08:30'", "ISO '2025-01-01T00:00:08:12345éZ'",
    "ISO '2025-01-01T00:00:08:1234567.89'", "ISO '2025-01-01T00:00:08+08:30:08:123456@'",
}
for node in ast.walk(ast.parse(reference.read_text())):
    if isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id == "parse_expr":
        if node.args and isinstance(node.args[0], ast.Constant) and isinstance(node.args[0].value, str):
            sources.add(node.args[0].value)
seeds = sorted(sources)
for source in seeds:
    for i in range(len(source) + 1):
        sources.add(source[:i])
randomizer = random.Random(7321)
for _ in range(800):
    source = randomizer.choice(seeds)
    i = randomizer.randrange(len(source) + 1)
    sources.add(source[:i] + randomizer.choice(["@", "'", "(", "!", "²", "\t", "and", "]", "$meta", "+"]) + source[i:])
for source in regressions:
    sources.add(source)
    for i in range(len(source) + 1):
        sources.add(source[:i])
regression_randomizer = random.Random(10321)
for _ in range(400):
    source = regression_randomizer.choice(sorted(regressions))
    i = regression_randomizer.randrange(len(source) + 1)
    sources.add(source[:i] + regression_randomizer.choice(["Z", ":", ".", ",", "+", "-", "0", "9", "@", "\t"]) + source[i:])
# 时间扫描组件的第二轮反例单列，不改变前两轮的种子与变异序列。
clock_regressions = {
    "ISO '2025-01-01T00200400Z'", "ISO '2025-01-01T002004001Z'",
    "ISO '2025-01-01T12345678901234Z'", "ISO '2025-01-01T00200400+08'",
    "ISO '20250101T00400000'", "ISO '20250101404000008'",
    "ISO '0001-01-01T00:00:00.3+00:00:00.4'",
    "ISO '0001-01-01T00:00:00.3+00:00:01.4'",
    "ISO '2025-01-01T00Z\x00+08'", "ISO '2025-01-01T00Z\x00Z'",
    "ISO '2025-01-01T00+08\x00ignored'", "ISO '2025-01-01T00+08Z\x00'",
    "ISO '2025-01-01T00+08:00:00.123456\x00ignored'",
}
# 长度边界及尾字节矩阵同时覆盖时间与偏移，避免只覆盖带冒号的基础串。
for clock in ["123456" + "78901234567890"[:n] for n in range(15)] + [
    "12", "1234", "12:34", "12:34:56", "12:34:56:", "12:34:56.",
]:
    for suffix in ["", ".1", ",1234567", ":123456", "@", "\x00"]:
        for zone in ["", "Z", "+08", "+08003012345678", "+08:30:30:1234567"]:
            clock_regressions.add("ISO '2025-01-01T" + clock + suffix + zone + "'")
for source in clock_regressions:
    sources.add(source)
    for i in range(len(source) + 1):
        sources.add(source[:i])
cases = []
for source in sorted(sources):
    try:
        parse_expr(source)
    except Exception as error:
        result = str(error)
    else:
        result = None
    cases.append({"source": source, "error": result})
target = root / "crates/core/tests/fixtures/filter_parse_v321.json"
payload = json.dumps({"reference": "v3.2.1 / 43d1257", "cases": cases}, ensure_ascii=False, indent=2) + "\n"
if "--verify" in sys.argv:
    assert target.read_text() == payload
else:
    target.parent.mkdir(exist_ok=True)
    target.write_text(payload)
print(f"{len(cases)} reference parse cases verified")
