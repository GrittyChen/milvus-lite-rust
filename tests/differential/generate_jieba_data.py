"""从参照环境的 jieba 0.42.1 生成 jieba 分词的内置数据（0036「设计」）：词典原样复制，HMM 模型按全精度写出。

用法：<装了 jieba 0.42.1 的 CPython 3.12> generate_jieba_data.py [--verify]

- `dict.txt`：jieba 包里的原文件，按 sha256 核对后原样复制；「B超」的重复行也保留，`total` 依赖它。
- `hmm.model`：finalseg 的 `prob_start.py`、`prob_trans.py`、`prob_emit.py`（CPython 上 jieba 读的是这三个 .py，
  不是 .p）按 jieba-rs `HmmModel::load` 的格式写出，浮点用 `repr`，解析回 f64 与 Python 的值逐位相同。
  jieba-rs 自带的模型把发射概率舍入到 6 位小数，所以不用它。缺省的转移与发射都是 `MIN_FLOAT`，与 Python 的
  `.get(..., MIN_FLOAT)` 相同。

`--verify` 只比较，不写文件。
"""

import hashlib
import sys
from pathlib import Path

import jieba
import jieba.finalseg as finalseg

DICT_SHA256 = "7197c3211ddd98962b036cdf40324d1ea2bfaa12bd028e68faa70111a88e12a8"
STATES = "BEMS"
MIN_FLOAT = -3.14e100

assert jieba.__version__ == "0.42.1", jieba.__version__
assert finalseg.MIN_FLOAT == MIN_FLOAT
target = Path(__file__).resolve().parents[2] / "crates/core/data/jieba"

dictionary = (Path(jieba.__file__).parent / "dict.txt").read_bytes()
assert hashlib.sha256(dictionary).hexdigest() == DICT_SHA256

lines = ["# jieba 0.42.1 finalseg 的 prob_start / prob_trans / prob_emit，全精度；由 "
         "tests/differential/generate_jieba_data.py 生成。状态顺序 B E M S。"]
lines.append(" ".join(repr(finalseg.start_P[s]) for s in STATES))
for source in STATES:
    lines.append(" ".join(repr(finalseg.trans_P[source].get(to, MIN_FLOAT)) for to in STATES))
for state in STATES:
    # jieba-rs 按最后一个 `:` 切分字符与概率，按码位排序使输出确定。
    lines.append(",".join(f"{ch}:{prob!r}" for ch, prob in sorted(finalseg.emit_P[state].items())))
model = ("\n".join(lines) + "\n").encode("utf-8")
assert all(len(ch) == 1 for state in STATES for ch in finalseg.emit_P[state])

outputs = {"dict.txt": dictionary, "hmm.model": model}
if "--verify" in sys.argv:
    for name, content in outputs.items():
        assert (target / name).read_bytes() == content, name
    print("ok")
else:
    target.mkdir(parents=True, exist_ok=True)
    for name, content in outputs.items():
        (target / name).write_bytes(content)
