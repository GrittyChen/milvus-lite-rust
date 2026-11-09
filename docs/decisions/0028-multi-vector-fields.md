# 0028：多向量字段

- 状态：设计已确认，已实施，独立评审的发现已修复。
- 日期：2026-11-09。
- 参照：milvus-lite `1320d26`，pymilvus 3.0.2。
- 前置：[0004](0004-m1-scope.md)、[0014](0014-index-definitions-and-filter-expressions.md)、[0026](0026-rerank-and-hybrid-search.md)、[0027](0027-reference-3.2.2-upgrade.md)。

## 范围

一个集合可以有多个 FLOAT_VECTOR 字段，维度各自独立，字段数没有上限（3.x 同样不限制）。`anns_field` 未指定时取 schema 中第一个 FLOAT_VECTOR 字段。

不纳入：稀疏向量（下一切片）与 nullable 向量（[0015](0015-nullable-defaults-and-array.md) 已延后）。持久化格式不变：多个向量字段本来就是各自的列，范数只在内存中。

## 3.x 的行为

| 项目 | 行为 |
| --- | --- |
| 检索目标 | 按 `anns_field` 选字段；数据、索引、度量都按该字段。度量解析见 [0027](0027-reference-3.2.2-upgrade.md) |
| HybridSearch | 每一路按自己的字段和度量检索，同一字段可以出现多次 |
| 投影 | 默认与 `*` 输出全部向量字段，顺序同 schema |
| 过滤与分组 | 任何向量字段都不能用于标量过滤和 `group_by` |
| 索引 | 每个字段一个定义，load 时全部构建；未建索引的字段走暴力检索，默认度量 COSINE |
| 写入校验 | `validate_record` 只校验第一个 FLOAT_VECTOR；其余字段的问题要到构建 Arrow 批次时才暴露 |

## 设计

### 范数

`ChunkData.norms` 仍是一个 `Vec<f32>`，每行每个 FLOAT_VECTOR 字段一个值，下标为 `row * 字段数 + 字段序号`。

- 单向量集合的内容与之前完全相同，小块合并时的追加也不用改。
- 不用 `Vec<Vec<f32>>`：所有结构都要改，还多一层分配。
- 不只存首字段：其余字段的 COSINE 每次都要重算。

### 检索

`scan` 按目标字段取向量和范数。分块按目标字段的维度，小块合并上限按所有向量字段的最大维度；两者只影响并行切分和内存块大小，不影响结果。

### 写入

第一个向量字段的校验不变。其余向量字段在所有行校验之后再检查，因此 3.x 在 `validate_record` 阶段拒绝的输入，报错不变：

1. 按 schema 顺序检查类型和维度：维度不符时报与 3.x 相同的 Arrow 文案（code 1）。
2. 再检查缺失或 None。

这项检查与 Arrow 阶段的转换（ARRAY 元素、整数到 FLOAT 的精确性等）不按 3.x 的 schema 顺序交错：insert 和 partition key 路径先做这项检查再转换；非 key upsert 的标量转换在逐行循环内，先于这项检查。同一请求里这类转换和向量维度同时出错时，报的可能是另一个字段。只有原始 RPC 能在同一请求里同时造出这两种错误，都是错误输入，不值得让转换与检查交错。

### 恢复

WAL 恢复原本把解码失败一律当作截断，只取已校验的前缀并打印警告。3.x 写入的 null 行会让 arrow-rs 以「非空列含 null」拒绝整批，按截断处理就丢掉了这一批及其后的全部写入，下一次 flush 清理 WAL 后无法挽回。这种批次是完整的，因此改为打开失败。Parquet 中同样的行是全 null 的子元素，由解码校验拒绝。

## 与 3.x 的差异

| 差异 | 3.x | 本实现 | 理由与影响 |
| --- | --- | --- | --- |
| 非首向量字段缺失或为 None（含 upsert 写入新行） | 写入成功。flush 后，输出该字段的 search 和 query 报 `float() argument must be a string or a real number, not 'NoneType'`，该字段的检索距离为 NaN | 拒绝，`vector field 'v2' missing or None`（code 6） | 3.x 的缺陷，`upsert(partial_update=True)` 写新行即可触发。负责人确认不复制 |
| 非首向量字段的 FieldData 不是向量类型（只有原始 RPC 能发送） | Arrow 内部文案 | `vector field 'v2' must be list/tuple, got int`（code 6） | 文案与首字段一致；pymilvus 按 schema 打包，不会发送这种请求 |
| 3.x 数据目录中非首向量字段含 null | 可以打开，读取行为同第一行 | 打开该集合时明确报错，文件不改动 | 只有上述缺陷才会产生这种目录 |

`drop_index(c, "<字段名>")` 删除全部索引的行为沿用 [0014](0014-index-definitions-and-filter-expressions.md#dropindex)。多向量集合上它的影响更明显：删 `v2` 也会删掉 `v1` 的索引，`v1` 的默认度量随之回到 COSINE。只有 `<字段名>_idx` 或显式的 `field_name` 才定向删除。

## 验收

| 项目 | 结果 |
| --- | --- |
| Rust 默认 / fault-injection | 551 / 693 通过，clippy 通过（含 fault-injection） |
| 端到端：pymilvus 3.0.2 / 2.6.17 | 各 571 条通过（迁入 `test_multi_vector_fields`） |
| 差分：pymilvus 3.0.2 / 2.6.17 | 各 702 条通过；[test_multi_vector.py](../../tests/differential/test_multi_vector.py) 60 条，覆盖目标字段、投影、过滤与分组、各字段的索引与度量、Hybrid、partition key、显式分区、upsert 合并和写入错误 |
| 互通：Arrow 15 / 25 | 266 条通过；[test_multi_vector.py](../../tests/storage_interop/test_multi_vector.py) 11 条，覆盖 Rust、3.x 交替写入的 WAL、flush、compaction，以及 3.x 写出的 null 行 |
| 范数 | 非首字段的 COSINE 在写入、flush、compaction 后的同一进程内、重开后都断言分数；查询与候选不共线，否则相似度被截断到 1，错误的范数看不出来 |
| 反证 | 范数取错（取首字段、compaction 切片错、各写入路径范数颠倒）的 7 个变异，[multi_vector.rs](../../crates/core/tests/multi_vector.rs) 全部抓到；用取首字段范数的构建跑差分和互通，分别 23 条、9 条失败 |
| 工具自检 | 956 条通过 |

单向量基准（200,000 行、128 维、FLAT、进程内，同机三轮轮换，中位数）与 `4ddddd3` 对比，没有退化，不做提速声明：

| 指标 | `4ddddd3` | 本切片 |
| --- | --- | --- |
| 写入（行/秒） | 130,900 | 130,700 |
| search nq=1，top10（ms） | 0.77 | 0.77 |
| search nq=100，top10（ms） | 24.16（23.85–24.19） | 24.49（23.91–24.75） |
| 过滤 10% search（ms） | 0.47 | 0.48 |
| query 带过滤 limit 100（ms） | 0.31 | 0.32 |
| get 100 个主键（ms） | 0.31 | 0.32 |

## 下一个候选需要写明

支持多向量字段；上表的三项差异；`drop_index` 传字段名时删除全部索引在多向量集合上的影响。
