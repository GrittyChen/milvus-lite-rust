# 0024 分组检索（group_by）

- 状态：设计评审通过；第 1 步（core 与 gRPC 翻译）已实施，第 2 步（差分、参照用例、文档与计数）待做
- 日期：2026-11-02
- 参照：milvus-lite v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`；pymilvus 3.0.2（2.6.17 线在实施阶段补测）
- 调查证据：探针脚本与 3.x 输出由负责人本地留存（131 个场景；第 2 步用同一脚本对本实现原样重跑，作为差分用例的来源）
- 前置：[0016](0016-query-iteration-and-range-search.md)（范围检索的候选窗口）、[0023](0023-aliases-rename-truncate-databases.md)（0.3.0rc3 之后的缺口统计基线）

## 依据与范围

0023 B 合入后重做的缺口统计中，分组检索挡住 7 条参照用例（其中 `group_by` 5 条、`dynamic_field_coverage` 与 `edge_cases` 各 1 条）和 1 项探针（`group_by_search`）；另有 HybridSearch 里的分组 1 条，依赖尚未实现的 HybridSearch，不属于本切片。统计的首个阻断原因不是收益承诺。

范围：单次 Search RPC 的 `group_by_field`、`group_size`、`strict_group_size`，含过滤、范围检索、offset、分区、多查询向量、可空分组字段与响应里的 `group_by_field_value`。

不在范围内：HybridSearch 的分组，rerank / function chain 内的分组，稀疏向量与全文检索。`search_iterator` 把分组参数放在 `params` 里，参照实现与本实现都不解读，行为相同，无需处理。

本切片不改变任何存储格式，不新增依赖，不引入锁，数据目录与 3.x 的互通不受影响。

## 参照行为（3.x 实测，逐项来自源码阅读与 131 个场景的真实服务结果）

参照在引擎层先多取候选、再对候选做后处理分组，**候选窗口的大小会改变结果**，所以窗口公式属于必须保留的行为。

### 流程与顺序

1. 没有查询向量：在一切检查之前返回空结果（与普通检索相同）。
2. 集合未加载：报错（与普通检索相同，优先于下面的分组字段校验）。
3. 校验 `group_by_field`：先于 anns_field 解析、度量校验与过滤编译。
   - 不在 schema 中（含动态字段名、`$meta`、JSON 路径写法）：`group_by_field 'x' not found in schema`。
   - 类型不是 INT8 / INT16 / INT32 / INT64 / BOOL / VARCHAR：`group_by_field 'x' has type FLOAT which is not supported for group_by`（FLOAT、DOUBLE、JSON、ARRAY、向量均如此）。
   - 两条都是 SchemaValidation 类错误（与 `anns_field` 不存在同一类）。空字符串视为没有分组。
4. 候选窗口：`end = limit + offset`，`window = max(end × group_size × 3, end × 10)`；同时有范围检索参数时再取 `max(window, end × 5)`。
5. 在窗口内按普通检索排序取候选（含过滤、分区、删除可见性），范围检索时先按范围过滤并截断到 `end`。
6. 分组：按距离顺序遍历候选，按分组值归组，每组最多保留 `group_size` 条，组按首个命中的出现顺序排列；`strict_group_size` 为真时丢弃不足 `group_size` 条的组；取前 `end` 个组并按组展平。
7. offset 为正时，**在展平后的命中列表上**取 `[offset, offset + limit)`，不是按组跳过；offset 为 0 时不再截断，展平结果最多 `limit × group_size` 条。offset 为负时 `end = limit + offset` 变小、不切片（`limit=2, offset=-1` 只取 1 个组）；现有 gRPC 翻译的 `skip = max(offset, 0)`、`limit = end − skip` 已能重建这一语义，core 继续以 `end = limit + offset` 作为窗口与组数上限。

### 参数的解析

| 参数 | 实测 |
| --- | --- |
| `group_size` | 先按 Python `int()` 解析，`0` 视为 `1`；负数使每组都留不下命中，结果为空（候选窗口按 `end × 10`）；`"2"` 与 `2.5` 分别按 2 处理；不可解析的文本报 InvalidArgument |
| `strict_group_size` | 先按 JSON 解析（失败则保留原字符串）再取 Python 真值：`"false"`、`0`、`""`、缺省为假；`"true"`、`1`、`2`、`"False"`（不是合法 JSON，按非空字符串）为真。因此 pymilvus 的 `strict_group_size=False` 实际发出字符串 `"False"`，在 3.x 上等价于**真** |
| `group_by_field` | 只认 `search_params` 顶层键；写在 `params` 内的不生效 |
| 极大的 `group_size` / `limit` | `1e8` 量级可在秒级完成，`1e10` 量级 3.x 长时间无响应；本实现不复制这一性能失效，结果与 3.x 能完成的情形一致 |

### 响应

- 命中按展平顺序排列，`topks` 是各查询展平后的条数，`top_k` 仍是请求的 limit。
- `SearchResultData.group_by_field_value` 存在于有命中时：字段名、类型与每个命中的分组值一一对应；可空字段的空值以占位值（整数 0、字符串 `""`）加 `valid_data = false` 表示，整列没有空值时 `valid_data` 为空。没有命中时该字段不出现。
- 分组字段不在 `output_fields` 中时不出现在命中里，但 `group_by_field_value` 仍然返回。
- 空值是一个独立的组（同一分组字段的所有空值归为一组，`strict` 同样适用）。

### 实测的 3.x 缺陷（已确认不复制，见末节）

分组值由引擎从命中的 `entity` 取，取不到（值为 None）时回退到命中的顶层键：

1. 主键字段不叫 `id` 时按主键分组：`entity` 里没有主键，顶层键里也没有同名项，所有命中变成同一个值为空的组，只返回 1 条命中。主键叫 `id` 时碰巧正常。
2. 可空分组字段恰好叫 `distance` 或 `id`：空值行不归到同一组，而是各取该命中的距离或主键作为分组值，每个空值行单独成组，响应里的分组值是主键或截断后的距离。

## 设计

不新增 module，在现有检索路径上接入：

1. **core**：`SearchRequest` 增加可选的分组参数（字段名、`group_size`、`strict`），`ReadSnapshot::search_filtered_with_range` 内部按上述顺序执行。
   - 在 `require_loaded` 之后、`resolve_anns_field` 之前校验分组字段；
   - 候选数 `take` 用 `i128` 计算后饱和到 `usize`，`TopK` 本来就是惰性分配，极大值不会预分配；
   - 范围过滤并截断到 `end` 之后分组，分组在已选定的行位置上进行：分组值直接读列（不依赖 `output_fields`），键为"有效 / 空 + 值"，空值归同一组；
   - offset 为正时在展平后切片，为 0 时不截断；
   - `Hits` 增加可选的分组值列（与命中一一对应，含有效位），仅在分组时填充。
2. **grpc**：`search.rs` 去掉 `grouping search` 的 UNIMPLEMENTED，解析 `group_size`（`int()` 后 `0 → 1`，沿用现有 `pyint` 的饱和规则）与 `strict_group_size`（JSON 或原字符串的 Python 真值，新增 `pyint::truthy`）；响应里增加 `group_by_field_value`，用现有的列编码器按分组字段的类型和有效位生成。HybridSearch 与 rerank 仍在各自的 UNIMPLEMENTED 之后。
3. **性能**：分组只处理窗口内至多 `window` 个命中（`O(window)`），不引入对总行数的逐行判定；窗口内的距离计算沿用已有的并行扫描，不改变计算池、锁与快照边界。

## 与参照的差异（预案，评审通过后正式登记）

| 项 | 参照 | 本实现 | 理由 |
| --- | --- | --- | --- |
| 主键不叫 `id` 时按主键分组 | 全部归为一个空值组，只返回 1 条 | 按主键值正常分组 | 回退取值缺陷，3.x 无人能依赖 |
| 可空分组字段叫 `distance` / `id` | 空值行各自成组，组值为距离或主键 | 空值归为一组 | 同上 |
| 极大 `group_size` / `limit` 的耗时 | `1e10` 量级长时间无响应 | 正常返回 | 本实现的候选集惰性分配 |

## 测试矩阵

- 引擎单元：窗口公式边界（`end × gs × 3` 与 `end × 10` 谁大、范围检索叠加）、展平与 offset 切片、`strict` 的相等判定、空值组、`group_size` 的负数与极大、分组字段类型校验与错误优先级、主键名为 `id` 与非 `id`、删除与 flush 后的 segment；期望值取自 3.x 实测。
- gRPC 翻译：`group_size` 的 `int()` 解析与 `0 → 1`、`strict_group_size` 的 JSON 真值、字段校验文案、`group_by_field_value` 的字段名 / 类型 / 值 / `valid_data`、分组字段不在 `output_fields` 时的输出、分组与 rerank 同时出现时 rerank 的 UNIMPLEMENTED 不被吞掉。
- 差分（真实 3.x 对本实现，两条 SDK）：同一探针脚本的 131 个场景逐项比较命中 id、距离、实体、`topks`、`top_k` 与 `group_by_field_value` 的字段名、类型、值和有效位。
  - **并列距离口径**：沿用现有检索差分的规则（0004 的顺序与误差）；并列落在 `group_size` 或窗口截断边界时，只比较组级不变量（组集合、每组条数、组值与有效位）。
  - **错误优先级**：补语法错误的过滤表达式加坏分组字段、非法度量名加坏分组字段两个场景（探针原有的 `bad filter` 在动态集合上是合法过滤，`bad metric` 用了合法度量）。
  - **预期不同的场景**单列，两边的输出都要断言，并写明原因：主键非 `id` 时，3.x 是单个空值组（`valid_data = false`），本实现是逐命中的主键值；可空 `distance` / `id` 字段时，3.x 把距离或主键强转进字段槽，本实现是空值加 `valid_data = false`。
  - **只登记、不对跑**：`group_size` 或 `limit` 在 `1e10` 量级时 3.x 长时间无响应，不进入差分，由引擎单测覆盖；`1e8` 以下仍可对跑。
- 参照用例：移入 `test_group_by.py` 的 gRPC 用例（含可空 VARCHAR）、`dynamic_field_coverage` 里分组与 `["*"]` 的用例、`edge_cases` 的分组用例，INVENTORY 随之更新。
- 预期计数：差分与 Python 套件的新增用例数在同一批里同步更新到所有固定点（清单见 0023 B 验收）。

## 实施步骤

1. core 与 grpc 一起实现，附引擎单元测试与 gRPC 翻译测试（已完成；探针在本实现上与 3.x 相比，131 个场景中 127 个逐项相同，其余 4 个正是上表登记的预期不同）。
2. 差分、参照用例、文档（README 范围与已知差异、本决策转为"已实施"、验收记录）与预期计数。

每步写完停下评审，评审通过且授权后才提交；推送另行授权。

## 已确认的取舍

1. 两个 3.x 缺陷（主键非 `id`、可空 `distance` / `id` 字段）**不复制**，按字段值正常分组，已列入上面的差异表；实施时同步登记到 0006 差异表。理由：它们是回退取值的偶然结果，复制会把明显错误变成合同；参照自身的单测与 adapter 用例都没有覆盖这两个输入。
2. HybridSearch 的分组与 rerank / function chain 内的分组留给对应切片，本切片遇到时保持现有的 UNIMPLEMENTED（有翻译测试钉住）。
