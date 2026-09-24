# 0002 数据目录格式：数据文件与 3.x 双向互通，索引文件各自独立

- 状态：已采纳
- 日期：2026-09-24
- 对照版本：milvus-lite 3.2.1（tag `v3.2.1`，`43d1257`）
- 前置决策：[0001 重构基线选择](0001-refactor-baseline.md)（架构沿用 3.x）

## 背景

[0001](0001-refactor-baseline.md) 确定沿用 3.x 的存储架构：WAL + memtable + 不可变 Parquet segment + delta + manifest。在此基础上，还要决定 Rust 实现的磁盘格式是否与 3.x 互通。

这个决定影响两件事：

1. **存量数据能否直接打开**：用户手里已有 3.x 生成的数据目录（`MilvusClient("./demo.db")` 中的 `demo.db` 在 3.x 下是一个目录）。换装 Rust 实现后，代码不用改，这个目录也应该能直接打开。
2. **能否在同一目录上做差分测试**：3.x 写、Rust 读，以及反过来。这比只经过 gRPC 做黑盒对照更能暴露持久化与恢复路径上的差异。

## 结论

- **数据文件双向互通**：目录布局、`LOCK`、WAL、Parquet data / delta、manifest（含其中的索引定义 `index_specs`）、schema、别名与数据库元数据、snapshot 元数据，都与 3.x 保持同一格式与语义。3.x 写的目录 Rust 能打开，Rust 写的目录 3.x 也能打开，包括崩溃后遗留 WAL 的恢复。
- **索引文件不互通**：Rust 的向量索引与标量索引使用自己的格式和 3.x 不会识别的文件名。打开对方写的目录时，缺少自己格式的索引，就按 segment 从数据重建。
- **第一阶段只实现 FLAT 时**（见 [AGENTS.md「技术栈」](../../AGENTS.md#技术栈)），本节是这一阶段行为的唯一权威描述：
  - 下列分类都以**字段类型**为前提。3.x 建索引时不校验字段类型：同一个类型名放在错误的字段上，也会被持久化，只是 load 失败（见约束 1）。
  - **稠密向量字段上的 FLAT / BRUTE_FORCE**：原生支持。
  - **稠密向量字段上的 AUTOINDEX**：新建与存量都按 FLAT 精确执行，并登记为已知差异。AUTOINDEX 的含义本来就是「由实现选择」，pymilvus 的快速建表 `create_collection(name, dimension=...)` 固定使用它；3.x 在没有 faiss 时自己也退回暴力检索，装了 faiss 时（3.2.1 的必装依赖）建成 HNSW。
  - **稠密向量字段上的显式 ANN 类型**（HNSW、HNSW_SQ、IVF_FLAT、IVF_SQ8）与**标量字段上的 INVERTED**：新建时明确报错。打开 3.x 写的、带这类定义的目录时，**原样保留**定义，让 3.x 回来后还能照常使用。向量检索按 FLAT 精确执行，召回只高不低，但更慢；标量过滤按扫描执行，结果相同，但更慢。
  - **稀疏向量字段**：第一阶段在稀疏字段上新建任何索引都明确报错。存量的 SPARSE_INVERTED_INDEX 定义原样保留，稀疏向量检索在全文检索阶段之前明确报错。
  - 以上执行方式与 3.x 不同的地方，都登记为已知的对外差异（[AGENTS.md](../../AGENTS.md) 规则 2）。
  - **3.x 自身也无法 load 的索引定义**：包括 3.x 会原样持久化的 DISKANN、IVF_PQ，以及放错字段类型的定义，例如稀疏字段上的 AUTOINDEX / FLAT / HNSW（见约束 1）。处理与 3.x 一致：目录照常打开，只有该 collection 在 load 时报错。

互通的前提是**共享文件的语义也完全一致**，不只是格式能读。下面「共享状态的语义约束」中标注「实测」的条目，都已在 3.2.1 上复现：违反它们会造成数据丢失、已删除的行复活、主键重复覆盖，或检索结果静默出错。

## 互通范围

以 v3.2.1 的实现为准。字段级细节以 3.x 的代码与 [wal-design.md](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/docs/wal-design.md) / [modules.md](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/docs/modules.md) 为权威来源，本文只记录互通需要特别守住的部分。

| 对象 | 是否互通 | 说明 |
| --- | --- | --- |
| 目录布局 | 是 | 数据库级：`collections/`、`databases/<db>/collections/`，各数据库自己的 `aliases.json`、`database_properties.json`，以及 `.database-staging/`（3.x 每次启动时清空，见 [db.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/db.py#L7)）。collection 级：`schema.json`、`manifest.json`（首次 flush 之前可能不存在）、`manifest.json.prev`、`wal/wal_{data,delta}_NNNNNN.arrow`、`partitions/<p>/{data,delta,indexes}/`、`snapshots/metadata/<name>.json`、`snapshots/manifests/<name>/`，以及原子写入时的 `*.json.tmp` |
| `LOCK` | 是 | 同一个文件，互斥语义一致，见约束 10 |
| WAL | 是 | Arrow IPC stream。data 列为 `_seq`、`_partition`、用户字段、可选 `$meta`；delta 列为主键、`_seq`、`_partition`（见 [arrow_builder.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/schema/arrow_builder.py)）。batch 语义见约束 3 |
| Parquet data / delta | 是 | data 列为 `_seq(uint64)` + 用户字段 + 可选 `$meta(string)`；delta 列为主键 + `_seq`。文件名语义见约束 2，写入参数见约束 9 |
| 字段编码 | 是 | 见[字段编码要点](#字段编码要点) |
| manifest | 是 | `manifest_format_version = 2`，write-tmp + replace 原子更新并维护 `.prev`，其中的 `index_specs` 是共享的索引定义，见约束 1、7 |
| schema | 是 | `schema_format_version = 2`，含 functions（BM25 等）的定义 |
| snapshot 元数据 | 是 | `snapshot_format_version = 1`；snapshot 同时钉住 data、delta 与**索引**文件，见约束 6 |
| 向量索引 / 标量索引 | **否** | 3.x 为 `indexes/<data_stem>.<field>.<index_type>.idx` / `.sidx`；Rust 的命名与放置规则见约束 5 |
| Rust 独有的派生状态 | **否** | 只能放在 3.x 不会删除的位置（约束 5），并且必须能从数据文件重建 |

## 共享状态的语义约束

1. **共享文件只写 3.x 能接受的键和值。**
   - 3.x 读 manifest 时只读它认识的键，重写时会丢掉未知的键（顶层和嵌套都会丢）。
   - 更危险的是**值**：3.x 解析 manifest 时一旦出现 `KeyError`、`ValueError` 或 `TypeError`，就静默回退到 `manifest.json.prev`（[manifest.py:164](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/storage/manifest.py#L164)）。随后 recovery 会把只登记在新 manifest 里的数据文件当作孤儿删掉。实测：把 `index_specs` 的 metric 改成 3.x 不接受的 `HAMMING` 后，10 行数据变成 5 行，较新的 Parquet 文件被删除。
   - 注意「能解析」与「能使用」不同：3.x 解析 manifest 时并不校验 index_type（[spec.py:58](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/index/spec.py#L58) 只要求非空字符串），`create_index` 也只做大写化就持久化（[collection.py:1454](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L1454)）。实测：向量字段上的 `DISKANN` 被写进 manifest，3.x 重开照常，只在 load 该 collection 时报 `unknown index_type`。
   - 因此，Rust 写入共享文件的枚举值必须落在 3.x 能 build / load 的集合内，而且要**按字段类型**判断。index_type 的值域为：稠密向量字段上的 FLAT、BRUTE_FORCE、AUTOINDEX、HNSW、HNSW_SQ、IVF_FLAT、IVF_SQ8（出自 [factory.py:53](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/index/factory.py#L53) 的 `KNOWN_VECTOR_INDEX_TYPES`，去掉 3.x 自己也无法 build 的 `IVF_PQ`）；稀疏向量字段上只有 SPARSE_INVERTED_INDEX；标量字段上只有 INVERTED（[capabilities.py:6](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/index/scalar/capabilities.py#L6)）。实测：稀疏字段上的 AUTOINDEX / FLAT / HNSW 都会被 3.x 持久化，但 load 时报 `vector column must be FixedSizeList, got binary`。metric 限于 COSINE / L2 / IP / BM25 / NONE（[spec.py:60](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/index/spec.py#L60)）；dtype、function 类型同理。Rust 需要的额外状态只能放进独立的、可重建的文件。
   - `.prev` 的维护协议与 3.x 相同：写 `manifest.json.tmp` 并 fsync，把当前 `manifest.json` 复制为 `.prev`，replace 为 `manifest.json`（提交点），再 fsync 父目录（[manifest.py:86](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/storage/manifest.py#L86)）。
2. **文件名中的 seq 范围是 tombstone GC 的依据，必须覆盖文件内容。**
   - 3.x 用 data 文件名里 seq_min 的全局最小值（[compaction.py:483](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/compaction.py#L483)），判断 delta 文件名里 seq_max 更小的 delta 文件可以删除（[compaction.py:446](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/compaction.py#L446)）。
   - 所以 data 文件名的 seq_min 必须不大于内容中最小的 `_seq`，seq_max 必须不小于最大的 `_seq`；delta 文件的 seq_max 同样必须是上界。
   - 命名固定为 `data_{seq_min:06d}_{seq_max:06d}.parquet` / `delta_..._....parquet`。6 位是最小宽度，数值更大时可以更长。不符合这个格式的文件不参与 GC 计算。
   - 实测：把一个含已删除行的文件改名、使 seq_min 偏大后，compaction 提前回收了 tombstone，已删除的行复活。
3. **WAL delta 的一个 batch 对应一次 delete 调用：只含一个 `_seq`、一个 `_partition`。**
   - 3.x 回放时只读每个 batch 第 0 行的 seq 和 partition（[memtable.py:127](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/storage/memtable.py#L127)）。实测：把两次 delete 合并成一个 batch 后回放，第二次删除失效。
   - `_partition` 为 `_all` 表示跨分区删除，flush 时复制到每个分区的 delta。
   - data WAL 的一个 batch 可以混合多个分区。
4. **`_seq` 与 auto_id 同源，两个方向都要约束。**
   - 3.x 重新打开时，`_seq` 从 `max(manifest.current_seq, WAL 中最大 seq) + 1` 继续分配（[collection.py:235](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L235)），auto_id 也从这个值开始（[collection.py:293](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L293)）。
   - **下界**：Rust 分配的 `_seq` 与 auto_id 都不得小于这个值，否则会与已持久化的数据重复。
   - **上界（规范）**：逐行保证 auto_id ≤ 该行的 `_seq`。两者都按行消耗（[collection.py:389](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L389)），可以同步分配；一次调用拆成多个 batch 时（例如按 partition key 分桶），每一行照样满足。这样留在盘上的每一行都自带上界，不依赖 batch 的划分，也不依赖哪些行后来被删掉。
   - **`current_seq` 的前提**：flush 提交的 `manifest.current_seq` 必须不小于该 memtable 消耗过的最大 `_seq`，包括 delete，也包括没有写进文件的操作；与 3.x 相同，取 memtable 的 `max_seq`（[flush.py:126](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/flush.py#L126)），**不能**取写出文件中的最大 `_seq`。实测：把 `current_seq` 从 4 改成写出数据的最大 seq 2 之后，重开再写，原有的主键 3 被覆盖。
   - 违反以上任一条，对方重开后都会从较小的值重新分配 auto_id，重复的主键按 upsert 覆盖旧行。3.x 自己就有两处不满足逐行上界，均已实测：一是在校验之前就消耗 auto_id（[collection.py:361](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L361)），一次校验失败的 insert 之后，重开再写，原有的一行被覆盖（应为 7 行，实际 6 行）；二是 partition key 路径先按输入顺序分配 auto_id，再按桶分别分配 `_seq` 并逐桶落盘（[collection.py:421](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L421)），在两个桶之间崩溃后，重开再写会覆盖已落盘的行（应为 6 行，实际 5 行）。
5. **索引文件与 Rust 独有文件的命名和放置。**
   - 3.x 在同名 `.idx` 存在时直接加载（[segment.py:265](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/storage/segment.py#L265)）。实测：内容不合法时 load 抛异常（FLAT 为 NPY 的 `ValueError`，HNSW 为 FAISS 的 `RuntimeError`），collection 保持 released，不会回退重建，也不会删除坏文件。所以 Rust 文件不能使用 3.x 的命名。3.x 的命名为 `<data_stem>.<field>.<index_type 小写>.idx|.sidx`（[files.py:23](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/index/files.py#L23)）。
   - 3.x 会删除 `data/`、`delta/`、`.database-staging/` 下的任何陌生文件。在 `indexes/` 下，它只删除文件名能按上述格式解析的文件，并且还要满足以下之一：stem 已不在 manifest 与 snapshot 中（recovery）；stem 已不在当前 manifest 中，或 field / type 与当前索引定义不符，并且没有被 snapshot 钉住（flush，[collection.py:1985](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L1985)）；后缀与被 drop 的索引匹配（drop_index）。
   - 为保守起见，Rust 的索引文件放在 `indexes/`，扩展名不以 `.idx` / `.sidx` 结尾。3.x 在打开、load、检索、flush、compaction、recovery、snapshot、`drop_index` 等路径下都会跳过这类文件（[collection.py:1945](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L1945)），只在 drop partition / drop collection / truncate 时连目录一起删除。
   - Rust 必须自己清理「源 segment 已不在 manifest 中」的 Rust 索引文件，包括 3.x compaction 删掉 segment 后遗留的那些。
6. **snapshot 钉住的文件任何一方都不得删除。**
   - snapshot 元数据的 `data_files`、`delta_files`、`index_files` 都会钉住文件（[snapshots.py:117](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/storage/snapshots.py#L117)）。restore 时逐个复制这些文件，缺失就直接失败。实测：删掉一个被钉住的 `.idx` 后，`restore_snapshot` 报 `FileNotFoundError`。
   - 在保持不变量 11（索引与 segment 生命周期对齐）的前提下：Rust 删除 segment 时，要一并删除同 stem 的 3.x 索引文件，**被任一 snapshot 引用的除外**。
7. **`index_specs` 是共享的索引定义。**
   - 3.x 索引文件名只由 stem、字段名、index_type 决定，不含 metric 和建索引参数。所以只有当字段的索引定义（type、metric、params）没有变化时，才可以保留 3.x 的 `.idx` / `.sidx`。
   - Rust 执行 drop_index，或改变某字段的索引定义时，要按 3.x `drop_index` 的规则（[collection.py:1552](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L1552)）删掉对应的 3.x 索引文件（被 snapshot 钉住的除外），否则 3.x 下次会加载参数不对的旧索引。实测：在 L2 定义下加载了按 COSINE 建的旧索引，返回的距离全部错误，排序也不同。
   - Rust 不能执行的索引定义（第一阶段的 HNSW 等）原样保留，不改写成自己的类型。
8. **分词与 BM25 必须和 3.x 逐位一致。**
   - 稀疏向量在写入时由 BM25 function 计算，存入 WAL 与 Parquet；查询时再对查询文本分词、求 term ID。要让「3.x 写入、Rust 查询」得到相同结果，分词和 term hash 都必须一致。
   - 3.x 的 standard 分词器是 Python `re` 的 `\w+`，作用在 `str.lower()` 之后（[standard.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/analyzer/standard.py#L26)），**不是** Lucene 的 standard。它的 Unicode 语义与 Rust `regex` 不同。实测：分解形式的 `"cafe\u0301"`（e 加组合重音符），Python 得到 `cafe`，Rust `regex` 得到 `cafe\u0301`；`"İstanbul"`，Python 得到 `i`、`stanbul` 两个词，Rust 得到一个词；`"x²y"`，Python 得到一个词，Rust 得到 `x`、`y`。而且 Python 各版本的 Unicode 表不同，3.x 支持 Python ≥ 3.10。
   - term hash 是 UTF-8 字节上的 32 位 FNV-1a（[hash.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/analyzer/hash.py)）。
   - jieba 的等价性另议。
   - BM25 的 IDF 是检索时按 segment 现算的，不落盘（[collection.py:884](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L884) 的 TODO）。打分一致属于行为层（规则 2），不属于格式层。
9. **Parquet 写入参数。**
   - 必须写入 `ARROW:schema` 元数据（pyarrow 的 `store_schema` 默认开启）。缺少时，3.x 把定长向量列读成变长 list，打开 collection 直接失败。
   - 压缩与 3.x 默认一致，用 snappy。3.x 调 `pq.write_table` 时没有指定压缩，用的是 pyarrow 的默认值。
10. **`LOCK` 的互斥。**
    - 3.x 在 Unix 上用 `fcntl.flock(LOCK_EX | LOCK_NB)`，在 Windows 上用 `msvcrt.locking(fd, LK_NBLCK, 1)`，后者是锁住第 0 字节的强制字节范围锁。
    - Rust 在 Unix 上必须用 flock，不能用 `fcntl` / `lockf` 锁：两者在 Linux 上互不感知。在 Windows 上，锁定区间必须覆盖第 0 字节。
    - 永远不删除、不重建 `LOCK` 文件，否则两个进程会锁在不同的 inode 上，互斥失效。
    - 3.x 在加锁之前就会创建 `collections/` 与 `databases/`，即使开库失败，这两个目录也可能出现。
11. **版本与内容都要 fail-closed。**
    - 3.x 不校验 `manifest_format_version`、`schema_format_version`、`snapshot_format_version` 这三个版本号，重写文件时还会把版本号改回 2。v3.0.0 到 v3.2.1 之间新增了 GEOMETRY、TIMESTAMPTZ、schema properties、snapshot 等内容，版本号却没有变。
    - 所以 Rust 除了只接受已知的版本号，遇到未知的 dtype、function 类型、metric 也要明确报错，不能宽松读取。未知的 index_type 是例外：3.x 自己就会写出这类目录（约束 1），因此与 3.x 保持同样的粒度，目录照常打开，只在 load 该 collection 时报错。
    - 读取老目录时，缺失的键按 3.x 的默认值处理；3.0 在 Windows 上写的 `\` 路径要规范化为 `/`。
    - `aliases.json` 与 `database_properties.json` 没有版本字段。
12. **3.x 升级格式时**，先评估新格式，再决定跟进或拒绝；跟进时按 [AGENTS.md](../../AGENTS.md) 规则 8 记录兼容性影响。对照版本升级时，同步更新本文的「对照版本」，并重跑[验证](#验证)中的全部跨实现测试。
13. **正确性不变量照旧适用**：互通不放宽 [AGENTS.md](../../AGENTS.md) 规则 3 中的任何一条。有效位图（[0001](0001-refactor-baseline.md#对-rust-实现的约束) 约束 2）只在内存中维护，由数据文件与 delta 推导，不成为共享格式的一部分。

### 字段编码要点

实现时按 3.x 代码逐项对齐，下面是容易出错的点：

| 类型 / 特性 | 3.x 的表示 |
| --- | --- |
| `FLOAT_VECTOR` | `fixed_size_list<float32>[dim]`；nullable 向量的 null 落盘为全零向量，不是 Arrow null（[collection.py:2111](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L2111)） |
| `SPARSE_FLOAT_VECTOR` | binary，内容为按 index 升序排列的小端 `(u32 index, f32 value)` 对（[sparse.py:37](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/analyzer/sparse.py#L37)）；空向量为 `b""`，不是 null |
| `JSON` 字段 | dict / list 经 `json.dumps(ensure_ascii=False)` 存为 UTF-8 文本，不排序；字符串原样存储（[collection.py:2106](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L2106)） |
| `$meta`（动态字段） | `json.dumps(extras, sort_keys=True)`，即按键排序、`ensure_ascii=True`；没有动态字段时为 null（[validation.py:518](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/schema/validation.py#L518)） |
| `ARRAY` | 变长 `list<elem>` |
| `GEOMETRY` | WKT 字符串 |
| `TIMESTAMPTZ` | `timestamp[us, tz=UTC]` |
| 默认值 | 写入时填充；显式传 `None` 也填默认值 |
| partition key | 分区名为 `_pk_<md5(str(value)) % 分区数>`（[collection.py:417](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L417)） |

## 验证

互通靠跨实现测试来保证，不靠逐字段比对。在测试环境里安装 3.x，版本固定为对照版本，至少覆盖：

- **3.x 写 → Rust 读**，以及 **Rust 写 → 3.x 读**：
  - 正常关闭的目录，以及未 flush、遗留 WAL 的目录（模拟崩溃），包括 WAL 尾部被截断的情况；
  - 读出的行、删除、upsert 覆盖、partition key 路由、`_all` 删除的结果，要与写入方自己读出的一致；
  - 目录中的文件既不多也不少。
- **交替写入**：两个实现轮流打开同一目录追加、删除、flush、compaction。每一步之后双方读出的结果一致，没有孤儿文件，没有 tombstone 被提前回收（约束 2）；包括「失败的 insert → 成功的 insert → 另一方重开再 insert」，「partition key 加 auto_id 的 insert 在两个 batch 之间崩溃 → 另一方重开再 insert」，以及「同一 memtable 内先插入后删除、再 flush → 另一方重开再 insert」，自动主键都不重复（约束 4）。
- **共享值域**：Rust 写出含索引定义的 manifest 后，3.x 打开，行数不变，文件也不少（约束 1）。
- **索引文件共存**：两种索引文件同时存在时，各自只加载自己的格式；删除 segment、drop_index、修改索引定义之后，陈旧文件被正确清理，被 snapshot 钉住的文件保留（约束 5–7）。
- **snapshot**：一方创建、另一方 restore；GC 不删除被钉住的文件。
- **第一阶段的索引定义**：pymilvus 快速建表（AUTOINDEX）可用；新建 HNSW / INVERTED 以及稀疏字段上的任何索引都明确报错；打开 3.x 写的 HNSW / AUTOINDEX / INVERTED / SPARSE_INVERTED_INDEX 目录时，定义原样保留，检索与过滤结果正确（稀疏检索明确报错），3.x 再次打开时仍能使用原索引；Rust 写出的任何索引定义，3.x 都能 load。
- **互斥**：一方持有 `LOCK` 时，另一方打开失败并明确报错。Linux（flock 与 fcntl 互不感知）与 Windows（字节范围锁）需要在对应平台上实测。
- **fail-closed**：伪造未知的格式版本号，以及未知的 dtype、metric 时，Rust 拒绝打开，不改动任何文件；未知的 index_type 与 3.x 一致，只在 load 该 collection 时报错。
- **库版本组合**：arrow-rs 写、pyarrow（3.x 支持的版本范围内）读，以及反过来，覆盖 IPC 与 Parquet；包括 TIMESTAMPTZ 时区串的写法。
- **分词**：包含 Unicode 组合字符、全角字符、各类标点的语料，Rust 与 3.x 产出的 term ID 序列一致。

测试夹具由 3.x 在测试运行时现场生成，不提交生成好的数据目录（[AGENTS.md](../../AGENTS.md) 规则 14）。

## 继承的 3.x 已知风险

互通意味着这些 3.x 自身的行为同样会作用在 Rust 写出的目录上，Rust 实现不得依赖它们，也不得放大它们：

- manifest 解析失败时回退 `.prev` 并删除「孤儿」文件，任何解析错误都可能静默丢数据（约束 1 正是为此）。
- 某个 snapshot 钉住旧 `.idx` 之后，如果先 drop_index，再以同类型、不同 metric 重建，3.x 自身也会加载到陈旧索引。
- nullable 向量用全零表示 null，与真正的零向量无法区分。
- 校验失败的 insert 会消耗 auto_id；partition key 加 auto_id 的一次 insert 会拆成多个 batch 逐个落盘，中途崩溃也一样。两种情况下，重开后自动主键都可能重复并覆盖旧行（约束 4 的上界正是为此）。

## 放弃的方案

- **完全自定义格式**：可以自由优化布局，但失去对 3.x 存量数据目录的直接打开能力，也失去同目录差分测试，还需要额外的迁移工具。
- **索引文件也互通**：3.x 的 FLAT 索引是 NPY，标量索引是 Arrow IPC，这些都不难复现；但 HNSW / IVF 的 `.idx` 是 FAISS 的序列化格式，要互通就得逐位复现 FAISS 的格式或引入 FAISS，违背不引入 C++ 构建依赖的决定（见 [AGENTS.md「技术栈」](../../AGENTS.md#技术栈)）。而索引都可以从数据重建，互通收益很小。
