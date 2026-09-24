# 0004 第一个里程碑（M1）的范围与验收

- 状态：已采纳
- 日期：2026-09-24
- 对照版本：milvus-lite 3.2.1（tag `v3.2.1`，`43d1257`），pymilvus 2.6.17 / 3.0.2
- 前置决策：[0001](0001-refactor-baseline.md)、[0002](0002-data-dir-compat.md)、[0003](0003-in-process-server-lifecycle.md)

## 背景

M1 验证整条链路能否成立：crate 划分、proto 生成、PyO3 打包、进程内服务的生命周期，以及与 3.x 的差分测试方式，不追求功能覆盖。持久化留到 M2，连同 WAL、flush、恢复与崩溃测试一起做。

[0001](0001-refactor-baseline.md) 要求用基准验证「3.x 架构在 Rust 下能达到 2.5.1 水平」。这个推断的核心是 segment 加增量维护的有效位图（0001 约束 2），M1 只有内存数据，测不到这一部分。因此 M1 只验证内存中的写入与检索，主体验证放到 M2（见「验收」第 4 项）。

M1 的对外行为与 3.x 有阶段性差异，本文按 [AGENTS.md](../../AGENTS.md) 规则 2 登记。后续里程碑扩大范围时，在对应文档中注销这里的条目。

## pymilvus 实际发出的 RPC

在 3.x 上给每个 servicer 方法加日志包装，记录 MilvusClient 各操作发出的请求。pymilvus 2.6.17 与 3.0.2 发出的请求逐字段一致，只有 `client_info.sdk_version` 不同。

| 操作 | RPC |
| --- | --- |
| `MilvusClient("./x.db")` | `Connect`；`close()` 不发 RPC |
| `create_collection(name, dimension=…)`，或带 `index_params` 的 `create_collection(schema=…)` | `CreateCollection` → `CreateIndex` → `DescribeIndex` → `LoadCollection` → `GetLoadingProgress` |
| 不带 `index_params` 的 `create_collection(schema=…)` | 只发 `CreateCollection` |
| `create_index` / `describe_index` / `list_indexes` | `CreateIndex` + `DescribeIndex` / `DescribeIndex` / `DescribeIndex` |
| `load_collection` / `get_load_state` / `release_collection` | `LoadCollection` + `GetLoadingProgress` / `GetLoadState` / `ReleaseCollection` |
| `has_collection`、`describe_collection` | 都发 `DescribeCollection`。`has_collection` 根据返回的错误码判断 collection 是否存在 |
| `list_collections` / `drop_collection` / `get_collection_stats` | `ShowCollections` / `DropCollection` / `GetCollectionStatistics` |
| `insert` | 先发 `DescribeCollection` 取 schema（客户端有缓存），再发 `Insert` |
| `search` | `Search`，固定携带 `round_decimal=-1`、`ignore_growing=False` 和一致性相关参数 |
| `get(ids=…)` | `Query`，`expr` 形如 `id in [1,2]` 或 `pk in ['k0','k1']`，`output_fields=['*']` |
| `query(filter="", output_fields=["count(*)"])` | `Query`，`expr` 为空 |

由此可知：
- 按主键 `get` 需要解析 `<pk> in [...]` 表达式；
- `create_collection(name, dimension=…)` 这种快速建表默认开启动态字段。Quick Start 里的 `text`、`category` 存在 `$meta` 中，所以 M1 必须能存储并返回动态字段。

## 范围

### 支持

凡在范围内的操作，结果、错误码和错误信息都与 3.x 一致；下文逐条写出了容易忽略的 3.x 语义。

- **进程内服务**：按 [0003](0003-in-process-server-lifecycle.md) 实现，其中与持久化有关的条目留到 M2。导入时检测官方发行包（[AGENTS.md「pymilvus 接入点」](../../AGENTS.md#核心业务术语)）。
- **连接**：
  - `Connect`；
  - `GetVersion`，返回 `milvus_lite-<本包版本>`，格式与 3.x 相同。
- **数据库**：只有 `default` 一个。
- **collection**：`CreateCollection`、`DescribeCollection`、`ShowCollections`、`DropCollection`、`GetCollectionStatistics`（行数）。collection 不存在时，与 3.x 一样返回 code 100 和 `collection '<name>' does not exist`，`has_collection` 依赖这个错误码。
- **load 状态**：`LoadCollection`、`GetLoadingProgress`、`GetLoadState`、`ReleaseCollection`。语义与 3.x 一致，以下均已实测：
  - 没有索引的 collection 一直处于 Loaded 状态，不 load 也能检索；对它 release 不生效。
  - 在 Loaded 状态下执行 `create_index`，状态保持 Loaded。
  - 有索引的 collection 被 release 后，状态为 NotLoad，search 和 get 报 code 101（不变量 13）；空 `expr` 的 `count(*)` 与 insert 照常可用。
- **schema**：
  - 主键：INT64 或 VARCHAR；`auto_id` 只允许 INT64 主键，VARCHAR 主键设 `auto_id` 时与 3.x 一样报错。
  - 向量：只能有一个 FLOAT_VECTOR 字段。
  - 标量：BOOL、INT8、INT16、INT32、INT64、FLOAT、DOUBLE、VARCHAR、JSON。
  - 动态字段 `$meta`。
- **索引**：稠密向量字段上的 `CreateIndex` 与 `DescribeIndex`，按 [0002「结论」](0002-data-dir-compat.md#结论)中 FLAT 阶段的规则处理：
  - FLAT / BRUTE_FORCE 原生支持；
  - AUTOINDEX 按 FLAT 执行；
  - 其余类型报错。
  - 度量为 L2、IP、COSINE。没有索引时按 COSINE 检索，与 3.x 一致。
- **写入**：`Insert`，包括 `auto_id` 和动态字段。
  - 主键重复时以后写者为准，同一批内、跨批次都如此，`count(*)` 与检索都只看到最新值。
  - `auto_id` 的生成规则与 3.x 一致：每个 collection 从 1 开始，由 `_seq` 派生（见 [0002](0002-data-dir-compat.md#共享状态的语义约束) 约束 4）。差分测试逐项比较生成的主键。
- **检索**：`Search`，支持以下参数：
  - `nq ≥ 1`、`limit`、`offset`；
  - `output_fields`，包括动态字段；
  - `round_decimal`，3.x 已实现。
  - 刚写入的数据立即可见。
  - `search_params` 中的 `metric_type` 与索引度量不一致时，按请求的度量计算，见下文「决策」。
- **与 3.x 一样忽略的参数**：Search 的 `ignore_growing`、`guarantee_timestamp`、`use_default_consistency`，以及 CreateCollection 的 `consistency_level`。3.x 全仓没有读取这些参数的代码；单进程内的读写本来就是强一致，忽略它们不改变结果。这是与 3.x 对齐，不是新的行为差异。
- **查询**：`Query` 只支持两种形式：
  - `expr` 为空，且 `output_fields=["count(*)"]`；
  - `expr` 为 `<pk> in [字面量列表]`，`output_fields` 可以是 `['*']`、字段列表或动态字段。

### 不支持

下列功能都返回明确错误，绝不静默成功（不变量 15）。这些功能 3.x 都支持，M1 如果忽略相应参数，就会在不自知的情况下返回与 3.x 不同的结果，所以同样报错。错误形式与 3.x 的 `_unimplemented`（[servicer.py:1563](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/adapter/grpc/servicer.py#L1563)）相同：gRPC 状态码 `UNIMPLEMENTED`，details 写成 `MilvusLite does not support <功能>: <说明>`，说明中注明当前阶段尚未实现。gRPC 状态非 OK 时响应体会被丢弃，3.x 在响应体里另放的 `Status` 客户端看不到（实测 pymilvus 只得到状态码与 details），因此不必复现。pymilvus 收到 `UNIMPLEMENTED` 时不重试。

- **持久化**：见下文「阶段性差异」。
- **过滤**：除上面两种 `Query` 形式外，不支持任何 `filter` / `expr`，包括 `text_match`、JSON 路径与几何谓词。
- **写入**：`Delete`、`Upsert`。
- **分区**：partition 相关 RPC；在检索与写入中指定分区；partition key。
- **多数据库与别名**：`CreateDatabase` 等数据库 RPC，以及 alias。
- **schema**：
  - 上面未列出的字段类型：其余向量类型、稀疏向量、ARRAY、GEOMETRY、时间类型；
  - nullable、default value、function（BM25、embedding）、多向量字段；
  - 修改 schema 与 collection properties。
- **检索**：`HybridSearch`、range search（`params` 中的 `radius` / `range_filter`）、`group_by`、iterator、rerank。
- **其他**：snapshot、compaction 与 flush 相关 RPC；CLI 与 `milvus-lite dump`。其余未列出的 RPC 一律返回 `UNIMPLEMENTED`。

## 阶段性差异（登记）

| 差异 | 3.x | M1 | 理由 | 影响的用例 |
| --- | --- | --- | --- | --- |
| 数据只在内存中 | 数据持久化，进程重启后仍在 | 进程退出后数据消失 | 持久化连同 WAL、恢复与崩溃测试在 M2 一起做 | 所有重开数据目录的用例，例如 [test_server_restart.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_server_restart.py) |
| 打开已有数据的目录 | 能打开 | `start_and_get_uri` 返回 `None`，日志写明原因，判定规则见 [0003「同路径复用」](0003-in-process-server-lifecycle.md#同路径复用) | 不能把已有数据显示为空库 | 用 3.x 写入过数据的目录启动 |
| 写入磁盘的内容 | 完整的数据目录 | 只创建目录与 `LOCK`，不写其他文件 | 保证 M1 用过的目录 3.x 仍能打开 | 无 |
| 索引类型 | 支持 HNSW 等 ANN 索引，AUTOINDEX 在 flush 后建成 HNSW | 按 [0002](0002-data-dir-compat.md#结论) 的 FLAT 阶段规则执行 | 0002 已登记 | 显式创建 ANN 索引的用例 |
| 检索或写入时指定分区 | 支持；分区不存在时检索返回空结果 | 返回 `UNIMPLEMENTED` | 分区不在 M1 范围内 | 带 `partition_names` 的用例 |
| 上面「不支持」中的其余功能 | 支持 | 返回 `UNIMPLEMENTED` | 不在 M1 范围内 | 用例清单中标为「范围外」的用例 |

由于数据在进程退出后消失，**M1 不发布到 PyPI**，只作为开发与测试用的构建。

## 验收

1. **端到端差分**：
   - **场景**：在 3.x README 的 Quick Start 基础上，去掉过滤与删除，再补上几种用法：显式 schema、`auto_id`、IP 与 L2 度量、VARCHAR 主键、先建表后建索引、release 后再 load。
   - **版本与运行方式**：在 pymilvus 2.6.17 与 3.0.2 下各跑一遍。本包与官方包占用同一个 `milvus_lite` 模块，不能装在一起，因此 3.x 装在另一个 venv 里，以 `milvus-lite server` 独立进程运行；差分脚本用 URI 分别连接两边。
   - **数据量**：每个 collection 少于 1 万行。3.x 满 1 万行才 flush，在此之前数据都在 memtable 中暴力检索，结果是精确的；AUTOINDEX 也不会建成 HNSW。
   - **比较口径**：主键集合（包括 `auto_id` 生成的主键）、输出字段、`count(*)`、load 状态、错误码与错误信息逐项相等；距离的允许误差见「决策」。
2. **复用参照实现的用例**：候选文件大多只通过 [conftest.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/conftest.py#L57) 的 `grpc_server` fixture 起服务，不在顶层导入内部模块。阻碍只有两个：一是起服务的 fixture 要替换；二是同一个文件里混有范围外的用例，而规则 4 不允许用 skip 占位。
   - **复用方式**：M1 把范围内的用例复制到本仓，fixture 换成本实现的起服务方式，并注明来源与 Apache-2.0 许可（[AGENTS.md](../../AGENTS.md) 规则 9）。复制出来的用例与上游脱钩，对照版本升级时要手工同步。某个文件涉及的功能全部进入范围后，改为替换 fixture、原样运行整个文件，不再复制。
   - **用例清单**：随 M1 代码入库，逐条列出候选文件中的每个用例，标注「已复制」「范围外」或「依赖内部模块」，后两类注明原因；「依赖内部模块」要写出具体行号。
   - **范围外用例**：不复制，也不用 skip 或 xfail 占位；为每类不支持的功能另写用例，断言返回 `UNIMPLEMENTED` 且原因正确。
   - **候选文件**（都在 [tests/adapter/](https://github.com/milvus-io/milvus-lite/tree/v3.2.1/tests/adapter)，每个文件都混有范围外的用例）：
     - `test_grpc_collection_lifecycle.py`、`test_grpc_crud.py`、`test_grpc_search.py`：不导入内部模块；
     - `test_grpc_server_startup.py`：只有 `test_server_data_dir_lock_held` 在函数体内导入内部模块（L34–35），其余用例不导入；
     - `test_auto_id.py`：12 条中有 8 条是引擎内部用例，内部模块都在函数体内导入，另外 4 条经 pymilvus；
     - `test_grpc_quickstart.py`：顶层导入 `milvus_lite.index.factory`（L19），整个文件被 `skipif` 包住，只能复制用例本体；
     - `test_grpc_error_mapping.py`：顶层导入内部模块（L14–21），只能复制其中经 pymilvus 的用例。
   - **`tests/compatibility`**：9 个文件共约 240 条用例，以范围外的功能为主，包括过滤、分区与 partition key、多数据库、function、delete / upsert。每个文件还各自用 `start_server_in_thread` 定义起服务的 fixture。M1 不复用，理由写进用例清单；复用的时机与方式同 adapter，也就是文件涉及的功能进入范围后，替换 fixture 整文件运行。
   - **防止静默跳过**：确认 pymilvus 已安装，套件没有被 `importorskip` 整目录跳过。
3. **生命周期**：[0003「验证」](0003-in-process-server-lifecycle.md#验证)中与持久化无关的用例全部通过，但 M1 只在本机平台上跑，Linux 与 Windows 另见「跨平台」。这收窄了 0003 的平台矩阵，要在 M2 发布前补齐。
4. **基准**：现有的 `bench.py` 和 `concurrency.py` 包含过滤检索、delete、upsert、reopen，还要经 CLI 起服务，这些都在 M1 范围之外。因此：
   - **补 M1 口径**：给 [benchmarks/baseline/](../../benchmarks/baseline/README.md) 增加一个 M1 口径，只测写入、无过滤 FLAT 检索、空 `expr` 的 `count(*)`、按主键 `get`，以及同一进程内多线程并发检索。
   - **同口径对照**：在同一台机器上，用同一口径重跑 2.5.1、3.x 和 Rust 三组。空 `expr` 的 `count(*)` 在 3.x 中走行数快路径，与 0001 中带过滤的计数不可比。
   - **定位与处理**：这组结果是对 0001 推断的**部分**验证，只覆盖内存路径；主体验证在 M2 做，包括冷启动、reopen 和按 segment 的有效位图。不设硬性通过门槛，结果与 0001 的推断对照后汇报负责人；如果 FLAT 检索或写入明显落后 2.5.1，先分析原因，由负责人决定是否在进入 M2 前先优化。
5. **构建与静态检查**：
   - `cargo tree --workspace -e all` 中没有 C++ 构建与 `protoc`；
   - `cargo fmt --check`、`cargo clippy -- -D warnings` 与 Rust 单测通过；
   - maturin 能在本机构建 abi3 wheel，并用它跑通第 1–3 项。

## 决策

以下两项由负责人确认，适用于 M1 及之后的所有里程碑。

- **`metric_type` 与索引度量不一致时，一律按请求的度量计算。**
  - 3.x 在 memtable 上按请求的度量计算，结果正确；数据 flush 成 segment 后，FLAT 与 HNSW 按索引的度量排序，再按请求的度量换算距离，结果错误。实测例子：FLAT/L2 索引、IP 请求，flush 前返回正确的 IP top-3；flush 后返回的是按 L2 排序得到的另一组主键，距离为负数。这是 3.x 的 bug，违反不变量 14。
  - M1 只有内存数据，与 3.x 完全一致。从 M2 起，flush 过的数据也返回正确结果，与 3.x 的错误行为不同，这一点在此登记为已知差异（[AGENTS.md](../../AGENTS.md) 规则 2），影响 segment 上度量不一致的检索。
  - 放弃的做法：像 Milvus 一样拒绝不一致的度量，这会让 3.x 在 memtable 上本来能正确返回的请求变成报错；照抄 3.x，则会明知故犯地返回错误结果。
- **距离类差分测试的允许误差**：3.x 的暴力检索用 numpy 在 float32 下计算，L2 先开方再平方，与 Rust 的 SIMD 求和顺序不同，结果不会逐位相同。
  - 每个距离满足 `|a − b| ≤ max(1e-5 × |b|, 1e-6)`；绝对误差下限用于距离接近 0 的情形，例如 L2 精确命中。
  - 距离差在误差以内的相邻结果允许互换顺序。截断位置上出现并列时，允许主键集合只在这些并列结果之间不同。
  - 这是后续所有距离类差分测试的默认规则；某个测试需要放宽时，须在该测试中说明理由并经负责人确认。

## 跨平台

M1 只在本机（macOS）验收。Linux 与 Windows 的 CI 与 wheel 构建单独立项，最迟在 M2 发布前完成。
