# jieba 0.42.1 的内置数据

这个目录里的文件编译时嵌入引擎，供 jieba 分词使用（[0036](../../../../docs/decisions/0036-jieba.md)）。

- `dict.txt`：jieba 0.42.1 的默认词典，原样复制，没有改动。来源是 PyPI 上 `jieba-0.42.1.tar.gz` 里的 `jieba/dict.txt`；sdist 的 sha256 为 `055ca12f62674fafed09427f176506079bc135638a14e23e25be909131928db2`，`dict.txt` 的 sha256 为 `7197c3211ddd98962b036cdf40324d1ea2bfaa12bd028e68faa70111a88e12a8`。
- `hmm.model`：由同一版本 `jieba/finalseg` 下的 `prob_start.py`、`prob_trans.py`、`prob_emit.py` 转换而来，概率保留全精度，只改了排列格式，以便 jieba-rs 的 `HmmModel::load` 读取。
- `LICENSE`：jieba 的 MIT 许可（Copyright (c) 2013 Sun Junyi），取自上游仓库 `fxsjy/jieba` 的 `v0.42.1` tag。sdist 里没有许可文件。

两个数据文件都由 [generate_jieba_data.py](../../../../tests/differential/generate_jieba_data.py) 生成，`--verify` 用来核对。
