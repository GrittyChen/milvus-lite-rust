"""直接对照真实 StandardAnalyzer 与 Rust 的分词结果，不经服务、不依赖固定语料。

运行前构建 cargo build -p milvus-lite-core --example analyzer_probe。
脚本在 CPython 3.12 的参照环境运行，种子固定；不进入默认测试。
"""
import argparse
import json
import random
import subprocess
from pathlib import Path

from milvus_lite.analyzer.standard import StandardAnalyzer

ROOT = Path(__file__).resolve().parents[2]
SPECIAL = ["Σ", "İ", "ͅ", "́", ".", "'", "_", " "]


def text(rng):
    out = []
    for _ in range(rng.randrange(0, 16)):
        kind = rng.randrange(4)
        if kind == 0:
            out.append(rng.choice(SPECIAL))
        elif kind == 1:
            out.append(chr(rng.randrange(0x20, 0x7f)))
        else:
            point = rng.randrange(0x110000)
            out.append("A" if 0xD800 <= point <= 0xDFFF else chr(point))
    return "".join(out)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--cases", type=int, default=200000)
    parser.add_argument("--seed", type=int, default=29)
    parser.add_argument("--probe", default=str(ROOT / "target/debug/examples/analyzer_probe"))
    args = parser.parse_args()
    rng = random.Random(args.seed)
    texts = [text(rng) for _ in range(args.cases)]
    analyzer = StandardAnalyzer()
    expected = [analyzer.tokenize(t) for t in texts]
    stdin = "".join(json.dumps(t) + "\n" for t in texts)
    done = subprocess.run([args.probe], input=stdin, capture_output=True, text=True, check=True)
    actual = [json.loads(line) for line in done.stdout.splitlines()]
    assert len(actual) == len(texts)
    bad = [(t, e, a) for t, e, a in zip(texts, expected, actual) if e != a]
    print(json.dumps({"cases": len(texts), "mismatches": len(bad), "first": bad[:5]}, ensure_ascii=True))
    raise SystemExit(1 if bad else 0)


main()
