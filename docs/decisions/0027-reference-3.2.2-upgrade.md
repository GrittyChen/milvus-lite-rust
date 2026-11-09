# 0027：参照实现升级到 v3.2.2 之后的 `1320d26`

- 状态：第 1 步（行为对齐）已实施，通过开发验收，独立代码评审的发现已修复；第 2 步（flush 字节预算）已实施，完成基线对照，独立代码评审的发现已修复。
- 日期：2026-11-09。
- 参照：milvus-lite `1320d26ec3d88b23b525e774df6e3c8e7c208f81`（v3.2.2 之后 3 个提交）；此前为 v3.2.1 / `43d1257`。
- 前置：[0004](0004-m1-scope.md)、[0006](0006-m2-scope.md)、[0017](0017-partitions.md)、[0026](0026-rerank-and-hybrid-search.md)。

## 背景与版本选择

`43d1257` 之后上游陆续改了稠密检索的对外行为，也为后续的稀疏向量提供了设计。因此在继续扩展功能之前先升级参照，否则新功能会对齐到已经被上游修改的行为。

钉在提交而不是 tag：工作流本来就按完整 SHA 取参照，没有额外成本；v3.2.2 之后的 3 个提交里，带分区的 `count(*)` 修复（#381）是对外行为，其余两个（#372、#377）或已经在本实现成立，或收窄已登记差异。代价是 `count(*)` 的行为与 PyPI 上已发布的 3.2.2 不同。

此前的决策与验收记录中的 `43d1257` 记录当时的基准，不随本次升级改写。`tests/compatibility_audit/` 下未进入候选门禁的调查工具及其固定样本同样保持原基准；进入门禁的 `schema_truth.py` 已更新。

## 对外行为变化

下列行为按 `1320d26` 对齐。实现集中在 [collection.rs](../../crates/core/src/collection.rs) 的 `resolve_search_metric`、[projection.rs](../../crates/core/src/projection.rs) 的 `emits_primary` 与 [query.rs](../../crates/grpc/src/query.rs) 的 count 分支。

| 项目 | 3.x 的新行为 | 对用户的影响 |
| --- | --- | --- |
| Search / HybridSearch 的主键投影 | 主键始终在 ids 中；只有显式请求或 `*` 时才进 `fields_data` 与响应的 `output_fields`。Query / Get 不变 | `hit["id"]` 不变；未请求主键时 `hit["entity"]` 不再含主键，依赖它的代码需要显式请求 |
| 请求度量的形式 | 转大写后比较，`ip`、`Cosine` 可用；空串与 JSON `null` 等同未传；非字符串在解析阶段报 `metric_type must be a string`（code 6） | 原先报错的小写度量现在成功 |
| 度量的默认值 | 请求未带时取目标字段索引的度量，再没有时取 COSINE；索引度量为 BM25 / NONE 时报错，不回退到 COSINE | 这类索引上未带度量的 Search 由成功变为失败 |
| 未知度量的报错 | code 6，`field 'v': unsupported dense metric 'X'`，`X` 是大写后的值 | 错误码与文案变化 |
| CreateIndex 的度量 | 转大写后校验并存储 | `add_index(metric_type="cosine")` 成功 |
| 带 `partition_names` 的无过滤 `count(*)` | 分区名去重后逐分区求和，同一主键跨分区时重复计入；不检查 load；分区不存在时 code 200，reason 为分区名 | 不再返回全集合行数 |

### 错误先后

- Search：解析（placeholder、topk、metric_type 的类型、radius、range_filter、group_size、function_score、offset、round_decimal）→ `anns_field` → 度量取值 → `function_score` 与 `function_chains` 的互斥检查 → load 与分区 → group_by → 过滤。
- HybridSearch：`rank_params` → `function_score` → 逐路（解析 → `anns_field` 与度量 → 完整执行该路）。第 1 路的过滤错误先于第 2 路的度量错误。

## 已登记差异的变化

- 请求度量与索引度量不一致时（[0004「决策」](0004-m1-scope.md#决策)）：3.x 在 segment 的有效行数不超过实际 top_k 时改走精确检索，与本实现一致；差异只留在有效行数大于 top_k 的 segment 上，判据是逐 segment 的，一次检索可能混用。
- 未知度量在过滤无候选时的差异消失，3.x 同样报错；错误维度的差异保留。

## 尚未对齐

- **`metric_type` 为含 NaN / Infinity 的 JSON 容器字面量**（如 `[NaN]`）：3.x 的 `json.loads` 解析为列表，报 `metric_type must be a string`；本实现按字符串走到度量校验，报 `unsupported dense metric '[NAN]'`。只有原始 gRPC 能触发，与 [0026](0026-rerank-and-hybrid-search.md) 对非严格 JSON 参数的处理同类，不单独实现。裸的 `NaN`、`Infinity`、`-Infinity` 与 3.x 一致。

- **稀疏向量相关改动**：随稀疏向量切片处理，当前仍为 UNIMPLEMENTED。

存储格式没有变化：WAL、Parquet、manifest 与 `schema.json` 的稠密字段编码相同，上游的 Parquet 持久化修复（#377）在本实现中原本成立。

## 第 2 步：flush 字节预算

3.x 自 #374 起不再按 1 万行触发 flush。按存活主键数触发时，反复改写少量主键不增加计数，而保留的批次与 WAL 持续增长：100 个主键、1536 维、重复 upsert 1200 轮，本实现改动前 WAL 增长到 729 MiB、进程内存 1550 MiB，从未 flush；改动后 WAL 峰值 132 MiB、内存 381 MiB，segment 数量的变化与 3.x 相同。

### 合同

- **触发**：每次写入之后，MemTable 保留字节达到 128 MiB，或未 checkpoint 的 WAL 达到 256 MiB。按 collection 分别计算，不是进程级内存上限；一批可以越过预算。flush 仍同步执行，写入路径与取锁顺序不变。
- **MemTable 计量**：WAL batch 的 Arrow 缓冲区字节（与 pyarrow 的 `get_total_buffer_size` 同口径，含已被覆盖的行），加 tombstone 的估算（沿用 3.x 按 CPython 对象大小的口径，删除同一主键不重复计）。WAL 回放时同样累计。
- **WAL 计量**：当前一代实际写出的字节，加打开时已有的 WAL 文件大小；后者在下一次冻结时清零，此后由该代的提交（失败时的重试）负责。超预算的可能只有 WAL，所以按预算触发时即使活跃表为空也提交一代。
- **恢复**：打开时已超预算，在提供服务之前 flush，只读使用也能回收。失败时打开失败，且不缓存，下次访问重试；3.x 同样让打开失败。这次 flush 发生在句柄注册之前，不进入维护队列（打开失败时句柄被丢弃，留在队列里的后台合并会用旧 manifest 覆盖新实例的提交），注册成功后才调度维护。
- **环境变量**：`MILVUS_LITE_MEMTABLE_MEMORY_LIMIT_BYTES`、`MILVUS_LITE_WAL_SIZE_LIMIT_BYTES` 在导入 `milvus_lite` 时读取一次，解析规则与错误文案同 3.x（Python `int()`，非正数或非法值抛 `ValueError`）。

### 差异

自动 flush 的位置不保证与 3.x 逐字节相同：tombstone 的字节按估算计；WAL 的计量取实际写出的字节，小批量时本实现的 Arrow IPC 编码更大（单行写入或单主键删除时，WAL 预算约提前 2.4 至 2.7 倍触发，`MemTable` 口径的位置与 3.x 基本相同）。这会影响依赖 segment 布局的量，例如后续 BM25 的 segment 级统计；需要逐位对齐分数时用显式 flush 控制布局。

### 基线对照

同机 macOS arm64（M2 Pro，桌面负载未隔离），`benchmarks/baseline` 的 `m2` + `memory` 口径，128 维 FLAT。「改动前」为第 1 步的构建。本实现两列 3 轮中位数；2.5.1 在 10 万行为 2 轮（首轮连接失败），100 万行的 2.5.1 与 3.x 各 1 轮。原始记录在仓库外留存。

| 10 万行 | 改动后 | 改动前 | 2.5.1 | 3.2.2 |
| --- | --- | --- | --- | --- |
| 写入（行/秒） | 130032 | 85111 | 51200 | 53928 |
| 单条检索 p50（ms） | 0.53 | 0.54 | 1.11 | 538.82 |
| 100 条检索 p50（ms） | 13.83 | 13.72 | 10.08 | 1771.50 |
| 重开到就绪（s） | 0.06 | 0.07 | 0.81 | 0.08 |
| 服务 RSS，flush 后（MiB） | 446 | 288 | 188 | 810 |

| 100 万行 | 改动后 | 改动前 | 2.5.1 | 3.2.2 |
| --- | --- | --- | --- | --- |
| 写入（行/秒） | 111249 | 85706 | 51777 | 36926 |
| 单条检索 p50（ms） | 2.52 | 2.57 | 8.74 | 371.20 |
| 100 条检索 p50（ms） | 87.02 | 88.87 | 77.62 | 12238.43 |
| 重开到就绪（s） | 0.57 | 0.59 | 3.31 | 0.51 |
| 服务 RSS，flush 后（MiB） | 1248 | 1233 | 814 | 2645 |

结论：写入提升（减少了小 flush），检索与重开时间不变。代价在内存：10 万行时 flush 后 RSS 多约 158 MiB，100 万行多约 15 MiB，100 万行重开后多约 91 MiB（`m2` 库，895 → 986 MiB）。100 万行时本实现高于 2.5.1 的差距是既有问题，不是本次引入。

## 开发验收

同一源码，macOS arm64，CPython 3.12（2.6 线为 3.10），参照 `1320d26`；其余平台由 CI 与候选工作流验证。

| 套件 | 结果 |
| --- | --- |
| `cargo fmt --check`、clippy `-D warnings` | 通过 |
| Rust 单测：默认 / 故障注入 | 545 / 687 通过（含第 2 步新增） |
| 差分：pymilvus 3.0.2 / 2.6.17 | 各 642 条通过（新增 [test_search_request_resolution.py](../../tests/differential/test_search_request_resolution.py) 144 条、[test_flush_budget_env.py](../../tests/differential/test_flush_budget_env.py) 23 条，`test_reopen` 新增 3 条） |
| 端到端：3.0.2 / 2.6.17 | 各 570 条通过 |
| 存储互通（Arrow 15 / 25） | 255 条通过（第 2 步之后重跑） |
| 固定真值：S1 语法、动态过滤、布尔树与 ARRAY 与 nullable（Arrow 15 / 25）、Unicode、时区、存储样本、分区可见性 | 全部通过 |
| CI / 发行 / 分片 / 打包 / 平台 / 基准工具自检 | 956 条通过 |

重新生成的过滤真值只有来源字段与生成器哈希变化，用例内容不变。

## 下一个候选需要写明

发行说明须列出上表对用户的影响，尤其是 `entity` 中主键的变化。还要写明自动 flush 不再按 1 万行触发、新增的两个环境变量，以及恢复时最多回放 256 MiB WAL。
