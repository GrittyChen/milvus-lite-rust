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
  - 下列分类都以**字段类型**为前提。3.x 只对标量字段校验索引类型，只放行 INVERTED（[collection.py:1463](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L1463)）；对稠密与稀疏向量字段则不校验，同一个类型名放在错误的向量字段上也会被持久化，只是 build 失败（见约束 1）。
  - **稠密向量字段上的 FLAT / BRUTE_FORCE**：原生支持。
  - **稠密向量字段上的 AUTOINDEX**：新建与存量都按 FLAT 精确执行，并登记为已知差异。AUTOINDEX 的含义本来就是「由实现选择」，pymilvus 的快速建表 `create_collection(name, dimension=...)` 固定使用它；3.x 在没有 faiss 时自己也退回暴力检索，装了 faiss 时（3.2.1 的必装依赖）建成 HNSW。
  - **稠密向量字段上的显式 ANN 类型**（HNSW、HNSW_SQ、IVF_FLAT、IVF_SQ8）与**标量字段上的 INVERTED**：新建时明确报错。打开 3.x 写的、带这类定义的目录时，**原样保留**定义，让 3.x 回来后还能照常使用。向量检索按 FLAT 精确执行，召回只高不低，但更慢；标量过滤按扫描执行，更慢，结果也不一定相同：3.x 自身在已建索引的 segment 与 memtable 上结果就不一致，索引又是 load 之后在后台构建的，本实现按扫描求值并登记差异（[0006](0006-m2-scope.md) 第 14 项）。
  - **稀疏向量字段**：第一阶段在稀疏字段上新建任何索引都明确报错。存量的 SPARSE_INVERTED_INDEX 定义原样保留，稀疏向量检索在全文检索阶段之前明确报错。
  - 以上执行方式与 3.x 不同的地方，都登记为已知的对外差异（[AGENTS.md](../../AGENTS.md) 规则 2）。
  - **3.x 自身也无法 load 的索引定义**：包括 3.x 会原样持久化的 DISKANN、IVF_PQ，以及放错字段类型的定义，例如稀疏字段上的 AUTOINDEX / FLAT / HNSW（见约束 1）。处理与 3.x 一致：目录照常打开，只有该 collection 在 build 索引时报错，即 load 时报错；如果 collection 已处于 load 状态，`create_index` 持久化定义后会立刻 build 并报错。rc1 尚未实现这一报错边界，见[发布验收记录](../testing/first-release-acceptance.md#留待-rc2)。

互通的前提是**共享文件的语义也完全一致**，不只是格式能读。下面「共享状态的语义约束」中标注「实测」的条目，都已在 3.2.1 上复现：违反它们会造成数据丢失、已删除的行复活、主键重复覆盖，或检索结果静默出错。

## 互通范围

以 v3.2.1 的实现为准。字段级细节以 3.x 的代码与 [wal-design.md](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/docs/wal-design.md) / [modules.md](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/docs/modules.md) 为权威来源，本文只记录互通需要特别守住的部分。

| 对象 | 是否互通 | 说明 |
| --- | --- | --- |
| 目录布局 | 是 | 数据目录根：默认数据库的 `collections/`、`aliases.json`、`database_properties.json`，以及只有一份的 `.database-staging/`（3.x 每次打开时清空；本实现的 drop 与 create 也借它完成原子的改名，见 [0006](0006-m2-scope.md) 第 17 项）；其他数据库各自在 `databases/<db>/` 下有 `collections/`、`aliases.json`、`database_properties.json`（见 [db.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/db.py#L7)）。collection 级：`schema.json`、`manifest.json`（首次 flush 之前可能不存在）、`manifest.json.prev`、`wal/wal_{data,delta}_NNNNNN.arrow`、`partitions/<p>/{data,delta,indexes}/`、`snapshots/metadata/<name>.json`、`snapshots/manifests/<name>/`，以及原子写入时的 `*.json.tmp` |
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
   - 更危险的是**值**：3.x 解析 manifest 时一旦出现 `KeyError`、`ValueError` 或 `TypeError`，就回退到 `manifest.json.prev`，只打两条 warning 日志，调用方不会收到任何错误（[manifest.py:164](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/storage/manifest.py#L164)）。随后 recovery 会把只登记在新 manifest 里的数据文件当作孤儿删掉。实测：把 `index_specs` 的 metric 改成 3.x 不接受的 `HAMMING` 后，10 行数据变成 5 行，较新的 Parquet 文件被删除。
   - 注意「能解析」与「能使用」不同：3.x 解析 manifest 时并不校验 index_type（[spec.py:58](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/index/spec.py#L58) 只要求非空字符串），`create_index` 也只做大写化就持久化（[collection.py:1454](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L1454)）。实测：向量字段上的 `DISKANN` 被写进 manifest，3.x 重开照常，只在该 collection build 索引时（load 时，或已 load 时的 `create_index`）报 `unknown index_type`。
   - 因此，Rust 写入共享文件的枚举值必须落在 3.x 能 build / load 的集合内，而且要**按字段类型**判断。index_type 的值域为：稠密向量字段上的 FLAT、BRUTE_FORCE、AUTOINDEX、HNSW、HNSW_SQ、IVF_FLAT、IVF_SQ8（出自 [factory.py:53](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/index/factory.py#L53) 的 `KNOWN_VECTOR_INDEX_TYPES`，去掉 3.x 自己也无法 build 的 `IVF_PQ`）；稀疏向量字段上只有 SPARSE_INVERTED_INDEX；标量字段上只有 INVERTED（[capabilities.py:6](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/index/scalar/capabilities.py#L6)）。实测：稀疏字段上的 AUTOINDEX / FLAT / HNSW 都会被 3.x 持久化，但 load 时报 `vector column must be FixedSizeList, got binary`。metric 限于 COSINE / L2 / IP / BM25 / NONE（[spec.py:60](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/index/spec.py#L60)）；dtype、function 类型同理。Rust 需要的额外状态只能放进独立的、可重建的文件。
   - `.prev` 的维护协议与 3.x 相同：写 `manifest.json.tmp` 并 fsync，把当前 `manifest.json` 复制为 `.prev`，replace 为 `manifest.json`（提交点），再 fsync 父目录（[manifest.py:86](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/storage/manifest.py#L86)）。
2. **文件名中的 seq 范围是 tombstone GC 的依据，必须覆盖文件内容。**
   - 3.x 用 data 文件名里 seq_min 的全局最小值（[compaction.py:483](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/compaction.py#L483)），判断 delta 文件名里 seq_max 更小的 delta 文件可以删除（[compaction.py:446](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/compaction.py#L446)）。
   - 所以 data 文件名的 seq_min 必须不大于内容中最小的 `_seq`，seq_max 必须不小于最大的 `_seq`；delta 文件的 seq_max 同样必须是上界。
   - 命名固定为 `data_{seq_min:06d}_{seq_max:06d}.parquet` / `delta_..._....parquet`。6 位是最小宽度，数值更大时可以更长。不符合这个格式的文件不参与 GC 计算。
   - 实测：把一个含已删除行的文件改名、使 seq_min 偏大后，compaction 提前回收了 tombstone，已删除的行复活。
3. **WAL delta 的一个 batch 对应一次 delete 调用：只含一个 `_seq`、一个 `_partition`。**
   - 3.x 回放时只读每个 batch 第 0 行的 seq 和 partition（[memtable.py:127](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/storage/memtable.py#L127)）。实测：依次执行 insert pk 1、delete pk 1、insert pk 2、delete pk 2，再把两次 delete 合并成一个 batch 回放，第二次删除按第一次的 `_seq` 处理，而这个 seq 小于 pk 2 插入时的 seq，于是删除失效，pk 2 复活。只有当两次删除属于同一分区、且其间没有对目标主键的插入时，合并后的结果才碰巧正确；分区不同时，第二次删除会按第一次的分区处理，tombstone 落进错误分区的 delta。当场看不出差别，因为 3.x 按 pk 全局判定删除（见约束 13）；但 drop 掉那个分区并重开后，另一分区中已删除的行会复活。实测：pk 1、pk 2 分别写入 p1、p2 并 flush 落盘后，p1 删 pk 1、p2 删 pk 2，合并回放后两条 tombstone 都进了 p1 的 delta，drop p1 并重开后 pk 2 复活。若被删的行还在同一个 memtable 中，回放时删除会直接清掉插入，不会出现复活。
   - `_partition` 为 `_all` 表示跨分区删除，flush 时复制到每个分区的 delta。
   - data WAL 的每个 batch 也只含一个分区：3.x 写出的 batch 都是如此，回放时虽然能容忍混合分区，但 Rust 不写出这种 batch。
4. **`_seq` 与 auto_id 同源，两个方向都要约束。**
   - 3.x 重新打开时，`_seq` 从 `max(manifest.current_seq, WAL 中最大 seq) + 1` 继续分配（[collection.py:235](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L235)），auto_id 也从这个值开始（[collection.py:293](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L293)）。
   - **下界**：Rust 分配的 `_seq` 与 auto_id 都不得小于这个值，否则会与已持久化的数据重复。
   - **上界（规范）**：逐行保证 auto_id ≤ 该行的 `_seq`。两者都按行消耗（[collection.py:389](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L389)），可以同步分配；一次调用拆成多个 batch 时（例如按 partition key 分桶），每一行照样满足。这样留在盘上的每一行都自带上界，不依赖 batch 的划分，也不依赖哪些行后来被删掉。
   - **`current_seq` 的前提**：flush 提交的 `manifest.current_seq` 必须不小于该 memtable 消耗过的最大 `_seq`，包括 delete，也包括没有写进文件的操作；与 3.x 相同，取 memtable 的 `max_seq`（[flush.py:126](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/flush.py#L126)），**不能**取写出文件中的最大 `_seq`。实测：在开启 auto_id 的 collection 中，一个 batch 显式写入 pk 3、1、2（seq 1–3；3.x 只为缺失的 pk 补 auto_id，但仍按行数消耗 auto_id，见 [collection.py:362](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L362)），删除 pk 2（seq 4）并 flush，此时 `current_seq` 为 4。把它改成写出数据的最大 seq 2 之后重开，再写入 3 行不带 pk 的数据：新行的自动主键从 3 开始，原有的 pk 3 被新行覆盖（应为 5 行，实际 4 行）。
   - 违反以上任一条，对方重开后都会从较小的值重新分配 auto_id，重复的主键按 upsert 覆盖旧行。3.x 自己就有两处不满足逐行上界，均已实测：一是在校验之前就消耗 auto_id（[collection.py:361](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L361)），一次校验失败的 insert 之后，重开再写，原有的一行被覆盖（应为 7 行，实际 6 行）；二是 partition key 路径先按输入顺序分配 auto_id，再按桶分别分配 `_seq` 并逐桶落盘（[collection.py:421](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L421)），在两个桶之间崩溃后，重开再写会覆盖已落盘的行（应为 6 行，实际 5 行）。
5. **索引文件与 Rust 独有文件的命名和放置。**
   - 3.x 在同名 `.idx` 存在时直接加载（[segment.py:265](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/storage/segment.py#L265)）。实测：内容不合法时 load 抛异常（FLAT 为 NPY 的 `ValueError`，HNSW 为 FAISS 的 `RuntimeError`），collection 保持 released，不会回退重建，也不会删除坏文件。所以 Rust 文件不能使用 3.x 的命名。3.x 的命名为 `<data_stem>.<field>.<index_type 小写>.idx|.sidx`（[files.py:23](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/index/files.py#L23)）。
   - 3.x 会删除 `data/`、`delta/`、`.database-staging/` 下的任何陌生文件。在 `indexes/` 下，它只删除文件名能按上述格式解析的文件，并且还要满足以下之一：stem 已不在 manifest 与 snapshot 中（recovery）；stem 已不在当前 manifest 中，或 field / type 与当前索引定义不符，并且没有被 snapshot 钉住（flush 之后，以及 collection 打开时调度的后台任务，[collection.py:1985](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L1985)）；后缀与被 drop 的索引匹配（drop_index）。
   - 为保守起见，Rust 的索引文件放在 `indexes/`，扩展名不以 `.idx` / `.sidx` 结尾。3.x 在打开、load、检索、flush、compaction、recovery、snapshot、`drop_index` 等路径下都会跳过这类文件（[collection.py:1945](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L1945)），只在 drop partition / drop collection / truncate 时随目录清理一并删除。drop_collection 与 truncate 是整目录删除（[db.py:268](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/db.py#L268)、[:519](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/db.py#L519)），snapshot 随之删除；只有 drop_partition 逐个删除未被 snapshot 钉住的文件，再删除空目录。对 Rust 文件而言效果相同。
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
   - 逐位一致的范围还包括 `analyzer_params` 的 filter：`{type: "stop", stop_words}` 在 lower 之后过滤 token，其他 filter 类型被静默忽略（[factory.py:92](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/analyzer/factory.py#L92)）。jieba 分词器对每个 token 做 `strip().lower()`，jieba 本身的等价性另议。
   - BM25 的 IDF 是检索时按 segment 现算的，不落盘（[collection.py:884](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L884) 的 TODO）。打分一致属于行为层（规则 2），不属于格式层。
9. **Parquet 写入参数。**
   - 必须写入 `ARROW:schema` 元数据（pyarrow 的 `store_schema` 默认开启）。缺少时，3.x 把定长向量列读成变长 list，打开 collection 直接失败。
   - 压缩与 3.x 默认一致，用 snappy。3.x 调 `pq.write_table` 时没有指定压缩，用的是 pyarrow 的默认值。
10. **`LOCK` 的互斥。**
    - 3.x 在 Unix 上用 `fcntl.flock(LOCK_EX | LOCK_NB)`，在 Windows 上用 `msvcrt.locking(fd, LK_NBLCK, 1)`，后者是锁住第 0 字节的强制字节范围锁。
    - Rust 在 Unix 上必须用 flock，不能用 `fcntl` / `lockf` 锁：两者在 Linux 上互不感知。在 Windows 上，锁定区间必须覆盖第 0 字节。
    - 永远不删除、不重建 `LOCK` 文件，否则两个进程会锁在不同的 inode 上，互斥失效。
    - 3.x 在加锁之前就会创建 `collections/` 与 `databases/`，即使开库失败，这两个目录也可能出现；加锁之后还会创建 `.database-staging/`，并在此时清空其中的全部条目，关闭时不清空、也不删除（[db.py:102](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/db.py#L102)），因此被 3.x 成功打开过的目录都有它。
11. **版本与内容都要 fail-closed。**
    - 3.x 不校验 `manifest_format_version`、`schema_format_version`、`snapshot_format_version` 这三个版本号，重写 manifest 与 schema 时还会把版本号改回 2；snapshot 元数据只在创建时写入一次，从不重写。v3.0.0 到 v3.2.1 之间新增了 GEOMETRY、TIMESTAMPTZ、schema properties、snapshot、标量索引 `.sidx`、多数据库布局等内容，版本号却没有变。
    - 所以 Rust 除了只接受已知的版本号，遇到未知的 dtype、function 类型、metric 也要明确报错，不能宽松读取。manifest、schema 与 snapshot 元数据中出现未知的键，同样明确报错：版本号不变，未知的键是新格式唯一的信号，而本实现重写 manifest 时会丢掉它们。
    - 这里的 fail-closed 按 collection 生效：只有出问题的 collection 不能使用，目录中其余 collection 照常（[0006](0006-m2-scope.md) 第 14 项）。未知的 index_type 是例外：3.x 自己就会写出这类目录（约束 1），因此与 3.x 保持同样的粒度，目录照常打开，只在该 collection build 索引时报错。
    - 读取老目录时，缺失的键按 3.x 的默认值处理；3.0.x 到 3.1.0 在 Windows 上写出的 `\` 路径（manifest 与 snapshot 元数据中都可能出现；修复首见于 v3.1.1）要规范化为 `/`。
    - `aliases.json` 与 `database_properties.json` 没有版本字段。
12. **3.x 升级格式时**，先评估新格式，再决定跟进或拒绝；跟进时按 [AGENTS.md](../../AGENTS.md) 规则 8 记录兼容性影响。对照版本升级时，同步更新本文的「对照版本」，并重跑[验证](#验证)中的全部跨实现测试。
13. **正确性不变量照旧适用**：互通不放宽 [AGENTS.md](../../AGENTS.md) 规则 3 中的任何一条。有效位图（[0001](0001-refactor-baseline.md#对-rust-实现的约束) 约束 2）只在内存中维护，由数据文件与 delta 推导，不成为共享格式的一部分。位图的删除判定必须与 3.x 语义相同：在 collection 级按 pk 判定，某个 pk 只要存在 `_seq` 更大的 tombstone，就视为已删除，**不看 tombstone 所在的 delta 属于哪个分区**（3.x 的 `DeltaIndex` 是 collection 级的 pk → 最大删除 seq 映射，[delta_index.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/storage/delta_index.py#L1)）。如果按分区套用 delta，读取约束 3 所述的误路由目录时，结果就会与 3.x 不同。

### 字段编码要点

实现时按 3.x 代码逐项对齐，下面是容易出错的点：

| 类型 / 特性 | 3.x 的表示 |
| --- | --- |
| `FLOAT_VECTOR` | `fixed_size_list<float32>[dim]`；nullable 向量的 null 落盘为全零向量，不是 Arrow null（[collection.py:2111](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L2111)） |
| `SPARSE_FLOAT_VECTOR` | binary，内容为按 index 升序排列的小端 `(u32 index, f32 value)` 对（[sparse.py:37](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/analyzer/sparse.py#L37)）；空向量与 None 都落为 `b""`，不是 null |
| `JSON` 字段 | dict / list 经 `json.dumps(ensure_ascii=False)` 存为 UTF-8 文本，不排序，使用默认分隔符 `", "` / `": "`；字符串原样存储（[collection.py:2106](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L2106)） |
| `$meta`（动态字段） | `json.dumps(extras, sort_keys=True)`，即按键排序、`ensure_ascii=True`、默认分隔符；没有动态字段时为 null（[validation.py:518](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/schema/validation.py#L518)） |
| `ARRAY` | 变长 `list<elem>` |
| `GEOMETRY` | WKT 字符串 |
| `TIMESTAMPTZ` | `timestamp[us, tz=UTC]` |
| 默认值 | 写入时填充；显式传 `None` 也填默认值 |
| partition key | 分区名为 `_pk_<md5(str(value)) % 16>`（[collection.py:417](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L417)）；桶数固定为 16（[collection.py:328](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L328)），不随客户端传入的 `num_partitions` 变化；`str(value)` 是 Python 语义（例如布尔值为 `True`，浮点数按 Python 的 repr） |

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
- **删除判定的分区语义**：tombstone 与其对应的数据行分处不同分区（包括约束 3 所述的误路由目录，以及 `_all` 删除），Rust 与 3.x 读出的结果一致（约束 13）。
- **第一阶段的索引定义**：pymilvus 快速建表（AUTOINDEX）可用；新建 HNSW / INVERTED 以及稀疏字段上的任何索引都明确报错；打开 3.x 写的 HNSW / AUTOINDEX / INVERTED / SPARSE_INVERTED_INDEX 目录时，定义原样保留，检索与过滤结果正确（稀疏检索明确报错），3.x 再次打开时仍能使用原索引；Rust 写出的任何索引定义，3.x 都能 load。
- **互斥**：一方持有 `LOCK` 时，另一方打开失败并明确报错。Linux（flock 与 fcntl 互不感知）与 Windows（字节范围锁）需要在对应平台上实测。
- **fail-closed**：伪造未知的格式版本号、未知的键，以及未知的 dtype、metric 时，Rust 拒绝打开该 collection、其余 collection 照常，不改动任何文件；未知的 index_type 与 3.x 一致，只在该 collection build 索引时报错。
- **库版本组合**：arrow-rs 写、pyarrow（3.x 支持的版本范围内）读，以及反过来，覆盖 IPC 与 Parquet；包括 TIMESTAMPTZ 时区串的写法。
- **分词**：包含 Unicode 组合字符、全角字符、各类标点的语料，Rust 与 3.x 产出的 term ID 序列一致。

测试夹具由 3.x 在测试运行时现场生成，不提交生成好的数据目录（[AGENTS.md](../../AGENTS.md) 规则 14）。

## 继承的 3.x 已知风险

互通意味着这些 3.x 自身的行为同样会作用在 Rust 写出的目录上，Rust 实现不得依赖它们，也不得放大它们：

- manifest 解析失败时回退 `.prev` 并删除「孤儿」文件，任何解析错误都可能静默丢数据（约束 1 正是为此）。
- 在不区分大小写的文件系统上，manifest 与 snapshot 的文件引用按精确路径字符串判断；外部工具若只改变被引用文件名的大小写，路径读取仍可能成功，但孤儿清理会把实际目录项判为未引用并删除。3.x 与本实现均继承这一风险；本实现首次打开可能读到内存中的行，随后重开会因被引用文件缺失而拒绝打开。此条针对分区内文件名，与 collection 名字的大小写处理不同；互通目录中的持久化文件名须保持引用中的原始拼写。
- 某个 snapshot 钉住旧 `.idx` 之后，如果先 drop_index，再以同类型、不同 metric 重建，3.x 自身也会加载到陈旧索引。
- 非有限 JSON 编码的边界：3.x 可能把 raw gRPC 的 `{"a":1e999}` 写成含 `Infinity` 的文本，raw Query 可成功返回这些字节，但 pymilvus 的严格 JSON 解码会失败。本实现对新的 JSON 字段写入提前拒绝这类浮点溢出，差异与用例见 [0006「与 3.x 的差异」](0006-m2-scope.md#与-3x-的差异)；已有 JSON 列仍按文本读取，不代表能由严格客户端解析或再次编码。
- **历史互通限制（尚未修复）**：`$meta` 含 `Infinity/-Infinity/NaN` 时，3.x 的 `json.loads` 可加载目录；本实现的 `$meta` 解码使用严格的 serde_json，collection 懒打开会返回 storage error，提示 `$meta is not a JSON object` 与解析位置。raw `$meta` 输入 `{"x":1e999}` 仍可能被双方接受写入并返回含 `Infinity` 的 raw 结果；本实现后续 flush 的批解码可在提交点前失败，并进入待提交重试状态；失败后一秒窗口内写入因节流被拒，窗口后重试仍会因同一元数据解析错误而失败，故故障未消除时对该 collection 的后续写入持续被拒绝。显式 Flush 和关闭同样不能完成该代提交；重启后该 collection 又会因 `$meta` 解码失败而拒绝打开，不能靠等待或重启自动恢复。其他健康 collection 不受影响。因此不能保证直接打开这类非标准元数据目录。此限制独立于 JSON 列的新写入政策；本轮仅登记，不改变 `$meta` 解析或 `pyjson` 输出规则。确认目录类型时，应检查 Arrow IPC/Parquet 的 `$meta` 文本，而不能将所有 `$meta` 解析错误都归因于非有限值，也不自动改写文件。
- nullable 向量用全零表示 null，与真正的零向量无法区分。
- 校验失败的 insert 会消耗 auto_id；partition key 加 auto_id 的一次 insert 会拆成多个 batch 逐个落盘，中途崩溃也一样。两种情况下，重开后自动主键都可能重复并覆盖旧行（约束 4 的上界正是为此）。
- 以下各条由 [0006](0006-m2-scope.md)「3.x 的缺陷」与第 8、10、12 项实测或推出：
  - flush 提交之后、删除 WAL 之前崩溃，3.x 重开时会回放已提交的 WAL，数据重复。本实现写出的目录若在这一点中断，3.x 打开时同样会重复；本实现在提交点之后的失败一直重试不成功时，盘上留着已提交的 WAL，3.x 打开时也会重复。
  - 本实现关闭时若仍删不掉已提交 WAL，3.x 随后打开同样可能重复计数。若同一编号的 data 已删除而只剩 delta，3.x 的 get 与部分 upsert 还可能读错；3.x 自身在两次 unlink 之间崩溃也会留下此形状（见 [0007「提交之后」](0007-m2-storage-engine.md#flush)）。
  - 已提交的 WAL 残留时，3.x 的 compaction 与 GC 可能抹掉删除的证据，之后任一方打开，已删除的行都会复活。
  - 3.x 并发写入同一个 collection 时，WAL 懒创建的竞态会写坏 WAL，崩溃后丢失已确认的写入。
  - WAL 追加失败后，3.x 继续往同一个文件追加，恢复时可能把残缺的 batch 当作合法数据回放。
  - WAL 被读取器识别为格式或批校验失败，且参考端捕获该异常时，两边都只取各自已校验的前缀，其后已确认的写入可能丢失。IPC 流中间的零长度消息按 EOS 停止读取，不再寻找之后的批。3.x 在抛出已捕获的错误后不重新扫描后续批；但向半截批继续拼接字节，可能使本应失败的批通过其校验，不能将这种故障目录等同于普通截断目录。
  - WAL 没有校验和，Arrow 完整校验只证明数组结构有效；例如翻转向量的符号位仍会被双方接受，不能承诺检出任意载荷变化，也不能推断任意损坏都被当作前缀终止。3.x 的 `_read_wal_file` 只捕获 `ArrowInvalid` 与 `OSError`/`IOError`；未被捕获的 `ArrowNotImplementedError` 或 `ArrowMemoryError` 会使 collection 访问失败，数据库构造本身仍是懒打开。例如单字节翻转 schema 的整数宽度 64→96，会让真实 3.x collection 访问抛 `ArrowNotImplementedError`；本实现对同一目录按 arrow-rs 解码错误取该 data WAL 的空前缀、继续打开并输出 WAL 告警，本用例的 `count(*)` 为 0，访问不报错。这是读取器判定差异的具体形态，不能将两侧都失败或恢复同一行集视为保证。共享格式保持不变，逐字节截断、载荷变异及 schema 损坏用例分别固定这些边界。
  - 异常目录中若同一主键的数据行与删除共用 `_seq`，3.x 的 [get](../../references/milvus-lite/milvus_lite/engine/collection.py) 与 [query/search 位图](../../references/milvus-lite/milvus_lite/search/bitmap.py) 可能给出不同可见性：前者会被 memtable 的本地 tombstone 直接判删，后者只在删除 `_seq` 严格大于数据行时屏蔽。本实现按 [0007「数据结构」](0007-m2-storage-engine.md#数据结构) 的同号来源与先后规则维护统一可见性，因而在这类目录上可能与 3.x 的 `get` 不同；正常唯一 `_seq` 的目录不受影响。

## 放弃的方案

- **完全自定义格式**：可以自由优化布局，但失去对 3.x 存量数据目录的直接打开能力，也失去同目录差分测试，还需要额外的迁移工具。
- **索引文件也互通**：3.x 的 FLAT 索引是 NPY，标量索引是 Arrow IPC，这些都不难复现；但 HNSW / IVF 的 `.idx` 是 FAISS 的序列化格式，要互通就得逐位复现 FAISS 的格式或引入 FAISS，违背不引入 C++ 构建依赖的决定（见 [AGENTS.md「技术栈」](../../AGENTS.md#技术栈)）。而索引都可以从数据重建，互通收益很小。
