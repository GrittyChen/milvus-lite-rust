# 0006 第二个里程碑（M2）的范围与验收

- 状态：已采纳（「决策项」中的 26 项于 2026-09-26 采纳；存储协议经 [0007](0007-m2-storage-engine.md) 于 2026-09-28 收敛）
- 日期：2026-09-25（提议），2026-09-26（采纳）
- 对照版本：milvus-lite 3.2.1（tag `v3.2.1`，`43d1257`），pymilvus 2.6.17 / 3.0.2，pyarrow 25.0.1（3.x 的 venv）与 15.0.0（3.x 允许的下限）
- 前置决策：[0001](0001-refactor-baseline.md)、[0002](0002-data-dir-compat.md)、[0003](0003-in-process-server-lifecycle.md)、[0004](0004-m1-scope.md)、[0005](0005-search-thread-model.md)

## 背景

M1 证明了整条链路可行，但数据只在内存中，不发布（[0004](0004-m1-scope.md)）。M2 要做三件已经定下的事：

1. **持久化**：WAL、memtable、flush、segment、delta、manifest、恢复与崩溃测试，沿用 3.x 的架构（[0001](0001-refactor-baseline.md) 约束 1）。
2. **与 3.x 数据目录双向互通**（[0002](0002-data-dir-compat.md)）。M1「打开已有数据的目录直接拒绝」的限制随之取消。
3. **删除与按 segment 增量维护的有效位图**（0001 约束 2），并完成 0001 推断的主体验证：冷启动、reopen、写入同口径、内存。

本文先列出调研中得到的、影响范围划分的 3.x 事实，再列出经负责人拍板的决策项，最后写出范围、步骤、差异与验收。

## 3.x 的相关事实

标「实测」的都在 3.2.1 上复现过；标「源码」的给出位置；其余为推断。

### 写入、flush 与恢复

- **flush 是同步的**。它在触发它的 RPC 线程上完成；放到后台的只有 compaction 与索引构建，每个 collection 一个单线程执行器（[collection.py:265](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L265)）。
  - 架构不变量 6 仍然成立。
  - 不变量 7「单写者、无内部锁」已与代码不符：collection 有 `_maintenance_lock`，MemTable 有自己的锁，gRPC 以 10 个线程直接调用 Collection（源码）。
- **flush 的触发**：
  - memtable 中存活主键数加 tombstone 数不少于 10000（[constants.py:2](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/constants.py#L2)），只在每次调用结束后检查。
  - 因此一次写入 25000 行会得到一个 25000 行的文件；9999 行再删除一个不在 memtable 中的主键就会触发 flush；删除 memtable 中的主键时，插入被移除、换成 tombstone，大小不变（实测）。
  - flush 不切分文件，每个分区每次 flush 写一个 data 文件；`MAX_SEGMENT_ROWS` 只约束 compaction 的输出。
- **WAL**：
  - 每一代 memtable 一对 `wal_{data,delta}_N.arrow`，首次写入时创建；flush 时切换到 N+1，并写进 manifest 的 `active_wal_number`。
  - 每次写入只把缓冲交给操作系统，不 fsync（[wal.py:184](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/storage/wal.py#L184)）。
  - 3.x 重开时不删除旧 WAL，只在下一次 flush 时删除编号不大于冻结编号的 WAL（[flush.py:133](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/flush.py#L133)）。
  - WAL 追加失败后，3.x 继续往同一个文件追加。实测：半个 batch 之后再追加一个完整的 batch，在 471 个截断位置中，有 72 个会让那半个 batch 通过校验并被回放，读出的主键与向量都是垃圾值。
- **memtable 的行序是 Python dict 的语义**：
  - 覆盖写保持原位，删除后再插入移到末尾；flush 按这个顺序写文件，不按 `_seq` 排序。
  - 实测：写入 1、2、3、4，覆盖写 1，删除 2 后再写入 2，查询与 flush 后的顺序都是 `[1, 3, 4, 2]`。
  - M1 的做法是追加新行、清除旧行的存活标记，被覆盖的主键会移到末尾，与 3.x 不同。
- **持久化强度**：
  - WAL 只在 flush 提交之后、删除之前 fsync 一次；
  - Parquet、`manifest.json.prev` 与 `data/` 目录从不 fsync（`pq.write_table`，[data_file.py:59](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/storage/data_file.py#L59)）；
  - manifest 的 tmp 文件与 collection 目录会 fsync，目录 fsync 失败被忽略。
  - 所以 3.x 只防进程崩溃，不防操作系统崩溃或断电（源码与 fsync 跟踪实测）。
- **打开与恢复**：
  - collection 在第一次被访问时才打开并恢复，打开时还会调度后台的 compaction 与索引清理。
  - `ShowCollections`（pymilvus 的 `list_collections`）为报告 load 状态会逐个打开所有 collection，任何一个打不开，整个调用失败（[servicer.py:272](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/adapter/grpc/servicer.py#L272)）。
  - 恢复的步骤：
    - manifest 解析失败时回退到 `.prev`；
    - 按编号回放全部 WAL，**包括编号小于 `active_wal_number` 的**；
    - 删除孤儿文件时扫描所有分区目录，并排除 snapshot 钉住的文件（[recovery.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/recovery.py)）。
  - 读取 WAL：每个文件各自取通过 `validate(full=True)` 的前缀，没有校验和。实测：翻转向量中的一个字节，被静默接受。
- **segment 文件出错**：
  - manifest 引用的 data 文件缺失时，只记一条 warning，行数静默变少（实测 8 行变 3 行）；
  - Parquet 损坏时，这个 collection 的每个 RPC 都报 code 1（实测）。
- **compaction**：
  - 只在后台执行。两种触发条件（[compaction.py:155](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/compaction.py#L155)）：
    - 按文件大小分桶，某个桶内不少于 4 个文件；
    - 分区内 data 文件总数超过 32 个（`MAX_DATA_FILES`）时跨桶合并。
    - 单次都不超过 20 万行。
  - 合并时按主键去重、按 `_seq` 排序，每 10 万行切一个文件；tombstone GC 的阈值见 0002 约束 2。
  - `ManualCompaction` RPC 什么都不做，返回成功；`GetCompactionState` 恒为 Completed（实测）。
  - 标量 INVERTED 索引也只在 load 之后由后台构建。

### 删除、upsert 与相关 RPC 的对外语义

以下都经 pymilvus 3.0.2 与 2.6.17 实测，两个版本发出的请求与收到的响应一致。

- **`delete(ids=...)`** 发出 `Delete{expr: "id in [1,2]"}`。3.x 的快路径（[servicer.py:1526](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/adapter/grpc/servicer.py#L1526)）识别顶层的 `<主键> in [...]`，与 M1 按主键 get 的识别规则相同：
  - 不检查 load 状态；
  - 原样回显请求中的主键，`delete_cnt` 也把重复与不存在的主键计入：`[1, 99, 1, 98]` 原样返回，`[2, 97, 2]` 的 `delete_cnt` 为 3；
  - 主键类型不符或溢出时报 code 1、error_code 0，原文来自 pyarrow 或 CPython，而且已经消耗了一个 `_seq`。
- **`delete(filter=...)`** 的其余表达式走回退路径：
  - 临时把 collection 置为 loaded，先按表达式跨全部分区查出主键，再删除，两步不是原子的；
  - 返回命中的主键，顺序为先 segment、后 memtable；
  - 没有命中时，`IDs` 子消息存在但为空，`delete_cnt` 为 0。
- **删除按主键在整个 collection 内生效**，不看 `partition_name`，与 0002 约束 13 一致。主键列表非空时一次调用消耗一个 `_seq`，与主键个数无关；空列表（包括过滤零命中）直接返回，不写 WAL、不消耗 `_seq`（[0007「写入」](0007-m2-storage-engine.md#写入)）。tombstone 计入 flush 阈值。
- **upsert 总是与旧行合并**，不论 `partial_update` 是否为 true：按主键读出旧行（不检查 load、跨分区），新行覆盖其上，再按 insert 写入（[collection.py:477](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L477)）。
  - 所以全量 upsert 也会保留旧行中新请求没有给出的动态字段；pymilvus 按空值发送省略的 nullable 字段，它们变成 None（实测）。
  - 在 auto_id 的 collection 上沿用调用方给出的主键。
  - `field_ops`（如 `ARRAY_APPEND`）被静默忽略、按替换处理（实测）。pymilvus 只在调用方传入非 REPLACE 的操作时才发送它。
- **upsert 以及删除之后的重新插入都不写 tombstone**：旧版本只靠「同一主键取最大 `_seq`」隐藏；重新插入还会清掉 memtable 中的 tombstone（[memtable.py:106](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/storage/memtable.py#L106)）。
- **Flush**：同步落盘；collection 不存在时返回成功；`GetFlushState` 恒为 flushed。MilvusClient 只有显式调用 `flush()` 与 `optimize()` 时，才会发出 Flush 与 ManualCompaction。
- **由 insert 或 delete 触发的自动 flush 失败时**，该 RPC 报 code 1，但数据已经写进 WAL 与 memtable，当前不可见；如果之后没有成功的 flush 就重启，数据会重新出现（实测）。
- **持久化之后的读数**：`GetCollectionStatistics` 的 `row_count`、空 expr 的 `count(*)`、`DescribeIndex` 的行数都是存活行数，与 flush 无关，release 状态下也能调用。
- **load 状态不持久化**：
  - 重开后，有索引的 collection 一律是 NotLoad，检索、查询、get 与带 expr 的 `count(*)` 都报 code 101；没有索引的恒为 Loaded（实测）。
  - pymilvus 的快速建表会建 AUTOINDEX 并 load，所以 Quick Start 在进程重启后必须重新 `load_collection`。
- **auto_id 续号**：
  - 重开后取 `max(manifest.current_seq, 回放后 memtable 的 max_seq) + 1`（0002 约束 4）。
  - delete 消耗 `_seq`，所以重开后编号会跳号。实测：删除前 1–3，同一会话内接着得到 4、5；一次删除后重开，得到 5、6。
- **drop_collection**：
  - 先 close（会 flush），再删除整个 collection 目录（snapshot 在其中，随之删除），并删除指向它的别名（[db.py:251](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/db.py#L251)）。
  - 删除不是原子的：中途崩溃会留下半个目录。`create_collection` 遇到没有 `schema.json` 的残留目录时永久失败（`makedirs(exist_ok=False)`，[db.py:205](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/db.py#L205)）。
- **`db.close()` 吞掉各 collection 关闭时的异常**（包括最后一次 flush），WAL 保留（[db.py:722](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/db.py#L722)）。

### 3.x 的缺陷

以下都已实测。它们发生在 3.x 自己的路径上，本实现不照搬（处理方式见「决策项」第 8 项）：

- **a. 提交之后、删除 WAL 之前崩溃，数据重复。**
  - 重开时回放了内容已在 segment 中的旧 WAL，同一 (pk, `_seq`) 出现两份。实测：10 行已 flush、再写 5 行并在该点崩溃，重开后 `num_entities` 为 20，查询返回 20 行、只有 15 个不同主键。
  - 之后若再有写入并 flush，重复会被写进新文件。若中间没有新写入就正常关闭，flush 会覆盖同名的 data 文件（违背不变量 4），manifest 中出现两条相同的路径。
- **b. flush 遇到 I/O 错误时数据丢失。** 冻结的数据立即不可见；下一次成功的 flush 把它的 WAL 一并删除，重开后仍然丢失（注入 ENOSPC 实测）。
- **c. flush 的换出窗口里，检索与查询看到行消失**：memtable 不在读快照里。后台 compaction 持锁时，这个窗口会被拉长。
- **d. 并发首次打开同一个 collection 得到两个实例**，共用同一个 WAL 编号（db 级没有锁）。
- **e. 并发写入同一个 collection 会写坏 WAL。**
  - WAL 懒创建时存在竞态，多个线程同时以 `wb` 打开同一个文件（[wal.py:173](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/storage/wal.py#L173)）。
  - 实测：8 个线程各 300 次 insert 后 `os._exit`，3 次运行都读不出当前 WAL，重开后只剩 20000 / 24000 行。
- **macOS 上 collection 名的大小写冲突。** 建了 `Case` 之后，`create_collection("case")` 报已存在，但 `insert("case")` 成功：3.x 为 `case` 另开一个实例，指向同一目录。0004「决策」已预告这一点。
- **与分区有关的两条**，M2 不支持分区，暂不涉及。分区进入范围时，有效位图须处理「按范围去重」：
  - `delete(pk, partition_name=p2)` 删除位于 p1 的行，之后 drop p2 并重开，该行复活；
  - 跨源去重只在请求的分区范围内取最大 `_seq`。

### 过滤

- 3.x 的过滤约 3300 行 Python：手写的词法与递归下降解析、语义检查、三个求值 backend，比较与转换的语义实际由 pyarrow 的 kernel 决定（[milvus_lite/search/filter/](https://github.com/milvus-io/milvus-lite/tree/v3.2.1/milvus_lite/search/filter)）。
  - 错误信息是多行原文，包含列号、插入符、排序后的字段列表、difflib 的 did-you-mean，按模板计约 60–70 种。
- **与持久化、删除、upsert、重启有关的上游用例，几乎都用 `id >= 0`、`id >= 1` 这类过滤读回结果**（实测统计）。可复用的用例数随过滤范围变化：

  | 过滤范围 | 新增可复用用例（adapter + compatibility） |
  | --- | --- |
  | 不含过滤 | 26 |
  | S1：`<标量字段> <比较> <字面量>`，可用 `and` 连接 | 103 |
  | S1 再加 `or`、`not`、`in`、`not in` | 112 |
  | 完整标量过滤（不含 JSON、数组、text_match、几何） | 122 |

- **基准的 build 阶段**用到 `cat == 3`、`tag == "t7"`、`cat == 5 and id > 1000`、`cat >= 0`（带过滤的 `count(*)`），都在 S1 之内。0001 第 7 项中「过滤追平 2.5.1」与「`count(*)` 追平 2.5.1」只能靠它们验证。
- **S1 的难点在对齐，不在求值**：
  - 解析与语义检查必须完整。3.x 对任何输入报 code 6 的地方都要照报原文，只有合法但不在子集内的表达式才返回 UNIMPLEMENTED。
  - 整数列对浮点字面量在 f64 上比较。被求值的物理行中只要有 |v| > 2^53，就报 code 1；已删除或已被覆盖、但还在 segment 中的行也算，memtable 中已删除的行不算（实测）。
  - 带 limit 的结果顺序取决于物理布局：先 segment、后 memtable。旧版本在 segment 中时，upsert 过的行排到末尾（实测）。
  - 开了动态字段的 collection 中，未知字段名会变成 `$meta` 查询，不报 unknown field。
- **3.x README 的 Quick Start 用的是动态字段过滤**：检索中的 `category in ['tech', 'ai']`，查询中的 `category == 'tech'`。Milvus 官方文档的删除示例同样是 `color in [...]`。这些都不在任何标量子集之内。
- **0002「结论」中「INVERTED 定义的标量过滤按扫描执行，结果相同」不成立**：
  - 3.x 自身在同一 collection 中，已建索引的 segment 与 memtable 结果就不同。例如 `i32 == 2.5` 在 segment 上命中值为 2 的行，在 memtable 上不命中（实测）。
  - 这些索引又是 load 之后在后台构建的，所以 3.x 的结果还取决于索引有没有建好。

### Rust 侧依赖

- **arrow-rs 60.0.0 满足需求**：arrow-ipc、arrow-array、arrow-schema、arrow-buffer，以及 `default-features = false, features = ["arrow", "snap"]` 的 parquet。
  - `cargo tree -e all` 在 Linux、macOS、Windows 各目标上都没有 cmake、protoc 或编译 C 的 crate。
  - 唯一新增的系统链接是 macOS 的 CoreFoundation：它是系统自带的框架，由 arrow-array 强制启用的 chrono `clock` 引入。
  - 全部依赖声明的 MSRV 最高为 1.88，不超过本仓的 1.89。
  - 代价：wheel 压缩后约增加 2 MB，clean release 构建约增加 17 s。
- **与 pyarrow 15.0.0 与 25.0.1 双向互读**：
  - 覆盖 3.x 真实写出的 WAL、data 与 delta，逐值一致；
  - 引擎级验证中，3.x 能打开、检索并合并 Rust 写出的文件；
  - 截断的 WAL 在所有试过的位置上，arrow-rs 与 pyarrow 恢复出的 batch 与行数完全一致。
- 写实现时必须照做的几点：
  - Parquet 显式设置 SNAPPY 与 `coerce_types`（arrow-rs 默认不压缩）；
  - 读入时统一 list 子字段名：Parquet 中为 `element`，WAL 中为 `item`，arrow-rs 的类型比较区分二者。
- **数据损坏（不是截断）时 arrow-rs 的读取器会 panic**：
  - 对真实 WAL 逐字节翻转，约三成位置触发 IPC 读取器 panic，另有约 27% 的翻转被静默接受；
  - Parquet 读取器也有少量位置会 panic。
- **持久化原语**：
  - Rust std 在 macOS 上的 `sync_all` / `sync_data` 是 `F_FULLFSYNC`，比 Python `os.fsync` 更强，每次约 3 ms。
  - Windows 上的原子替换与 Python `os.replace` 走同一个主路径（`MoveFileExW`）。std 在 ACCESS_DENIED 时还会改用 POSIX 语义重试，Python 不会。
  - Windows 上无法 fsync 目录，3.x 实际上也不做。

## 决策项

每项都写明了选项与建议，负责人已全部按建议采纳。「范围」「步骤」「差异」「验收」各节按采纳的结果写成。

### 一、范围

1. **过滤进入 M2 的范围。** 选项：
   - 不含过滤；
   - S1，放在 M2；
   - S1，另立 M3；
   - S1 再加 `or`、`not`、`in`、`not in`；
   - 完整标量过滤。

   **建议 S1，放在 M2，排在删除、upsert 与崩溃测试之后。**
   - 理由：
     - 基准的全部过滤操作与上游删除、upsert、重启用例的全部表达式都在 S1 之内，可复用用例从 26 条增加到 103 条；
     - 0001 第 7 项的过滤与计数目标只能靠它验证；
     - 有效位图与过滤掩码在同一份设计说明中考虑，避免返工（0001 约束 2、3）。
   - 代价：解析、语义检查与错误原文加 S1 求值，估计 Rust 1.5–2.5k 行，外加与 3.x 的差分语料。
   - 另立 M3 的好处：M2 的风险集中在存储上，持久化、删除、upsert 的正确性可以用自写的 get、count、search 差分来证明。代价是 0001 的两项目标与 77 条上游用例推迟到 M3。
   - S1 的边界：
     - **求值的构造**：
       - schema 中的标量字段（INT8–INT64、FLOAT、DOUBLE、VARCHAR、BOOL，包括主键）与字面量（整数、浮点、字符串、布尔）的六种比较；
       - `and` 与括号；
       - 所有 RPC（Query、Search、Delete、`count(*)`）中顶层的 `<主键> in [...]`；
       - 空 expr 的 Query（含 limit / offset）。
     - query iterator 仍返回 UNIMPLEMENTED，保留 M1 按 `iterator` 键拒绝的检查：pymilvus 的 query_iterator 发出的 `<主键> > x` 加 limit 正好落在 S1 内，而 3.x 见到这个键就改为按主键排序（[servicer.py:442](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/adapter/grpc/servicer.py#L442)）；忽略它，分页结果会静默出错。
     - 字面量写在左边、`&&` 等写法：凡能归约为上述构造的都属于 S1，由过滤一步的实现按 3.x 的解析结果确定。
     - **解析**：对 3.x 的全部语法完整实现，语法错误与原文一致。
     - **语义检查**：只对 S1 求值的构造完整实现。其余构造解析成功后返回 UNIMPLEMENTED，即使 3.x 会对它报 code 6；这沿用 0004「同时含不支持的功能与其他错误时」的先后规则。这一条界定了上面工作量估计的范围。
2. **upsert 进入 M2。** **建议进入**：
   - 有了删除与有效位图之后成本不高，基准的 build 阶段也要用；
   - 语义与 3.x 相同，总是与旧行合并，支持 `partial_update`；
   - `field_ops` 非空时，3.x 静默按替换处理，违背不变量 15。建议返回 UNIMPLEMENTED 并登记差异。
3. **compaction 进入 M2。** **建议不进入**，M2 不做任何 compaction。
   - `ManualCompaction` 与 `GetCompactionState`：**建议返回 UNIMPLEMENTED**（不变量 15）。
     - 3.x 的 `ManualCompaction` 什么都不做就返回成功，理由是它有自动 compaction（[servicer.py:1490](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/adapter/grpc/servicer.py#L1490) 的注释）；M2 没有，返回成功就是静默成功。
     - 另一个选项是照 3.x 返回成功，作为不变量 15 的例外登记。
     - 代价：显式调用 `client.compact()` 或 `optimize()` 会报错，上游的两条 compaction 用例不能复用。
   - 代价：
     - 被覆盖的旧版本与已删除的行永不回收，文件数与内存随写入总量增长；
     - 带 limit 的过滤结果顺序在 3.x 后台 compaction 之后与本实现不同；
     - 冷启动要读更多文件：100 万行时本实现约 100 个 data 文件，3.x 合并后约 10 个，基准汇报时注明。
   - 所以 compaction 是首次发布的前提。
   - 互通不受影响：3.x 打开时会替本实现 compaction。设计不排斥将来由 compaction 替换 segment，但不为它预留实现。
4. **M2 是否发布到 PyPI。** **建议不发布**。
   - 没有动态字段过滤（包括 `in`），3.x README 的 Quick Start 跑不通，而这是 pymilvus 用户最常见的写法。首次发布的范围另行确定，至少包括动态字段过滤与 compaction。
   - 0004 中两处「在 M2 发布前」（「验收」第 3 项与「跨平台」）改为「在首次发布前」。
5. **CLI `milvus-lite server`。** **建议不进入 M2**。
   - 服务端内存用与 [tests/differential/server3x.py](../../tests/differential/server3x.py) 同类的辅助脚本，在独立进程中起进程内服务来测；
   - 并发改用 bench.py `m1` 阶段已有的线程与进程口径，concurrency.py 不再使用。
   - CLI 作为对外契约随首次发布提供，届时再定位置、`--max-workers` 的含义与信号处理。
6. **基准是否作为 M2 的通过门槛。** **建议不作为硬门槛**，与 0004 相同：结果对照 0001 的目标汇报，未达到的先分析原因，由负责人决定是否在进入下一阶段前优化。
7. **M2 期间的跨平台验证。** **建议 M2 期间在本机的 Linux 容器里运行 Rust 单测与 LOCK 的互斥用例**，验证 flock 与 fcntl 互不感知这一点。
   - 这需要启动本机的 Docker Desktop；strace 可能需要 `--cap-add=SYS_PTRACE`，第 19 项的 strace 核对也在这个容器里做。
   - 用例在容器自己的文件系统上运行，不用挂载进来的目录；3.x 也装在容器里。
   - Windows 随 CI 立项，在首次发布前完成。持久化是对平台最敏感的部分，Linux 不宜推迟。

### 二、与 3.x 不同的处理

第 8–12 项定的是对外语义和必须满足的约束。具体的协议，包括提交顺序、回放规则与文件命名，在第 1 步的设计说明中完整给出，并由崩溃注入与模型测试验证（「验收」第 4、5 项）。

8. **3.x 缺陷 a–e 的修正，以及 flush 的提交语义。** **建议在本实现自己的路径上修掉，并登记差异。**
   - **提交点**：flush 与 manifest 的提交以 `manifest.json` 的 replace 为准。
   - **a：回放全部 WAL，只跳过已被已提交状态覆盖的操作**。
     - 记 segMax[pk] 为 data 文件中该主键的最大 `_seq`，deltaMax[pk] 为已提交删除中该主键的最大 `_seq`：
       - 数据行 `(pk, s)`：`s ≤ segMax[pk]`，或 `s < deltaMax[pk]` 时跳过；
       - tombstone `(pk, t)`：`t ≤ max(segMax[pk], deltaMax[pk])` 时跳过。
     - 可见结果与「全部回放、按最大 `_seq` 判定」相同，但不产生重复。这正是 [flush.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/flush.py) 开头注释所说的「按 seq 去重使其幂等」，3.x 实际没有做到。
     - 两个条件缺一不可：
       - 数据行不看 deltaMax 的话，同一代中插入后又被删除的行从未写进 segment，会作为死行回到 memtable，打乱物理行序（第 23 项）并被写进新文件；
       - tombstone 不看 segMax 的话，删除后又重新插入的主键，它的 tombstone 已被 3.x 的 memtable 清掉、不在 delta 文件里，会被重新写出，文件名与已有的 delta 相同，这一代就永远提交不了。
       - 两种情况都在 3.x 与状态模型上复现过。
     - memtable 中的行同样按 0002 约束 13 判定是否已被删除，不能像 3.x 的 get 那样命中 memtable 就直接返回。
     - 所需的两个索引就是有效位图在打开时本来要建的，不增加结构。
     - 不依据 `active_wal_number` 删除或跳过任何 WAL，所以以下三种情况都能恢复：
       - 3.x 在 flush 失败（缺陷 b）之后又在提交与清理之间崩溃，留下的未提交旧 WAL；
       - 3.x 的并发竞态写进旧 WAL 的行（缺陷 e 的同类问题）；
       - 本实现在提交点之后失败而保留的 WAL。
   - **WAL 的删除**：完整的提交之后，按 [0007「提交之后」](0007-m2-storage-engine.md#提交之后) 的统一规则，从最小编号开始删除已发布代对应的 WAL；打开时已存在的 WAL 须等第一次活跃代发布。Linux 与 macOS 每次 unlink 后同步 `wal/`，使断电后的删除顺序可证明。memtable 为空时关闭仍做空提交，清除已提交而残留的 WAL。
   - **b**，提交点之前失败：
     - 冻结的数据保持可见，它的 WAL 保留；
     - 下一次 flush 按代的顺序连同它一起提交。
   - **提交点之后失败**（例如 replace 之后的目录 fsync 失败）：
     - 按已提交处理，但保留这一代的 WAL，并 fsync 它与 `wal/` 目录，这样断电后即使回到旧 manifest，也能从它完整恢复；
     - collection 进入与第 9 项相同的待完成状态：之后的写入、显式 Flush 与关闭都先重试这次提交，成功后按已发布代的规则删除保留的 WAL；重试失败时拒绝写入。提交点之后失败直接保留的代至多一代；WAL 删除失败则可能跨多代残留（[0007「提交之后」](0007-m2-storage-engine.md#提交之后)）。
     - 重试必须重新执行完整的 manifest 提交：新的 tmp、fsync、replace、目录 fsync，不能只再 fsync 一次目录。Linux 上 fsync 的错误只上报一次，之后对同一脏状态再 fsync 会返回成功，而数据未必落盘（PostgreSQL 因此选择停机）。
     - 错误写入日志；显式 Flush 返回这个错误。
     - 重试一直失败时，盘上留着已提交的 WAL，3.x 打开时会重复计数。这一点登记差异，并补进 0002「继承的 3.x 已知风险」。
     - 放弃的做法：像 PostgreSQL 那样停止服务。对嵌入式用户代价过大，而数据并没有丢失的风险。
   - **manifest 只写已提交的状态**：
     - 所有写 manifest 的路径（flush、CreateIndex 等）都在 collection 的 flush 锁内互相串行，锁序见 [0007「取锁顺序」](0007-m2-storage-engine.md#取锁顺序)；
     - `active_wal_number` 只由 flush 提交推进，取「最后一个已提交的代 + 1」。
   - **文件名**：
     - 被 manifest 或任一 snapshot 引用的文件，绝不覆盖；
     - 重试时遇到未被引用的同名孤儿（上一次失败留下的），先删除再写；若同 stem 的 3.x `.idx` / `.sidx` 侧车仍在，则向外放宽 seq_max 取新名，避免 3.x 把旧索引用到新数据（[0007「flush」](0007-m2-storage-engine.md#flush)）；
     - 与只被 snapshot 钉住、已不在 manifest 中的文件同名时，向外放宽 seq_max 取新名。这对 GC 是安全的，3.x compaction 的 `_pick_unique_seq_range` 也这样做；seq_min 与区间的下界不改（0002 约束 2）。
   - **WAL 编号**：
     - 打开之后的新写入使用新的编号 `max(已有的最大编号 + 1, active_wal_number)`，绝不续写或截断已有的 WAL，与 3.x 相同。
     - 3.x 建过索引、尚未 flush 的 collection，`active_wal_number` 为 null。
   - **c**：换出 memtable 与发布新 segment 对读者是原子的。
   - **d**：打开 collection 互斥。
   - **e**：`_seq` 分配、WAL 追加与 memtable 写入在同一个写锁内完成。
   - **flush 按代的顺序提交**：不论第 21 项选哪种加锁方式，后冻结的一代都不得先于前一代提交。
   - **读取 3.x 留下的数据**：
     - 3.x 写出的目录中若已有重复的 (pk, `_seq`) 或 manifest 中重复的路径，读取时各按一份计。
     - 3.x 在这类目录上不只是行数计两次：get 返回旧版本，query 返回新版本，upsert 与旧行合并，带 limit 的顺序也不同（实测）。
   - **无法从本实现一侧避免的**，都补进 0002「继承的 3.x 已知风险」：
     - 本实现写出的目录若在 a 的崩溃点中断，3.x 打开时照样会重复；
     - 3.x 并发写入时的 e 也会作用在共享目录上；
     - 已提交的 WAL 残留时，3.x 的 compaction 与 GC 可能抹掉删除的证据，之后任一方打开，已删除的行都会复活（3.x 上实测）。
9. **flush 失败之后的写入。** 触发 flush 的那次写入已经进了 WAL 与 memtable。选项：
   - (i) 这次写入返回成功，collection 进入「有待提交的数据」状态：
     - 之后每次写入都先重试提交，**在写入 WAL 之前**进行；
     - 重试仍然失败时，拒绝这次写入，不写 WAL、不改 memtable，错误写明原因；
     - 重试要节流：距上次失败不足最小间隔时不再尝试，直接拒绝，错误中带上上次失败的原因；显式 Flush 不受节流。否则磁盘写满时，客户端的每次重试都会触发一次完整的 flush（编码 1 万行并写 Parquet）。间隔在设计说明中定；
     - 读照常；显式 Flush 同样重试并返回结果。
   - (ii) 与 3.x 相同，写入报 code 1，而数据实际已经写入。
   - (iii) 这次写入也报错，collection 进入只读状态，直到重开。

   **建议 (i)。**
   - 任何报错的写入都没有生效，任何生效的写入都返回成功。
   - 待提交的数据有上界：只有失败时已经冻结或正在写入的数据，之后的写入在重试失败时被拒绝，不会累积。
   - 错误没有被吞掉：它出现在日志、之后被拒绝的写入与显式 Flush 中（规则 5）。
   - (ii) 会诱发客户端重试，在 auto_id 的 collection 上产生重复行；(iii) 要求用户重启进程。
10. **WAL 追加失败。** 例如磁盘写满时，文件尾部会留下半个 batch。若之后继续往这个文件追加，恢复时可能把那半个 batch 当作合法数据回放（见上文事实）。**建议**：
    - 失败的那次写入报错，memtable 不变；
    - 当前这一代立即冻结，按 flush 处理，提交失败时进入第 9 项的状态；
    - 之后的写入使用新一代的 WAL，从不在半个 batch 之后继续追加；
    - 新 WAL 创建失败时，写入报错、不生效；创建失败可能留下残缺的文件，下一次必须再换新的编号，不续写它。
    - 放弃的做法：
      - 停止写入直到重开：进程内服务下等于直到进程重启；
      - 截回上一个完整 batch：截断本身也可能失败。
    - 3.x 在这种情况下继续追加，登记差异，并补进 0002「继承的 3.x 已知风险」。
11. **manifest 与 segment 文件出错。** 3.x 的做法：
    - manifest 解析失败时回退 `.prev`，并把只登记在新 manifest 里的文件当孤儿删掉；
    - 引用的文件缺失时静默少数据；
    - Parquet 损坏时每个 RPC 报 code 1。

    **建议**：
    - 这三种情况都让该 collection fail-closed（见第 14 项），不回退、不删除任何文件；
    - `manifest.json` 缺失而 `.prev` 存在时同样 fail-closed：3.x 的 drop 中途崩溃可能留下这种目录，3.x 会读 `.prev`，本实现若当作新 collection，孤儿清理就会删掉全部 Parquet；
    - manifest 引用的文件不存在（ENOENT）按文件错误处理，判定结果缓存；
    - 除 DescribeCollection 与 drop 外，该 collection 的 RPC 报 code 1，错误信息写明原因与出错的文件；
    - schema 完好时 DescribeCollection 照常返回；
    - 登记差异。
12. **WAL 的读取错误。** **建议**：
    - 先把整个文件读进内存，再从内存解码：
      - 读文件时的错误是 I/O 错误，让该 collection 打开失败，下次访问时重试（3.x 吞掉并保留前缀）；
      - 解码时的一切错误都是格式错误，包括 arrow-rs 对截断报出的 `UnexpectedEof`。否则每次崩溃留下的截断 WAL 都会被当成 I/O 错误，collection 永远打不开。
    - 格式错误、截断与损坏都取已校验的前缀，与 3.x 相同：
      - 断电后未 fsync 的 WAL 可能中间为零、之后仍有完整的消息，按前缀处理，才符合第 19 项「断电后 collection 照常打开，只丢未 flush 的写入」；
      - 代价与 3.x 相同：中间损坏时，其后已确认的写入静默丢失，补进 0002「继承的 3.x 已知风险」；
    - 两边的校验项不同，损坏时判定的位置可能与 3.x 不同，登记差异。后果不只是前缀长短不同：本实现从较短的前缀续号，3.x 之后打开时，可能看到两条不同的操作共用同一个 `_seq`；
    - WAL 与 Parquet 的读取都先做边界检查，再以 `catch_unwind` 兜底，panic 按格式错误处理（规则 6）。
13. **整数列对浮点字面量、数据超出 2^53 时的 code 1。** **建议只看存活行，并登记差异**。
    - 3.x 的结果取决于已删除或已被覆盖的旧版本是否还在 segment 中，也就取决于后台 compaction 的时机，本实现无法稳定对齐。
    - 存活行超出 2^53 时，两边都报错。

### 三、数据目录的兼容

14. **本实现不支持或无法安全处理的内容。** **建议按 collection 做 fail-closed**。
    - **判定条件**，满足任一条即 fail-closed：
      - schema 属于 M1 的 CreateCollection 会拒绝的：M1 范围之外的字段类型、nullable 或默认值、function、多个向量字段、partition key、`enable_analyzer` / `enable_match` / `analyzer_params`、properties 中的 `timezone`；
      - manifest 中有 `_default` 以外的分区，或 WAL 中出现 manifest 没有的分区。delta WAL 中的 `_all` 表示跨分区删除，不算分区：pymilvus 不指定分区的 delete，3.x 在 WAL 中就写 `_all`（实测）；data WAL 中不应出现 `_all`；
      - 未知的格式版本、dtype 或 metric；
      - manifest、schema 或 snapshot 元数据中有未知的键；
      - snapshot 元数据无法解析：3.x 会静默跳过它（[snapshots.py:190](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/storage/snapshots.py#L190)），被钉住的文件就可能被删除；
      - 第 11、12 项中的文件错误。
    - **未知的键**：3.x 升级格式时不改版本号（0002 约束 11），未知的键是新格式唯一的信号。本实现每次 flush 都会重写 manifest，会丢掉这些键。代价是：3.x 的新版本即使只加了无害的键，本实现也打不开，要跟进时按 0002 约束 12 评估。
    - **判定的位置**：放在 core，grpc 层建表时复用同一份判定，避免两份清单逐渐不一致。0002 约束 11 与「验证」中的「拒绝打开」相应改为按 collection 生效。
    - **这类 collection 的表现**：
      - 出现在 `ShowCollections` 中，load 百分比为 0（见第 16 项）；drop 照常执行。
      - 不支持的内容：`schema.json` 能完整解析（没有未知的键与 dtype）时，DescribeCollection 照常返回完整的 schema；其余 RPC 返回 UNIMPLEMENTED，写明原因。
        - 理由：pymilvus 的 `has_collection` 经 DescribeCollection 判断，「存在就 drop 再重建」是 Milvus 官方 Quickstart 的写法（[milvus.io/docs/quickstart.md](https://milvus.io/docs/quickstart.md)），也是用户代码里最常见的模式。首次发布时仍会有 ARRAY、nullable、function 这类不支持的内容，DescribeCollection 报错的话，这个差异会一直带到发布。
        - 这只是 schema 的 proto 编码，不增加能力，不违反不变量 15，也与第 11 项中文件错误时「DescribeCollection 照常」一致。
        - 代价：gRPC 层的 schema 编码要覆盖 3.x 的全部字段类型、function 与属性，估计一两百行，并与 3.x 的 DescribeCollection 做差分。fail-closed 的判定本来就要完整解析 schema。
        - 第 15 项中别名命中的情况不变，仍返回 UNIMPLEMENTED：别名少见。
      - 文件错误：按第 11 项处理。
      - 目录内容不动。
    - 其余 collection 照常使用。
    - **不论是否支持，以下几点都必须做到**：
      - drop 时维护 `aliases.json`，删除指向被删 collection 的别名，与 3.x 相同；
      - 任何删除文件的路径都读 snapshot 的钉住集合；
      - 不触碰 `databases/` 下的其他数据库。
    - **索引定义**：显式 ANN 与 INVERTED 的索引定义仍按 0002 原样保留、按 FLAT 或扫描执行。INVERTED 扫描的结果与 3.x 不同，登记差异并修订 0002 的那句话。
15. **已有目录中的别名与其他数据库。** 3.x 的每个 RPC 都先解析别名，数据库是否存在按目录判断。M1 对这两种情况分别报「collection 不存在」与「数据库不存在」，会把已有数据显示为不存在，违反 0003 的原则。**建议**：
    - 请求的名字命中 `aliases.json`，或 `databases/<db>` 已存在时，返回 UNIMPLEMENTED；
    - 新建 collection 的名字与已有别名相同时，与 3.x 报同样的错误；
    - `.database-staging/` 的残留在打开数据库、拿到 LOCK 之后清空，与 3.x 相同（M1 拒绝打开）。
16. **collection 何时打开、按什么顺序打开。** **建议与 3.x 相同，在第一次被访问时打开并恢复，失败只影响这个 collection。**
    - **顺序：先完成全部读取与判定，再做任何删除**：
      - 读取与判定：schema、manifest、snapshot 元数据；manifest 引用的全部文件都读入并校验；读取 WAL。
      - 全部通过之后，才按 3.x 的规则删除孤儿文件。
      - 任何一步失败都 fail-closed，不删除任何文件。
      - 反例：manifest 已提交，它引用的 Parquet 因断电而为空，但旧 WAL 完好。若先清理，数据就可能永久丢失。
    - **缓存**：fail-closed 的判定结果在进程内缓存；I/O 错误不缓存，下次访问重试。
    - **`ShowCollections` 不做恢复，只读 `schema.json` 与 manifest**：
      - 两者之一读不了，或者命中第 14 项中只凭这两个文件就能判断的条件（schema、manifest 中的分区、版本、未知的键），或者已被判定为 fail-closed，load 百分比为 0；
      - 第 14 项中要读 WAL 或 snapshot 元数据才能判断的条件，以及文件错误，要等打开时才能发现；
      - 其余未打开的，由 manifest 中是否有索引定义推出：有索引为 NotLoad，没有索引为 Loaded，与 3.x 打开后的状态相同；
      - 打开时发现之后，按缓存的判定显示为 0；
      - 所以它不会因为某个 collection 打不开而整体失败（3.x 会），登记差异。
    - 0003 中「引入 WAL 后启动包括恢复……届时再改为按路径加锁」随之不再需要。
17. **drop 与 create 的原子性。**
    - 锁序、改名前后失败的收尾、无句柄与已损坏句柄的 drop，以及目录 fsync 失败后的写入封锁以 [0007「drop、create 与关闭」](0007-m2-storage-engine.md#dropcreate-与关闭) 为准。
    - **drop**：
      - 持数据库写入闸门的独占许可与目录变更锁；有句柄时再持该 collection 的 flush 锁与状态写锁并标记为已删除。之后持有旧句柄的请求一律报「collection 不存在」，也不再创建任何文件。
      - 目录整体改名移入 `.database-staging/`，再 fsync `collections/` 与 staging，然后删除。改名之后的删除失败只写日志，下次打开时随 staging 一并清空。
      - staging 中的名字要唯一，并避开 3.x 使用的 `database-` 前缀。两边打开时都会清空这个目录。
      - drop 之前不 flush：对外没有可观察的效果，只多一个失败点。3.x 的 drop 在 flush 失败时报错，本实现不会，登记差异。
    - **create**：
      - 在 `.database-staging/` 中建好目录与 `schema.json` 并 fsync，确认目标不存在后，改名移入 `collections/`，再 fsync `collections/`；
      - 改名之后核对 `collections/` 的目录列表中有这个精确的名字；没有（文件系统改写了名字，见第 18 项）就撤销并报错；
      - 必须先确认目标不存在：POSIX 的 rename 会覆盖一个空目录。
    - 改名成功是可见性的提交点；目录 fsync 全部成功后才确认断电持久化。fsync 失败时报「目录已变更、持久化未确认」，本服务实例停止写入直到重开，不再对同一脏目录原样重试 fsync。drop 此时保留别名与 staging，重开后按实际目录状态收尾。3.x 的 drop 中途崩溃会留下半个目录，`schema.json` 若还在，collection 仍可见但数据不全；create 先建目录后写 schema，中途崩溃后这个名字永久不能再用。登记差异。
    - 遇到 3.x 留下的、没有 `schema.json` 的残留目录时，create 与 3.x 一样报错。
18. **不区分大小写的文件系统上的 collection 名。** **建议**：
    - 按 `collections/` 目录列表中的精确名字判断 collection 是否存在；
    - 新建时，若路径已存在但不在精确列表中（只差大小写，或 Unicode 规范化不同），报与 3.x 相同的「已存在」错误；不做大小写折叠，Linux 等区分大小写的文件系统上照常与 3.x 相同。
    - 访问或 drop 一个只在大小写上相同的名字时，按「collection 不存在」处理，登记差异：3.x 会另开一个实例，drop 时会删掉真实的那个 collection。
    - HFS+ 会把文件名规范化为 NFD。
      - 3.x 写的目录：非 ASCII 的名字在 3.x 中按路径能找到，本实现按目录列表精确比较会得到「不存在」、新建时又报「已存在」，登记差异。
      - 本实现自己：create 的核对（第 17 项）拒绝会被改写的名字，所以不会建出之后访问不到的 collection。

### 四、持久化

19. **要达到的保证。** **建议**：
    - **Linux 与 macOS**：进程崩溃不丢已确认的写入（与 3.x 相同）；操作系统崩溃或断电不丢已 flush 的数据（3.x 不保证）。断电后 collection 照常打开，只丢未 flush 的写入（与 3.x 相同）。
    - **Windows**：只保证进程崩溃不丢已确认的写入，与 3.x 相同。那里没有目录 fsync，std 的 rename 也不带 `MOVEFILE_WRITE_THROUGH`；要做到需要 unsafe 或新依赖。
    - **开销**（负责人本机 APFS 上的实测）：

      | 操作 | fsync | F_FULLFSYNC |
      | --- | --- | --- |
      | 4 KB 文件 | 0.025 ms | 2.7 ms |
      | 5 MB 文件 | 4.9 ms | 4.1 ms |
      | 目录 | 0.000 ms | 2.9 ms |

      - 一次 flush 约 5–7 次 sync，多出约 10 ms；100 万行约 100 次 flush，共多约 1 秒。M1 写入约 9 万行/秒，100 万行约 11 秒；2.5.1 为 3.46 万行/秒，余量足够。
      - 普通 fsync 在 APFS 上对目录几乎不做事，改用它就得不到「断电不丢已 flush 的数据」。
    - 据此需要：
      - **WAL**：
        - 每个 batch 在确认写入之前交给操作系统，不在用户态缓冲；
        - 不逐次 fsync：3.x 不做，逐次 fsync 会使写入吞吐大幅下降；
        - 删除 WAL 之前 fsync 它，与 3.x 相同，满足不变量 10「WAL 关闭前 fsync」的字面要求；每次 flush 在 macOS 上多约 3 ms；
        - WAL 被保留时，包括停止时 flush 失败（0003「进程退出」）与第 8 项「提交点之后失败」，fsync WAL 文件与 `wal/` 目录。
      - **flush**：
        - manifest 提交前，fsync 新写的 Parquet 与它们所在的目录；
        - 新建目录时逐级 fsync 上级目录：
          - 新建数据目录根（例如 `x.db`，由本实现创建）时 fsync 它的上级目录；
          - 首次创建 `collections/` 时 fsync 数据目录根；
          - 建 collection 目录时 fsync `collections/`；
          - `partitions/`、`_default/`、`data/`、`delta/`、`wal/` 各自的上级；
        - 本进程对 collection 的第一次 manifest replace 之前，fsync manifest 引用的、不是本进程写出的文件及其目录（[0007「flush」](0007-m2-storage-engine.md#flush)）；
        - manifest 按 3.x 的五步提交。
      - **`schema.json` 与 `aliases.json`**：写 tmp、fsync、改名、fsync 目录。3.x 写 `aliases.json` 不做 fsync。
      - **macOS**：沿用 std 的 `F_FULLFSYNC`。
      - **错误**：fsync 的错误向上传递（规则 5）。Linux 与 macOS 上目录 fsync 返回 EINVAL 或 ENOTSUP 时同样报错，不能确认断电持久化的 Flush 或删除恢复所需 WAL；create/drop 改名之后的目录 fsync 错误按第 17 项封锁写入。
    - **验证**：只注入进程崩溃发现不了缺失的 fsync。用第 20 项的钩子记录创建、sync、rename、unlink 的顺序，断言提交与删除 WAL 之前，新文件与新目录项（从数据目录根的上级开始）都已 sync；在第 7 项的 Linux 容器里再用 strace 核对一遍。
20. **崩溃与 I/O 故障的注入机制。** **建议**：
    - 发布代码中不含注入点（沿用 0005 的做法），改用默认关闭的 cargo feature，在各持久化点放置手写的钩子。钩子同时记录文件系统操作的顺序（第 19 项）。
    - 引擎级的崩溃用例由 Rust 测试在子进程中触发；跨实现用例由 pytest 驱动启用了该 feature 的测试程序写出目录，再交给 3.x 读取。
    - ENOSPC 在 macOS 上用小容量的磁盘镜像构造。

### 五、工程

21. **写入路径的并发与 flush 的加锁方式。** 已由 [0007](0007-m2-storage-engine.md) 决定：写锁内冻结、锁外写 Parquet 与提交 manifest、锁内发布；读者取快照后放锁计算。数据库写入闸门隔离 create/drop 的目录改名与其他写入、首次懒打开，目录 fsync 故障时封锁写入直到重开。取锁顺序、失败处理、原型测量与 0005 遗留项的结论均以 0007 为准；验收仍须测持续写入时检索的 p99。
22. **flush 的参数。** **建议与 3.x 完全相同**：
    - 阈值 10000，按 memtable 中的存活主键加 tombstone 计（删除 memtable 中的主键不改变大小），每次调用结束后检查；
    - 每个分区每次 flush 一个文件。
    - 理由：带 limit 的过滤与空 expr 查询的结果顺序绑定在物理布局上，改参数就要登记差异。0001 约束 6 允许调整，但目前没有需要调整的证据。
23. **物理行序。** 选项：memtable 的行序按 Python dict 的语义与 3.x 对齐；或者沿用 M1 的追加方式，登记带 limit 结果顺序的差异。
    - **建议对齐**：逻辑上的行序与 3.x 相同，flush 按这个顺序写文件：
      - 新主键排到末尾；
      - 覆盖写（insert 或 upsert）只有在旧版本还在当前活跃的 memtable 中时才保持原来的位置；旧版本在 segment 中，或在正在 flush 的冻结 memtable 中时，新版本排到活跃 memtable 的末尾（实测：upsert 已 flush 的 2，得到 `[1, 4, 2, 5]`）；
      - 删除之后再插入，排到末尾；
      - 同一批中重复的主键，位置取第一次出现的，内容取最后一次的。
    - 读路径的全局顺序：segment（严格按 manifest 中 `data_files` 的列表顺序，新文件追加在末尾）→ 冻结的 memtable（按代的先后）→ 活跃的 memtable。单分区、没有 compaction、至多一代未提交时，flush、重开与回放都不改变读法的顺序；多代 WAL 回放的合并规则见 [0007「打开」](0007-m2-storage-engine.md#打开)。
    - 理由：带 limit 或 offset 的过滤、空 expr 的查询，以及检索中并列结果之间的先后，都取决于这个顺序。
    - 检索中距离并列、且落在截断位置上时取哪几行，无法与 3.x 对齐：3.x 每个源各取前 limit + offset 个，用的是 numpy 的 `argpartition`，取的个数不少于 4 时近似任意（实测：100 行向量相同、limit 5，3.x 返回 66–70，本实现返回 0–4）。逐位复刻要放弃流式 TopK 与按行块并行，不做，登记差异；差分测试沿用对截断位置并列的容忍（0004「决策」）。
    - 列式 memtable、位次与有效位图的机制见 [0007「数据结构」](0007-m2-storage-engine.md#数据结构)。
24. **arrow-rs 依赖。** **建议采纳，版本约束写为 `~60.0`**。升级时重跑 0002 的互通矩阵。macOS 上链接系统自带的 CoreFoundation 框架，不属于需要安装的系统库，在 AGENTS.md「技术栈」中注明。
25. **用例复用规则。** **建议扩展 0004「验收」第 2 项**：
    - `tests/compatibility` 也允许逐条复制，现在没有任何一个文件能整文件进入范围；
    - 重启类用例允许替换起停服务的 helper，替换后须先在 3.x 上通过；
    - 「改后复制」中把 HNSW 换成 FLAT 的前提加上「用例中不 flush、不重开」：close 时会 flush，之后 3.x 会为 segment 建 HNSW。
    - 删除用例若只断言已知的有限主键集合中哪些行仍存活，可把上游的 `<pk> >= N` 过滤读回改为 `get(ids=[...])`，但列出的 ids 必须覆盖该过滤原本可能返回的全部主键；不以过滤结果的行序、分页或过滤语义本身为断言对象。替换后须先在 3.x 上通过。此规则只适用于这种读回断言，不授权任意更换查询方式；过滤表达式仍在 S1 单独验收。

26. **JSON 的落盘文本。** M1 的 JSON 字段存客户端发来的原始字节，`$meta` 经 `pyjson` 按 `json.dumps` 输出；3.x 两者都经 `json.dumps`。**建议**（负责人提出）：
    - 两者都按 3.x 的 `json.dumps` 形式写出（[0002「字段编码要点」](0002-data-dir-compat.md#字段编码要点)）：
      - JSON 字段的 dict 与 list 为 `ensure_ascii=False`、默认分隔符、不排序；
      - `$meta` 为 `sort_keys=True`、`ensure_ascii=True`；
      - 顶层标量按 0004「决策」写出，3.x 自身写不出或读不回这些值。
    - 开启 serde_json 的 `preserve_order` 与 `arbitrary_precision`，并相应修改 `pyjson`：
      - **键序**：工作区现在只开了 `float_roundtrip`（[Cargo.toml](../../Cargo.toml)），Map 按 key 排序。这对 `$meta` 恰好正确，但 JSON 字段要保留客户端的键序（[collection.py:2103](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L2103) 不排序），没有 `preserve_order` 就做不到字节一致。开启之后，`$meta` 改由 `pyjson` 显式地递归排序（`sort_keys` 对嵌套的对象同样生效）。
      - **数字**：`arbitrary_precision` 只是把数字保留为原文，`pyjson` 现在按 `as_i64 / as_u64 / as_f64` 三选一输出（[pyjson.rs](../../crates/core/src/pyjson.rs)），超出范围的整数仍会落进浮点分支。规则改为：
        - 整数按 Python `int` 的十进制输出，与原文相同，只有 `-0` 输出为 `0`；
        - 浮点经 f64 按 Python `repr` 输出，例如 `1E2` 输出为 `100.0`。
      - 不开 `arbitrary_precision` 的话，超出 i64 / u64 范围的整数会被改写成浮点，违背 0004「原样读回」。
      - 替代方案：不建 Value，直接对原始文本做流式重写。工作量更大，由设计说明选择。
      - 两个 feature 都对整个依赖图生效。开启后要核对 M1 中 serde_json 的全部用法（search_params、索引参数、`$meta`、`pyjson`、`pyint` 等），确认对外行为不变，包括键序改变的影响。
    - 这样跨实现测试中 JSON 列的「逐值一致」可以简化为字节比较。

## 范围

### 支持

- **持久化**：
  - 3.x 格式的 WAL、memtable、flush、data 与 delta Parquet、manifest、`schema.json`、`aliases.json`；
  - 正常关闭时 flush 并删除 WAL（0003「进程退出」）；
  - 崩溃后按 WAL 恢复；
  - 孤儿清理与 3.x 规则一致，并排除 snapshot 钉住的文件；
  - 第 8–12 项的失败语义，以及第 19 项的保证。
- **打开已有数据目录**：3.x 写的与本实现写的都能打开；collection 懒打开，不支持的内容按第 14–18 项处理。
- **删除**：
  - `delete(ids=...)` 由完整解析识别主键快路径，`delete(filter=...)` 支持 S1；
  - 返回值、分区语义与 `_seq` 的消耗与 3.x 相同，包括快路径中主键类型转换失败时也消耗一个 `_seq`；
  - 快路径中主键类型不符或溢出时，与 3.x 一样报 code 1、error_code 0，错误原文照搬 pyarrow 与 CPython 的三种文案（M1 的 `pyint` 已有复现 Python 报错原文的先例）。
- **upsert**：按第 2 项。
- **有效位图**：
  - 在写入、upsert、删除、flush 与打开时增量维护，查询路径不做 O(总行数) 的逐行判定；
  - 删除判定按 0002 约束 13；
  - 打开时对全部 segment 按主键取最大 `_seq`，因为 upsert 与重新插入不写 tombstone。
- **物理行序**：按第 23 项。
- **过滤 S1**：按第 1 项的边界，作用于 Query（含 limit / offset 与 `count(*)`）、Search 与 Delete。
- **RPC**：
  - Delete、Upsert、Flush、GetFlushState；ManualCompaction 与 GetCompactionState 在 M2 历史范围按第 3 项返回 UNIMPLEMENTED；后续首发扩展按 0010 接通；
  - 持久化之后 `row_count`、`count(*)`、`DescribeIndex` 行数与 load 状态的语义与 3.x 相同。
- **0004 交给 M2 的事项**：
  - `metric_type` 与索引度量不一致时，flush 过的数据按请求的度量计算（0004「决策」）；
  - 读取 3.x 目录时容忍 3.x 写出的畸形值，JSON 字段的顶层标量能互通（0004「决策」）；
  - JSON 字段与 `$meta` 的落盘文本按第 26 项；
  - 第 18 项的大小写冲突。

### 不支持（沿用 0004，另加）

- 除 S1 和后续 [0009 顶层动态字段](0009-top-level-dynamic-filter.md)之外的过滤，包括嵌套 JSON、数组、`or`、`not`，以及其范围外的 `in`；M2 原始范围仍按第 1 项记录；
- query iterator；
- compaction 与 tombstone GC，以及 compaction 相关的 RPC；
- `field_ops`；
- CLI。

## 步骤

本节列完整 M2 的实施步骤；每个切片的实际进度以 [README](../../README.md) 与相应测试为准，不把下面的计划当作已完成的验收。

1. **设计说明与存储读写**：
   - [0007](0007-m2-storage-engine.md) 已按规则 3 确认 memtable、segment、有效位图、锁序、提交与故障处理，并收敛 0005 的遗留项。
   - 按该设计实现：
     - arrow-rs 依赖、WAL、memtable、flush 及其提交语义；
     - data 与 delta 的读写，包括跨 segment 按主键去重与删除判定；
     - manifest、`schema.json`、`aliases.json`；
     - 打开已有目录的顺序与 fail-closed，以及 WAL 回放（第 8 项 a、b）；
     - 正常关闭、drop 与 create；
     - Flush 类 RPC 与 load 状态。
   - 差分测试加入重开场景。
2. **删除与 upsert**：
   - Delete 的快路径、upsert 的合并、有效位图在各路径上的维护；
   - 差分测试与不需要过滤的上游用例。
3. **崩溃、故障与跨实现矩阵**：
   - 按持久化点注入崩溃（第 20 项），覆盖写入、删除、upsert、flush、drop、create、CreateIndex 等写 manifest 的路径，以及 `aliases.json`；
   - 包括「有待提交的数据 → CreateIndex → 进程被杀 → 重开」，数据完整；
   - 文件系统操作顺序的断言（第 19 项）；
   - WAL 逐字节截断与中间损坏、ENOSPC、并发写入后崩溃；
   - 0002「验证」中 M2 范围内的跨实现条目，包括读取 3.x compaction 之后的目录，以及 pyarrow 15.0.0 下限。
4. **过滤 S1**：
   - 词法、解析、语义检查与错误原文，以及 S1 求值；
   - 仿照 M1 主键识别的做法，与 3.x 做差分模糊测试；
   - 复用需要过滤的上游用例。
5. **基准与验收**：
   - bench.py 允许本实现运行 build 与 reopen；
   - 服务端内存的辅助脚本；
   - 并发按 `m1` 阶段的口径；
   - 0001 的主体验证写进 0001。

## 与 3.x 的差异

[0004「决策」](0004-m1-scope.md#决策)已登记「auto_id 取该行的 `_seq`」：3.x 的 delete 消耗 `_seq` 而不消耗 auto_id，所以从一次 delete 到下次重开之间生成的自动主键与 3.x 不同，重开后两边又从同一个值继续。M2 引入 delete 后这条差异开始出现，差分测试的处理见「验收」第 1 项。

### 决策（长期保持）

| 差异 | 3.x | 本实现 | 理由 | 影响的用例 |
| --- | --- | --- | --- | --- |
| 已登记句柄的 collection 目录或 schema 被外部删除 / 移走，再显式 drop | schema 不存在时直接返回，保留旧内存句柄；仅重建同名 collection 才替换，期间直接访问可能仍返回旧行 | 同步仍持有的原 WAL File 成功后撤销旧句柄；旧快照有效，后续按名字访问报不存在，同名 create 使用空的新实例；同步失败保留句柄并报错 | drop 的生命周期必须包含已登记句柄，不能因目录早已不存在就留下可写旧实例；不改变正常目录提交协议，见 [0007](0007-m2-storage-engine.md#dropcreate-与关闭) | 已打开、目录删除 / 移走、drop、同名重建、重开；两侧在 drop 到 create 之间的结果分别断言 |
| 过滤表达式与错误 repr 的 Unicode 分类、递归边界 | 分类随运行 Python 的 Unicode 版本变化；嵌套限制与错误尾句取决于 Python 调用栈 | 词法与错误 repr 固定 CPython 3.12 / Unicode 15.0 分类；解析嵌套计数上限 128，超出报 code 1 `maximum recursion depth exceeded`；平面长链使用 arena，不按链长递归析构 | 固定可复核的词法真值并防止服务输入导致 Rust 栈溢出，不绑定 Python；见 [0007](0007-m2-storage-engine.md#s1-过滤的当前实现与验收范围) | 非 ASCII 数字 / 字段名、深括号 / unary / 函数嵌套；Unicode 与语法语料可重生成核验 |
| Search 过滤后没有候选时的非法维度 / 度量 | 不进入距离计算，可能返回空成功结果 | 仍返回 code 1，检查查询向量维度及度量枚举；过滤的语法与类型错误先报告 | 沿用 [0004](0004-m1-scope.md#决策) 对查询向量与请求参数的严格校验，不让相同非法请求的结果取决于是否有命中；格式未变 | [test_filter_s1.py](../../tests/differential/test_filter_s1.py) 的手写 Search 请求分别固定两侧结果；topk 为零时仍编译表达式，但不触发数据求值的数值错误 |
| 提交之后、删除 WAL 之前崩溃 | 重开后数据重复：行数计两次，get 返回旧版本而 query 返回新版本，upsert 与旧行合并，带 limit 的顺序不同 | 不重复：回放时跳过已被已提交状态覆盖的操作 | 缺陷 a | 该崩溃点之后的读取 |
| 提交点之后失败（例如目录 fsync 失败） | 忽略目录 fsync 的错误，照常删除 WAL | 保留这一代的 WAL，之后的写入先重试完成提交，失败则拒绝；重试一直失败时，3.x 打开会重复计数 | 第 8 项 | 目录 fsync 失败 |
| manifest 已提交后的 WAL 清理失败 | flush 报错；触发它的写入报 code 1，已提交的数据在重开前不可见 | 显式 Flush 报错，触发自动 flush 的写入仍成功且数据可见；后续写入不因清理失败而受阻，下一次提交、显式 Flush 或关闭重试清理 | [0007「提交之后」](0007-m2-storage-engine.md#flush)；写入已越过提交点 | WAL 文件 unlink 或 `wal/` 同步失败；关闭时仍有残留 WAL，3.x 打开可能重复计数 |
| Parquet 或目录 fsync 失败 | 不 fsync Parquet，目录 fsync 的错误被忽略，照常成功 | 按第 8、9 项进入待完成状态，显式 Flush 返回错误 | 第 19 项，规则 5 | fsync 失败的文件系统或磁盘 |
| flush 在提交点之前失败 | 数据不可见，下一次 flush 后永久丢失；触发它的写入报 code 1，之后的写入照常接受 | 数据保持可见，WAL 保留；触发它的写入返回成功，之后的写入先重试提交，失败则拒绝 | 缺陷 b，第 9 项 | 磁盘写满等 I/O 故障 |
| WAL 追加失败 | 继续往同一个 WAL 追加，恢复时可能回放垃圾数据 | 这一代立即冻结，之后使用新的 WAL | 第 10 项 | 磁盘写满等 I/O 故障 |
| flush 期间的读 | 可能看到行消失 | 不会 | 缺陷 c | flush 与读并发 |
| 并发首次打开、并发写入 | 两个实例；WAL 损坏，崩溃后丢数据 | 互斥 | 缺陷 d、e | 多线程客户端 |
| 读取含重复 (pk, `_seq`) 或重复路径的目录 | 计两次 | 计一次 | 缺陷 a 留下的数据 | 3.x 在缺陷 a 的崩溃点中断过的目录 |
| manifest 解析失败、`manifest.json` 缺失而 `.prev` 存在、引用的文件缺失、Parquet 损坏、snapshot 元数据无法解析 | 回退 `.prev` 并删文件；读 `.prev`；静默少数据；每个 RPC 报 code 1；静默跳过这个 snapshot | 该 collection fail-closed，不删文件；DescribeCollection 与 drop 照常 | 规则 5，第 11 项 | 损坏的目录 |
| `schema.json` 的 JSON 语法错误、根节点不是对象或顶层必需字段缺失（不含下述未知键/dtype 判定） | `SchemaValidationError`，引擎分支 RPC 返回 code 6 / error_code 6，并带 schema 校验诊断 | 按磁盘格式错误返回 code 1 / error_code 0，信息带 `storage error:`；该 collection fail-closed，不改文件 | 区分用户建表参数校验与已有目录损坏，保留既有恢复错误分类 | [test_reopen.py](../../tests/differential/test_reopen.py) 的损坏 schema 首次访问与缓存断言；两侧分别固定状态码 |
| WAL 读取遇到真实 I/O 错误 | 忽略，保留前缀 | 该 collection 打开失败，下次访问重试 | 规则 5 | I/O 故障 |
| 首次打开失败、尚未登记句柄，且确定性 fail-closed 判定已缓存之后，外部工具改变同名目录 | 修复后再次尝试打开；删除或移走目录后按 collection 不存在处理 | 先检查精确名字与目录/schema 是否仍存在，再读取缓存；删除或移走后按不存在处理，缓存保留。路径重新出现或原地修复后仍返回原错误类型与信息；重建服务实例或同名 drop/create 清除判定。真实 I/O 与清理扫描故障不缓存 | 缓存不能掩盖不存在的 collection；也不因任意外部替换就重新信任同名目录，避免每个 RPC 重复解析无法安全处理的内容，[0007「打开」](0007-m2-storage-engine.md#打开) | 修复 schema/manifest/snapshot、补回引用文件、删除或移走后再次访问；缓存、重试、移回路径及同名重建用例固定边界 |
| 孤儿清理路径的目录形态异常 | `os.path.isdir` 跳过非目录的 data/delta/indexes 路径，跟随目录符号链接；清理可能删除链接目标中的库外文件 | `partitions/`、`partitions/_default/` 与 data/delta/indexes 必须是真目录；符号链接或其他非目录形态使该 collection fail-closed，删除前即拒绝打开 | 防止孤儿清理穿透托管目录；这会使本仓加入清理前可读取的软链目录不再可用，[0007「打开」](0007-m2-storage-engine.md#打开) | 存量目录将分区目录或扫描目录软链到别处，或扫描路径被普通文件占据；相关目录形态反证测试固定拒绝行为 |
| 损坏的 WAL 在哪里判定出错 | 按 pyarrow 的校验 | 按 arrow-rs 的校验，可能不同；本实现从较短的前缀续号时，3.x 之后可能看到两条操作共用一个 `_seq` | 两边的校验项不同 | 损坏的目录 |
| manifest、schema 或 snapshot 元数据中有未知的键 | 能打开，重写时丢掉这些键 | 该 collection fail-closed，返回 UNIMPLEMENTED。`schema.json` 中有未知的键时，DescribeCollection 也报错，`has_collection` 抛异常；只有 manifest 或 snapshot 元数据中有时，DescribeCollection 照常 | 第 14 项 | 3.x 新版本写出的目录 |
| `ShowCollections` 时有 collection 打不开 | 整个调用失败 | 其余照常列出 | 第 16 项 | 含损坏或不支持内容的目录 |
| drop 时 flush 失败 | drop 报错 | drop 不 flush，照常完成 | 第 17 项 | 磁盘写满时的 drop |
| drop 与 create 中途崩溃 | drop 留下半个目录，collection 可能仍可见但数据不全；create 留下没有 schema 的目录，这个名字永久不能再用 | 两者都是原子的 | 第 17 项 | drop、create 时崩溃 |
| create/drop 改名后目录 fsync 失败 | 3.x 不执行这两处目录 fsync | 返回「目录已变更、持久化未确认」；本服务实例停止所有 collection 的写入、Flush、CreateIndex、create/drop 与可能清理孤儿文件的首次打开，直到重开；已打开 collection 的纯读可继续 | 未确认的目录改名不能靠原样重试 fsync 证明持久化，[0007](0007-m2-storage-engine.md) | 该故障后的无关 collection 写入及首次 Query/Search/Get |
| create 遇到指向不存在目标的旧别名 | 保留旧别名；目标同名重建后旧别名重新指向新 collection | create 改名前删除悬空别名，后续别名写入也过滤悬空目标 | 避免旧别名静默接到新 collection，见 [0007](0007-m2-storage-engine.md#dropcreate-与关闭) | 3.x 写出 `aliases.json` 后目标消失，再同名建表并由 3.x 读取别名；Rust 别名 RPC 仍不支持 |
| staging 根路径是目录符号链接 | 跟随链接并清理目标中的残留，可能删除库外文件 | 拒绝打开数据库，不删除链接目标中的文件 | 防止启动期清理穿透托管目录；普通文件占据根路径时两边均拒绝打开 | `.database-staging/` 被软链到库外；互通用例固定两边的打开与删除行为 |
| staging 单条目的类型读取失败 | `isdir/islink` 把 stat 错误转换为否；后续单条目删除错误被忽略，不阻断打开 | 完整判定期间 `DirEntry::file_type` 报错则拒绝打开，尚未删除任何残留 | 类型不明时保留全部残留，维持先判定后删除；整目录扫描失败两边均向上传递 | 文件系统返回条目类型读取错误；不将外程删除条目必然视为这种错误 |
| drop 目录已移走而 `aliases.json` 写入失败 | 报错，旧别名可能悬空 | 报错并说明目录已移走；同名 drop 重试、同名 create 或后续写 `aliases.json` 时清理悬空别名；仅重开不清理 | 规则 5、[0007](0007-m2-storage-engine.md) | drop 后立即重试或同名 create |
| 只差大小写或 Unicode 规范化不同的 collection 名 | 另开一个实例指向同一目录；drop 会删掉真实的 collection；HFS+ 上非 ASCII 的名字按路径能找到 | 访问与 drop 都按「collection 不存在」处理，新建时报「已存在」 | 第 18 项 | 不区分大小写或做规范化的文件系统 |
| INVERTED 定义的字段上的过滤 | 结果取决于数据在 segment 还是 memtable，以及后台索引是否建好 | 按扫描求值 | 3.x 自身不一致 | 3.x 写的、带 INVERTED 定义的目录 |
| S1 求值的数值转换与字面量范围检查（含整数列对浮点字面量、数据超出 2^53） | 物理 segment 中已删除或已被覆盖的旧版本仍参与求值，可能触发 code 1；没有存活 MemTable 行时不调用 Arrow | 只对有存活行的块求值；空块不触发求值阶段的转换或字面量范围错误，语法与 schema 检查仍执行 | 3.x 的结果随 compaction 时机变化，不能让不存在的候选行决定结果；沿用第 13 项与 0007 的有效位边界 | 主键或标量超出 2^53、FLOAT 与整数字面量超过 2^24、字面量超出 int64；[test_filter_s1.py](../../tests/differential/test_filter_s1.py) 对空 MemTable 与全失效 segment 分别固定两侧结果 |
| `field_ops` 非空的 upsert | 静默按替换处理 | UNIMPLEMENTED | 不变量 15 | 使用 `field_ops` 的 upsert |
| JSON 字段的浮点形式数字超出有限 float64 范围（raw gRPC，如 `{"a":1e999}`） | 接受写入；raw Query 可返回 code 0 和含 `Infinity` 的非标准 JSON，pymilvus 解码失败 | 在 JSON 归一化时返回 code 1、明确的有限 float64 范围错误，不推进序号或追加 WAL | 不复制参照实现接受写入但无法由客户端读回的行为；宽整数和字符串不受此限制，`pyjson` 的兼容输出规则不改 | [test_reopen.py](../../tests/differential/test_reopen.py) 的 raw 插入拒绝、两侧响应与有限输入重试用例；正常值在写前、发布后、重开后的字节一致 |
| 同一批内重复主键的部分 upsert | 每行都与调用前的旧行合并，后一行会丢掉前一行新增但自己未提供的字段 | 每行与该主键在本批中的上一版本合并，后一行保留前一行新增的字段 | [0007「写入」](0007-m2-storage-engine.md#写入) 的批内顺序合同；避免同一次原子写入丢字段 | 同批重复主键且各行提供不同字段；`tests/differential/test_scenarios.py` 固定两边结果 |
| 已提交删除覆盖旧行时的部分 upsert（待 Upsert 接入后验收） | 3.x 在缺陷 a 留下的重复目录上可能仍读旧行并合并字段 | 只合并仍有效的最新行，不读已提交删除覆盖的旧版本 | 缺陷 a；[0007「写入」](0007-m2-storage-engine.md#写入) | 3.x 崩溃后带重复 WAL 的目录 |
| 检索中距离并列且落在截断位置时，取哪几行 | 由 numpy `argpartition` 决定，每个源取的个数不少于 4 时近似任意 | 取全局顺序中位置最先的 | 第 23 项；复刻要放弃流式 TopK 与并行。M1 起即如此 | 距离大量相同的检索 |

### 阶段性差异（随后续里程碑注销）

后续顶层动态过滤沿用仅对原有效位求值的边界：失效的 segment 旧行不参与动态类型推断、转换或 in。3.x 仍可能因失效物理行的宽整数/异构值/数组对象选择不同 backend 或报错；本实现避免让不可见旧版本控制当前读取，理由沿用第 13 项。影响用例为删除/覆盖后仍保留旧 meta 的 segment，验证见 [0009](0009-top-level-dynamic-filter.md) 的组件有效位与生命周期用例；新切片不改变纯 S1 的既有差异或存储策略。

| 差异 | 3.x | M2 | 理由 | 影响的用例 |
| --- | --- | --- | --- | --- |
| 含不支持内容的 collection | 能使用 | 列出；DescribeCollection 与 drop 照常，其余返回 UNIMPLEMENTED | 第 14 项 | 用 3.x 的其他功能写过的目录 |
| 已有的别名与其他数据库 | 能使用 | UNIMPLEMENTED | 第 15 项 | 经别名或 `db_name` 的访问 |
| 带 limit 的过滤结果顺序 | 后台 compaction 之后改变 | M2 历史不合并；首发扩展按 [0010](0010-compaction-and-tombstone-gc.md) 的来源序/差异验收 | 第 3 项及后续 0010 | 带 limit 且文件数不少于 4 的过滤 |
| ManualCompaction 与 GetCompactionState | 返回成功，恒为 Completed | M2 历史 UNIMPLEMENTED；首发扩展成功应答/Completed，不能当成 drain | 第 3 项、0010，不变量 15 | `client.compact()`、`optimize()` |
| 被覆盖的旧版本与已删除的行 | compaction 回收 | M2 历史不回收；首发按 0010/0011 合并与 GC | 第 3 项、0010/0011 | 长期运行的写入 |

## 验收

1. **差分测试**：tests/differential 加入重开、删除、upsert、S1 过滤场景，在 pymilvus 2.6.17 与 3.0.2 下各跑一遍。规则如下：
   - **auto_id**：
     - 按插入位置建立两边主键的对应关系，之后的 get、delete、upsert 各用自己一侧的主键；
     - 重开后两边生成的主键须相同，这一条用来检出 `_seq` 消耗的差异。
   - **索引**：重开类场景显式使用 FLAT。close 时会 flush，之后 3.x 会把 AUTOINDEX 的 segment 建成 HNSW。
   - **专门场景**：
     - 度量不一致的检索落在 flush 过的数据上；
     - 快速建表并重启后，load 状态为 NotLoad；
     - 覆盖写、删除后再插入之后，带 limit 的查询顺序。
   - **compaction**：比较带 limit 的结果顺序时，控制 3.x 一侧的文件数少于 4，避免它在后台 compaction。
2. **复用上游用例**：按第 25 项扩展后的规则复制，INVENTORY.md 逐条更新状态；复用数以当时的实际统计为准。
3. **跨实现互通**：0002「验证」中能在本机执行、且在 M2 范围内的条目全部通过，包括：
   - 双向读写、交替写入、崩溃后由另一方恢复；
   - auto_id 中不涉及 partition key 的两种场景；
   - 共享值域、fail-closed、读取 3.x compaction 之后的目录；
   - JSON 列与 `$meta` 与 3.x 写出的字节相同（第 26 项）；含不支持内容的 collection，DescribeCollection 的结果与 3.x 相同（第 14 项）；
   - pyarrow 15.0.0 下限；
   - 3.x 读取本实现在故障后留下的目录：多代 WAL 与同名孤儿、尾部半个 batch 的 WAL、staging 中的残留；
   - 本实现读取 3.x 在缺陷 a 的崩溃点中断过的目录，以及「manifest 引用的 Parquet 为空、旧 WAL 完好」的目录（fail-closed，不删除任何文件）；
   - 提交点之后失败 → 正常关闭 → 3.x 打开，行数不变；提交点之后失败 → 不再写入 → 正常关闭 → 3.x 打开，同样不变；
   - 打开时全部跳过（WAL 都已提交）→ 只读 → 正常关闭 → 3.x 打开，行数不变；
   - 别名与其他数据库（第 15 项）、只差大小写与 HFS+ 上的名字（第 18 项）各有对应的用例。

   Linux 上的 LOCK 互斥与 strace 核对按第 7 项在本机容器中执行；Windows 留到首次发布前。pyarrow 15.0.0 需要单独的 venv（numpy 须为 1.26.x），由基准或测试的准备脚本建出。
4. **崩溃与故障**：
   - **参考模型**：每次 RPC 是原子的。
     - 已返回成功的写入，重开后必须存在；
     - 返回错误的写入，重开后不存在；
     - 没有返回的写入，要么整条存在，要么整条不存在。
     - create/drop 改名之后的目录 fsync 错误，以及实现缺陷触发的锁内 panic，按 [0007](0007-m2-storage-engine.md) 的提交点与故障语义单列；这两类错误不能套用上述写入 RPC 的断言。
   - **注入崩溃**：第 3 步列出全部持久化点，逐个注入崩溃后重开，count、query、search、get 四条读路径都与参考模型一致，文件集合正确。
   - **故障**：
     - WAL 逐字节截断与中间损坏（按前缀恢复）、中间为零的 WAL、ENOSPC、并发写入后崩溃；
     - 向 WAL 与 Parquet 的读取路径注入 panic，断言它变成格式错误（第 12 项）。`catch_unwind` 依赖 unwind 语义，将来若为缩小 wheel 体积设了 `panic = "abort"`，这条用例会失败，比在文档里写「不得设 abort」更可靠；
     - 文件系统操作顺序的断言（第 19 项）。
   - **0003**：与持久化有关的用例，包括停止失败的故障注入。
5. **有效位图的模型测试**：随机的写入、upsert、删除、flush、重开、崩溃序列，与逐行重算的结果一致，包括物理行序。
6. **并发**：写入、删除、flush 与检索并发的压力测试；并发首次打开同一个 collection。
7. **基准**：
   - 0001 第 7 项的全部目标与约束 5（内存），加上持续写入时检索的 p99；
   - 同机对照 2.5.1 与 3.x，3 轮取中位数，结果写进 0001；
   - 按第 6 项，不设硬门槛。
8. **构建与静态检查**：同 0004 第 5 项，依赖图核对包括 arrow-rs，以及 serde_json 两个新 feature 带来的依赖（`preserve_order` 引入 indexmap）。

## 配套修改

分两类：采纳时修改的，是规则、决定与事实更正；描述实现之后才成立的行为的，随对应步骤的实现修改，避免文档先于代码（规则 10）。

### 采纳时修改

- **0002**：
  - 目录布局：注明 `.database-staging/` 也用于本实现的 drop 与 create。
  - 约束 5 改正原文：drop_collection 与 truncate 在 3.x 中是整目录删除，只有 drop_partition 逐个删除未被钉住的文件；并补充：按类型删除 `.idx` 的时机还包括 collection 打开时的后台任务。
  - 约束 11 的 fail-closed 改为按 collection 生效，并加入未知的键（第 14 项）。
  - 修订「结论」中关于 INVERTED「结果相同」的一句。
  - 「验证」中「fail-closed……拒绝打开」改为按 collection。
  - 约束 10 中「关闭时只清空」`.database-staging/` 改正为打开时清空（[db.py:102](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/db.py#L102)）。
  - 「继承的 3.x 已知风险」加入第 8、10、12 项列出的各条：缺陷 a、e；已提交的 WAL 残留时，3.x 的 compaction 与 GC 抹掉删除的证据、已删除的行复活；提交点之后的失败重试一直不成功时，3.x 打开会重复计数；WAL 追加失败后继续追加；WAL 中间损坏时其后已确认的写入静默丢失。
- **0003**：
  - 「引入 WAL 后启动包括恢复……届时再改为按路径加锁」改为按第 16 项懒打开；
  - 「fsync 的时机……留到 WAL 的设计里决定」改为链接第 19 项。
- **0004**：
  - 「验收」第 3 项与「跨平台」中的「在 M2 发布前」，按第 4 项改为「在首次发布前」；
  - 「验收」第 2 项注明 M2 起的复用规则见第 25 项；
  - 「决策」中 collection 名区分大小写一条，注明 M2 的处理见第 18 项。
- **0001**：旧并发表下的说明注明 M2 用 bench.py 的口径重测（第 5 项）。
- **AGENTS.md**：
  - 「当前状态」与「知识索引」加入本文；
  - 「WAL 的 fsync 时机在 WAL 设计中确定」改为链接本文第 19 项；
  - 规则 3 中不变量 7 的说明补一句：3.x 代码已有 collection 级的锁。

### 随对应步骤的实现修改

- **0003**（第 1 步）：
  - 「同路径复用」中「尚不支持读取某类已有数据时返回 `None`」与「验证」表中对应的一行，改为按第 14–16 项处理；
  - `.database-staging/` 的残留改为清空。
- **0004**：
  - 「决策」中以「本实现不解析过滤表达式」为例的说明，随 S1 的完整解析而过时，相应修改（第 4 步）；
  - 「不支持」中的 Delete、Upsert 与 flush 相关 RPC 随实现注销（第 1、2 步）；「过滤」一条改为按第 1 项的 S1，并写明 query iterator 仍不支持（第 4 步）；compaction 相关 RPC 保留在「不支持」中；
  - 「阶段性差异」中「数据只在内存中」「打开已有数据的目录」「写入磁盘的内容」三条随第 1 步注销。
- **0005**：留给 M2 的几项在第 1 步的设计说明定下之后，改为链接它。
- **基准 README**（第 5 步）：concurrency.py 不再使用；新增服务端内存的辅助脚本。
- **AGENTS.md**：
  - 架构地图、技术栈（arrow-rs 与 CoreFoundation 的说明）、常用命令，随引入的 module、依赖与命令更新；
  - 「技术栈」中 serde_json 一条：「它的 Map 按 key 排序，与 3.x 存 `$meta` 时的 `sort_keys=True` 一致」，在开启 `preserve_order` 与 `arbitrary_precision` 时改为第 26 项的做法。

## 首发合并扩展的范围说明

M2 的“不做 compaction”是当时冻结范围，后续首发扩展以 [0010](0010-compaction-and-tombstone-gc.md) 与 [0011](0011-committed-delete-state.md) 为准。正常关闭排空的生命周期同步到 0003；共享格式与原有失败协议保持。

该扩展的差异表在 0010：单 Db 串行维护、文件大小桶的编码器影响、准备期并发 flush、create/drop 取消及异常收尾、原子合并 data/delta 提交、打开时 pins 的边界，以及歧义重复主键/序号的整轮拒绝。原保守最大删除表的 20% 残留已触发负责人确认的 0011；最终回收与内存结论以本轮复跑证据为准。
