# S1 性能专项

此专项复用 [baseline](../baseline/README.md) 的 schema、随机种子 42、1000 行写入批、128 维均匀向量、FLAT L2、预热和采样数，比较已实现的 S1。结果说明见 [专项记录](../../docs/testing/s1-performance.md)。

## 口径

- 10 万与 100 万行，建库后显式 Flush，再在新进程中 load。数据全部存活；无删除、upsert、并发写入。不将结果混入原 M2 全操作表。
- 计数：`cat >= 0`，10 个样本；Query：`cat == 5 and id > 1000`、输出 id/tag、limit 100，50 个样本；两个过滤 Search：`cat == 3` 和 `tag == "t7"`，固定查询向量，top 10，各 100 个样本。
- 每项在正式计时前先调用一次作正确性预检查，再沿用 M2 的一次不计时预热，共两次调用；结束后另检查一次。保存预热及每个计时样本的返回值，计时后校验；时间包含客户端、RPC、计算、响应及轻量列表追加，不包含正确性校验。四个版本的调用数相同，这一额外预检查也使本表不能直接混入历史 M2 表。
- Query 的行、顺序、计数从生成器推导；Search 对照 float64 FLAT 真值，距离误差上限为 0004 的 `max(1e-5 * abs(d), 1e-6)`，并拒绝非有限距离。错误、丢行、顺序或召回偏移直接终止，失败不输出测量记录。
- Rust 优化前后各用独立 wheel/venv；2.5.1 与 3.2.1 各用独立环境。Python、pymilvus、NumPy 版本须一致；基准在同机顺序运行，轮换顺序三轮，分别保留每轮 p50/p99，不把 10 个计数样本的 p99 当高精度尾延迟。
- `--prepare` 只接受不存在的库；读测使用已建库，不清空或覆盖历史产物。所有库与 JSON 原始输出必须放仓库外。Rust 两版本顺序读取同一目录，从而保持物理布局一致。
- 输出记录脚本指纹、实际加载的 Rust 扩展 SHA256、构建身份、Python / pymilvus / NumPy 版本、环境安装了 pyarrow 时的版本、平台和 load average。`--build` 是操作者提供的版本标签，不能替代扩展 SHA256。

## 运行

先按 baseline 建好 `v2`、`v3` 环境，并为优化前与优化后 wheel 分别创建 `before`、`after` 环境。创建 Rust 库时只用 before；after 顺序读取同一个库。`work_dir` 在仓库外，库的父目录须存在。

```bash
<work_dir>/before/bin/python benchmarks/s1/run.py <work_dir>/rust-100000.db --rows 100000 --prepare --round 0 --build <before_commit>
<work_dir>/v2/bin/python benchmarks/s1/run.py <work_dir>/v2-100000.db --rows 100000 --prepare --round 0 --build 2.5.1
<work_dir>/v3/bin/python benchmarks/s1/run.py <work_dir>/v3-100000.db --rows 100000 --prepare --round 0 --build 43d1257
```

准备阶段的输出只作冒烟，不计入正式三轮。对 100 万行重复准备；之后两规模各按 before/v2/v3/after、v2/v3/after/before、v3/after/before/v2 三轮顺序读取，去掉 `--prepare`、传入实际轮号并将 stdout 追加到仓库外 JSONL。不要在测量期间同时构建或跑其他测试。

```bash
<work_dir>/after/bin/python benchmarks/s1/run.py <work_dir>/rust-100000.db --rows 100000 --round 1 --build <after_identity> >> <work_dir>/results.jsonl
```

完整流程有 24 条正式记录，按 rows/build/脚本指纹/扩展指纹分组，比较三轮 p50 的中位数及最小/最大值。差分与端到端测试是正确性证据，专项性能表不替代它们；存活位密度、MemTable 与 segment 的组合、写入并发及大范围输出不由这个只读场景作性能结论。

## 历史测量脚本

2026-10-08 两轮历史表使用脚本指纹 `cce9abebc190c27a38ad9b1c3f14397d7a259b07331c1d8b19efd14376e06766`。复审后脚本增加 NumPy/pyarrow 版本记录，并将旧的 `np.allclose(rtol=1e-5, atol=1e-5)` 校验收紧到 0004 的最大值公式；新指纹不能替换旧表的身份，旧原始输出也不回填不存在的版本字段。

baseline 的文件清单和字节都固定取自提交 `e7632ad8e38e226e3efcf69cc24baac2271e0ba4`：先以 `git ls-tree --name-only <提交>:benchmarks/baseline` 枚举该历史树中的九个 `.py` / `.sh` 文件，再以 `git show <提交>:benchmarks/baseline/<文件>` 取得内容，不读取当前工作树。将这些文件放到仓库外的 baseline/，当前 s1/run.py 放到相邻的 s1/；于副本的 s1/ 中执行 `git apply <本仓>/benchmarks/s1/measured-20261008.patch`，即可恢复测量时的 run.py。按脚本自身的目录名/文件名/字节顺序复算，得到上述完整指纹；自检固定这一结果。恢复补丁只用于历史自证，新的测量使用当前严格校验版本。

历史自检需要本仓 Git 历史包含上述提交；浅克隆应先取得它，例如 `git fetch --unshallow origin`，不能在历史对象缺失时退回当前文件或跳过自检。自检本身不访问远端。

以后修改 run.py 时，须同步维护补丁：**修改前**先用当前文件及现有补丁在仓库外还原并保存测量时的 run.py；**修改后**从新的当前文件向该冻结副本生成反向补丁。目标仍是原测量源码，不能修改历史指纹迁就失败。可用以下命令生成补丁（冻结副本在仓库外，命令在仓库根执行）：

```bash
python3 - benchmarks/s1/run.py "<work_dir>/frozen/run.py" benchmarks/s1/measured-20261008.patch <<'PY'
from pathlib import Path
import difflib
import sys
current, frozen, patch = map(Path, sys.argv[1:])
patch.write_text("".join("\n" if line == " \n" else line for line in difflib.unified_diff(
    current.read_text().splitlines(keepends=True), frozen.read_text().splitlines(keepends=True),
    fromfile="a/run.py", tofile="b/run.py", n=3)))
PY
```

空上下文行不带前缀空格，以通过 Git 空白检查；这只改变补丁表示，不改变恢复字节。随后运行 `tests/benchmarks`；恢复结果必须仍为上述完整指纹。测试还在本地副本里修改、删除和新增 baseline 文件，验证 S1 与 M2 的历史输入都不受工作树变动影响；不会改动本仓基准脚本。
