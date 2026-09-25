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
| `create_collection(name, dimension=…)`，或带 `index_params` 的 `create_collection(schema=…)` | `CreateCollection` → `CreateIndex` → `AllocTimestamp` → `DescribeIndex` → `LoadCollection` → `GetLoadingProgress` |
| 不带 `index_params` 的 `create_collection(schema=…)` | 只发 `CreateCollection` |
| `create_index` / `describe_index` / `list_indexes` | `CreateIndex` + `AllocTimestamp` + `DescribeIndex` / `DescribeIndex` / `DescribeIndex` |
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
- `AllocTimestamp` 必须返回 `UNIMPLEMENTED`：pymilvus 等待索引建成时调用它，对 `UNIMPLEMENTED` 按时间戳 0 处理（`@ignore_unimplemented(0)`），返回其他错误则抛出异常。3.x 同样不实现它。

## 范围

### 支持

凡在范围内的操作，结果、错误码和错误信息都与 3.x 一致；下文逐条写出了容易忽略的 3.x 语义。

- **进程内服务**：按 [0003](0003-in-process-server-lifecycle.md) 实现，其中与持久化有关的条目留到 M2。导入时检测官方发行包（[AGENTS.md「pymilvus 接入点」](../../AGENTS.md#核心业务术语)）。
- **连接**：
  - `Connect`；
  - `GetVersion`，返回 `milvus_lite-<本包版本>`，格式与 3.x 相同。
- **数据库**：只有 `default` 一个。
- **collection**：`CreateCollection`、`DescribeCollection`、`ShowCollections`、`DropCollection`、`GetCollectionStatistics`（行数）。collection 不存在时，与 3.x 一样返回 code 100 和 `collection '<name>' does not exist`，`has_collection` 依赖这个错误码。CreateCollection 的 properties 与 3.x 一样原样保存并在 DescribeCollection 中回显，按 Python dict 的语义处理重复与空的 key；其中 `timezone` 不支持，见「不支持」。
- **load 状态**：`LoadCollection`、`GetLoadingProgress`、`GetLoadState`、`ReleaseCollection`。语义与 3.x 一致，以下均已实测：
  - 没有索引的 collection 一直处于 Loaded 状态，不 load 也能检索；对它 release 不生效。
  - 在 Loaded 状态下执行 `create_index`，状态保持 Loaded。
  - 有索引的 collection 被 release 后，状态为 NotLoad，search 和 get 报 code 101（不变量 13）；空 `expr` 的 `count(*)` 与 insert 照常可用。
- **schema**：
  - 主键：INT64 或 VARCHAR；`auto_id` 只允许 INT64 主键，VARCHAR 主键设 `auto_id` 时与 3.x 一样报错。
  - 向量：只能有一个 FLOAT_VECTOR 字段。这是 M1 的限制：3.x 的 `validate_schema` 并不限制个数（其 docstring 所说的「at most one」没有对应的检查）。
  - 标量：BOOL、INT8、INT16、INT32、INT64、FLOAT、DOUBLE、VARCHAR、JSON。
  - 动态字段 `$meta`。
- **索引**：稠密向量字段上的 `CreateIndex` 与 `DescribeIndex`，按 [0002「结论」](0002-data-dir-compat.md#结论)中 FLAT 阶段的规则处理：
  - FLAT / BRUTE_FORCE 原生支持；
  - AUTOINDEX 按 FLAT 执行；
  - 其余类型报错。
  - 度量为 L2、IP、COSINE。没有索引时按 COSINE 检索，与 3.x 一致。
- **写入**：`Insert`，包括 `auto_id` 和动态字段。
  - 主键重复时以后写者为准，同一批内、跨批次都如此，`count(*)` 与检索都只看到最新值。
  - `auto_id` 的生成规则除下文「决策」登记的情形外与 3.x 一致：每个 collection 从 1 开始，由 `_seq` 派生（见 [0002](0002-data-dir-compat.md#共享状态的语义约束) 约束 4）。差分测试逐项比较生成的主键。
  - `partition_name` 为空或 `_default`。
- **检索**：`Search`，支持以下参数：
  - `nq ≥ 1`、`limit`、`offset`。`limit` 不为正或 `offset` 为负时与 3.x 相同：取前 `limit + offset` 个结果，`offset` 为正时再跳过 `offset` 个；
  - `output_fields`，包括动态字段；
  - `round_decimal`，3.x 已实现；
  - `partition_names` 为空或只含 `_default`：M1 只有这一个分区，结果与不指定相同。
  - 刚写入的数据立即可见。
  - `search_params` 中的 `metric_type` 与索引度量不一致时，按请求的度量计算，见下文「决策」。
- **与 3.x 一样忽略的参数**：Search 的 `ignore_growing`、`guarantee_timestamp`、`use_default_consistency`、`nq`、`namespace`、`order_by_fields` 与 `expr_template_values`，CreateCollection 的 `consistency_level`、`shards_num`、`num_partitions`、`description` 与 `is_clustering_key`，Load 系列 RPC 的 `partition_names`、`load_fields` 与 `replica_number`，以及空 `expr` 的 `count(*)` 的 `partition_names`。3.x 不读取这些参数，或读取后不改变结果；单进程内的读写本来就是强一致。这是与 3.x 对齐，不是新的行为差异。
- **查询**：`Query` 只支持两种形式：
  - `expr` 为空，且 `output_fields=["count(*)"]`；
  - `expr` 为 `<pk> in [字面量列表]`，`output_fields` 可以是 `['*']`、字段列表或动态字段；`partition_names` 为空或只含 `_default`。这种形式的识别规则见「决策」。

### 不支持

下列功能都返回明确错误，绝不静默成功（不变量 15）。这些功能 3.x 都支持，M1 如果忽略相应参数，就会在不自知的情况下返回与 3.x 不同的结果，所以同样报错。错误形式与 3.x 的 `_unimplemented`（[servicer.py:1563](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/adapter/grpc/servicer.py#L1563)）相同：gRPC 状态码 `UNIMPLEMENTED`，details 写成 `MilvusLite does not support <功能>: <说明>`，说明中注明当前阶段尚未实现。gRPC 状态非 OK 时响应体会被丢弃，3.x 在响应体里另放的 `Status` 客户端看不到（实测 pymilvus 只得到状态码与 details），因此不必复现。pymilvus 收到 `UNIMPLEMENTED` 时不重试。

- **持久化**：见下文「阶段性差异」。
- **过滤**：除上面两种 `Query` 形式外，不支持任何 `filter` / `expr`，包括 `text_match`、JSON 路径与几何谓词；`expr` 为空且不含 `count(*)` 的 `Query`、`count(*)` 带非空 `expr`、以及 query iterator 也不支持。
- **写入**：`Delete`、`Upsert`。
- **分区**：partition 相关 RPC；在检索、查询与写入中指定 `_default` 以外的分区；partition key。
- **多数据库与别名**：`CreateDatabase` 等数据库 RPC，以及 alias。
- **schema**：
  - 上面未列出的字段类型：其余向量类型、稀疏向量、ARRAY、GEOMETRY、时间类型；
  - nullable、default value、function（BM25、embedding）、多向量字段；
  - 修改 schema 与 collection properties；
  - properties 中的 `timezone`，以及 Search 与 Query 请求中非空的 `timezone`：它们只作用于 TIMESTAMPTZ 字段。
- **检索**：`HybridSearch`、range search（`params` 中的 `radius` / `range_filter`）、`group_by`、rerank。
  - search iterator 没有专门的请求：pymilvus 先按 v2 探测，服务端不支持时退回 v1，而 v1 的第一批是普通检索，之后各批才带 range search 与过滤表达式。3.x 的 Search 同样忽略 `iterator` 等参数，所以只需要第一批时（`limit` 不超过 `batch_size`）结果与 3.x 相同，需要后续批次时返回 `UNIMPLEMENTED`。
- **其他**：snapshot、compaction 与 flush 相关 RPC；CLI 与 `milvus-lite dump`。其余未列出的 RPC 一律返回 `UNIMPLEMENTED`。

## 阶段性差异（登记）

| 差异 | 3.x | M1 | 理由 | 影响的用例 |
| --- | --- | --- | --- | --- |
| 数据只在内存中 | 数据持久化，进程重启后仍在 | 进程退出后数据消失 | 持久化连同 WAL、恢复与崩溃测试在 M2 一起做 | 所有重开数据目录的用例，例如 [test_server_restart.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_server_restart.py) |
| 打开已有数据的目录 | 能打开 | `start_and_get_uri` 返回 `None`，日志写明原因，判定规则见 [0003「同路径复用」](0003-in-process-server-lifecycle.md#同路径复用) | 不能把已有数据显示为空库 | 用 3.x 写入过数据的目录启动 |
| 写入磁盘的内容 | 完整的数据目录 | 只创建目录与 `LOCK`，不写其他文件 | 保证 M1 用过的目录 3.x 仍能打开 | 无 |
| 索引类型 | 支持 HNSW 等 ANN 索引，AUTOINDEX 在 flush 后建成 HNSW | 按 [0002](0002-data-dir-compat.md#结论) 的 FLAT 阶段规则执行 | 0002 已登记 | 显式创建 ANN 索引的用例 |
| 检索、查询或写入时指定 `_default` 以外的分区 | 支持；分区不存在时检索与查询返回空结果，写入报 code 200 | 返回 `UNIMPLEMENTED` | 分区不在 M1 范围内 | 带 `partition_names` 的用例 |
| 上面「不支持」中的其余功能 | 支持 | 返回 `UNIMPLEMENTED` | 不在 M1 范围内 | 用例清单中标为「范围外」的用例 |

由于数据在进程退出后消失，**M1 不发布到 PyPI**，只作为开发与测试用的构建。

## 验收

1. **端到端差分**：
   - **场景**：在 3.x README 的 Quick Start 基础上，去掉过滤与删除，再补上几种用法：显式 schema、`auto_id`、IP 与 L2 度量、VARCHAR 主键、先建表后建索引、release 后再 load。
   - **版本与运行方式**：在 pymilvus 2.6.17 与 3.0.2 下各跑一遍。本包与官方包占用同一个 `milvus_lite` 模块，不能装在一起，因此 3.x 装在另一个 venv 里，在独立进程中运行它的 gRPC 服务；差分脚本用 URI 分别连接两边。
     - 独立进程不用 `milvus-lite server`，而是在 3.x 的 venv 里运行一个辅助脚本，由它调用 `start_server_in_thread(port=0)`。二者经同一个 `_build_server` 构造服务，servicer、256MB 消息上限与线程数都相同。
     - 改用脚本的原因：CLI 的 `--port 0` 不回报实际端口；gRPC 在端口上设了 SO_REUSEPORT，两个 CLI 服务绑同一端口时都不报错，请求全落在先启动的那个上，所以先挑空闲端口再启动存在察觉不到的竞争；CLI 只在 KeyboardInterrupt 时关闭数据库。辅助脚本打印实际端口，stdin 关闭时关闭服务与数据库，日志写到文件。
   - **数据量**：每个 collection 少于 1 万行。3.x 满 1 万行才 flush，在此之前数据都在 memtable 中暴力检索，结果是精确的；AUTOINDEX 也不会建成 HNSW。
   - **比较口径**：主键集合（包括 `auto_id` 生成的主键）、输出字段、`count(*)`、load 状态、错误码与错误信息逐项相等；距离的允许误差见「决策」。
2. **复用参照实现的用例**：候选文件大多只通过 [conftest.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/conftest.py#L57) 的 `grpc_server` fixture 起服务，不在顶层导入内部模块。阻碍有三个：
   - 起服务的 fixture 要替换；
   - conftest 在每个用例后的清理也要替换：它先调用 `using_database`，pymilvus 为此发出的 DescribeDatabase 在 M1 返回 `UNIMPLEMENTED`，清理于是跳过，一个 collection 都不删，后续用例因重名而失败；
   - 同一个文件里混有范围外的用例，而规则 4 不允许用 skip 占位。
   - **复用方式**：M1 把范围内的用例复制到本仓，fixture 换成本实现的起服务方式，并注明来源与 Apache-2.0 许可（[AGENTS.md](../../AGENTS.md) 规则 9）。复制出来的用例与上游脱钩，对照版本升级时要手工同步。某个文件涉及的功能全部进入范围后，改为替换 fixture、原样运行整个文件，不再复制。
   - **复用范围**：整个 [tests/adapter/](https://github.com/milvus-io/milvus-lite/tree/v3.2.1/tests/adapter)，47 个文件共 637 条用例。其中 72 条在 M1 范围内，分布在 19 个文件里；`test_milvus_lite_compat.py` 另有 4 条只用 `server_manager` 的公开接口，它是 pymilvus 接入点的契约、本仓同样提供，也复用。下面的 7 个候选文件是最初按文件名挑出的，只含 72 条中的 34 条。
   - **改后复制**：只允许两种替换，改后的副本须先在 3.x 上通过：
     - 显式的 HNSW 等 ANN 索引换成 FLAT，前提是用例写入少于 1 万行、不断言索引类型与参数。3.x 满 1 万行（`>=`，按 memtable 中的主键数加 tombstone 数计）才 flush，在此之前总在 memtable 上暴力检索，索引类型不影响结果：实测 9999 行时 FLAT、HNSW、AUTOINDEX 的主键与距离逐位相同，1 万行时才不同。
     - `<pk> == N` 换成 `<pk> in [N]`，前提是只有一个主键、没有 `offset`、字面量类型与主键相符。这三个条件下 3.x 两条路径的结果逐行相同；否则不同，例如 `offset=1` 时 `id == 3` 返回空而 `id in [3]` 返回该行，INT64 主键上 `id == '3'` 报 code 6 而 `id in ['3']` 返回空。
   - **用例清单**：[tests/python/adapter/INVENTORY.md](../../tests/python/adapter/INVENTORY.md)，随 M1 代码入库。7 个候选文件以及含范围内用例的文件逐条列出，其余文件整个记一行原因。状态为「已复制」「改后复制」「范围外」「依赖内部模块」，后两类注明原因；「依赖内部模块」要写出具体行号。
   - **范围外用例**：不复制，也不用 skip 或 xfail 占位；为每类不支持的功能另写用例，断言返回 `UNIMPLEMENTED` 且原因正确。只断言抛出异常的范围外用例同样不复制，因为 `UNIMPLEMENTED` 也会让它通过。
   - **候选文件**（每个文件都混有范围外的用例）：
     - `test_grpc_collection_lifecycle.py`、`test_grpc_crud.py`、`test_grpc_search.py`：不导入内部模块；
     - `test_grpc_server_startup.py`：`test_server_data_dir_lock_held` 在函数体内导入内部模块（L34–35），`test_server_starts_and_binds_port` 断言 fixture 返回的 3.x 内部对象（L26–28），其余用例不依赖内部模块；
     - `test_auto_id.py`：12 条中有 8 条是引擎内部用例，内部模块都在函数体内导入；另外 4 条经 pymilvus，其中 `test_grpc_auto_id_query` 带过滤；
     - `test_grpc_quickstart.py`：顶层导入 `milvus_lite.index.factory`（L19），整个文件被 `skipif` 包住；两条用例都在范围外，`test_quickstart_multi_query_search` 只因 HNSW 在范围外，可按上文改后复制；
     - `test_grpc_error_mapping.py`：顶层导入内部模块（L14–25），`test_hybrid_search_unimplemented` 还在函数体内导入（L104），只能复制其中经 pymilvus 的用例。
   - **`tests/compatibility`**：9 个文件共约 240 条用例，以范围外的功能为主，包括过滤、分区与 partition key、多数据库、function、delete / upsert。每个文件还各自用 `start_server_in_thread` 定义起服务的 fixture。M1 不复用，理由写进用例清单；复用的时机与方式同 adapter，也就是文件涉及的功能进入范围后，替换 fixture 整文件运行。
   - **防止静默跳过**：确认 pymilvus 已安装，套件没有被 `importorskip` 整目录跳过。
3. **生命周期**：[0003「验证」](0003-in-process-server-lifecycle.md#验证)中与持久化无关的用例全部通过，但 M1 只在本机平台上跑，Linux 与 Windows 另见「跨平台」。这收窄了 0003 的平台矩阵，要在 M2 发布前补齐。
4. **基准**：现有的 `bench.py` 和 `concurrency.py` 包含过滤检索、delete、upsert、reopen，还要经 CLI 起服务，这些都在 M1 范围之外。因此：
   - **补 M1 口径**：给 [benchmarks/baseline/](../../benchmarks/baseline/README.md) 增加一个 M1 口径，只测写入、无过滤 FLAT 检索、空 `expr` 的 `count(*)`、按主键 `get`，以及同一进程内多线程并发检索。
   - **同口径对照**：在同一台机器上，用同一口径重跑 2.5.1、3.x 和 Rust 三组。空 `expr` 的 `count(*)` 在 3.x 中走行数快路径，与 0001 中带过滤的计数不可比。
   - **定位与处理**：这组结果是对 0001 推断的**部分**验证，只覆盖内存路径；主体验证在 M2 做，包括冷启动、reopen 和按 segment 的有效位图。不设硬性通过门槛，结果与 0001 的推断对照后汇报负责人；如果 FLAT 检索或写入明显落后 2.5.1，先分析原因，由负责人决定是否在进入 M2 前先优化。
   - **结果**：见 [0001「M1 部分验证」](0001-refactor-baseline.md#m1-部分验证)。
5. **构建与静态检查**：
   - `cargo tree --workspace -e all` 中没有 C++ 构建与 `protoc`；
   - `cargo fmt --check`、`cargo clippy -- -D warnings` 与 Rust 单测通过；
   - maturin 能在本机构建 abi3 wheel，并用它跑通第 1–3 项。

## 决策

以下各项由负责人确认，适用于 M1 及之后的所有里程碑。与 3.x 不同的地方在此登记为已知差异（[AGENTS.md](../../AGENTS.md) 规则 2）。

- **`metric_type` 与索引度量不一致时，一律按请求的度量计算。**
  - 3.x 在 memtable 上按请求的度量计算，结果正确；数据 flush 成 segment 后，FLAT 与 HNSW 按索引的度量排序，再按请求的度量换算距离，结果错误。实测例子：FLAT/L2 索引、IP 请求，flush 前返回正确的 IP top-3；flush 后返回的是按 L2 排序得到的另一组主键，距离为负数。这是 3.x 的 bug，违反不变量 14。
  - M1 只有内存数据，与 3.x 完全一致。从 M2 起，flush 过的数据也返回正确结果，与 3.x 的错误行为不同，这一点在此登记为已知差异（[AGENTS.md](../../AGENTS.md) 规则 2），影响 segment 上度量不一致的检索。
  - 放弃的做法：像 Milvus 一样拒绝不一致的度量，这会让 3.x 在 memtable 上本来能正确返回的请求变成报错；照抄 3.x，则会明知故犯地返回错误结果。
- **距离类差分测试的允许误差**：3.x 的暴力检索用 numpy 在 float32 下计算，L2 先开方再平方，与 Rust 的 SIMD 求和顺序不同，结果不会逐位相同。
  - 每个距离按度量满足下列条件，`a` 与 `b` 分别是两边的距离，`q` 是查询向量，`v` 是命中行的向量：
    - L2：`|a − b| ≤ max(1e-5 × |b|, 1e-6)`；绝对误差下限用于距离接近 0 的情形，例如精确命中。
    - IP：`|a − b| ≤ max(1e-5 × ‖q‖ × ‖v‖, 1e-6)`。float32 求和的误差与 `‖q‖ × ‖v‖` 成正比，而正负项相消时 `|b|` 可以远小于它，按 `|b|` 定误差会误报：负责人实测 dim 1536 时 300 行中有 12 行超出 `1e-5 × |b|`，最大差 3.2e-5，L2 与 COSINE 都没有超出。求和顺序取决于 numpy 所用的 BLAS 库，不同平台不同，因此不模拟。
    - COSINE：`|a − b| ≤ 1e-5`，即 IP 的规则作用在归一化后的向量上；相似度接近 0 时与 IP 同理。
    - 适用的维度：误差余量随 √dim 缩小。顺序累加 float32 时，dim 32768（Milvus 的维度上限）有 L2 与 COSINE 超界（最大 1.09 倍），超界来自本实现：与 float64 真值相比用掉误差的 101%，而 numpy 分组求和只用掉 2%。第 5 步起距离改为 16 路累加（[flat.rs](../../crates/core/src/flat.rs)），差分测试覆盖到 dim 32768：最大用到误差的比例 L2 为 0.063，IP 为 0.64，COSINE 为 0.55，IP 与 COSINE 的余量主要被 3.x 自身的误差占用（其 BLAS 求和相对 float64 约用掉 0.6），随 3.x 所在平台的 BLAS 而变。
    - L2 必须先逐元素相减再平方求和，不得展开成 `‖q‖² + ‖v‖² − 2q·v`：距离很小而向量模长很大时，展开式相消会丢掉全部有效位，上面按 `|b|` 定的 L2 误差就不再成立。
  - 两个结果 x、y 在两边的先后相反时，只要 3.x 的距离之差不超过两者的允许误差之和（`|b_x − b_y| ≤ t_x + t_y`），就算一致；这样的两个结果不必相邻。同理，截断位置两侧的结果满足这一条件时，允许主键集合只在这些结果之间不同。
  - 这是后续所有距离类差分测试的默认规则；某个测试需要放宽时，须在该测试中说明理由并经负责人确认。
  - 带 `round_decimal` 的检索：3.x 对 float64 的对外距离舍入（L2 在 float32 开方后于 float64 平方，COSINE 为 `1 − float64(距离)`），本实现按同样的口径换算后再舍入。舍入前的值在上述误差之内时，允许舍入结果相差 `max(10^-round_decimal, 上述误差)`：`round_decimal` 较大时 float32 的精度比 `10^-round_decimal` 更粗。经 pymilvus 比较时要注意：pymilvus 在 `round_decimal > 0` 时会在客户端对收到的值再 `round` 一次，只有 `round_decimal = 0` 时看到的是服务端的舍入结果；因此差分测试另检查本实现的值再舍入一次不变，服务端的舍入另由手写请求的用例对照 3.x。

- **检索请求的维度与度量总是校验。** 3.x 不校验查询向量的维度，维度不符时返回 numpy 的原始报错（code 1），collection 为空时反而成功；非法度量也要到计算距离时才报错，空 collection 上同样成功。本实现在检索前校验：维度不符报 `query vector has dim <n>, but field '<f>' expects dim <d>`，非法度量报与 3.x 相同的信息，两者都是 code 1，空 collection 上也报错。影响：维度或度量有误、且 collection 为空的检索。
- **auto_id 取该行的 `_seq`。** 3.x 的 auto_id 与 `_seq` 是两个计数器：校验失败的 insert 也会消耗 auto_id，于是 auto_id 会超过该行的 `_seq`，违反 [0002](0002-data-dir-compat.md#共享状态的语义约束) 约束 4（0002「继承的 3.x 已知风险」）。本实现令 auto_id 等于所在行的 `_seq`，失败的 insert 不消耗任何编号。3.x 的 delete 每次消耗一个 `_seq` 而不消耗 auto_id（实测删除一次后下一个 auto_id 仍是 2），所以只有在同一会话内既没有失败的 insert、也没有 delete 这类只消耗 `_seq` 的操作时，生成的主键才与 3.x 相同。影响：失败的 insert 或 delete 之后再写入时生成的 auto_id；M1 没有 delete，引入 delete 时差分测试要按此处理。
- **畸形的 JSON 与动态字段明确报错。** 3.x 把无法解析的 JSON 字段值原样存下（之后 pymilvus 读取时崩溃），静默丢弃不是 JSON 对象的 `$meta`，并让 `$meta` 中与 schema 字段同名的 key 覆盖该字段（实测主键被改写）。本实现对这三种输入都报 code 6 的校验错误；JSON 字段收到的不是 json_data 时同样报类型不符（3.x 对 VARCHAR 槽位的文本接受并存下，合法的 JSON 文本能正常读回，其他文本读回时 pymilvus 解析失败；其他槽位报 code 1），见下文「只能由手写请求构造的其他输入」。另外，JSON 的嵌套深度上限为 127 层（serde_json 的递归上限，用来防止恶意输入导致栈溢出），更深报 code 6；3.x 受 Python 递归限制，约 1000 层以内都能写入。这类输入只能由手写的 proto 请求构造，pymilvus 不会发出。M2 读取 3.x 写出的目录时，要能容忍 3.x 已存下的畸形值。
- **JSON 字段接受任意 JSON 值并原样读回。** 3.x 把 JSON 字段存为字符串列：顶层是数字或布尔时 pyarrow 报 code 1 `Expected bytes, got a 'int' object`；顶层是字符串时能写入，但读回时丢了引号，pymilvus 解析失败。二者都能经 pymilvus 发出（例如 `{"j": 1}`、`{"j": '"abc"'}`）。本实现都接受并正确读回，与 Milvus 的语义一致；M2 落盘后 3.x 读取这些值时按字符串原样透传，pymilvus 仍能解析数字与布尔。影响：JSON 字段写入顶层标量的用例。
- **按主键读取的表达式只识别部分非 ASCII 主键名。** 3.x 用 Python 的 `str.isalpha` / `str.isalnum` 切分标识符，结果随 Python 的 Unicode 版本变化。本实现的主键名只认 ASCII 字母、数字、下划线，以及在 Python 3.10–3.13 中逐码位核对过、全部判为字母的范围（Latin-1 与 Latin Extended-A 的字母、CJK 统一汉字 4E00–9FFC 与扩展 A、平假名、片假名、韩文音节）；数字只认 ASCII。其余情形返回 `UNIMPLEMENTED`，而 3.x 可能按主键读取。与 3.x 的差分模糊测试（约 110 万组输入）中，没有本实现接受而 3.x 拒绝、或主键列表不同的输入。影响：主键名含未收录字符的 collection 上的 `get`，以及含非 ASCII 数字的手写表达式。
- **同时含 M1 不支持的功能与其他错误时，报错的先后不保证与 3.x 相同。** 3.x 翻译阶段对 M1 支持的参数所做的检查，以及 load 状态检查，先于 `UNIMPLEMENTED` 报告；写入时一列不支持不影响其后各列的解码错误先报出。3.x 对 M1 不支持的功能自身参数的校验不复现，这类请求报 `UNIMPLEMENTED`：例如 nullable 主键 3.x 报 code 6；rerank 函数的参数有误（如 boost 函数带了 `input_field_names`、未知的 reranker）3.x 报 code 6；过滤表达式的语法错误 3.x 报 code 6，本实现不解析过滤表达式。pymilvus 对含单引号、反斜杠、换行或回车的 VARCHAR 主键调用 `get` 时就会拼出语法错误的表达式（例如 `id in ['b'c']`）。影响：同时触发两类错误的请求。
- **按 Python `int()` / `float()` 解析的参数，只对齐合法输入与常见错误。** 涉及 search_params 的 `topk`、`offset`、`round_decimal`、`group_size`、`radius`、`range_filter`，以及 type_params 的 `dim`、`max_length`。整数、截断取整的浮点数、布尔，以及带空白、正负号或下划线的数字字符串，结果与 3.x 相同；非数字字符串、null、列表与对象的错误码与信息相同。NaN 与 Infinity（Python 的 `json.loads` 接受、serde_json 不接受）、非 ASCII 数字与超出 int64 的值，错误信息或结果可能不同，超出 int64 时按饱和处理。pymilvus 只发送整数，这些输入只能由手写请求构造。
- **只能由手写请求构造的其他输入。**

  | 输入 | 3.x | 本实现 |
  | --- | --- | --- |
  | 整个请求无法解析 | gRPC `INTERNAL`，`Exception deserializing request!` | gRPC `INTERNAL`，prost 的错误信息 |
  | search_params 的 `topk` 为 JSON `true` 或 `false` | 编码响应时报 code 1 | 按 1 或 0 处理 |
  | FieldData 的 `valid_data` 含 False | 标量列与主键报 code 6；按完整长度发送的向量列忽略 valid_data；`$meta` 列丢弃该行的动态字段 | `UNIMPLEMENTED`（空值不支持） |
  | 类型为 VARCHAR 的动态列，或内容是包着 JSON 对象的字符串的 `$meta` | 解开后合并 | code 6，同上文「畸形的 JSON 与动态字段明确报错」 |
  | JSON 值是非法 UTF-8，或含 NaN、Infinity、超出 float64 的数、孤立的代理转义 | 非法 UTF-8 报 code 1；其余接受并存下 | code 6 |
  | 动态字段中超出 64 位的整数 | 原样保存 | code 6：serde_json 会把它静默转成浮点数（pymilvus 在客户端拒绝这种值） |
  | search_params 的 `params`、索引的 `params` / `search_params`、字段的 `analyzer_params` 是 Python 能解析而 serde_json 不能解析的对象或数组（含 NaN、Infinity 等），或以 `{`、`[` 开头但 Python 也解析不了 | 能解析时使用其内容；解析不了时当作原字符串或空对象 | `UNIMPLEMENTED`：无法得知其中是否有 range search 等参数，不能静默忽略 |
  | search_params 的其他标量参数取 NaN、Infinity 等写法 | 按 Python 的值处理 | 按原字符串处理，报错或结果可能不同 |
  | 文本查询向量是非法 UTF-8 | code 1，Python 的解码错误 | 按文本查询报 code 6 |
  | 不在 schema 中的列，类型为 ARRAY、GEOMETRY、TIMESTAMPTZ 或稀疏向量 | 解码后存入 `$meta` | `UNIMPLEMENTED` |
  | 列的槽位与目标字段类型不同：json_data 的列写入非 JSON 字段，或其他槽位的列写入 JSON 字段 | json_data 的列写入非 JSON 字段时按解码出的 Python 值校验，类型相符即成功，例如 `7` 写入 INT64、`"x"` 写入 VARCHAR、`[1.0, 0.0]` 写入向量，`null` 写入主键报 `primary key 'id' missing or None`。写入 JSON 字段时，VARCHAR 槽位的文本成功，其他槽位报 code 1（例如 `Expected bytes, got a 'int' object`） | 按槽位的值类型校验，报 code 6 的类型不符 |
- **collection 名区分大小写。** 3.x 在 macOS 默认的文件系统上把 `Case` 与 `case` 视为同一个 collection，这是文件系统不区分大小写的副作用，Linux 上没有。M1 的数据在内存中，按原样区分大小写。M2 落盘后在 macOS 上会自然出现同样的冲突。

## 跨平台

M1 只在本机（macOS）验收。Linux 与 Windows 的 CI 与 wheel 构建单独立项，最迟在 M2 发布前完成。

跨平台 CI 还要验证：本实现在 x86 与 aarch64 上对同一输入算出的距离逐位相同（求和顺序只由源码决定，见 [flat.rs](../../crates/core/src/flat.rs)），与 3.x 之间仍按「决策」中的误差比较。
