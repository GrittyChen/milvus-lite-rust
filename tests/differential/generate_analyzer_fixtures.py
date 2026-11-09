"""导出 standard 分词的真值：真实 StandardAnalyzer（CPython 3.12 / Unicode 15.0）对固定语料的结果。

--verify 只比较，不改写语料文件。
"""
import json
import random
import sys
import unicodedata
from pathlib import Path

from milvus_lite.analyzer.standard import StandardAnalyzer

assert unicodedata.unidata_version == "15.0.0", unicodedata.unidata_version
target = Path(__file__).resolve().parents[2] / "crates/core/tests/fixtures/analyzer_standard.json"

SIGMA = "Σ"
CURATED = [
    "", " ", "!!!", "Hello, World!", "foo_bar baz-qux", "__", "a_", "HELLO hello HeLLo", "tab\tnew\nline\r",
    "café", "résumé", "café", "àb", "İstanbul", "IİI", "ı", "x²y", "½", "①",
    "中文 混合text", "١٢٣", "Ａｂｃ", "\U0001d400\U0001d401", "\U0001f600smile", "‍zwj‌",
    f"{SIGMA}", f"A{SIGMA}", f"{SIGMA}A", f"A{SIGMA}A", f"A{SIGMA} B", f"A{SIGMA}.B", f"A.{SIGMA}", f"A'{SIGMA}", f"A{SIGMA}'",
    f"ͅ{SIGMA}", f"Aͅ{SIGMA}", f"A{SIGMA}ͅ", f"ʰ{SIGMA}", f"A{SIGMA}ʰ", f"1{SIGMA}", f"中{SIGMA}",
    f"{SIGMA}{SIGMA}{SIGMA}", f"ΣΑΣ", "ΑΣΑΣ ΑΣ",
    "ꟋᲉ\U00010d50\U00016ea0", "ẞǅΩKÅ", "̇ͅ", "ßﬁ",
]
POOL = ["a", "Z", "0", "_", " ", ".", "'", "-", SIGMA, "σ", "ς", "İ", "ı", "́", "ͅ", "ʰ", "²",
        "中", "١", "‍", "­", "Ɤ", "\U00010d50", "\U0001d400", "ǅ", "Ⅰ"]


def corpus():
    rng = random.Random(29)
    texts = list(CURATED)
    texts += ["".join(rng.choice(POOL) for _ in range(rng.randrange(1, 12))) for _ in range(400)]
    # 所有有小写映射的字符，以及所有非 ASCII 的词字符边界，分块放进同一条文本。
    mapped = [chr(p) for p in range(0x110000) if not 0xD800 <= p <= 0xDFFF and chr(p).lower() != chr(p)]
    for start in range(0, len(mapped), 40):
        texts.append(" ".join(mapped[start:start + 40]))
        texts.append("a" + "".join(mapped[start:start + 40]) + SIGMA)
    non_ascii_words = [chr(p) for p in range(0x80, 0x110000, 97) if not 0xD800 <= p <= 0xDFFF and chr(p).isprintable()]
    for start in range(0, len(non_ascii_words), 80):
        texts.append("|".join(non_ascii_words[start:start + 80]))
    return texts


analyzer = StandardAnalyzer()
text = json.dumps([{"text": t, "tokens": analyzer.tokenize(t)} for t in corpus()], ensure_ascii=True, indent=0) + "\n"
if "--verify" in sys.argv:
    assert target.read_text(encoding="ascii") == text
else:
    target.write_text(text, encoding="ascii")
