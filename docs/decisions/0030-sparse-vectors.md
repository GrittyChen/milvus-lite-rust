# 0030：稀疏向量与 IP

状态：设计已通过，四步已实施并评审通过，待 CI 与 Compare 通过后合入。参照版本 `1320d26`。

## 范围

- 普通 `SPARSE_FLOAT_VECTOR` 字段：建表、写入、输出、Upsert / partial upsert、flush、重开、compaction，与 3.x 数据目录双向互通。
- `SPARSE_INVERTED_INDEX` + IP 的定义、load 与检索，以及稀疏字段在 Search、HybridSearch、range、boost、分组、offset、`round_decimal`、search iterator 中的使用。
- 稠密字段上的 `SPARSE_INVERTED_INDEX` 定义，按 3.x 的实际行为接受。

不包含：BM25 function 与 BM25 度量（第 6 步，已由 [0031](0031-bm25.md) 实现，倒排与缓存沿用本文的槽位）；稀疏字段默认值（登记为差异，见下文）。

## 3.x 的行为

**值规则**（[sparse.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/schema/sparse.py#L9)）：维度是 `[0, 2^32-2]` 的整数，权重有限、非负，并且转成 float32 之后仍然有限；零权重丢弃。新写入的非空值至少要有一个非零权重。存储为 binary 列，每行是按维度排序的 `<uint32, float32>` 小端对（[analyzer/sparse.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/analyzer/sparse.py)）。WAL 回放不做校验。

**检索**（[`_search_sparse`](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/engine/collection.py#L975)、[sparse_ip.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/index/sparse_ip.py)）：

- 每个 segment 缓存一份倒排，构建时不带有效位，规范化全部行；只为被检索分区中的非空 segment 构建。构建失败不缓存，下次检索重新构建。tombstone、旧版本和过滤的有效位在打分时才应用。
- MemTable 每次检索临时构建：先排除已删除行和旧版本，解码剩下的全部行（字节长度错在这一步报），再逐行走 Python 路线过滤（`_eval_row`，动态字段可见），最后带上过滤结果规范化并构建倒排；被过滤掉的行同样要规范化。
- 分数按查询维度的顺序用 double 累加 `q_i * w`，转成 float32 输出，溢出为 `inf`。只要和查询有一个共同的非零维度就会返回，下溢为 +0.0 的行也返回；`radius=0` 会把它们排除。空查询或没有共同维度时没有结果。
- 每个数据源用 `heapq.nlargest` 按 double 分数取 top-k，同分时按插入字典的先后，也就是先比首个命中的查询维度序号、再比行号。之后按数据源次序（segment 按快照次序，MemTable 最后）拼接，再按 float32 距离稳定排序。整体等价于按（float32 分数降序，数据源名次，double 分数降序，首个命中的查询维度序号，源内行位次）排序。pymilvus 发出的查询字节已经按维度排好序。
- 错误的先后：先校验查询，再按数据源次序依次处理每个 segment（先建倒排、再过滤），最后处理 MemTable（解码 → 过滤 → 规范化）。

**索引与度量**：

- 稀疏字段只接受 `SPARSE_INVERTED_INDEX` + IP，否则报 code 6 `sparse field 'sp' requires SPARSE_INVERTED_INDEX with metric IP; got ...`。`params` 里的调参项在建索引、load 和每次检索时都会校验：`bm25_k1` / `bm25_b` 在 IP 字段上报 `IP field ... does not accept ...`；`drop_ratio_build` / `drop_ratio_search` 按 `float(x) == 0` 判定，所以 `"0"`、`-0.0`、`False` 都被接受，`True`、`None`、`"0.1"` 被拒绝；`inverted_index_algo` 默认 `TAAT_NAIVE`，大小写不敏感。pymilvus 把调参项拆成独立的 kv，3.x 不看这些 kv。索引定义的 `params` 与 `search_params` 两份都按上述规则校验；检索请求只取 `search_params` 里的 `params` 对象，顶层的同名键与 3.x 一样忽略。
- 稀疏字段的请求度量必须是 IP。不建索引也能 load 和检索。集合里只有稀疏向量字段时，不指定 `anns_field` 会报 `collection has no FLOAT_VECTOR field; specify anns_field explicitly for sparse search`。
- 稠密字段上的 `SPARSE_INVERTED_INDEX`：五种度量都能建，调参项不校验并原样回显；load 时什么也不做；检索按字段类型走稠密暴力检索，COSINE / L2 / IP 各按自己的度量计算，BM25 / NONE 报 code 6 `field 'v': unsupported dense metric 'BM25'`。
- `release()` 立即清空稀疏缓存，`create_index` / `drop_index` 也会清空对应字段的缓存。

**历史坏行**（实测时直接改写 parquet 构造）：打开、load、count 都正常，query / get 原样输出坏值；只要被检索分区里的 segment 含有负权重、NaN、Inf 或维度 2^32−1 的行，稀疏检索就报 code 1，文案是 normalize 的原文。这一行被过滤、删除或被 upsert 覆盖之后照样报错。字节长度不是 8 的倍数时，凡是解码这一列的操作都报 code 1 `sparse vector bytes length 7 is not a multiple of 8`，包括输出该字段的稠密检索。`b""` 和零权重的行不命中。乱序或重复的维度按字节顺序解码，后出现的覆盖先出现的。

**nullable 与默认值**：3.x 允许 nullable 稀疏字段，引擎 API 可以写 null；经 gRPC 写 null 一律被拒。稀疏检索跳过 null 行。默认值只能经 3.x 的 Python API 设置，`schema.json` 中存为字符串键的字典，DescribeCollection 不回显；pymilvus 无法声明它。

**输出**（[records.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/adapter/grpc/translators/records.py#L544)）：bytes 行原样输出，dict 行编码成排好序的字节，null 行写 `b""`；`SparseFloatArray.dim` 取所有解码出的维度号（含零权重）的最大值加 1，`VectorField.dim` 为 0；schema 中稀疏字段的 `dim` 为 null。一批结果中只要有 null，就写 `valid_data`，但 contents 仍按行补齐，而 pymilvus 2.6.17 / 3.0.2 都按有效行紧凑读取，所以有效行被读成 `{}`。这是 3.x 的缺陷。

## 设计

**类型**：新增 `DataType::SparseFloatVector`。原来的 `is_vector()` 改名为 `is_dense_vector()`，另加 `is_sparse_vector()` 和 `is_any_vector()`，原有调用点逐个重新判定：「至少有一个向量字段」、`anns_field` 解析和建索引时的度量必填用 any，`vector_field`、`vector_count`、范数、FAISS 度量检查、稠密索引类型和 `unbuildable` 只认稠密。`vector_pos` 改为 `Option<usize>`，`rows_per_block`、范数、`check_other_vectors`、partition key 和 partial upsert 都要显式处理没有稠密字段的情况；稀疏字段不调用 `field_dim`。

**schema**：稠密字段的 nullable 和默认值仍然拒绝；稀疏字段允许 nullable；`schema.json` 中稀疏字段的 `dim` 写 null。各属性在稀疏字段上的处理按 3.x 的 `_decode_field` 与 `validate_schema` 逐项对齐。

**存储**：`ColumnData::Sparse` 保存每行的原始打包字节（偏移数组加字节数组），可空沿用现有的包装。打开时只检查列类型与行数，不解码。新写入的值存规范化后的字节。解码只发生在三个时机：输出（检查长度是否为 8 的倍数，并计算 `dim`）、建倒排（规范化全部行）、partial upsert 继承旧值（解码后按用户输入校验）。count 以及不输出该字段的 query、检索都不解码。

**写入**：每个稀疏字段单独检查是否存在，缺失时报 `sparse vector field 'sp' missing and not nullable / no default`。gRPC 层拒绝 null，文案与 3.x 一致；引擎的 `Collection::insert` 接受 null。

**索引**：`build_spec` 按字段类型分支。稀疏字段只接受 `SPARSE_INVERTED_INDEX` + IP，并按上文规则校验 `params` 与 `search_params`。稠密字段接受 `SPARSE_INVERTED_INDEX` 和五种度量，不校验调参项，load 时什么也不做。load 时的定义校验加入稀疏组合，失败回滚到 released 状态。检索按字段类型分路。

**检索**：

- 稀疏检索与稠密检索共用候选窗口（`effective_top_k`），接入现有的后半段（range、boost、分组、offset）和 HybridSearch 的 `rank_route`。
- 已提交的块（segment）：每个块缓存一份倒排，维度映射到 `(行, f32)` 列表，构建时规范化全部物理行。有效位在打分时应用，过滤沿用 `filtered_chunks_compiled` 按数据源选择路线。
- MemTable：活跃代和冻结代合在一起，算一个数据源。新增一个强制走 Python 逐行求值的入口（`Tree::compile` 加 `python::evaluate_tree`，`DynamicRow` 带 `meta`），不改 boost 用的 `RowFilter`。处理顺序为解码、过滤、规范化，与 3.x 相同。
- 排序：键为（f32 分数降序，数据源先后，f64 分数降序，首个命中的查询维度序号，源内行位次），即 3.x 源内 `heapq.nlargest` 按 double 取前 k、并列保持 dict 插入顺序，再跨源按 float32 稳定排序。查询维度按查询给出的顺序遍历、不排序（pymilvus 发出的查询已排序，原始 RPC 可以不排序）。稠密路径的序号固定为 0，double 层取同一个 f32 值，所以现有稠密次序不变。
- 打分的计算任务是「一个数据源上的一段查询」，查询按计算池的线程数分段：只有一个 segment 时多条查询也能并行。每条查询仍在同一数据源上按同样的次序累加，结果与不分段时逐位相同。工作区每行 12 字节（double 分数与首个命中序号），每个任务一份、算完即释放。
- 错误：按 3.x 的处理次序报出第一个错误：调参项 → 查询 → 过滤编译 → 逐个 segment（建倒排 → 过滤求值）→ MemTable（解码 → 过滤 → 规范化）。缺失的倒排先在计算池上并行构建，错误留到按次序遍历时再报。
- 分区选择：3.x 的稀疏检索先在全部分区上判版本（`global_pk_seq` 含所有 segment 与 MemTable），再按 `partition_names` 剪枝；这与稠密检索「先选分区再判版本」（[0017](0017-partitions.md)）不同，被其他分区新版本遮住的旧版本在稀疏检索中不露出。实现上取全局有效位与所选分区的交集（MemTable 按行所属分区，已提交 segment 按所在分区），不逐行判版本；所选分区里没有有效行的 segment 仍建倒排（坏行照样报错），未选分区的 segment 不建。Search（含 iterator）与 HybridSearch 的逐路检索都经 `read_search_snapshot` 取这份快照。
- 负的有效 top_k：3.x 在打分时才报 `top_k must be a non-negative integer`（code 1），所以只在某个 segment 过滤后还有有效行、或 MemTable 有行（不看过滤结果）时报错，否则返回空结果。gRPC 层算出有效 top_k 后把「为负」交给引擎。

**缓存**：每个 `ChunkData` 中有一组按字段存放的槽位（`lock_order` 的叶子锁，见 [0007](0007-m2-storage-engine.md#取锁顺序)），手写 `Clone`，克隆时不复制缓存。槽位只在 RPC 线程上读写：缺失的倒排由计算池并行构建，叶子任务只写自己的结果槽（[0005](0005-search-thread-model.md)），构建完回到 RPC 线程再写入。写入前比对集合的缓存代号，代号变了就丢弃这次的结果。release、drop_index、create_index 都会让代号加一，并立即清空槽位。构建失败不写入缓存。两个首次检索同时到达时可能各建一份，以先写入的为准，不让任何一方等待。缓存键只有字段：稀疏字段只有 IP，打分与调参项无关，定义的变化都经过上面三个入口；BM25 的 k1 / b 到第 6 步再加进键里。

**输出**：bytes 原样输出，不重新排序。nullable 列中有 null 时按紧凑格式编码：`valid_data` 长度等于总行数，`contents` 只含有效行，`SparseFloatArray.dim` 取有效行的最大维度号加 1，全为 null 时 `contents` 为空、`dim` 为 0。没有 null 或字段不是 nullable 时，与 3.x 相同。Query、Get、Search、`output_fields=*`、稠密检索输出该字段，都走同一套编码。

**过滤**：在稀疏字段上过滤报 `field 'sp' of type sparse_float_vector cannot be used in scalar filter expressions`。`tree`、`eval`、`legacy_shape` 的类型表和 `python` 的穷举都补上稀疏类型。

## 与 3.x 的差异

| 差异 | 3.x | 本实现 | 理由 |
| --- | --- | --- | --- |
| 稀疏字段默认值 | 3.x Python API 可设置；插入时补默认值 | 带稀疏默认值的 `schema.json` 打开时报固定错误；该集合可以 drop，同库其他集合不受影响 | gRPC / pymilvus 无法表达，3.x 自己的 DescribeCollection 也不回显；完整支持涉及默认值的解析、写入补值、partial upsert 和描述省略，不值得在本步实现 |
| nullable 稀疏输出 | 有 null 时 contents 按行补齐，pymilvus 把有效行读成 `{}` | contents 紧凑编码，pymilvus 读出正确值 | 3.x 的输出缺陷会静默返回错误的值；紧凑编码也是真实 Milvus 的约定 |
| 原始 gRPC 把稀疏 FieldData（type 104）写进 JSON 字段 | 接受，按 `json.dumps` 存成字符串键的对象 | 报 `field 'j' value {...} does not match dtype DataType.JSON` | pymilvus 按 schema 类型编码，到不了这条路径；字段类型与值不符时拒绝，与其他类型的处理一致 |
| 指定分区、尚未 flush 的删除，目标主键在其他分区的已提交 segment 里 | MemTable 对每个主键只记最近一次删除的序号与分区，稀疏检索只在这条删除不限分区或与 segment 的分区相同时屏蔽该行，所以 flush 之前仍返回它；flush 之后 delta 全局生效，稠密检索也始终全局屏蔽 | 稀疏检索与稠密检索一样始终按主键全局屏蔽 | 3.x 的稀疏检索在这一点上与自己 flush 之后的结果、与稠密检索都不一致；本实现沿用 [0017](0017-partitions.md) 已实现的「删除按主键全局生效」。照搬需要在分区可见性组件中记录每条待提交删除的分区，冻结代还要另定规则，风险与收益不相称 |
| 动态字段里的稀疏值带 NaN / Inf 权重 | 写出非法 JSON，之后读取时 pymilvus 报 `JSONDecodeError` | 写入时报 `field 'extra' value nan is not representable in JSON`，不落盘 | 与本实现对动态字段里其他非有限浮点的处理相同，不写出坏数据 |

稀疏字段的 `bytes_data` 默认值（只有原始 gRPC 能发出）沿用已有的「binary default values」UNIMPLEMENTED，先于 3.x 的 `must be a dict, got bytes` 报出。建索引的 `params` / `search_params` 与检索请求的 `params` 不是严格 JSON（含 `NaN`、`Infinity`、`-Infinity` 字面量或像 JSON 容器的坏文本）时沿用 0014 的 UNIMPLEMENTED；3.x 把坏文本当作 `{}`，把 `NaN` 等读成浮点后由调参项校验拒绝。溢出的数字（如 `1e999`）是严格 JSON：调参项照常按 3.x 校验并报相同的错误；其余键里的溢出数在建索引时无法写进定义，沿用 0014 的 UNIMPLEMENTED。稀疏字段让这些参数第一次有了语义，差异由此可见，但仍只有原始 gRPC 能发出。

稠密字段上的 `SPARSE_INVERTED_INDEX` 定义，以及 MemTable 走 Python 路线，都与 3.x 一致，不算差异，但会在 [0006](0006-m2-scope.md) 的差异表中注明，避免被误当作缺陷修掉。

## 实施步骤

每一步做完由子 agent 复核，直到没有问题。

1. **类型、schema 与存储**：类型与谓词改名，`vector_pos` 改为可选，schema 校验，`schema.json`，Arrow / Parquet / WAL 列，写入校验与存在性检查，引擎接受 null 而 gRPC 拒绝，输出编码（原样输出、紧凑 nullable、`dim`），partial upsert 继承，compaction，过滤类型表。稀疏字段上各 schema 属性在 3.x 的行为逐项实测后固定。
2. **索引与度量**：`build_spec` 分支、调参项校验、load 校验与回滚、检索度量解析，以及稠密字段上的 SPARSE 定义。
3. **检索**：HybridSearch 的 `group_by_field` 可以指向稀疏字段：3.x 候选行里的稀疏值是 bytes，`rerank::pyval` 相应增加 bytes 值，按字节分组，与 3.x 实测一致；倒排与缓存（槽位、代号、release / drop / create 时清空），segment 与 MemTable 两条路径，排序键，错误先后，接入后半段、HybridSearch 与 iterator。
4. **差分、互通、文档与性能**：
   - 差分覆盖 MemTable、flush 与重开；compaction 由互通的 compacted 状态覆盖（后台合并的时机两边不同，差分不比较合并前后的瞬时结果）。
   - 坏行与 nullable 用 3.x 引擎 API 建的目录和改写 parquet / WAL 的目录测试。
   - Arrow 15 / 25 双向互通。
   - 文档同步 0002、0006、0014、AGENTS、README。
   - 对照 3.x 与 2.5.1 的检索性能（热查询、首次检索的建倒排耗时、RSS、倒排的实测内存）。

## 验收

下表是 0030 合入时（`59716a4`）的当次结果，之后的用例计数见各切片自己的验收记录。

| 项目 | 结果 |
| --- | --- |
| Rust 默认 / fault-injection | 613 / 751 通过，clippy 通过；[sparse_vectors.rs](../../crates/core/tests/sparse_vectors.rs) 20 条，另有 `sparse_index` 与倒排缓存的单测 |
| 端到端：pymilvus 3.0.2 / 2.6.18 | 各 602 条通过（门槛判定）；迁入参照 `test_milvus_sparse_compat.py` 15 个函数 27 条，盘点见 [INVENTORY](../../tests/python/adapter/INVENTORY.md) |
| 差分：pymilvus 3.0.2 / 2.6.18 | 各 782 条通过；[test_sparse_vectors.py](../../tests/differential/test_sparse_vectors.py) 9 条跑 [sparse_cases.py](../../tests/differential/sparse_cases.py) 的 8 个场景（含 partition key 换桶）与重开，逐步比较，稀疏分数要求逐位相同，含稠密距离的步骤按 0004 的误差；登记差异两边分别断言 |
| 互通：Arrow 15 / 25 | 296 条通过；[test_sparse_vectors.py](../../tests/storage_interop/test_sparse_vectors.py) 10 条，覆盖三方轮写的 WAL / flush / compaction，以及 3.x 引擎 API 写的 nullable、默认值、parquet 坏行（含分区 p1 的坏行在其行全部删除后按分区检索）与 WAL 坏行目录 |
| 反证 | 去掉缓存代号检查、把带分区的稀疏检索改回稠密的分区语义、把查询分段的合并下标改错，对应用例都失败 |
| 工具自检 | 956 条通过 |

稀疏检索专项（[benchmarks/sparse](../../benchmarks/sparse/README.md)：10 万行、每行 32 个非零维、3 万维幂律分布，SPARSE_INVERTED_INDEX + IP，同机三轮轮换取中位数，每次检索都核对精确内积）：

| 指标 | 本实现 | 3.x（`1320d26`） | 2.5.1 |
| --- | --- | --- | --- |
| 写入 10 万行（ms） | 1545 | 2643 | 2549 |
| flush（ms） | 99 | 404 | 0.4 |
| 首次检索，含建倒排（ms） | 52 | 1128 | 1.35 |
| nq=1，top10（ms） | 1.16 | 159 | 1.13 |
| nq=32，top10（ms） | 5.82 | 4363 | 4.59 |
| 过滤一半行，nq=1（ms） | 0.90 | 122 | 1.14 |
| 进程树 RSS（MiB） | 638（637–740） | 1292 | 641 |

机器 15 核，本实现的计算池取全部核；RSS 中约 505 MiB 是基准脚本自身的数据，三方共有，本实现有一轮多出约 100 MiB 是分配器暂未归还写入阶段释放的内存。2.5.1 在 flush / load 时建好索引，首次检索不含建倒排；本实现与 3.x 一样在首次检索时按 segment 构建。nq=32 仍比 2.5.1 慢约 27%，单条查询与过滤检索追平或更快。

稠密单向量回归（20 万行、128 维、FLAT、进程内，m2 口径三轮轮换，`17334e1` 对本切片）：写入 129,800 对 131,100 行/秒，nq=1 均为 0.77 ms，nq=100 为 24.46 对 24.48 ms，过滤 10% 为 0.48 对 0.47 ms，各项都在轮次波动内，`RankedRow` 增加的两个排序字段没有造成可测的退化。

## 下一个候选需要写明

支持稀疏向量字段与 SPARSE_INVERTED_INDEX + IP；上表的五项差异；此前的版本把带稀疏字段的集合当作含不支持内容的集合（可以 drop，读写返回 UNIMPLEMENTED）；nq 较大时的检索仍慢于 2.5.1。

