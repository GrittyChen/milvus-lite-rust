# 0038：nullable 稠密向量

状态：已实施（2026-11-19），待提交前评审。设计经首轮评审与复审，「决策」三项由负责人确认（决策 1 在首轮评审发现 pyarrow 低于 26 的互通问题后重新确认）；实施结果与设计调整见「实施结果」。

参照版本 `1320d26`。实测用 3.2.2 正式包；`v3.2.2..1320d26` 的三个提交（HNSW 精确检索回退、Parquet 先落盘再提交 manifest、无过滤 count 的分区）都不涉及本功能。

## 范围

本文覆盖 `FLOAT_VECTOR` 字段上的 `nullable=True`，包括以下环节：
- 建表与 DescribeCollection；
- 写入：Insert、Upsert、partial upsert；
- 输出：Query、Get、Search、Query / Search iterator、`output_fields=*`；
- 检索：普通、range、分组、HybridSearch、function chain；
- 多向量字段集合的三种布局：首向量 nullable、非首向量 nullable、两者都 nullable；
- flush、重开、compaction；
- 与 3.x 数据目录的互通、降级、dump。

不包含：
- **向量默认值**：pymilvus 在客户端就拒绝（`Default value unsupported data type: 999`），proto 的 `ValueField` 也表达不了向量。引擎与 `schema.json` 读取器继续拒绝，文案改为只提 vector defaults。
- **在向量字段上过滤**（如 `vec is null`）：3.x 报 `field 'vec' of type float_vector cannot be used in scalar filter expressions`，本实现保持一致。
- **按主键检索**（`search(ids=...)`）：两边都不支持，缺少 placeholder_group 时报同一个错。
- **AddCollectionField 新增向量字段**：3.x 没有实现。

[0015](0015-nullable-defaults-and-array.md#nullable-向量调查结论与延后) 定的两条约束继续沿用：
- 不为空值语义引入私有磁盘标记；
- 不把真实的零向量判为 null。

## 3.x 的行为

### 代码

- **写入**：[collection.py](../../references/milvus-lite/milvus_lite/engine/collection.py) 的 `_build_wal_data_batch` 在构造 WAL batch 时把 None 换成零向量。同一个 batch 随后进入 WAL 和 MemTable，之后的 Parquet 和 compaction 都没有空值信息。0002 已把这一点登记为继承风险。
- **校验**：[validation.py](../../references/milvus-lite/milvus_lite/schema/validation.py) 的 `validate_record` 只检查第一个向量字段。nullable 时，None 或缺键都放行；其余向量字段不检查。
- **解码**：[records.py](../../references/milvus-lite/milvus_lite/adapter/grpc/translators/records.py) 的 `_extract_vector_column` 分三种情况：
  - 完整形态（数据长度 = 行数 × dim）：不看 `valid_data`；
  - 紧凑形态（数据长度 = 有效行数 × dim）：按 `valid_data` 插入 None，不校验 `valid_data` 的长度；
  - 没设 `float_vector`：报 `FieldData 'vec' declared FloatVector but no float_vector data is set`。`float_vector` 已设置但为空、`valid_data` 全为 False 时，满足 `0 == 有效行数 × dim`，得到全 None，可以写入。
- **读 segment**：[segment.py](../../references/milvus-lite/milvus_lite/storage/segment.py) 的 `_extract_vector_array` 遇到 Arrow null 会生成 `vector_null_mask`，`executor_indexed` 检索时排除这些行。3.x 自己从不写 Arrow null，所以这条路径只在读别人写的数据时生效。
- **MemTable 检索**：把 None 换成零向量，不排除。
- **输出**：[records.py](../../references/milvus-lite/milvus_lite/adapter/grpc/translators/records.py) 的 `_build_field_data` 遇到 None 时，按完整形态填零占位，再附上 `valid_data`。pymilvus 按紧凑形态解码：物理下标由 `valid_data` 的前缀和算出（`search_result._get_physical_index`、`field_data_extractors.decode_cell`）。结果是 null 之后的有效行全部错位，与 [0030](0030-sparse-vectors.md#与-3x-的差异) 登记的稀疏输出缺陷相同。

### 实测

**测试数据**：id 0 `[1,0]`、id 1 None、id 2 缺键、id 3 真实零向量、id 4 `[0,1]`；查询向量 `[1,0]`。两条 SDK 线（pymilvus 3.0.2、2.6.18；CI 锁的是 2.6.17，复审核对过它与 2.6.18 的打包与解码代码在向量上逐字相同）在 flush 前、flush 后、换进程重开三个时点的结果逐项相同：

| 操作 | 3.x 结果 |
| --- | --- |
| DescribeCollection | `nullable: true` |
| Query / Get / query iterator / `*` | id 1、2 返回 `[0.0, 0.0]`，与 id 3 无法区分 |
| Search（FLAT；L2 / IP / COSINE） | null 行按零向量参与：L2 距离 1.0，IP 与 COSINE 为 0.0。range、分组、search iterator、HybridSearch 各路相同 |
| 整批都是 null 的 Insert / 全量 Upsert / partial Upsert | pymilvus 两条线都发 `vectors.dim=2`、不设 `float_vector`、`valid_data` 全为 False，3.x 报 `declared FloatVector but no float_vector data is set`。批中有一行非 null 即可写入 |
| partial upsert 省略向量 | 继承旧值 |
| 先建表、后建索引再 load | 同上 |
| 多向量布局 A（首 nullable）/ B（非首 nullable）/ C（都 nullable） | 批量写入、检索、flush、partial upsert 都按零向量处理（评审实测） |
| 向量默认值 | pymilvus 客户端拒绝 |
| 非 nullable 字段写 None | 报错同整批 null；缺键时 pymilvus 客户端先报 `Insert missed an field` |

**3.x 读取 Arrow null（互通方向）**：
- 构造方法：
  - 用 arrow-rs 60 按本实现的写入属性写出 Parquet 与 WAL；
  - 或者让 3.x 自己写入并 flush 一批，再写一批后直接退出（留下 WAL），然后用 pyarrow 26 把 null 行改写成 Arrow null。
- 结果取决于 3.x 环境的 pyarrow 版本。3.x 声明的依赖是 `pyarrow>=15.0`，见 [pyproject.toml](../../references/milvus-lite/pyproject.toml)。

| 3.x 环境 | 含 null 的 Parquet | 只有 WAL 含 null |
| --- | --- | --- |
| pyarrow 15.0.0、18.1.0、20.0.0、22.0.0、24.0.0、25.0.1 | 读取报 `Expected all lists to be of size=2 but index 2 had size=0`，load、count、query、search 全部失败，get 在 Describe 阶段失败，整个集合不可用。pyarrow 25 连自己写的这类 Parquet 都读不回 | 可以回放，null 行按零向量参与检索。3.x flush 时失败：Arrow 25 报上面的错，Arrow 15 报 `Lists with non-zero length null components are not supported`。Arrow 25 上 flush 失败后，同一进程内 count 为 0、query 为空，重开后报错。盘上数据仍在，pyarrow 26 与本实现都能读 |
| pyarrow 26.0.0、26.0.1 | count 正确；null 行 Query / Get 返回 None；多行输出中排在 null 之后的有效行错位（id 4 读出 `[0,0]`，实为 `[0,1]`），单行 Get 不受影响；检索排除 null 行；保留或删除 `.idx` 侧车结果相同；compaction 保留 null | 回放后 null 行按零向量参与检索；flush 后 null 保留在 Parquet 中，重开后被排除。3.x 对这些行做 upsert 或 partial upsert，会把 null 永久改写成零向量 |

pyarrow 26 没有 CPython 3.10 的 wheel（见 [0037「放弃的方案」](0037-dump.md#放弃的方案)），所以 3.x 在 Python 3.10 上只能用 pyarrow 25 及以下。

**真实 Milvus 的语义**（上游 `tests/python_client` 中 hybrid、range、search_v2、按主键检索的 nullable 向量用例）：
- null 行不参与该字段的检索，距离不会出现 NaN；
- 按主键检索时，主键对应的向量为 null，该路返回空结果；
- 输出用紧凑形态，pymilvus 读出 None。

**本实现现状**：
- 0.6.0rc1 建表时返回 `UNIMPLEMENTED: MilvusLite does not support nullable vectors`。
- 打开含 nullable 向量的目录时，`list_collections` 成功，`describe`、`load`、`count`、`insert` 都返回 UNIMPLEMENTED（评审实测）。

## 决策

### 1. 空值语义：按真实 Milvus，登记与 3.x 的差异

null 落盘为 Arrow null：输出读出 None，检索排除 null 行。

**理由**：
- 3.x 的做法等于静默改写用户数据：写入 None 读回 `[0,…]`，检索还把这些行当作与任何查询“等距”的候选混进结果。在 COSINE / IP 下，它们排在所有负相关的真实行前面。
- 0002 已把这一点登记为风险，0015 明确不照搬。
- 两条 pymilvus 线都按真实 Milvus 的约定处理紧凑形态。
- 3.x 自己的稀疏向量就是“存为 null、检索排除”，这样做稠密与稀疏一致。

**接受的互通代价**：只有真正写过 null 向量的集合受影响。3.x 在 pyarrow 低于 26 时打不开这类集合，flush 含 null 的 WAL 会失败（见上表）；pyarrow 26 时有输出错位等问题。数据不会丢，可以升级 pyarrow 到 26 及以上（Python 3.11 及以上），或者用本实现打开。

这些代价修订 0002 的约束，并登记为风险（见「与 3.x 的差异」）。负责人在首轮评审给出上述证据后，再次确认选择本方案。

**反方向**：3.x 写下的目录中，null 已经是零向量，本实现只能按零向量读出。这是 3.x 写入时就丢失的信息，不算差异，写进限制说明。

**已放弃的备选**：照搬 3.x 存零向量（见「放弃的方案」）。

### 2. 整批都是 null：接受

nullable 字段遇到下面两种形态时，都按全部为 null 写入：
- `float_vector` 未设置、`valid_data` 全为 False：两条 SDK 线的实际编码，3.x 对此报错；
- `float_vector` 已设置但为空、`valid_data` 全为 False：3.x 本来就接受。

3.x 本来就接受后一种，对前一种报错只是解码器没考虑到。

非 nullable 字段：
- 不带 `valid_data`、`float_vector` 未设置：仍报 3.x 原文 `declared FloatVector but no float_vector data is set`。这是 pymilvus 对非 nullable 字段的实际编码。
- 带全为 False 的 `valid_data`：解码层不知道字段是否 nullable，只能产出空值，由引擎报 `vector field 'vec' missing or None`。只有原始 gRPC 能发出这种形态，登记为差异（见决策 3）。

### 3. 原始 gRPC：`valid_data` 是空值的唯一依据

- 3.x 对向量按完整形态解码时忽略 `valid_data`，占位值被当成真实向量写入。3.x 对标量的规则则是：完整形态带 `valid_data` 时，把对应行置为 None，非 nullable 字段随后报 `is None but not nullable`。
- 本实现对向量采用标量的规则：
  - 只要 `valid_data` 中有 False，对应行就是 null；
  - 字段不是 nullable 时，由引擎按 3.x 第一个向量字段的文案 `vector field 'vec' missing or None` 拒绝；
  - `valid_data` 非空但长度不等于行数时，报标量路径的原文 `FieldData 'vec' valid_data length X != num_rows N`。
- 这几条只有原始 gRPC 能发出（pymilvus 总是发紧凑形态，并且只为 nullable 字段附 `valid_data`），一并登记为差异。

## 设计

### schema

- 解除以下三处对稠密向量 nullable 的拒绝，向量默认值仍然拒绝：
  - [schema.rs](../../crates/core/src/schema.rs) 的 `validate`；
  - [grpc/schema.rs](../../crates/grpc/src/schema.rs) 的 `note("nullable vectors")`；
  - [schema_file.rs](../../crates/core/src/storage/schema_file.rs) 读取 `schema.json` 时的 `float_vector` 属性检查。
- `schema.json` 中 `nullable` 的写法本来就与 3.x 相同。

### 写入

- **解码**（[insert.rs](../../crates/grpc/src/insert.rs)）：
  - `float_vector` 与 `check_valid_data` 按决策 2、3 转成 `InputValues::Nullable { FloatVector }`。
  - 紧凑形态与整批 null 都展开成完整长度（`n × dim`），null 槽位填零。[data.rs](../../crates/core/src/data.rs) 的 `check_column_len` 递归要求内层长度等于 `n × dim`。
  - `vectors.dim` 仍须为正。
  - 单测覆盖四种输入：`float_vector` 未设置、已设置但为空、紧凑、完整。
  - 解码层不知道字段是否 nullable，只负责产出空值；nullable 检查交给引擎。
  - 现有的 `unsupported("null values")` 删除。
- **引擎校验**（[collection.rs](../../crates/core/src/collection.rs)）：
  - `validate_row`（第一个向量字段）与 `check_other_vectors`（其余向量字段）会遇到三种空值：
    - insert 路径的 `Nullable { FloatVector }` 中的 null 行；
    - upsert 与 partition key 路径中，旧行为 null 时 `input_from_column` 给出的 `Null(1)`；
    - 缺列。
  - 字段 nullable 时，三种空值一律放行；有效行先用 `values()` 解开包装，再校验 dim。
  - 字段不是 nullable 时，报 3.x 原文。
  - `check_other_vectors` 对 nullable 的非首向量字段放行 None 与缺键，非 nullable 的非首向量字段仍按 [0028](0028-multi-vector-fields.md#与-3x-的差异) 拒绝。
  - 收集 `other_vectors` 的转换（[collection.rs](../../crates/core/src/collection.rs) 中 `Some(i) != self.vector_pos` 一段）要能处理空值。
  - 两处缺列检查对 nullable 向量字段跳过：insert 路径（`!has_column(&vector.name)`）、partition key 路径（[partition_key.rs](../../crates/core/src/collection/partition_key.rs) 的 `!has(&vector.name)`）。缺整列视为全 null，与 3.x `validate_record` 一致。
- **转换**：`convert_optional` 现在遇到 null 时逐行克隆、再复制一次，`append_column` 遇到 validity 也退化为逐行 `push_row`。对 `Nullable { FloatVector }` 改走整列路径：输入已是完整长度的值加 validity，直接拼接，避免每行一次分配。

### 数据层与 MemTable

- [data.rs](../../crates/core/src/data.rs)：
  - `push_row` 遇到 null 行时，构造空列的 `match` 补上 `FloatVector` 分支。这里目前是 `unreachable!`，compaction、查询结果的 gather、partition key 重组、逐行写入都经过它。
  - `nulls()` 的注释改为：nullable 向量与 boost 剥掉同名字段时的占位都走这里。
- [flat.rs](../../crates/core/src/flat.rs) 的 `row_norms`：用 `values()` 解开包装，null 行的值槽为零、范数为 0，不会被用到。现在它只认未包装的列，会漏算，导致 `norms` 长度不足。
- [collection.rs](../../crates/core/src/collection.rs) 的 `vector_values`：先 `.values()` 再取向量，同时返回 validity。
- [memtable.rs](../../crates/core/src/storage/memtable.rs) 的 `validate_decoded`：
  - 第 636 行目前禁止向量列带 validity，改为只在字段是 nullable 时允许；
  - 第 675 行的 dim 检查改为先解开包装。
- 内存与 WAL 中，null 行的值槽占 `dim × 4` 字节（填零）；Parquet 不存 null 子元素。这是接受的取舍：值槽完整，扫描与范数才能按行号直接定位。

### 持久化（[arrow_data.rs](../../crates/core/src/storage/arrow_data.rs)）

- **写入**：
  - WAL 与 Parquet 都用带 validity 的 `FixedSizeList<float32>`，null 槽位的子元素为非 null 的零。3.x 的 `_extract_vector_array` 按 `n × dim` 重塑 values，要求值缓冲区完整。
  - 实现位置：`nullable_array` 目前对 `ColumnData::FloatVector` 走 `_ => TypeMismatch`，要补一个 `FixedSizeListArray::try_new(..., Some(NullBuffer))` 分支。
  - 只在确有 null 时附 `NullBuffer`。`with_validity` 在全部有效时已经解开包装，这一点要保持：没写过 null 的 nullable 向量集合，写出的文件与非 nullable 时相同，pyarrow 15 / 25 的 3.x 照常能读。
  - 3.x 在 `_build_wal_data_batch` 的注释里写了 `Arrow Parquet doesn't support null in FixedSizeList`，说明原作者是有意避开这种形态的。本实现写入它，是决策 1 接受的代价。
- **读取**：
  - 父级 null 只在字段 nullable 时允许。
  - 子元素 null 只允许出现在父级为 null 的行内，读出后这些槽位填零。Parquet 读回的 null 行子元素是 null（arrow-rs、pyarrow 25、pyarrow 26 写出的文件均如此），WAL 中是原样的零。
  - 以下情况照旧拒绝：有效行内的子元素 null；非 nullable 字段上的父级 null，包括 0028 登记的“3.x 目录中非首向量字段含 null”。
- **compaction**：经修正后的 `push_row` 携带 validity。
- **格式版本不变**：Arrow schema 的 nullable 标记两边本来就按字段属性写，Arrow null 也是 3.x 读路径已有处理的取值。按规则 8，兼容影响登记在 0002，包括 pyarrow 版本要求。

### 检索

- 稠密扫描（`scan` / `scan_tiled`）取得列时一并取得 validity，在 `chunk.live` 之外再查它，跳过 null 行。`scan_tiled` 把这项判断提到循环外，不带 validity 的列走与引入空向量之前相同的循环（第一轮服务器对照中，逐行判断让 100 万行、nq 1000 慢了约 1.8%）；`scan` 只处理至多 4 条查询，对照中没有可见差异，保留逐行判断。
- 普通检索、range、分组、search iterator、HybridSearch 的每一路、function chain 都经过这两个函数。实施时用 grep 核对是否另有入口。
- 多向量集合只在被检索的字段上排除 null。
- 不引入 O(总行数) 的额外遍历，扫描仍在计算池的叶子任务内，不拿锁（0001、0005）。
- 非 nullable 集合的检索不应回退：在 x86 服务器上用 [flat_batch_probe](../../crates/core/examples/flat_batch_probe.rs) 比较改动前后各一轮，作为规则 7 的反面证据。

### 输出（[output.rs](../../crates/grpc/src/output.rs)）

- 结果中既有 null 又有有效行时：`valid_data` 长度等于结果行数，`float_vector` 只含有效行，`vectors.dim` 取 schema 的 dim。
- 全部为 null 时与 3.x 相同：每行 dim 个零的完整占位加全 False 的 `valid_data`。没有可读错的有效行，pymilvus 两条线都读出 None；这样 boost 剥掉同名向量字段的输出与 3.x 逐字节相同（[test_rerank_boost.py](../../tests/differential/test_rerank_boost.py) 固定了这一点）。
- 是否附 `valid_data`，沿用标量列现有的条件。
- Query、Get、Search、iterator、`*` 共用同一套编码，与 0030 的稀疏输出相同。
- boost 剥掉同名向量字段时的占位输出（[collection.rs](../../crates/core/src/collection.rs) 的 `strip_rows`）：全部剥掉时与 3.x 相同；只剥掉部分行（HybridSearch 中只有部分路经过 boost）时变成紧凑形态，不再错位。只有向量字段名恰为 `$id` 等时才会触发。

### Upsert

- 全量 upsert：省略向量或给 None，都写为 null。
- partial upsert：省略向量时继承旧值（包括 null），显式给 None 时置为 null。
- partition key 的合并路径与非 key 路径相同。

### 其他

- **dump**：不改代码。BulkWriter 收下 None，输出 `"vec": null`（pymilvus 3.0.2 实测）。dump 差分中，3.x 导出零向量、本实现导出 null，登记为差异。[0037](0037-dump.md) 已知限制中的 nullable 向量一条随之改写。
- **降级**：0.6.0rc1 及更早版本打开含 nullable 向量的集合时，照旧拒绝该集合（UNIMPLEMENTED），数据保留。

## 与 3.x 的差异

| 差异 | 3.x | 本实现 | 理由 |
| --- | --- | --- | --- |
| null 向量的存储与输出 | 写入时换成零向量，读出 `[0,…]` | 存为 Arrow null，读出 None | 决策 1 |
| 检索中的 null 行 | 按零向量参与 | 排除 | 决策 1；与 3.x 自己的稀疏向量、真实 Milvus 一致 |
| 整批 null，`float_vector` 未设置 | 报 `declared FloatVector but no float_vector data is set` | nullable 时接受 | 决策 2；pymilvus 的实际编码 |
| 原始 gRPC：完整形态带 `valid_data` | 忽略 `valid_data`，占位值当作向量写入，非 nullable 字段也一样 | 按 `valid_data` 置 null，非 nullable 时报 `vector field 'vec' missing or None` | 决策 3；与 3.x 对标量的规则一致 |
| 原始 gRPC：向量的 `valid_data` 长度不等于行数 | 不检查，按长度匹配的形态解码或报长度错误 | 报 `valid_data length X != num_rows N` | 决策 3；与 3.x 对标量的规则一致 |
| 非首向量字段为 nullable 时写入 None | 接受，按零向量处理 | 接受，存为 null | 决策 1；0028 对非 nullable 非首向量字段的拒绝不变 |
| dump 导出 null 向量 | 导出零向量 | 导出 `null` | 决策 1 的直接结果 |
| 空行补零后超过一个请求能携带的浮点数（行数 × dim 大于 `MAX_MESSAGE_SIZE / 4`，即 6400 万；含缺列、没有任何列时 `num_rows` 很大的情形） | 不设上限，按行补零，内存不足时报 MemoryError | 不分配，报 `vector field 'v' has N rows of dim D, more than the 67108864 floats one insert request can carry; split the batch` | 提交前评审发现：一个空行在请求里只占 `valid_data` 的 1 字节，补零后占 4 × dim 字节，原始 gRPC 能让进程因分配失败而 abort（规则 6）。全部有效时能写入的批量都不受影响；只有借助大量空行才能凑出的大批量被拒绝，拆小即可。负责人确认 |

### 登记进 0002 的内容

- **约束修订**：
  - 约束 1“共享文件只写 3.x 能接受的键和值”加一条例外：nullable 稠密向量中的 null 写为 Arrow null，pyarrow 低于 26 的 3.x 无法读取含它的 Parquet，也无法 flush 含它的 WAL。
  - 「验证」中“库版本组合”一条（第 148 行）同步写明：含 null 向量的文件，3.x 一侧需要 pyarrow 26 及以上。
  - 规则 8 要求不得静默：除 0002 与 README 外，服务首次为某个集合写入 null 向量时，向 stderr 打一行 warning 日志（集合每次打开后只打一次；stderr 写不出去时忽略，不影响写入），说明“3.x 在 pyarrow 低于 26 时无法打开该集合”。
- **新增风险**（「继承的 3.x 已知风险」）：
  - pyarrow 低于 26 时，含 null 向量 Parquet 的集合在 3.x 中整个不可用；只在 WAL 含 null 时，3.x 能回放，但 flush 失败。Arrow 25 上 flush 失败后，同一进程内 count 为 0，重开后报错。数据保留在盘上。CPython 3.10 没有 pyarrow 26 的 wheel。
  - pyarrow 26 及以上时：
    - 多行输出中排在 null 之后的向量错位（3.x 输出缺陷）；
    - 尚未 flush 的 null 行按零向量参与检索；
    - 3.x 对这些行做 upsert 或 partial upsert，会把 null 永久改写成零向量。
- **原有一条**“nullable 向量用全零表示 null”，改为只描述 3.x 自己写下的数据。

## 测试

### Rust 单测

- 解码：三种形态与整批 null 的两种形态、非 nullable 的报错、`valid_data` 长度不符的报错。
- `Nullable { FloatVector }` 经以下路径往返或处理：`push_row`、gather、compaction、`row_norms`、Arrow / WAL / Parquet。
- Parquet 读取：arrow-rs 自写的 null 文件读回；有效行内的子元素 null、非 nullable 字段上的父级 null 照旧被拒绝。
- memtable 校验。
- 扫描排除 null 行：单条查询、一组 BATCH、分片多组，各在 MemTable 与 segment 上。

### 端到端

新增 `tests/python/test_nullable_vectors.py`，pymilvus 两条线都跑：
- 建表与 Describe；None、缺键、真实零向量的写入与读出；
- flush 前、flush 后、重开三个时点；
- L2 / IP / COSINE 下的普通检索、range、分组、search iterator、HybridSearch；
- 多向量布局 A / B / C；
- 全量与 partial upsert；整批 null；
- Query 结果全为 null 时两条 SDK 线都读出 None；
- 原始 gRPC 的完整形态带 `valid_data`，以及 `valid_data` 长度不符；
- 向量过滤与默认值的拒绝；
- 首次写入 null 时的 warning 日志只打一次；stderr 指向已断开的管道时写入照常成功。

boost 剥掉同名向量字段的占位输出只在字段名恰为 `$id` 等时出现，由 [data.rs](../../crates/core/src/data.rs) 的单测覆盖 `blank_rows` 与紧凑输出，不另写端到端。

### 差分（`tests/differential`）

- 一致的部分：Describe、count、非 null 行的检索与输出、各项报错，以及布局 A / B / C 的非 null 行。
- 登记的差异：两边各自按上表断言，并在用例中注明。

### 互通（`tests/storage_interop`）

新增 pyarrow 26 的参照环境：
- 新锁文件 `ref26`：`ref26.in` 是把 `ref25.in` 中的 pyarrow 换成 26.0.1。
  - 生成命令沿用 `ref25` 的 `--exclude-newer 2026-10-08T00:00:00Z`，另加 `--exclude-newer-package pyarrow=2026-11-05T00:00:00Z`。
  - pyarrow 26.0.1 的 wheel 最晚于 2026-11-03 10:54 上传；只放宽 pyarrow，其余包与 `ref25` 逐行相同。整体放宽日期会改动 13 个共同包，0037 已否决这种做法。
  - `.txt` 头部的命令会记录这个参数。
- 新环境变量 `MILVUS_LITE_ARROW26_PYTHON`。

用例：
- 本实现写下的 Parquet 与 WAL 交给 3.x：
  - Arrow 15 / 25：按登记的失败形态断言，即 Parquet 读取报错、WAL 回放后检索出零向量、flush 报错。这些失败就是风险的证据。
  - Arrow 26：count、单行 Get 正确；检索排除 Parquet 中的 null 行。登记的错位、WAL 中的零向量参与检索、upsert 改写为零向量，按 3.x 的实际结果断言。3.x 的 compaction 保留 null 是首轮评审的实测，没有纳入用例。
- 没写过 null 的 nullable 向量集合：WAL 与 Parquet 都由 pyarrow 15 / 25 的 3.x 正常打开，检索结果一致。这条是决策 1 中“只有真正写过 null 的集合受影响”的证据。
- 3.x（Arrow 26）回放本实现的 null WAL 并 flush 后，写出的 Parquet 由本实现读回。
- 3.x 写下的 nullable 向量目录交给本实现：null 读为零向量，可以继续写入 null。
- 本实现一侧经 pymilvus 写入目录（与 [test_nullable_scalars.py](../../tests/storage_interop/test_nullable_scalars.py) 相同），不需要改 [storage_interop.rs](../../crates/core/examples/storage_interop.rs)。

### 其他测试

- **降级**：[previous_reader.py](../../tests/release/previous_reader.py) 加一个用例：
  - 新版本写出含 null 向量的目录，交给上一版本打开；
  - 用例先探测上一版本能否创建 nullable 向量集合：
    - 不能（本功能发布前的版本）：断言拒绝该集合、文件不变、新版本重开后值正确；
    - 能（本功能发布后的版本）：断言值一致，包括 None。
  - 这样用例在本功能发布后的候选中仍然有效。写入与读取代码按 `snippet()` 的约定，从互通用例的 `RUST_WRITE=r'''` 一类标记中取。
- **dump**：`tests/dump/e2e` 加一条，null 向量导出为 `null`。
- **需要修改的既有测试**：
  - [test_grpc_translation.py](../../tests/python/test_grpc_translation.py) 中断言 nullable 向量返回 UNIMPLEMENTED 的一项删去；
  - `test_delete_with_filters_nullable_vector_field` 改后复制：去掉建表失败时的 `try/except + pytest.skip`，让失败直接报错（规则 4）。INVENTORY 按“改后复制”登记，并同步表头的“范围外”“已复制”与展开后的条数。

### 反证

用变异确认以下改动各有用例能抓住：

| 变异 | 应抓住它的用例 |
| --- | --- |
| 去掉扫描中的 validity 检查 | 端到端检索、Rust 扫描单测 |
| 输出改回完整占位 | 端到端多行 Query（错位） |
| compaction 丢掉 validity | Rust compaction 单测、端到端重开后输出 |
| Parquet 读取不填零或不放行子元素 null | Rust Parquet 单测、端到端 flush 后重开 |
| 解码忽略完整形态的 `valid_data` | 原始 gRPC 端到端 |
| 非首向量字段放行非 nullable 的 None | 布局 B 的端到端与差分 |
| 全部有效时也附 `NullBuffer` | 互通中没写过 null 的集合交给 Arrow 15 / 25 |

## 需要同步的计数与清单

- **计数**：以下套件的新计数要同步到各处：
  - 套件：`tests/python`（752）、差分（867 条与 405 个分组，`--expect-group` 随新用例使用的夹具变化）、互通（321）、`tests/dump/e2e`（28）、`tests/release`（previous-reader 6 个用例）、`tests/ci`；
  - 位置：AGENTS「常用命令」、[tests/packaging/README.md](../../tests/packaging/README.md)、[ci.yml](../../.github/workflows/ci.yml) 与 [compare.yml](../../.github/workflows/compare.yml) 的 `--expect`、[scorecard.py](../../tests/release/scorecard.py)（`PYTHON_SUITE`、`DIFFERENTIAL`、`INTEROP`、`DUMP_E2E`、`PREVIOUS_READER_CASES`）、[test_candidate.py](../../tests/release/test_candidate.py) 的硬编码计数。
- **工作流**：
  - [compare.yml](../../.github/workflows/compare.yml) 与 [release.yml](../../.github/workflows/release.yml) 的互通步骤：建 `ref26` 环境（同样装 jieba 与参照 checkout），设置 `MILVUS_LITE_ARROW26_PYTHON`。
  - `ref26` 锁文件按 0037 的方式生成，并沿用 `argon2-cffi-bindings<26`、`cryptography<49` 两条约束。[tests/ci/test_dump_requirements.py](../../tests/ci/test_dump_requirements.py) 的参数化加上 `ref26`，再加一条断言：`ref26.txt` 与 `ref25.txt` 除 pyarrow 一行外逐行相同，防止以后整体放宽日期。
  - [.github/ci/README.md](../../.github/ci/README.md) 的固定依赖表加一行。
- **重新计数**：按 0020 判定规则 2，对受影响的 job 重新计数（CI 的 python 与 Rust job、Compare 的差分与互通 job、发行的 python、diff 与 previous-reader）。0037 剩下的 CI 1 次、Compare 2 次并入本批。次数与运行预算在实施完成后另行提出。

## 文档

- [README](../../README.md)：
  - nullable 一条改为“主键的 nullable 仍不支持”；
  - 加入 nullable 稠密向量的语义、与 3.x 的差异、3.x 打开时的 pyarrow 要求与限制、3.x 写下的 null 读为零向量、降级行为；
  - 改完后必跑 `tests/packaging`。
- [0002](0002-data-dir-compat.md)：
  - 类型表中 `FLOAT_VECTOR` 一行；
  - 约束 1 的例外与「验证」第 148 行；
  - 「继承的 3.x 已知风险」（见上文）。
- [0006](0006-m2-scope.md)：差异总表加一行；第 323 行起的拒绝条件去掉 nullable 向量。
- [0015](0015-nullable-defaults-and-array.md)：「nullable 向量」一节末尾指向本文。
- [0028](0028-multi-vector-fields.md)：
  - 差异表中“非首向量字段缺失或为 None”与“3.x 目录中非首向量字段含 null”两行，限定为非 nullable 字段；
  - 「恢复」一节相应说明。
- [0037](0037-dump.md)：已知限制中的 nullable 向量一条。
- [tests/storage_interop/README.md](../../tests/storage_interop/README.md)：Arrow 26 环境的准备与覆盖说明。
- [docs/README.md](../README.md) 加索引。
- [tests/python/adapter/INVENTORY.md](../../tests/python/adapter/INVENTORY.md) 同步。
- AGENTS「常用命令」中的互通命令补上 `MILVUS_LITE_ARROW26_PYTHON`。

## 实施步骤

与 0037 一样，设计与实施合成一个提交：
1. 引擎部分与 Rust 单测：schema、写入、数据层、持久化、扫描、upsert；
2. gRPC 的解码与编码，端到端、差分、互通（含 `ref26`）、降级、dump 用例，反证，文档与计数；
3. 在 x86 服务器上跑一轮 `flat_batch_probe` 对照；
4. 评审通过后提交，按负责人的授权推送到临时分支，跑 CI 与 Compare。

## 实施结果

**计数**：`tests/python` 752 → 765；差分 867 → 874（分组 405 不变）；互通 321 → 336；`tests/dump/e2e` 28 → 29，`tests/dump/diff` 19 → 20；previous-reader 6 → 7 个用例；`tests/ci` 144 → 146；`tests/release` 475 不变；Rust workspace 717 条。INVENTORY：改后复制 20、范围外 31，共 345 个函数、展开后 413 条。

**本地验证**（macOS arm64，均在新建的环境中）。共跑了两轮全量。第一轮之后改了两处：全为 null 时的输出、`scan_tiled` 的外提（见下文“调整”）。第二轮只重跑受这两处影响的 Rust、`tests/python`（3.0.2）与差分全量；其余各项取第一轮的结果，另外对改动所涉的差分 10 条与 `tests/python` 124 条，在两条 SDK 线上做了定向重跑。

- Rust：workspace 单测与 clippy 两轮均通过；fault-injection 单测第一轮通过。
- `tests/python`：第一轮 pymilvus 3.0.2 与 2.6.17 两条线各 763 条通过；第二轮 3.0.2 763 条通过。
- 差分全量 874 条（参照为 3.2.2 之后的 `1320d26`）：第一轮 873 条通过、1 条失败，失败的是下文“调整”中全为 null 的 boost 输出；修正后第二轮 874 条全部通过。
- 提交前评审后的第三轮（改动只涉及空向量的解码上限与提示的写出）：Rust workspace 717 条与 clippy 通过；`tests/python`（3.0.2）765 条通过；差分 `test_nullable_vectors.py` 与 `test_rerank_boost.py` 共 10 条通过；`tests/release` 与 `tests/ci` 共 621 条、`tests/packaging` 212 条通过。
- 互通 336 条通过（Arrow 15 / 25 / 26 三套参照环境）。
- dump：e2e 29、diff 20 通过。
- `tests/release` 475、`tests/ci` 146、`tests/packaging` 212 通过；actionlint 对三个工作流零告警。
- previous-reader：上一版本取 0.6.0rc1 时走“拒绝”分支，取本实现的开发构建时走“读出相同值”分支，两种情况下 7 个用例均通过。

**变异验证**：去掉 `scan` 与 `scan_tiled` 中的有效位检查、compaction 丢掉 validity、`scan_tiled` 外提后的 `Some` 分支改错，对应用例均失败。

**服务器对照**（x86_64 服务器的绑核容器，`flat_batch_probe`，dim 128、16 线程、每组 30 次取 p50；基线为 `fb676a0`）：

- 第一轮共 12 组（8000、10 万、100 万行 × nq 1、10、100、1000），结果指纹全部一致。nq 1 走逐行判断的 `scan`，这部分代码第一轮之后没有再改：8000 行 0.36 → 0.31 ms，10 万行 0.86 → 0.85 ms，100 万行 7.61 → 7.61 ms。100 万行、nq 1000 慢约 1.8%（1196.6 → 1217.7 ms），因此把 `scan_tiled` 的有效位判断提到循环外。
- 外提后的第二轮，每组 3 轮取中位数，指纹全部一致：

| 行数 | nq | 基线（ms） | 本实现（ms） | 变化 |
| --- | --- | --- | --- | --- |
| 10 万 | 100 | 24.31 | 22.44 | −7.7% |
| 10 万 | 1000 | 163.49 | 164.34 | +0.5% |
| 100 万 | 100 | 119.17 | 119.63 | +0.4% |
| 100 万 | 1000 | 1218.05 | 1206.14 | −1.0% |

各组差异都在轮间波动之内，没有回退。

**与设计相比的调整**：

- **全为 null 的输出**：混合 null 与有效行时用紧凑格式；所有行都为 null 时按 3.x 写零占位。boost 剥掉 `$id` 等同名向量字段时的输出被 `test_rerank_boost.py::test_boost_same_named_fields_match_3x` 固定，改用紧凑格式会让这条差分失败。两条 SDK 线都把这种输出读成 None。
- **补救路径不成立**：合并至少需要 4 个数据文件才会触发，删除也不重写文件，“删掉 null 行再合并”不能让 pyarrow 低于 26 的 3.x 重新打开集合。README 与测试中不写这条补救。
- **检索**：`scan_tiled` 把有效位判断提到循环外，只在字段确有 null 时逐行判断；`scan` 保留逐行判断。
- **解码的 dim**：pymilvus 能发出“某行维度不对、另有 None”的请求。为保持 3.x 的维度错误文案，按请求的 dim 展开后不超过 `MAX_EXPANDED_FLOATS`（`MAX_MESSAGE_SIZE / 4`）时照常展开，交给引擎报维度错误；超过时原样返回，由行数检查报错。全为 null 时按 schema 的 dim 写空值。
- **展开上限**（提交前评审后加入，负责人确认）：按 schema 的 dim 补零时，行数 × dim 超过 `MAX_EXPANDED_FLOATS` 即报错，不分配；缺列的 nullable 向量字段（function 输出除外）同样检查，因为 auto_id 加 nullable 向量的集合可以不带任何列，这时 `num_rows` 只受 u32 约束。差异登记在「与 3.x 的差异」表的最后一行。
- **提示的写出**：首次写入 null 的提示在 WAL 追加之后、不可失败的区段里写出。stderr 不可写时 `eprintln!` 会 panic 并毒化集合，改为写失败即忽略（提交前评审发现）。
- [storage_interop.rs](../../crates/core/examples/storage_interop.rs) 未改，与设计一致。

**待完成**：提交前评审；临时分支上的 CI 与 Compare；按 0020 判定规则 2 的重新计数（受影响的 job 与次数另行提出，0037 剩余的 CI 1 次、Compare 2 次并入）。

## 放弃的方案

- **照搬 3.x，存成零向量**：互通无风险、差分一致、改动最小，但会把“写入 None 读回零向量、null 行参与检索”固化成本实现的对外行为，将来再改还会遇到同样的互通问题。首轮评审后，负责人在知情下放弃这个方案。
- **零向量加私有标记，兼顾 3.x 读取**：违反 0015 的约束；3.x 不认识标记，它自己做 upsert 或 compaction 时会让标记失真。
- **读取时把全零向量判为 null**：会把真实的零向量误判为 null，违反 0015 的约束。
- **写入时检测或警告 3.x 的 pyarrow 版本**：本实现无法知道之后由哪个 3.x 环境打开这个目录，只能靠文档说明。
