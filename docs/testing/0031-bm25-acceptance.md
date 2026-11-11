# 0031：BM25 函数与全文检索实施验收

- 状态：设计 v3 评审通过；第 1 步（schema、分词器与写入）、第 2 步（索引与度量）、第 3 步（检索）、第 4 步（差分、互通、参照用例、文档与性能）各自评审通过；本记录随第 4 步提交。**未发布**。
- 日期：2026-11-11
- 合同与设计：[0031](../decisions/0031-bm25.md)，继承 0002、0006、0014、0029（standard 分词与 `analyzer_params`）、0030（稀疏向量、倒排槽位与排序键）。
- 仓库基线：0030 合入后的 `59716a4`。
- 参照：milvus-lite `1320d26`（v3.2.2 之后），保持只读；差分与基准的参照环境为 Arrow 25、NumPy 2.5.x，互通另有 Arrow 15、NumPy 1.26.4。SDK 固定 2.6.x / 3.0.2。
- 开发产物沿用 `0.4.0rc1` 版本字符串，不是新的发行候选。

## 结论

BM25 function（`FunctionType.BM25`）的建表校验、持久化与回显，写入时生成输出字段，`SPARSE_INVERTED_INDEX` + BM25 的定义与 k1 / b，以文本为查询的检索，以及 BM25 在 range、offset、分组、boost、L2 rerank、`round_decimal`、search iterator、HybridSearch 与带分区检索中的行为已实施。行为以真实 3.x 为真值：打分与次序在 NumPy 2 环境中逐位一致（仓库差分逐位比较距离，基准的全部检索结果摘要相同），数据目录与 Arrow 15 / 25 的 3.x 双向互通。与 3.x 的差异只有 [0031「与 3.x 的差异」](../decisions/0031-bm25.md#与-3x-的差异) 登记的各项，0006 的差异总表同步新增两行。

## 实施范围

| 步骤 | 内容 |
| --- | --- |
| 1 | core：`analyzer::Bm25Analyzer`（停用词、FNV-1a、按首次出现顺序的词频）、`schema::Function` 与函数校验、`analyzer_params` 原值、写入时的输出字段检查与生成（不经 `normalize`）、Upsert / partial upsert 重新生成；gRPC：函数解码与 DescribeCollection 回显；`schema_file` 读写函数 |
| 2 | `Metric::Bm25` 与度量换算表（含 boost 的 `public_score` 与 rerank 链入口的取负）；`build_spec` / load / 检索按期望度量参数化；k1 / b 校验；文本 placeholder 解码与 Python 的 UTF-8 报错；HybridSearch 子请求 |
| 3 | `sparse_index` 的 BM25 倒排与块统计、按数据源汇总、NumPy 2 的 float32 打分；`rank_sparse` 的 BM25 分支；排序键对负分单调 |
| 4 | 差分（打分随机对照、各检索路径、分区、重开）、NumPy 主版本守卫、互通分数比较与维度 `2^32-1`、参照用例迁入、计数同步、文档、基准与变异验证 |

## 仓库内可重跑的验证

| 套件 | 结果 | 命令与判据 |
| --- | --- | --- |
| Rust 单测与引擎用例 | 640 条通过 | `cargo test --workspace`；BM25 相关的是 [bm25.rs](../../crates/core/tests/bm25.rs) 10 条，以及 `sparse_index`、`index`、`collection`、`rerank` 的单测 |
| fault-injection | 777 条通过 | `cargo test -p milvus-lite-core --features fault-injection` |
| clippy（默认与 fault-injection） | 无告警 | 见 AGENTS.md「常用命令」 |
| 端到端（pymilvus 3.0 / 2.6） | 各 648 条通过 | `tests/python`，其中新迁入的参照用例 46 条 |
| 差分（pymilvus 3.0 / 2.6） | 各 792 条通过，会话组 404 | `tests/differential`；BM25 场景 10 条，见下 |
| 互通 | 305 条通过 | `tests/storage_interop`；BM25 9 条 |
| 工具自检 | 956 条通过 | `tests/packaging tests/platform tests/ci tests/sharding tests/release tests/benchmarks` |

**BM25 差分**（[bm25_cases.py](../../tests/differential/bm25_cases.py)、[test_bm25.py](../../tests/differential/test_bm25.py)）逐项比较本实现与 3.x 的结果，距离按 repr 逐位比较：

- 函数校验、`analyzer_params`、函数参数、写入与读回、停用词与两个函数（第 1 步）。
- `index_metric_and_queries`：建索引的度量与约 30 种 k1 / b 取值、load 后检索的度量与查询种类、非法 UTF-8 文本（含 HybridSearch 子请求）、普通稀疏字段与稠密字段上的 BM25 度量。
- `scoring_random`：固定种子的 30 轮随机写入（upsert、删除、空文本与 null）、随机 flush（多 segment、一个 segment 多个块）、随机 k1 / b 与过滤，共 120 次检索。
- `search_paths`：offset、两个方向的 range、`round_decimal`、分组、boost（乘与加）、rrf、weighted 归一化、decay、boost 接 rrf、HybridSearch 的四种合并与分组、search iterator。
- `partitions`：MemTable、flush 后与混合三个阶段，四种分区选择 × 有无过滤。
- 重开（`before_reopen` / `reopened`）：带 k1 / b 的集合，segment 从 Parquet 读回、WAL 回放进 MemTable 后的分数。
- `REGISTERED` 两边分别断言的差异：jieba、函数参数里的非严格 JSON、调参项里的 `NaN` / `Infinity` / `-Infinity`（建索引与检索请求）。

差分在启动时检查参照环境的 NumPy 主版本，NumPy 1 直接报错（[conftest.py](../../tests/differential/conftest.py)）。

**互通**（[test_bm25.py](../../tests/storage_interop/test_bm25.py)）：三个实现轮流写读 WAL、segment 与 compaction 三种状态；函数定义、输出字段标记、停用词与生成的稀疏值（含 FNV-1a 恰为 `0xFFFFFFFF` 的词 `95h9wb8`）三方一致；只读时本实现与 Arrow 25 的 3.x 的检索结果逐位相同，Arrow 15（NumPy 1）只比数据。

**参照用例**：从 `1320d26` 迁入 `test_grpc_fts.py`、`test_milvus_fts_compat.py` 全部，`test_multi_index.py`、`test_sparse_field_contract.py` 中不依赖内部模块的用例，以及 `test_hybrid_search.py` 中依赖 BM25 的 9 个函数，共 39 个函数、46 条，逐条状态见 [INVENTORY.md](../../tests/python/adapter/INVENTORY.md)。

## 变异验证

在仓库副本上逐个施加下列错误，只跑 core 的单测与 BM25 引擎用例，每一个都至少被一条用例抓住：

| 变异 | 抓住它的用例 |
| --- | --- |
| `tf·(k1+1)` 改在 float32 中计算（差 1 ULP） | `bm25_tuning_from_index_definition_matches_3x` |
| rerank 链入口不再对 BM25 取负 | `chain_input_negates_only_bm25_distances` |
| 空行不计入 N | `bm25_blocks_of_one_source_score_like_a_single_block` |
| df 只取数据源的第一个块 | 同上 |
| 总长度用 float32 累加 | `bm25_total_length_accumulates_in_f64` |
| MemTable 的统计不看过滤 | `bm25_scores_match_3x_in_memtable_and_segments` |

最初的变异中，前一项与总长度一项没有被抓住；本批补了后两条用例（真值取自 3.x），重跑后全部抓住。

## 性能

[benchmarks/bm25](../../benchmarks/bm25/README.md)：10 万行文本、2 万词的幂律分布，三方轮换先后共三轮，取中位数；本实现与 3.x 三轮的全部检索结果摘要相同。

| 中位数 | 本实现 | 3.x（`1320d26`） | 2.5.1 |
| --- | --- | --- | --- |
| 分 10 批写入（含生成稀疏向量） | 746 ms | 1735 ms | 2652 ms |
| flush | 120 ms | 477 ms | 0.4 ms |
| 首次检索（含建倒排） | 32.6 ms | 440 ms | 0.9 ms |
| nq=1 热查询，中位数 / p95 | 0.5 / 0.8 ms | 78 / 120 ms | 0.3 / 0.5 ms |
| nq=32 | 2.4 ms | 2265 ms | 1.7 ms |
| 带过滤（一半行）的 nq=1 | 0.4 ms | 40 ms | 0.3 ms |
| 进程树 RSS | 320 MiB | 743 MiB | 242 MiB |

同机（arm64，15 核）、Python 3.12、pymilvus 3.0.2。本实现在写入、flush、检索与内存上都优于 3.x；与 2.5.1 相比写入更快，检索与内存仍有差距（2.5.1 的 flush 是异步的，统计是全局的，口径不同）。原始结果由负责人本地留存，可用上述脚本重跑。

## 未完成与风险

- 手工改写成非整数 tf 的输出列，在一个 segment 有多个块时总长度可能差 1 ULP（0031 已说明，不单列为差异）。
- NaN 分数的结果集合与次序不复现，已登记。
- 本批尚未经 CI 与 Compare；提交、推送与合入由负责人授权。
