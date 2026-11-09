"""固定参照环境 CPython 3.12 / Unicode 15.0 的词法分类；升级参照环境时重生成并差分。"""
import sys
import unicodedata
from pathlib import Path

assert unicodedata.unidata_version == "15.0.0", unicodedata.unidata_version
target = Path(__file__).resolve().parents[2] / "crates/core/src/filter/unicode.rs"

def ranges(predicate):
    out, start, end = [], None, None
    for point in range(0x110000):
        if predicate(chr(point)):
            if start is None:
                start = point
            end = point
        elif start is not None:
            out.append((start, end))
            start = None
    if start is not None:
        out.append((start, end))
    return out

SIGMA = '\u03a3'
FINAL = '\u03c2'

def cased(ch):
    # 只有不可忽略的 cased 字符才会被词尾 sigma 的规则看到，这里按 lower() 的行为反推。
    return (ch + SIGMA).lower()[-1] == FINAL

def case_ignorable(ch):
    return ('a' + ch + SIGMA).lower()[-1] == FINAL and not cased(ch)

lower = {point: chr(point).lower() for point in range(0x110000)
         if chr(point).lower() != chr(point)}
# str.lower() 里唯一的一对多映射；单独实现，让映射表保持 (码位, 码位)。
assert {p: v for p, v in lower.items() if len(v) != 1} == {0x130: 'i\u0307'}
del lower[0x130]

parts = ['//! CPython 3.12 / Unicode 15.0 的 isalpha、isalnum、isdigit、isprintable、十进制数字表，\n'
         '//! 以及 str.lower() 的映射和词尾 sigma 用到的两个属性。\n'
         '//! 由 tests/differential/generate_filter_unicode.py 生成，避免 Rust Alphabetic 属性包含组合标记，\n'
         '//! 也避免 Rust 标准库的大小写表随工具链的 Unicode 版本变化。\n'
         'fn contains(c: char, ranges: &[(u32,u32)]) -> bool {\n'
         '    let c = u32::from(c);\n'
         '    let index = ranges.partition_point(|(_,end)| *end < c);\n'
         '    ranges.get(index).is_some_and(|(start,_)| *start <= c)\n}\n']
for name, predicate in [('alpha', str.isalpha), ('alnum', str.isalnum), ('digit', str.isdigit), ('printable', str.isprintable),
                        ('cased', cased), ('case_ignorable', case_ignorable)]:
    entries = ranges(predicate)
    parts.append(f'pub(crate) fn {name}(c: char) -> bool {{ contains(c, {name.upper()}) }}\n')
    parts.append(f'const {name.upper()}: &[(u32,u32)] = &[\n')
    for start in range(0, len(entries), 4):
        parts.append('    ' + ', '.join(f'(0x{a:x},0x{b:x})' for a, b in entries[start:start+4]) + ',\n')
    parts.append('];\n')
zeros = [point for point in range(0x110000) if unicodedata.decimal(chr(point), None) == 0]
parts.append('/// `str.lower()` 的单字符映射；不含 U+0130，U+03A3 给出非词尾形式。\n'
             'pub(crate) fn lower(c: char) -> Option<char> {\n'
             '    let c = u32::from(c);\n'
             '    LOWER.binary_search_by_key(&c, |(from, _)| *from).ok().and_then(|i| char::from_u32(LOWER[i].1))\n}\n')
parts.append('const LOWER: &[(u32,u32)] = &[\n')
items = sorted(lower.items())
for start in range(0, len(items), 4):
    parts.append('    ' + ', '.join(f'(0x{a:x},0x{ord(b):x})' for a, b in items[start:start+4]) + ',\n')
parts.append('];\n')
parts.append('pub(super) fn decimal(c: char) -> Option<u32> {\n'
             '    let c = u32::from(c);\n'
             '    ZERO.iter().find_map(|zero| c.checked_sub(*zero).filter(|n| *n < 10))\n}\n')
parts.append('const ZERO: &[u32] = &[' + ','.join(hex(n) for n in zeros) + '];\n')
text = ''.join(parts)
if '--verify' in sys.argv:
    import subprocess
    import tempfile
    with tempfile.NamedTemporaryFile(suffix='.rs', mode='w+', encoding='utf8') as file:
        file.write(text)
        file.flush()
        subprocess.run(['rustfmt', '--edition', '2024', file.name], check=True)
        assert target.read_text() == Path(file.name).read_text()
else:
    target.write_text(text)
