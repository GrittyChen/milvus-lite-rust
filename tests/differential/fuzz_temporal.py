"""直接对照真实 3.x parse_expr 与 Rust，不经服务、不依赖固定语料。

运行前构建 cargo build -p milvus-lite-core --example filter_parse_probe。
脚本在 3.x 的可编辑安装环境运行；时间部分按结构生成，再做固定种子的字节/字符变异。
"""
import argparse
from collections import Counter
import json
from pathlib import Path
import random
import subprocess
import sys
import tempfile

import milvus_lite
from milvus_lite.search.filter.parser import parse_expr

ROOT = Path(__file__).resolve().parents[2]

def literal(value):
    for source, replacement in [("\\", "\\\\"), ("'", "\\'"), ("\n", "\\n"), ("\r", "\\r"), ("\t", "\\t")]:
        value = value.replace(source, replacement)
    return "ISO '" + value + "'"

def expressions(count, seed, coverage):
    rng = random.Random(seed)
    dates = ['2025-01-01', '20250101', '0001-01-01', '9999-12-31']
    components = ['00', '01', '04', '08', '12', '23', '30', '59', '60', '99']
    suffixes = ['', '0', '3', '30', '001', '12345', '123456', '1234567', '78901234', '12345678901234',
                '123456@', '123456é', '1234567.89', '12345678,9', '\x00', '\x000']
    zones = ['', 'Z', 'z', 'Z08:30', 'Z+08', '+08', '-08', '+0800', '-0800', '+08:30',
             '+080030', '+08:30:30', '+23:59:59', '+24:00', '+99:99', '+00:00:00.4']
    for i in range(count):
        separated = bool(i % 2)
        size = 1 + i % 3
        fields = [rng.choice(components) for _ in range(size)]
        clock = (':' if separated else '').join(fields)
        clock += rng.choice(['', '.', ',', ':']) + rng.choice(suffixes)
        zone = rng.choice(zones)
        if i % 5 == 0:
            offset = (':' if i % 10 else '').join(rng.choice(components) for _ in range(3))
            offset += rng.choice(['', '.', ',', ':']) + rng.choice(suffixes)
            zone = rng.choice(['+', '-']) + offset
            coverage['fractional_offset'] += 1
        coverage['separated' if separated else 'compact'] += 1
        coverage['aware' if zone else 'naive'] += 1
        time = clock + zone
        if i % 3 == 0:
            position = rng.randrange(len(time) + 1)
            mode = rng.randrange(3)
            if mode == 0:
                time = time[:position] + time[position + 1:]
            elif mode == 1:
                time = time[:position] + rng.choice([':', '.', ',', 'Z', '+', '-', '0', '9', '@', 'é', '\x00', '\t']) + time[position:]
            else:
                time = time[:position]
            coverage['mutated'] += 1
        timestamp = rng.choice(dates) + rng.choice(['T', ' ', '4', '🐈']) + time
        if i % 2:
            yield literal(timestamp)
        else:
            # 同时校验时间字面量处于比较子节点时的位置与 caret。
            yield literal(timestamp) + " == " + literal('2025-01-01T00:00:00Z')

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--probe', type=Path, default=ROOT / 'target/debug/examples/filter_parse_probe')
    parser.add_argument('--cases', type=int, default=200000)
    parser.add_argument('--seed', type=int, default=29)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    assert args.cases > 0
    assert sys.version_info[:2] == (3, 12), 'ISO 对照固定 CPython 3.12 的 C 解析器'
    assert milvus_lite.__version__ == '3.2.1'
    assert Path(milvus_lite.__file__).resolve().is_relative_to(ROOT / 'references/milvus-lite')
    assert subprocess.check_output(['git', '-C', str(ROOT / 'references/milvus-lite'), 'rev-parse', '--short=7', 'HEAD'], text=True).strip() == '43d1257'
    coverage = Counter()
    with tempfile.TemporaryDirectory(prefix='mlr_temporal_fuzz_') as scratch:
        path = Path(scratch)
        inputs, expected, actual = path/'input.jsonl', path/'reference.jsonl', path/'rust.jsonl'
        with inputs.open('w') as sources, expected.open('w') as reference:
            for source in expressions(args.cases, args.seed, coverage):
                try:
                    parse_expr(source)
                except Exception as error:
                    result = str(error)
                else:
                    result = None
                sources.write(json.dumps(source, ensure_ascii=False) + '\n')
                reference.write(json.dumps(result, ensure_ascii=False) + '\n')
        with inputs.open('rb') as source, actual.open('wb') as results:
            subprocess.run([str(args.probe.resolve())], stdin=source, stdout=results, check=True)
        mismatches = []
        checked = 0
        with inputs.open() as sources, expected.open() as reference, actual.open() as rust:
            for source, a, b in zip(sources, reference, rust, strict=True):
                checked += 1
                a, b = json.loads(a), json.loads(b)
                if a != b:
                    mismatches.append({'source': json.loads(source), 'reference': a, 'rust': b})
        assert checked == args.cases, (checked, args.cases)
        assert all(coverage[k] for k in ['compact', 'separated', 'aware', 'naive', 'fractional_offset', 'mutated'])
    report = {'reference': '3.2.1 / 43d1257', 'seed': args.seed, 'checked': checked,
              'coverage': dict(coverage), 'mismatches': mismatches}
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({k: v for k, v in report.items() if k != 'mismatches'}), flush=True)
    print(f'{len(mismatches)} mismatches; report: {args.output}', flush=True)
    if mismatches:
        raise SystemExit(1)

if __name__ == '__main__':
    main()
