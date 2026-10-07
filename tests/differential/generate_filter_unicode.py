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

parts = ['//! CPython 3.12 / Unicode 15.0 的 isalpha、isalnum、isdigit、isprintable 与十进制数字表。\n'
         '//! 由 tests/differential/generate_filter_unicode.py 生成，避免 Rust Alphabetic 属性包含组合标记。\n'
         'fn contains(c: char, ranges: &[(u32,u32)]) -> bool {\n'
         '    let c = u32::from(c);\n'
         '    let index = ranges.partition_point(|(_,end)| *end < c);\n'
         '    ranges.get(index).is_some_and(|(start,_)| *start <= c)\n}\n']
for name, predicate in [('alpha', str.isalpha), ('alnum', str.isalnum), ('digit', str.isdigit), ('printable', str.isprintable)]:
    entries = ranges(predicate)
    parts.append(f'pub(super) fn {name}(c: char) -> bool {{ contains(c, {name.upper()}) }}\n')
    parts.append(f'const {name.upper()}: &[(u32,u32)] = &[\n')
    for start in range(0, len(entries), 4):
        parts.append('    ' + ', '.join(f'(0x{a:x},0x{b:x})' for a, b in entries[start:start+4]) + ',\n')
    parts.append('];\n')
zeros = [point for point in range(0x110000) if unicodedata.decimal(chr(point), None) == 0]
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
