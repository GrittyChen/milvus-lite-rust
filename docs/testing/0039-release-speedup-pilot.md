# 0039 发行提速：实施期检查、会话组测量与试点

本文记录 [0039](../decisions/0039-release-speedup.md) 的三部分证据：实施期的本地检查、D 的测量，以及合入后的试点。本文写作时，合入后的试点尚未执行（见末节）。

证据的分类与复核范围见[证据的存放与复核范围](README.md)：

- 本地检查与测量的原始输出由负责人本地留存。
- 回放结果可用仓库内的 `runs.py basis` 对公开的 tag 与运行记录重算。

## 实施期的本地检查

均在实施分支的工作树上执行，2026-11-21。

### 工具自检

| 套件 | 收集数 | 结果 |
| --- | --- | --- |
| `tests/release` | 560（实施前 475） | 全部通过 |
| `tests/ci` | 147 | 全部通过 |
| `tests/packaging` | 212 | 全部通过 |
| `tests/platform` | 15 | 全部通过 |
| `tests/sharding` | 52 | 全部通过 |

`tests/release` 新增或修改的用例：

- **[test_signing_basis.py](../../tests/release/test_signing_basis.py)**：在临时 git 仓库中，以真实的 `release.yml` 与 `tests/release` 工具构造提交，覆盖三类情形：
  - 签发路径内外的改动；
  - 基线的跳过与整体失败；
  - 前置检查的判定顺序。
- **[test_candidate.py](../../tests/release/test_candidate.py)**：候选结果格式 2 与 `dry_run_basis` 的取值组合；24 份已测摘要的成对核对及其反例；import 闭包与登记清单一致。
- **[test_sums.py](../../tests/release/test_sums.py)**：`sums.py tested` 的真实输出经 `candidate.py tested` 的命令行通过核对；用错平台的 FI wheel 被拒绝。
- **[test_release_workflow.py](../../tests/release/test_release_workflow.py)**：依赖表、macOS 两个测试 job 的矩阵与并发上限、测试 job 不再下载装配产物、`MODE` / `VERSION` / `defaults.run.shell`、签发两段只引用 `MODE` 与 `VERSION` 两个 env 键、不含 YAML 锚点与别名、只调用签发入口。每条约束各有一个变异用例。

`.github/workflows/ci.yml` 中工具自检的预期收集数随之改为 `release:560`。

评审中发现的回归，已修正：

- `tests/packaging` 中的一个用例以 `ast.literal_eval` 读取 `candidate.py` 的 `WHEEL_TAGS`。实施初稿把它改成了表达式，该用例因此失败。
- 修正：恢复为字面量，与新增的平台映射是否一致改由 `tests/release` 核对。

### 静态检查

所用工具：actionlint 1.7.12（与 0.6.0rc1、0.7.0rc1 记录中的同一官方二进制），ShellCheck 0.11.0（Homebrew 安装）。

- **实施分支**：四个工作流以 `actionlint -shellcheck <shellcheck>` 检查，零告警，退出码 0。
  - 为确认 ShellCheck 确实参与：在一份临时副本中，把 `release.yml` 的一处命令改成未加引号的变量，actionlint 随即报出 SC2086。
  - 仓库中的 `.sh` 脚本逐个以 ShellCheck 检查，零告警。`.github/ci/crash_capture.sh` 以 source 方式执行、没有 shebang，按 `-s bash` 检查。
- **补做 0.7.0rc1 的 ShellCheck**：[0.7.0rc1 验收记录](0.7.0rc1-acceptance.md)登记过「ShellCheck 没有执行」，本次对 tag `v0.7.0rc1` 的树（`git archive`）补做：

  | 检查 | 结果 |
  | --- | --- |
  | `actionlint -shellcheck` 检查四个工作流 | 零告警 |
  | `shellcheck .github/ci/*.sh` | 返回 1，唯一问题是 `crash_capture.sh` 既有的 SC2148（没有 shebang） |
  | `shellcheck -s bash .github/ci/crash_capture.sh` | 零告警 |
  | `shellcheck benchmarks/baseline/*.sh` | 零告警 |

  与 0.6.0rc1 记录的结论相同。这是事后补做，不改变 0.7.0rc1 发布时的证据状态。

### 签发路径回放

用 `runs.py basis` 对仓库中真实的 tag、台账与运行记录回放过去四个版本的冻结提交：

| 冻结提交 | 基线 | 判定 | 变化的路径 |
| --- | --- | --- | --- |
| `4fdffbb`（0.4.0rc1） | 0.3.0rc3 | 须演练 | `release.yml#signing-jobs`、`preserve.py`、`release_set.py`、`runs.py` |
| `fd70faf`（0.5.0rc1） | 0.4.0rc1 | 可省 | 无 |
| `5f7c191`（0.6.0rc1） | 0.5.0rc1 | 可省 | 无 |
| `eb4bbd3`（0.7.0rc1） | 0.6.0rc1 | 可省 | 无 |

- 后三个冻结提交的签发路径摘要相同，都是 `32c50fe468c0…`。
- 结果与 0039 第 6 条的预期一致。
- 这一步也让前置检查新增的本地 git 读取与运行记录查询在真实数据上执行了一次。

## D：差分会话组的测量

### 环境

- **机器**：本机，Apple M5 Pro，15 核。
- **本实现**：开发扩展为当前提交的 `--release` 构建。
- **参照**：参照环境为 `1320d26`、Arrow 25。
- **jieba**：参照环境原来没有 jieba，按 CI 的做法从固定哈希的 sdist 构建 wheel 后装入（[0036](../decisions/0036-jieba.md) 决策 G），词典 SHA256 `7197c321…` 与 CI 一致。

### 串行基线

`shard.py --shards 1`：874 项全部通过，会话组 405 项。

| 部分 | 用例数 | 耗时（秒） | 占比 |
| --- | --- | --- | --- |
| 会话组 | 405 | 399 | 25% |
| 其余 | 469 | 1209 | 75% |
| 合计 | 874 | 1607 | |

会话组内最重的两个模块：

- `test_search_request_resolution.py`：144 项，136 秒；
- `test_scenarios.py`：25 项，87 秒。

### 分片的分配

用分片工具对同一收集结果分别做两种计划，再以串行基线的逐项耗时估算各片耗时：

- **按用例数**：不给 `--durations`，即 CI 的现状；
- **按耗时**：给 `--durations`。

| 片数 | 按用例数：各片（用例数，秒） | 最慢 | 按耗时：最慢 |
| --- | --- | --- | --- |
| 3（macOS） | (405, 399)、(235, 669)、(234, 540) | 669 | 536 |
| 4（Linux） | (405, 399)、(157, 376)、(156, 505)、(156, 328) | 505 | 402 |

**结论**：

- **3 片时，会话组不是最慢的一片。** 最慢的是按用例数分到的非会话组片。
- **4 片时，按耗时分配后的下限正好落在会话组上**，399 秒，约等于整轮的四分之一。
- **改为按耗时分配，两种片数下最慢一片都缩短约 20%。** 0039「问题」一节推测会话组是差分 job 的长杆，对 3 片并不成立；那一节的数据（3 片的计划为 `[405, 235, 234]`）本身无误。

### 会话组按模块拆分

把会话组按模块分成 3 份：每份内保持收集顺序，按耗时做最长处理时间优先分配。每份一个 pytest 进程，三个进程并发，重复 10 轮。

| 份 | 模块 | 用例数 | 10 轮的耗时（秒） |
| --- | --- | --- | --- |
| 1 | `test_search_request_resolution.py` | 144 | 135.8–137.1 |
| 2 | `test_scenarios.py`、`test_multi_vector.py`、`test_text_match.py`、`test_filter_s1.py`、`test_read_snapshot.py`、`test_connections.py` | 161 | 134.4–135.3 |
| 3 | `test_filter_tree.py`、`test_filter_dynamic.py`、`test_sparse_vectors.py`、`test_index_definitions.py`、`test_bm25.py`、`test_search_across_sources.py` | 100 | 131.5–132.5 |

- 30 个进程全部退出码 0，没有失败用例。
- 会话组的墙钟时间从 399 秒降到约 137 秒。
- 每个进程各自启动一套会话级服务。

### D 的结论

本文只测量，不改分片工具或 0019，后续按收益排序：

1. **按耗时分配**（分片工具已支持 `--durations`，0019「P1」第 2 条本就按耗时分配）对 `Compare` 与发行工作流的差分 job 都有约 20% 的收益，而且不改变会话组约束。CI 现在没有提供耗时数据，全部用例按相同权重计，即按用例数切分。需要另定的是耗时数据的来源：0019 规定耗时数据不入库，需改由同一运行或上一运行的产物提供。
2. **会话组按模块拆分**在 10 轮并发下没有发现模块间的状态依赖。只有片数达到 4 及以上、会话组成为下限时才有收益，需要修订 0019「P1」第 2 条中会话组整组放入同一片的约束，并按 0019 的敏感用例要求在三个平台上重复验证。

以上耗时都来自本机的一次串行运行。CI 的 Intel 与 arm64 执行机上，各用例的相对耗时可能不同，另立决策时须以 CI 运行的 JUnit 复核。两项都另立决策。

## 合入后的试点

尚未执行。按 0039「实施与验证」第 5 条，合入后进行两项，结果补记于此：

- 在 `main` 上以 `0.7.0rc1` 跑一次正向演练，核对编排、24 份已测摘要的交叉核对与候选结果格式 2，并记录实际耗时与 macOS 放行时间；
- 以测试版本 `0.0.1rc905` 做失败注入，预期前置检查在判定处失败，不消费版本。

## 本地留存

由负责人本地留存：

- 串行基线的计划与运行输出（含 JUnit 与逐项耗时）；
- 四组分配估算的计划；
- 拆分测量的分组、30 份日志与 JUnit，以及测量脚本；
- 签发路径回放的原始输出；
- 实施分支与 `v0.7.0rc1` 的 actionlint 及 ShellCheck 输出。
