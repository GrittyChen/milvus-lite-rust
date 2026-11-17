"""导出 jieba 分词的固定真值（0036）：jieba 0.42.1（CPython 3.12）与 3.x `create_analyzer` 对固定语料的结果。

用法：<装了 jieba 0.42.1 的 3.x 参照解释器> generate_jieba_fixtures.py --jieba-test <jieba-0.42.1 sdist 的 test/test.py> [--verify]

- `segments`：没有用户词的 `cut` 与 `cut_for_search`（开 HMM）。语料是 jieba 自带 `test/test.py` 的测试句（MIT），
  加上固定种子的随机混合串与边界串。
- `analyzer`：3.x 真实 `create_analyzer(params).tokenize(text)`，覆盖 `mode` 的取值与停用词（含浮点停用词）。
  这些参数都没有用户词，不改变 jieba 的全局状态。
- `user`：每组用户词用一个新的 `Tokenizer`（复制初始化后的词典）按列表顺序 `add_word`，记录 `total` 与切分；
  这是本实现「每个集合独立、每一项加一次」的口径（0036 决策 C），不是 3.x 进程全局状态下的结果。

--verify 只比较，不改写文件。
"""
import argparse
import ast
import json
import random
from pathlib import Path

import jieba

from milvus_lite.analyzer.factory import create_analyzer

assert jieba.__version__ == "0.42.1", jieba.__version__
target = Path(__file__).resolve().parents[2] / "crates/core/tests/fixtures/jieba.json"

EDGES = ["", " ", "\r\n", "a\r\nb\r\r\n", "　 \u0085​﻿\x1c", "B超", "b超", "中a-b", "T.S.Eliot", "1.5.6%",
         "a_b-c.d", "价格1.5元，折扣0.5%", "鿕鿖鿿㐀\U00020000", "Ａ1 ǅ İ ΑΣ Σ", "C++和c#以及AT&T",
         "深度学习" * 50, "。" * 30, "a." * 40]
PARAMS = [
    {"tokenizer": "jieba"},
    {"type": "jieba"},
    {"tokenizer": {"type": "jieba", "mode": "exact"}},
    {"tokenizer": {"type": "jieba", "mode": "SEARCH"}},
    {"tokenizer": {"type": "jieba", "mode": None, "hmm": False}},
    {"tokenizer": "jieba", "filter": [{"type": "stop", "stop_words": ["的", "，", 1.5, True, None, "了"]}]},
    {"tokenizer": {"type": "jieba", "mode": "exact"}, "filter": ["lowercase", {"type": "stop", "stop_words": ["中国"]}]},
]


def random_text(rng, words):
    pools = [
        lambda: rng.choice(words),
        lambda: chr(rng.randint(0x4E00, 0x9FD5)),
        lambda: chr(rng.randint(0x9FD6, 0x9FFF)),
        lambda: chr(rng.randint(0x3400, 0x4DBF)),
        lambda: rng.choice("abcXYZ0123456789+#&._%-"),
        lambda: rng.choice(" \t\r\n　 \u0085，。！？、；：“”（）《》ΣΑİǅＡ"),
        lambda: str(rng.randint(0, 99999)) + rng.choice(["", ".5", "%", ".25%", "-a", "_b.c"]),
    ]
    weights = [50, 15, 3, 3, 10, 10, 9]
    return "".join(rng.choices(pools, weights)[0]() for _ in range(rng.randint(1, 30)))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--jieba-test", required=True)
    parser.add_argument("--verify", action="store_true")
    args = parser.parse_args()

    tree = ast.parse(Path(args.jieba_test).read_text(encoding="utf-8"))
    sentences = [node.args[0].value for node in ast.walk(tree)
                 if isinstance(node, ast.Call) and getattr(node.func, "id", None) == "cuttest"]
    assert len(sentences) == 89, len(sentences)

    base = jieba.Tokenizer()
    base.initialize()
    words = [line.split(" ")[0] for line in open(Path(jieba.__file__).parent / "dict.txt", encoding="utf-8")]
    rng = random.Random(36)
    texts = sentences + EDGES + [random_text(rng, words) for _ in range(1500)]

    segments = [{"text": t, "cut": base.lcut(t), "search": base.lcut_for_search(t)} for t in texts]
    analyzer = []
    for params in PARAMS:
        a = create_analyzer(params)
        analyzer.append({"params": params, "tokens": [a.tokenize(t) for t in texts[:200]]})
    assert jieba.dt.total == base.total, "create_analyzer 不应改变全局词典"

    groups = [["米尔维斯"], ["米尔维斯", "米尔维斯"], [""], ["", "向量库", ""], ["的", "了", "是"], ["Deep Learning", "a.b"],
              ["T.S.Eliot", "中a"], ["深度学习框架", "云原生", "\u0001"], ["B超", "b超"], [" ", "\n", "1.5%", "x-y_z"]]
    for _ in range(40):
        groups.append([rng.choice(words) + rng.choice(["", "版", "a.b", "2.0"]) for _ in range(rng.randint(1, 5))])
    user = []
    for group in groups:
        t = jieba.Tokenizer()
        t.FREQ, t.total, t.initialized = dict(base.FREQ), base.total, True
        for word in group:
            t.add_word(word)
        sample = texts[:20] + [random_text(rng, words + group) for _ in range(10)]
        user.append({"words": group, "total": t.total, "texts": sample,
                     "cut": [t.lcut(x) for x in sample], "search": [t.lcut_for_search(x) for x in sample]})

    text = json.dumps({"segments": segments, "analyzer": analyzer, "user": user}, ensure_ascii=True, indent=0) + "\n"
    if args.verify:
        assert target.read_text(encoding="ascii") == text
        print("ok")
    else:
        target.write_text(text, encoding="ascii")


main()
