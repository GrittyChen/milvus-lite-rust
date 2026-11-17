"""直接对照真实 jieba 0.42.1 与本实现的 jieba 切分（0036），不经服务、不依赖固定语料。

运行前构建：cargo build -p milvus-lite-core --release --example jieba_probe
脚本在装了 jieba 0.42.1 的 CPython 3.12 参照环境运行，种子固定，不进入默认测试：

    <参照 python> tests/differential/fuzz_jieba.py --cases 200000 --groups 150 --seed 36 --output <仓库外>/jieba.json

两类用例：
- 没有用户词：`--cases` 条随机混合串，比较 `cut` 与 `cut_for_search`（开 HMM）。
- 用户词：`--groups` 组随机用户词，每组 8 条文本。Python 侧每组用一个新的 `Tokenizer`（复制初始化后的词典），
  按列表顺序 `add_word`，比较 `total`、`cut` 与 `cut_for_search`。用户词的规模远低于本实现的两个上限。
"""
import argparse
import json
import random
import subprocess
from pathlib import Path

import jieba

ROOT = Path(__file__).resolve().parents[2]
assert jieba.__version__ == "0.42.1", jieba.__version__


def make_text(rng, words):
    pools = [
        lambda: rng.choice(words),
        lambda: chr(rng.randint(0x4E00, 0x9FD5)),
        lambda: chr(rng.randint(0x9FD6, 0x9FFF)),
        lambda: chr(rng.randint(0x3400, 0x4DBF)),
        lambda: chr(rng.randint(0x20000, 0x2A6DF)),
        lambda: rng.choice("abcXYZ0123456789+#&._%-"),
        lambda: rng.choice(" \t\r\n　 \u0085​﻿，。！？、；：“”（）《》ΣΑİǅＡ"),
        lambda: str(rng.randint(0, 99999)) + rng.choice(["", ".5", "%", ".25%", "-a", "_b.c"]),
    ]
    weights = [50, 15, 3, 3, 2, 10, 10, 7]
    return "".join(rng.choices(pools, weights)[0]() for _ in range(rng.randint(1, 40)))


def make_words(rng, words):
    def one():
        kind = rng.randrange(6)
        if kind == 0:
            return rng.choice(words)
        if kind == 1:
            return "".join(rng.choice(words) for _ in range(rng.randint(2, 3)))
        if kind == 2:
            return "".join(chr(rng.randint(0x4E00, 0x9FD5)) for _ in range(rng.randint(1, 5)))
        if kind == 3:
            return rng.choice(["Deep Learning", "a.b", "T.S.Eliot", "C++", "", " ", "\n", "1.5%", "x-y_z", "\u0001"])
        if kind == 4:
            return rng.choice(words) + rng.choice(["a.b", "-", "2.0", "版"])
        return rng.choice(words)[:1] + chr(rng.randint(0x4E00, 0x9FD5))
    out = [one() for _ in range(rng.randint(1, 6))]
    if rng.random() < 0.2:
        out.append(rng.choice(out))
    return out


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--cases", type=int, default=200000)
    parser.add_argument("--groups", type=int, default=150)
    parser.add_argument("--seed", type=int, default=36)
    parser.add_argument("--probe", default=str(ROOT / "target/release/examples/jieba_probe"))
    parser.add_argument("--output", required=True)
    args = parser.parse_args()

    base = jieba.Tokenizer()
    base.initialize()
    dictionary = [line.split(" ")[0] for line in open(Path(jieba.__file__).parent / "dict.txt", encoding="utf-8")]
    rng = random.Random(args.seed)

    cases = [{"words": [], "texts": [make_text(rng, dictionary) for _ in range(args.cases)]}]
    for _ in range(args.groups):
        cases.append({"words": make_words(rng, dictionary), "texts": [make_text(rng, dictionary) for _ in range(8)]})

    expected = []
    for case in cases:
        tokenizer = jieba.Tokenizer()
        tokenizer.FREQ, tokenizer.total, tokenizer.initialized = dict(base.FREQ), base.total, True
        for word in case["words"]:
            tokenizer.add_word(word)
        expected.append({"total": tokenizer.total,
                         "cut": [tokenizer.lcut(t) for t in case["texts"]],
                         "search": [tokenizer.lcut_for_search(t) for t in case["texts"]]})

    stdin = "".join(json.dumps(case) + "\n" for case in cases)
    done = subprocess.run([args.probe], input=stdin, capture_output=True, text=True, check=True)
    # serde_json 原样输出 U+0085、U+2028 等，`splitlines()` 会在字符串中间断开。
    actual = [json.loads(line) for line in done.stdout.split("\n") if line]
    assert len(actual) == len(cases)

    mismatches = []
    rows = 0
    for case, want, got in zip(cases, expected, actual):
        if "error" in got:
            mismatches.append({"words": case["words"], "error": got["error"]})
            continue
        if want["total"] != got["total"]:
            mismatches.append({"words": case["words"], "total": [want["total"], got["total"]]})
        for kind in ("cut", "search"):
            for text, w, g in zip(case["texts"], want[kind], got[kind]):
                rows += 1
                if w != g:
                    mismatches.append({"words": case["words"], "kind": kind, "text": text, "want": w, "got": g})
    report = {"seed": args.seed, "cases": args.cases, "groups": args.groups, "compared_rows": rows,
              "mismatches": len(mismatches), "first": mismatches[:20]}
    Path(args.output).write_text(json.dumps(report, ensure_ascii=False, indent=1), encoding="utf-8")
    print(json.dumps({k: report[k] for k in ("seed", "cases", "groups", "compared_rows", "mismatches")}))
    raise SystemExit(1 if mismatches else 0)


main()
