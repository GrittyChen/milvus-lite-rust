# 0001 重构基线选择：以 3.x 为基础，而非 2.5.1

- 状态：已采纳
- 日期：2026-09-24
- 对照版本：milvus-lite 2.5.1（tag `v2.5.1`）、milvus-lite 3.2.1（tag `v3.2.1`，`43d1257`）、pymilvus 3.0.2

## 背景

本项目用 Rust 重构 milvus-lite，动机是当前 3.x（纯 Python）性能不足。milvus-lite 2.5.1 是 C++ 实现，性能明显好于 3.x。需要回答的问题是：

> 以 3.x 为基础重构，相对 2.5.1 是否会有性能瓶颈？也就是说，3.x 的性能损失来自 2.x → 3.x 的**架构迁移**，还是仅仅来自 **Python 实现**？

如果损失只来自 Python 实现，就以 3.x 为基础；如果来自架构本身，就应考虑以 2.5.1 为基础并适配最新接口。

## 结论

**以 3.x 为基础重构。**

- 3.x 相对 2.5.1 的性能损失**主要来自 Python 实现**。
- 架构层面的损失只有一处：查询时逐行重算跨源去重与删除判定，代价是 O(总行数)。这一处可以在 3.x 架构内部修正，改为按 segment 增量维护有效位图，不需要换架构（见[归因](#3x-架构内唯一的架构性损失)）。
- 2.5.1 有两项架构级瓶颈：一是行存储加冷启动全量重放，二是索引不落盘、每次加载都要重建。3.x 架构在这两项上都更好。2.5.1 的其他慢点（全局锁、删除）属于实现问题。
- 内存方面，两者**都是全量常驻内存**，3.x 目前更耗内存。这一项哪个架构都不占优，Rust 实现需要单独设定目标。

2.5.1 与 3.x 同为**性能基线**：在 2.5.1 占优的指标上，Rust 实现要追平 2.5.1；在 3.x 架构占优的指标上，要保持优势。

「3.x 架构在 Rust 下能达到 2.5.1 水平」这一点，目前是根据 profile 与微基准**推断**出来的，还没有经过实现验证。Rust 实现要用 [benchmarks/baseline/](../../benchmarks/baseline/README.md) 验证它：第一个里程碑（M1）只有内存数据，只能验证内存中的写入与检索；主体验证放在引入持久化的 M2，覆盖冷启动、reopen 和按 segment 的有效位图（见 [0004](0004-m1-scope.md)）。

## 两个版本的架构

| 维度 | 2.5.1（C++） | 3.x（Python） |
| --- | --- | --- |
| 进程形态 | pymilvus 拉起 C++ 子进程，经 UDS / TCP 走 gRPC | Python 进程内起 gRPC 服务 |
| 持久化 | SQLite 行存：每行一个 protobuf `InsertRecord` 序列化后存为 BLOB（[insert_task.cpp:174](https://github.com/milvus-io/milvus-lite/blob/v2.5.1/src/insert_task.cpp#L174)、[collection_data.cpp:40](https://github.com/milvus-io/milvus-lite/blob/v2.5.1/src/storage/collection_data.cpp#L40)） | WAL（Arrow IPC）+ 不可变 Parquet segment + delta + JSON manifest |
| 内存数据 | 每个 collection 一个 segcore growing segment，全量常驻 | memtable（每 1 万行 flush）+ 多个 segment（单个上限 10 万行，size-tiered compaction）；segment 加载后常驻内存（[segment.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/storage/segment.py#L1)） |
| 加载 | 从 SQLite 每页读 20 万行，**逐行**插回 segcore（[milvus_local.cpp:365](https://github.com/milvus-io/milvus-lite/blob/v2.5.1/src/milvus_local.cpp#L365)） | 读 manifest、Parquet 与已落盘的索引 |
| 向量索引 | FLAT / IVF_FLAT / AUTOINDEX，另有 BIN_FLAT 与稀疏索引；HNSW 在源码中被注释掉（[create_index_task.cpp:56](https://github.com/milvus-io/milvus-lite/blob/v2.5.1/src/create_index_task.cpp#L56)）；索引不落盘，每次加载重建 | FAISS HNSW / HNSW_SQ / IVF_FLAT / IVF_SQ8 等；每个 segment 一份索引，持久化为 `.idx` |
| 并发 | `MilvusLocal` 的绝大多数操作，包括检索，共用一把 `mutex_`（[milvus_local.cpp:282](https://github.com/milvus-io/milvus-lite/blob/v2.5.1/src/milvus_local.cpp#L282)） | 受 GIL 限制 |
| 功能面 | 不支持 partition、alias；支持 binary / float16 / bfloat16 向量 | 支持 partition、alias、database、几何、`text_match`、iterator 等（snapshot 仅限嵌入式 API）；不支持 binary / float16 / bfloat16 向量 |

2.5.1 快，靠的是内嵌的 Milvus C++ segcore（表达式执行、检索）与 knowhere。它自身的手写 C++ 代码约 1.2 万行（不含单测与 ANTLR 生成代码）。3.x 的 LSM + 不可变 segment + 按 segment 建索引 + insert / delete 分离，与 Milvus 本体的 growing / sealed segment、binlog / deltalog、compaction 结构一致。

从提交内容看，官方从 2.x 转向 3.x 不是出于性能考虑。v3 的首个提交（[`04fb87a`，#335](https://github.com/milvus-io/milvus-lite/commit/04fb87a)）删除了 CMake、Conan、C++ 源码与内嵌的 Milvus / Knowhere 构建资产；3.x README 列出的卖点是「Pure Python implementation with inspectable code and Python stack traces」。

## 测量

方法与脚本见 [benchmarks/baseline/](../../benchmarks/baseline/README.md)，下面三张表的数据、profile、SQLite 删除实验与 Rust 去重微基准都由该目录的脚本产生。

- 环境：Apple M2 Pro（10 核，32 GB），macOS，Python 3.12。
- 数据：128 维均匀随机向量，另带一个 INT64 和一个 VARCHAR 标量字段，度量为 L2。
- 延迟均为 p50。「重启到可检索」= 连接（含拉起服务端进程）+ `load_collection`，不含首次检索。

### 10 万行（3.x 为 inproc 模式）

| 指标 | 2.5.1 FLAT | 2.5.1 IVF_FLAT | 3.x FLAT | 3.x HNSW | 3.x IVF_FLAT |
| --- | --- | --- | --- | --- | --- |
| 写入（行/秒） | 34130 | 34592 | 21228 | 20946 | 21303 |
| 单条检索 top10 | 1.69 ms | 0.49 ms | 36.6 ms | 27.5 ms | 26.4 ms |
| 100 条检索 top10 | 21.1 ms | 4.0 ms | 794 ms | 55.1 ms | 56.5 ms |
| 带过滤检索（选择率 10%） | 0.61 ms | 0.42 ms | 28.6 ms | 28.3 ms | 26.9 ms |
| 过滤查询 limit 100 | 0.46 ms | 0.42 ms | 54.5 ms | 54.9 ms | 53.9 ms |
| `count(*)` | 0.29 ms | 0.27 ms | 1135 ms | 1156 ms | 1141 ms |
| 按 id 点查 100 条 | 1.45 ms | 1.47 ms | 2.25 ms | 2.19 ms | 2.32 ms |
| 删除 1000 条 | 39.6 ms | 33.7 ms | 3.8 ms | 4.0 ms | 3.9 ms |
| 重启到可检索 | 0.98 s | 0.94 s¹ | 0.15 s | 0.18 s | 0.12 s |
| 重启到可检索（3.x server 模式） | — | — | 0.56 s | 0.55 s | — |
| 重启后单条检索 top10 | 1.56 ms | 1.55 ms¹ | 36.5 ms | 28.1 ms | 27.4 ms |

¹ build 阶段末尾删除了 1000 条，重启时只剩 99,000 行。据 2.5.1 README，不足 10 万行时 IVF_FLAT 会退回 FLAT，因此这两个值不含 IVF 的重建时间，检索也是按 FLAT 执行的。100 万行的数据不受此影响。

10 万行时，重启差距主要来自进程形态，不是存储架构。2.5.1 的 0.98 s 中有 0.52 s 用于拉起 C++ 子进程；3.x 同样以独立进程启动（server 模式）时，重启到可检索是 0.55–0.56 s。

### 100 万行（3.x 为 server 模式）

| 指标 | 2.5.1 FLAT | 3.x FLAT | 2.5.1 IVF_FLAT | 3.x HNSW |
| --- | --- | --- | --- | --- |
| 写入（行/秒） | 34650 | 22052 | 34638 | 21925 |
| 建索引 | 4.9 s | 1.4 s | 19.4 s | 38.9 s |
| 单条检索 top10 | 11.7 ms | 348 ms | 1.13 ms | 258 ms |
| 100 条检索 top10 | 202 ms | 8409 ms | 22.2 ms | 344 ms |
| 过滤查询 limit 100 | 2.2 ms | 626 ms | 2.3 ms | 634 ms |
| `count(*)` | 1.6 ms | 11.4 s | 1.7 ms | 11.6 s |
| 按 id 点查 100 条 | 7.3 ms | 2.4 ms | 7.1 ms | 2.3 ms |
| 删除 1000 条 | 261 ms | 4.0 ms | 260 ms | 3.8 ms |
| **重启到可检索** | **4.96 s** | **1.31 s** | **19.5 s** | **1.11 s** |
| 重启后单条检索 top10 | 11.8 ms | 357 ms | 0.80 ms | 260 ms |
| 服务端内存（检索后） | 817 MB | 2541 MB | 863 MB | 3855 MB |

### 并发检索吞吐（10 万行，服务端独立进程，每个客户端一个进程）

| 配置 | 1 个客户端 | 2 个 | 4 个 | 8 个 |
| --- | --- | --- | --- | --- |
| 2.5.1 FLAT | 532 QPS | 713 QPS | 724 QPS | 726 QPS |
| 3.x FLAT | 26.5 QPS | 32.4 QPS | 35.0 QPS | 35.6 QPS |
| 3.x HNSW | 34.0 QPS | 36.1 QPS | 36.6 QPS | 36.9 QPS |

- 2.5.1 在 2 个客户端时触顶，约 720 QPS，相当于每次约 1.4 ms，与它重启后的单条检索延迟（1.56 ms）接近。数据量小时上限随之升高：5000 行时，2 个客户端就超过 4000 QPS。检索持有全局 `mutex_`，上限应当就是这把锁把检索计算串行化的结果。
- 并发测试复用的是 build 之后的库，只剩 99,000 行，IVF_FLAT 已退回 FLAT（见上表注¹），所以表中没有单列 2.5.1 IVF_FLAT。
- 3.x 几乎不随客户端数增长。
- 本表由 [concurrency.py](../../benchmarks/baseline/concurrency.py) 测得，口径早于下文「M1 部分验证」：吞吐按固定的 8 秒计算（截止后才返回的请求也计入），各客户端也不同时起跑，这两点都使吞吐偏高，前者对单次耗时长的 3.x 影响较大；测量时没有清除代理环境变量，shell 设有代理时连接会多经过一跳，这对单次耗时短的 2.5.1 影响较大。这些偏差不改变上面的结论，M2 重测并发时统一到 `m1` 阶段的口径。

## 归因

### 3.x 的损失主要是 Python 实现

用 [profile_v3.py](../../benchmarks/baseline/profile_v3.py) 绕过 gRPC，直接对嵌入式引擎做 profile（10 万行，5 个 segment）：

- **HNSW 检索每次 27.6 ms，其中 FAISS 调用合计约 1.3 ms（约 5%）**：HNSW 搜索约 0.65 ms，构建 ID 过滤器约 0.65 ms。其余时间几乎都在 Python 逐行循环里：`_update_max_seqs` 与 `_build_local_mask`（[executor_indexed.py:125](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/search/executor_indexed.py#L125)、[:165](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/search/executor_indexed.py#L165)）每次检索都遍历全部 segment 与 memtable 的每一行，做跨源去重和删除判定。20 次检索累计调用 `dict.get` 802 万次、`is_deleted` 200 万次。
- **检索 memtable 中的数据**：每次都要在 Python 里逐行 `.as_py()`，把缓冲区重建成 numpy 数组（[memtable.py:219](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/storage/memtable.py#L219)）。5000 行时这一步约占检索耗时的八成，真正的距离计算不到 1 ms（在数据仍留在 memtable 的库上运行 `profile_v3.py` 测得）。
- **`count(*)` 与过滤查询**：先把命中的每一行物化成 Python 字典，再投影或计数（`row_to_dict` / `project_record`，10 万行时每次约 20 万次调用），不在列式数据上直接计算。
- **并发**：GIL 使检索无法并行。

这些都属于实现层问题。Rust 可以在不改变 3.x 架构的前提下消除它们：向量列连续存放、计数与投影直接在列式数据上计算、检索并行执行。

### 3.x 架构内唯一的架构性损失

「每次查询逐行重算跨源去重与删除判定」的代价是 O(总行数)，与实现语言无关，Python 只是把常数放大了。

我们在 Rust 里写了一个单线程微基准（[dedup_microbench.rs](../../benchmarks/baseline/dedup_microbench.rs)：`rustc -O`，pk 为随机顺序的 0..N、`_seq` 递增，用 `HashMap` 先求每个 pk 的全局最大 `_seq`，再逐行生成有效掩码，不含删除判定；取 5 次最好值）。结果如下：

- 10 万行：每次查询 0.6–1.8 ms；
- 100 万行：每次查询 15–41 ms。

区间的两端对应 Fx 风格哈希和标准库 SipHash。100 万行时，这个开销与 2.5.1 FLAT 的单条检索（11.7 ms）同一量级，是 2.5.1 IVF_FLAT（1.13 ms）的 10 倍以上。如果原样照搬，就成了 Rust 版的性能瓶颈。

因此 Rust 实现必须改为**按 segment 增量维护有效位图**：在写入、upsert、删除、flush、compaction 时更新，查询时直接使用。

- 判定依据仍然是 `_seq`，与[架构不变量](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/docs/modules.md) 1–3 一致，只是把计算时机从查询时移到了写入时与 flush 时。
- 位图只在内存中维护，也不违反 3.x「磁盘上不写可变辅助文件」的原则（[MVP.md:17](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/docs/MVP.md#L17)）。

### 2.5.1 的架构级瓶颈

这两项源于存储结构，随数据量线性恶化，换语言也消除不了：

1. **行存储 + 冷启动全量重放**：每次加载都把 SQLite 里的全部行读出，逐行插回 segcore，没有可以直接加载的不可变列式 segment。100 万行 FLAT 重启到可检索需要 4.96 s；同规模的 3.x 架构（Python 实现）只要 1.31 s。
2. **索引不落盘、单个 growing segment**：索引没有与持久化数据绑定的生命周期，每次加载都要重建。100 万行 IVF_FLAT 重启需要 19.5 s。

### 2.5.1 的实现层问题

- **删除慢**：SQLite 表只在自增 `id` 上有主键，删除按 `milvus_id` 列匹配，而这一列没有索引（[collection_data.cpp:155](https://github.com/milvus-io/milvus-lite/blob/v2.5.1/src/storage/collection_data.cpp#L155)）。我们用 [sqlite_delete.py](../../benchmarks/baseline/sqlite_delete.py) 在 100 万行库的副本上，预热页缓存后按同样的 SQL 删除 1000 条：没有索引时 170–213 ms，与整个删除请求的 261 ms 吻合；给 `milvus_id` 建索引（耗时 0.34 s）后降到 1.4–2.3 ms。
- **全局锁**：一把 `std::mutex` 串行化了绝大多数操作，包括检索，把并发吞吐限制在单条检索延迟的倒数附近。这是代码层面的选择。
- **没有 HNSW**：knowhere 本身支持 HNSW，是在 lite 里被注释掉了。

### 内存：两者都全量常驻

2.5.1 把数据全部放在 growing segment 里；3.x 的 segment 加载后也常驻内存，同时保存 pk、seq、numpy 向量与 Arrow 表。100 万行时，3.x 的服务端内存 FLAT 为 2541 MB、HNSW 为 3855 MB，分别是 2.5.1 FLAT（817 MB）的 3.1 倍和 4.7 倍。这一项两个架构都不占优，Rust 实现需要单独设定内存目标。

### 为什么不「以 2.5.1 为基础适配最新接口」

2.5.1 的性能来自 Milvus C++ segcore。以它为基础只有两条路：

- **继续用 C++**：把内嵌的 Milvus 从 2.5 升级到能对上 pymilvus 3.x 协议的版本。3.0 的 `MilvusService` 比 2.6 多 27 个 RPC，另外新增了独立的 `ClientTelemetryService`。从 v3 的首个提交删掉整套 C++ 构建来看，官方已经放弃了这条路；而且它也不是 Rust 重构。
- **用 Rust 重写 segcore 的能力**：这等于从零写引擎，同时还继承了上面两项架构级瓶颈。

两条路都得不到一个可以直接继承的基础。

## 对 Rust 实现的约束

1. **架构沿用 3.x**：WAL + memtable + 不可变 segment + delta + manifest，每个 segment 一份持久化索引。正确性不变量见 [AGENTS.md](../../AGENTS.md) 规则 3。
2. **有效性增量维护**：跨源去重与删除判定按 segment 维护内存中的有效位图，查询路径不做 O(总行数) 的逐行判定。
3. **列式到底**：memtable 的向量列与标量列连续存放；`count(*)`、过滤、投影直接在列式数据上计算，只物化最终返回的行。
4. **检索可并行**：多个 segment 与多个客户端的检索可以并行执行，不设全局锁。
5. **内存目标**：服务端内存不高于 2.5.1 的同规模水平（100 万行 × 128 维约 0.8 GB）。要突破这一目标（例如 HNSW 图的额外开销），须显式说明。
6. **可重新评估的参数与机制**：同步 flush（3.x 不变量 6）、单写者（不变量 7）、`MEMTABLE_SIZE_LIMIT = 10_000`、`MAX_SEGMENT_ROWS = 100_000` 都是 Python 版的实现选择。调整前按 AGENTS.md 规则 3 先写设计说明。
7. **性能基线**：同时对照 2.5.1 与 3.x，用 [benchmarks/baseline/](../../benchmarks/baseline/README.md) 测量。
   - 最低目标：FLAT 检索、过滤、`count(*)`、写入追平 2.5.1；
   - 冷启动、删除、点查不低于 3.x；
   - 并发吞吐超过 2.5.1 的上限。

## 测量局限

- 每组配置在同一台机器上跑了两轮，一次探索、一次用仓库脚本正式重跑。两轮的差距量级一致，文中数字取自第二轮。作为精确的性能基线，后续仍应多轮重复并报告波动。
- 数据是均匀随机向量，ANN 召回偏低（例如 3.x HNSW `ef=64` 时 recall@10 为 0.59，2.5.1 IVF_FLAT `nprobe=16` 为 0.19–0.23），因此不同 ANN 索引之间的延迟不宜直接比较。结论依据的是两个规模下的 FLAT 对 FLAT、与召回无关的操作，以及 profile 中的耗时占比。
- 延迟包含 pymilvus 客户端开销，约 0.3–0.5 ms 的下限。
- 页缓存是热的，重启时间反映的是进程级冷启动，不是磁盘冷启动。

## M1 部分验证

M1（[0004](0004-m1-scope.md)）只有内存数据，只能验证上文推断中的内存路径：写入、无过滤 FLAT 检索、空 expr 的 `count(*)`、按主键 get 与并发检索。核心推断，即按 segment 增量维护有效位图后 3.x 架构能达到 2.5.1 的水平，要到引入持久化的 M2 才能验证。本节是验证证据，不改变上文的结论。

- 日期：2026-09-25。本实现的数据取自引入本节的提交，只有一处差别：测量所用的构建中 Query 多取一次读锁，只涉及按 id 点查。两者同机复测，点查 p50（测量所用的构建对提交的代码）为 0.39 对 0.36 ms（10 万行）、0.37 对 0.41 ms（100 万行），都在表中三轮的范围内。「优化前」为 `9dcbdf0`（M1 第 4 步，距离顺序累加、全量排序、单线程）。
- 环境：Apple M2 Pro（10 核，32 GB），macOS 27，Python 3.12.14，pymilvus 3.0.2；milvus-lite 2.5.1 与 3.2.1。本实现的计算池 10 个线程。
- 口径：[benchmarks/baseline/](../../benchmarks/baseline/README.md) 的 `m1` 阶段。128 维均匀随机向量，带 `cat` / `tag` 两个标量字段，FLAT、L2，三个实现都是进程内模式（`MilvusClient("x.db")`），recall@10 均为 1。并发吞吐 = 完成的请求数 ÷ 从放行到最后一次返回的实际时长；测量前清掉 shell 中的代理环境变量（否则经 TCP 连接的 3.x 与本实现会多一跳代理，2.5.1 走 Unix socket 则不会）。
- 轮次：各跑 3 轮，每轮轮换三个实现的先后，表中为中位数；「优化前」只跑 1 轮。测量期间机器上有桌面应用在运行，每行结果记录的 load average 大多在 5–20 之间；本实现 100 万行那几行末尾记录的 1 分钟负载达 39–52，来自它自己刚结束的并发阶段（8 个客户端进程加 10 个计算线程）。轮间波动：单条检索除本实现 10 万行（0.87–1.04 ms）外，三轮相差不超过 4%；100 条检索 2.5.1 波动最大（10 万行 19.5–27.2 ms）。
- 延迟均为 p50，包含 pymilvus 客户端的开销。

### 10 万行

| 指标 | 2.5.1 | 3.x | 本实现 | 本实现（优化前） |
| --- | --- | --- | --- | --- |
| 写入（行/秒）¹ | 34600 | 21980 | 90240 | 90530 |
| 单条检索 top10 | 1.62 ms | 35.7 ms | 0.91 ms | 14.7 ms |
| 100 条检索 top10 | 20.3 ms | 783 ms | 30.3 ms | 1442 ms |
| 空 expr 的 `count(*)`² | 0.37 ms | 29.6 ms | 0.24 ms | 0.29 ms |
| 按 id 点查 100 条 | 1.60 ms | 2.16 ms | 0.39 ms | 0.35 ms |
| 同进程 8 线程并发（QPS / p99） | 747 / 27.6 ms | 35.7 / 306 ms | 2000 / 12.1 ms | 430 / 31.1 ms |
| 8 个客户端进程并发（QPS / p99） | 732 / 25.8 ms | 35.7 / 302 ms | 2003 / 12.7 ms | 426 / 31.3 ms |

### 100 万行

| 指标 | 2.5.1 | 3.x | 本实现 | 本实现（优化前） |
| --- | --- | --- | --- | --- |
| 写入（行/秒）¹ | 34900 | 22140 | 89170 | 89690 |
| 单条检索 top10 | 11.7 ms | 349 ms | 5.05 ms | 160 ms |
| 100 条检索 top10 | 188 ms | 8353 ms | 226 ms | 16250 ms |
| 空 expr 的 `count(*)`² | 2.00 ms | 288 ms | 0.23 ms | 0.22 ms |
| 按 id 点查 100 条 | 7.41 ms | 2.27 ms | 0.35 ms | 0.35 ms |
| 同进程 8 线程并发（QPS / p99） | 84.8 / 228 ms | 3.7 / 2797 ms | 222 / 71.0 ms | 39.1 / 278 ms |
| 8 个客户端进程并发（QPS / p99） | 84.0 / 227 ms | 3.7 / 2714 ms | 223 / 65.2 ms | 38.8 / 262 ms |

¹ 2.5.1 与 3.x 每次写入都要落盘（SQLite、WAL），本实现在 M1 不落盘，写入不是同口径，写入目标要到 M2 才能验证。
² 与上文表中带过滤的 `count(*)`（`cat >= 0`）不是同一个操作：3.x 对空 expr 走 `num_entities`，对带过滤的查询逐行物化。

### 对照「对 Rust 实现的约束」第 7 项

| 目标 | M1 的结果 |
| --- | --- |
| FLAT 检索追平 2.5.1 | 单条检索达到：10 万行 0.91 对 1.62 ms，100 万行 5.05 对 11.7 ms。100 条的批量检索未达到：10 万行 30.3 对 20.3 ms（慢约 49%），100 万行 226 对 188 ms（慢约 20%），原因见下 |
| 过滤追平 2.5.1 | M1 不支持过滤，测不了，留到支持过滤的里程碑 |
| `count(*)` 追平 2.5.1 | 目标指带过滤的计数，M1 测不了。空 expr 的计数：10 万行时 0.24 对 0.37 ms，都接近客户端开销的下限；100 万行时 0.23 对 2.00 ms；本实现在 M1 直接取存活主键数，不随行数增长 |
| 写入追平 2.5.1 | 数值约为 2.6 倍，但不是同口径（见注¹），留到 M2 |
| 冷启动、删除不低于 3.x | M1 没有持久化与删除，留到 M2 |
| 点查不低于 3.x | 达到：10 万行 0.39 对 2.16 ms，100 万行 0.35 对 2.27 ms |
| 并发吞吐超过 2.5.1 的上限 | 达到：8 线程与 8 进程时，10 万行约 2.7 倍，100 万行约 2.6 倍 |
| 内存不高于 2.5.1（约束 5） | M1 的口径不含内存，留到 M2 |

### 批量检索落后的原因

- 本实现的距离逐元素相减再平方，16 路累加（[flat.rs](../../crates/core/src/flat.rs)），100 万行时并行后受内存带宽限制（约 120 GB/s）；同一请求的 4 条查询一起打分，共用读入的候选向量。
- 2.5.1 一次检索的查询不少于 20 条时，大概率改用 `‖q‖² + ‖v‖² − 2q·v` 的矩阵乘法（faiss 的 BLAS 路径），计算量与内存访问都更省。这一点是根据 faiss 的实现推断的，没有单独测量。
- 0004「决策」为了 L2 的精度禁止这种展开，负责人已决定维持这一规则，批量检索落后如实报告（M1 第 5 步）。
