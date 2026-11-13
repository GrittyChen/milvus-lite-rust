# 0034：GEOMETRY 字段与几何谓词

状态：设计第二稿 2026-11-13 复审通过（第一稿未通过：大小写与 `float()` 的实现路线达不到承诺的行为、`ARRAY<GEOMETRY>` 的打开路径没有守卫、实施步骤依赖倒置）；负责人已确认决策 A–E；第 1 步（类型、解析与读写）已实施并评审通过（2026-11-13）；第 2、3 步未实施。参照版本 `1320d26`（pyarrow 25.0.1、CPython 3.12）。排序见 [0032](0032-arithmetic-filters.md)、[0033](0033-hybrid-dynamic-inference.md)：本切片排在两者之后。

## 范围

- `GEOMETRY` 字段：建表、Describe 回显、nullable 与默认值、Insert / Upsert / partial upsert 的校验、Query / Search / HybridSearch / iterator 的输出、flush、重开、compaction，以及与 3.x 数据目录的双向互通。
- 五个几何谓词及其别名：`ST_CONTAINS` / `geometry_contains`、`ST_WITHIN` / `geometry_within`、`ST_INTERSECTS` / `geometry_intersects`、`ST_ISVALID` / `geometry_isvalid`、`ST_DWITHIN` / `geometry_dwithin`，可用于 Query、Search、count、Delete、boost、HybridSearch 子请求、query iterator 与 search iterator（均已在 3.x 上实测；search iterator 是 V1，V2 的边界见 [0016](0016-query-iteration-and-range-search.md)）。

不包含：3.x 也不支持的能力（POINT 与单环 POLYGON 之外的 WKT、WKB 写入、GEOMETRY 上的索引、分组、主键与 partition key）；ARRAY 的 GEOMETRY 元素（决策 C）。

## 3.x 的行为

3.x 的几何是纯 Python 实现（[geometry.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/schema/geometry.py)，274 行），不依赖 GEOS / shapely。以下都在真实 3.x 上实测，「服务端」指原始 gRPC 看到的结果。证据见文末。

**类型与存储**

- GEOMETRY 是校验过的字符串列：Arrow 类型 `string`，`schema.json` 的 `dtype` 为 `"geometry"`，存的是**写入的原文**（不规范化，`'  point ( 1e1   -2.5 )  '` 原样读回）。不增加持久化格式。
- 建表：nullable、默认值、nullable 加默认值都接受；`max_length` 被忽略且不回显。
- 主键与 partition key：服务端 code 6，`primary key 'g' must be VARCHAR or INT64, got DataType.GEOMETRY` 与 `partition key 'geo' must be INT64 or VARCHAR, got GEOMETRY`；pymilvus 2.6 / 3.0 在客户端先拦截（code 1），到不了服务端。
- 默认值只认 `string_data`，其余槽位或无效 WKT 报 `GEOMETRY field 'geo' default_value is invalid`（code 6）；Describe 以 `string_data` 回显。
- 有默认值时，缺失与显式 `None` 都取默认值，**nullable 加默认值时也一样**，这样的列不会出现 NULL。
- 索引：`INVERTED` 报 `field type GEOMETRY does not support scalar index`，`RTREE` 报 `unknown scalar index_type 'RTREE'; supported scalar index_type: INVERTED`。分组报 `group_by_field 'geo' has type GEOMETRY which is not supported for group_by`。

**协议**

- 写入读 `geometry_wkt_data`，没有时读 `string_data`；`geometry_data`（WKB）与其他槽位得到 0 行，报 `FieldData 'geo' has 0 rows, expected N`。`FieldData.type` 为 VarChar（21）时同样接受。
- 输出固定用 `geometry_wkt_data`，空值位置填 `""` 并带 `valid_data`。
- 校验失败报 `field 'geo' value 'x' does not match dtype DataType.GEOMETRY`（code 6），整批拒绝，partial upsert 相同。

**WKT 解析**（写入、默认值、过滤字面量与逐行求值共用）

- 两个正则，`re.IGNORECASE`，Unicode 模式：`^\s*POINT\s*\(\s*([^\s,()]+)\s+([^\s,()]+)\s*\)\s*$` 与 `^\s*POLYGON\s*\(\s*\(\s*(.*?)\s*\)\s*\)\s*$`（`DOTALL`）。多边形主体按 `,` 切开，每段 `str.split()` 后必须恰好两个坐标。
- 坐标用 Python `float()`，必须有限。
- 多边形至少 4 个点、首尾相同、非相邻边不相交（容差 1e-9）。只有一个环；带洞的写法因主体里出现括号而被坐标解析拒绝。
- 需要原样复刻的边角（穷举全部码位得到）：
  - 空白：`\s` 与 `str.split()` 是同一组 29 个码位（U+0009–U+000D、U+001C–U+001F、U+0020、U+0085、U+00A0、U+1680、U+2000–U+200A、U+2028、U+2029、U+202F、U+205F、U+3000）；U+200B 不在其中。
  - `$` 允许结尾多一个换行，加上 `\s*` 后结尾任意空白都接受。
  - 忽略大小写：`POINT` / `POLYGON` 用到的字母里，只有 `I` 在 ASCII 大小写之外还匹配 `İ`（U+0130）与 `ı`（U+0131），其余字母（P、O、N、T、L、Y、G）只匹配自己的 ASCII 大小写。这是正则的匹配规则，**不是** `str.lower()`：`'POİNT'.lower()` 是六个码位，不等于 `point`。
  - `float()` 接受下划线（`1_0`、`1e1_0`）、任意 Unicode 十进制数字（`١`、`１２`、`١_٢`）、`+1`、`-.5`、`1.`；拒绝 `0x10`、`1__0`、`1_`、`.e1`；`inf`、`nan`、`1e400` 能转换但因非有限被拒绝。
  - 退化多边形 `POLYGON((0 0,0 0,0 0,0 0))` 合法。

**谓词**

- 几何计算是固定的浮点运算序列：射线法判内（边界算在内）、带 1e-9 容差的方向与共线判断、线段距离、`math.hypot`。`ST_DWITHIN` 的判据是 `距离 <= d + 1e-9`。
- **CPython 的 `math.hypot` 不是 libm 的 `hypot`**：在本机 30 万组随机输入上有 16% 的结果不同，最大相差 1 ULP。要逐位一致必须移植 CPython 的算法。
- `contains(多边形, 多边形)`、`intersects` 等是 3.x 自己的近似算法，不是 OGC 语义。
- 语义检查的先后：字段先按普通字段引用检查（未知字段报 `unknown field`，动态字段同样报这个；向量字段报 `field 'v' of type float_vector cannot be used in scalar filter expressions`），再要求是 GEOMETRY 列，否则 `geometry_within requires a GEOMETRY field, got int (field 'id')`；最后字面量必须是合法 WKT，否则 `geometry predicate requires a valid WKT literal`。文案里的函数名是规范名（`geometry_within`），`ST_ISVALID` / `ST_DWITHIN` 固定大写。
- 含几何谓词的表达式**整条走 Python 路线**（`requires_python`）。逐行求值：
  - 字段为 NULL 时返回 False 而不是未知，所以 `not ST_WITHIN(geo, …)` 选中 NULL 行。
  - 三个二元谓词与 `ST_DWITHIN` 把**谓词内部**（解析字段值、几何计算、距离转浮点）的任何异常当作 False；`ST_ISVALID` 只是「字段值能否解析」。表达式其余部分（算术、比较）的错误处理不变，按 [0032](0032-arithmetic-filters.md)。
  - 3.x 每行重新解析字面量；解析是纯函数，结果与只解析一次相同。
- `ST_DWITHIN` 的距离只能是非负数字字面量：`-1` 与 `+5` 是解析错误，`-0`、`-0.0`、`1e400`、`1e-400` 可用；整数字面量大到 `float()` 溢出时（如 400 位）每行返回 False，`not` 之后选中全部行。本实现的解析器对 `-0`、`-0.0`、`-1`、`+5` 已与 3.x 一致（评审时用 `filter_parse_probe` 复核），其余输入在第 2 步由用例固定。
- GEOMETRY 列在其他运算中的语义类型是 `geometry`：
  - 两个 GEOMETRY 字段之间的比较（`==`、`!=`、`<`、`>=` 等）可用，按原文的字符串比较。
  - `is null` / `is not null`、`text_match(geo, 'point')`（按原文分词）可用。
  - 与字符串或数字字面量比较、`in`、`like`、算术、`exists`、`array_length`、`json_contains` 都是类型错误，文案中的类型名是 `geometry`。

**ARRAY 的 GEOMETRY 元素**

- 3.x 接受建表。pymilvus 写入时在客户端报 `Unsupported element type: 24`。
- 原始 gRPC 可以写入：`ArrayData` 每行的 `string_data` 被接受，**不校验 WKT**（`'zzz'` 也成功），落盘为 `list<string>`；每行用 `geometry_wkt_data` 时也返回成功，但取不到元素。读出时每行都是空数组（输出没有对应的槽位）。也就是说 3.x 自己写得进、读不出。

## 设计

1. **类型**：`DataType::Geometry`，列沿用字符串列（与 TIMESTAMPTZ 沿用 int64 列同样的做法），类型差异只在 schema。存储、WAL、Parquet、compaction 只在按类型分支处补一项。
2. **`geometry` 模块（core）**，分两部分，对应实施的前两步：
   - **解析与校验**：不引入正则依赖，手写与两个正则等价的扫描。
     - 空白用上面 29 个码位的常量；关键字逐字母比较，`I` 额外接受 U+0130、U+0131。两者都由组件真值的生成器对全部码位穷举核验。**不使用、也不修改** [unicode.rs](../../crates/core/src/filter/unicode.rs) 的 `lower`（它是 `str.lower()` 的表，分词与 BM25 在用）。
     - 坐标用 core 已有的 Python `float()` 实现：[pyval.rs](../../crates/core/src/rerank/pyval.rs) 的 `float_literal`（处理 Unicode 数字与下划线），把它的可见性提到 crate 内并注明 WKT 解析也在用；只有一份实现。**不改** grpc 的 [pyint.rs](../../crates/grpc/src/pyint.rs)：它对请求参数的行为是 [0004](0004-m1-scope.md) 登记过的差异，不属于本切片。
   - **谓词**：按 3.x 的运算顺序逐步移植；`hypot` 移植 CPython 3.12 的 `vector_norm`（两个参数的情形）。
3. **写入与默认值**：写入的校验接在现有的逐字段校验处；schema 的默认值在 schema 校验中用同一个解析器检查，建表与打开数据目录两条路径都经过它。错误文案与先后按 3.x；有默认值时显式 `None` 取默认值，沿用其他类型已有的规则。
4. **过滤**：解析器已经识别这些函数（现在产出 `Kind::Unsupported`），改为产出几何节点；语义检查加 `geometry` 语义类型（两个 GEOMETRY 字段的比较按字符串列处理，其余组合按上文报类型错误）；路线判定把几何节点标为强制 Python 路线；`python.rs` 加逐行求值。字面量在编译时解析一次，字段值逐行解析。去掉 [eval.rs](../../crates/core/src/filter/eval.rs) 里几何谓词的前置 UNIMPLEMENTED 扫描。
5. **gRPC**：schema 解码放开类型 24；写入按 `geometry_wkt_data` → `string_data` 的顺序取值；输出用 `geometry_wkt_data`。
6. **`ARRAY<GEOMETRY>` 继续拒绝，建表与打开两条路径都要守住**：
   - 建表：[grpc/schema.rs](../../crates/grpc/src/schema.rs) 对 ARRAY 元素类型 24 的拒绝保留，不随顶层类型 24 的放开而消失。
   - 打开：[schema.rs](../../crates/core/src/schema.rs) 的 ARRAY 元素拒绝列表加入 `Geometry`。现在这种集合打不开只是因为 [schema_file.rs](../../crates/core/src/storage/schema_file.rs) 不认识 `"geometry"`、元素类型解析成缺失；认识之后必须由这份列表拒绝，否则会被当作字符串数组打开。
7. **降级**：旧版本打开带 GEOMETRY 字段的集合时已经明确返回 UNIMPLEMENTED（`schema_file.rs` 把 `geometry` 列为已知但不支持的类型），数据保留。发行说明照此写。

**性能**：几何谓词在 3.x 中就是逐行解析加逐行计算，本实现同样逐行，但不经 Python。不做空间索引，不作性能声明；如需数字，另按规则 7 做专项。

## 决策

| | 事项 | 结论 |
| --- | --- | --- |
| A | Unicode 与 `float()` 的边角 | 逐项复刻，实现路线见设计第 2 条。数据目录互通要求两边对同一原文的判定一致 |
| B | `math.hypot` | 移植 CPython 3.12 的算法，补 0、次正规数、接近溢出与非有限输入的边界用例 |
| C | `ARRAY<GEOMETRY>` | 建表与打开都返回 UNIMPLEMENTED，登记差异；已有这种集合的 3.x 数据目录，该集合打不开，可以 drop |
| D | 多边形谓词不是 OGC 语义 | 照 3.x 复刻，不修正；参照实现自己的凹多边形等用例纳入组件真值，作为「这是 3.x 的行为」的证据 |
| E | 实施分步 | 三步，各自带测试、评审后单独提交，见下 |

## 实施步骤

1. **类型、解析与读写**：`geometry` 模块的解析与校验部分（含 `float_literal` 的可见性调整）；`DataType::Geometry`、schema 的解码 / 校验 / 持久化 / 回显、默认值、写入校验、输出、flush / 重开 / compaction、互通；`ARRAY<GEOMETRY>` 的两处守卫。
   - 这一步结束时过滤还没有 `geometry` 语义类型：**引用 GEOMETRY 字段的任何过滤表达式仍返回 UNIMPLEMENTED**，不得静默按字符串求值，也不报类型错误（文案沿用几何谓词原有的 `filter expressions: geometry predicates are not supported`）；第 2 步放开。这是只存在于两次提交之间的中间状态，不随任何版本发布。
   - 中间状态的用例：`geo is null`、`geo == geo`、`geo == 1`、`text_match(geo, 'x')`、`ST_ISVALID(geo)` 经 Query、count、Delete、Search、boost、HybridSearch、query iterator、search iterator 各至少一条，断言 UNIMPLEMENTED 与文案；第 2 步把它们改为与 3.x 对照。
   - 测试：解析的单元测试（上文每个边角）、建表各变体与拒绝文案、写入校验与错误文案、原始 gRPC 的各槽位、输出、重开与 compaction、互通（3.x 写本实现读与反向）、`ARRAY<GEOMETRY>` 的建表 / 打开 / drop。
2. **谓词与过滤**：`geometry` 模块的谓词部分与 `hypot`；过滤的节点、语义类型、路线与逐行求值；去掉前置扫描。
   - 测试：谓词的单元测试、过滤的类型错误文案、NULL 与 `not`、`ST_DWITHIN` 的各种距离字面量、两个 GEOMETRY 字段的比较、`text_match`、Delete / count / Search / HybridSearch / boost / iterator。
3. **证据与文档**：组件真值、模糊对照、端到端差分、参照用例、已有用例与清单的更新、文档与验收记录。

每一步提交时本步改动的行为都有测试；第 3 步补的是与 3.x 的系统性对照，不是前两步缺失的测试。

## 与 3.x 的差异

- CPython 各版本的 `math.hypot` 实现不完全相同；本实现固定为 3.12 的算法，与 Unicode 表的做法一致。影响只在 `ST_DWITHIN` 恰好落在阈值上的行。
- `ARRAY<GEOMETRY>`：3.x 可以建表，原始 gRPC 可以写入但读出为空数组；本实现建表与打开都返回 UNIMPLEMENTED。
- 原始 gRPC 把 GEOMETRY 的默认值放在 `bytes_data` 槽位，或在浮点槽位给非有限值：3.x 报 `GEOMETRY field 'geo' default_value is invalid`（code 6），本实现沿用 [0015](0015-nullable-defaults-and-array.md) 对任何类型的这两种默认值都先返回 UNIMPLEMENTED 的既有做法（通用检查先于按类型的检查）。pymilvus 发不出这些请求。手改 `schema.json` 得到的非有限或超过 4300 位的数值默认值同样先被通用检查拒绝；3.x 写不出这样的文件，没有对照。

## 测试矩阵

- **组件真值**：新生成器直接调用 3.x 的 `geometry.py`。
  - 解析：上文每个边角；对关键字的每个字母位置与空白位置穷举全部码位，保存被接受的码位（`--verify` 时重新穷举）；Rust 单测遍历全部码位，断言接受的集合与它完全相同，多接受或少接受都失败。
  - 谓词：五个谓词的真值表，含参照实现 [test_geometry.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/tests/search/filter/test_geometry.py) 的全部用例（凹多边形、边界、近似情形）。
  - `hypot`：随机输入加 0、次正规数、接近溢出、非有限值。
- **模糊对照**：新的 probe example 加 Python 驱动，随机 WKT 文本与随机几何对，比较解析结果与五个谓词（含阈值附近的 `ST_DWITHIN`），不经服务。
- **端到端差分**（MemTable、flush、重开、compaction；两条 SDK 线）：
  - 建表各变体；默认值与 nullable 加默认值时显式 `None` 的取值。
  - 写入校验与错误文案；输出（Query、Search、HybridSearch、两种 iterator）。
  - 五个谓词与 `not` / NULL；`ST_DWITHIN` 的 `-0`、`-0.0`、`1e400`、`1e-400`、`+5`、`-1`、400 位整数。
  - 与算术混用（`ST_ISVALID(geo) or id/0 == 1` 及交换操作数、`and`）。
  - 两个 GEOMETRY 字段的各比较运算、`is null`、`text_match`。
  - 非 GEOMETRY 字段上的谓词：int、varchar、float_vector、未知字段、动态字段各一条。
  - GEOMETRY 列与字面量比较、`in`、`like`、算术、`exists`、`array_length`、`json_contains` 的类型错误文案。
  - Delete、count、boost、HybridSearch 子请求。
- **原始 gRPC**（差分）：主键、partition key 的服务端文案；默认值各槽位；WKB 槽位得到 0 行；`FieldData.type=21`；`geometry_wkt_data` 加 `valid_data` 的输出；`INVERTED` / `RTREE` 索引与分组的拒绝文案。
- **`ARRAY<GEOMETRY>`**：pymilvus 与原始 gRPC 建表返回 UNIMPLEMENTED；3.x 建出并写入的数据目录由本实现打开返回 UNIMPLEMENTED、数据不变、drop 成功；同一目录里的顶层 GEOMETRY 集合正常打开。
- **参照用例**：[test_grpc_geometry.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/tests/adapter/test_grpc_geometry.py) 的 8 条按 adapter 规则复制并替换起服务的 fixture；[INVENTORY.md](../../tests/python/adapter/INVENTORY.md) 中这 8 条与 schema 一行的状态同步。
- **互通**：3.x 写、本实现读与反向，Arrow 15 / 25。
- **已有用例的更新**（都属于「测试断言的是切片前的范围」，提交说明写明）：
  - [test_grpc_translation.py](../../tests/python/test_grpc_translation.py) 中 GEOMETRY 建表与 `st_isvalid(id)` 返回 UNIMPLEMENTED 的两条：前者改为成功，后者改为类型错误。
  - [test_filter_s1.py](../../tests/differential/test_filter_s1.py) 中 `geometry_contains(title, 'POINT (0 0)')` 返回 UNIMPLEMENTED 的断言：`title` 是 VARCHAR，改为与 3.x 对照的类型错误。
  - [filter.rs](../../crates/core/src/filter.rs) 中断言几何谓词返回 `Unsupported` 的单元测试。
  - 解析真值（`generate_filter_parse_fixtures.py`）不变。

## 第 1 步验收

开发验收（2026-11-13，最终代码，本机 macOS arm64，串行执行）：

| 项目 | 结果 |
| --- | --- |
| 解析的单元测试（[geometry.rs](../../crates/core/src/geometry.rs)） | 4 条通过：空白集合遍历全部码位恰为 29 个；`POINT` / `POLYGON` 每个字母位置遍历全部码位，接受的只有 ASCII 大小写与 I 的两个额外码位；点与多边形的接受 / 拒绝清单。清单里的每条期望另用 3.x 的 `parse_wkt` 逐条核对，零不一致 |
| 引擎测试（[tests/geometry.rs](../../crates/core/tests/geometry.rs)） | 5 条通过：原文经 WAL、flush、合并与重开不变，WAL 与 Parquet 的列类型是 `string`，`schema.json` 的 `dtype` 是 `geometry`；无效 WKT 整批拒绝且不落盘；主键、partition key、`ARRAY<GEOMETRY>`、默认值的规则；引用 GEOMETRY 字段的 15 种过滤全部 UNIMPLEMENTED；改写 `schema.json` 得到的 `ARRAY<GEOMETRY>` 集合打不开、可以 drop、不影响同目录的集合 |
| 端到端差分（[test_geometry.py](../../tests/differential/test_geometry.py)，10 条） | 78 种 WKT 写法逐条写入并读回，两边的成败、错误文案与原文一致，覆盖 MemTable、重开、flush、compaction 与再次重开；nullable、默认值、nullable 加默认值、upsert、partial upsert、Delete、Get、Search、HybridSearch 与 query iterator 的输出一致，三个集合的 `schema.json` 逐字节相同；原始 gRPC 的主键 / partition key 文案、四种默认值槽位、12 种写入槽位组合与输出槽位一致；登记差异（`ARRAY<GEOMETRY>` 经 pymilvus 与原始 gRPC 建表、`bytes_data` 与非有限浮点默认值）分别断言；与 rerank 虚拟列同名的 GEOMETRY 字段（`_decay_score`）在 decay 下输出衰减因子的文本，与 3.x 一致；3.x 建出并经原始 gRPC 写入的 `ARRAY<GEOMETRY>` 目录由本实现打开：明确拒绝、文件逐字节不变、drop 成功；中间状态的 5 种表达式经 8 个入口全部 UNIMPLEMENTED 且没有被执行 |
| 互通（[test_geometry.py](../../tests/storage_interop/test_geometry.py)，6 条） | 本实现、3.x（Arrow 15）、3.x（Arrow 25）各自写出 WAL 与已 flush 的目录，三方读到的行、默认值与 Describe 相同；另一方插入、upsert、删除并 flush 后三方仍相同 |
| fmt / clippy（默认与 fault-injection） | 无告警 |
| cargo test --workspace / fault-injection | 665 / 802 通过 |
| tests/python（pymilvus 3.0.2 / 2.6.18） | 各 653 通过 |
| 差分（pymilvus 3.0.2 / 2.6.18，参照 Arrow 25） | 各 847 通过（会话组 404）；评审后追加 1 条（虚拟列同名），该文件 10 条在两条 SDK 线上重跑通过，总数 848 |
| 互通（Arrow 15 / 25） | 311 通过 |
| 工作流与发行工具自检（tests/release、ci、sharding、packaging） | 871 通过；计数已同步到 ci.yml、compare.yml、scorecard、发行测试、分片与打包 README |

已有用例的更新：[test_grpc_translation.py](../../tests/python/test_grpc_translation.py) 里「GEOMETRY 建表返回 UNIMPLEMENTED」断言的是切片前的范围，改为断言 `ARRAY<GEOMETRY>` 建表返回 UNIMPLEMENTED；`st_isvalid(id)` 的断言留到第 2 步。

没有执行：Linux 与 x86_64 上的运行（由 CI 与 Compare 覆盖）；性能测量（本步不改变已有类型的路径，不作性能声明）。

## 证据

实测脚本与原始输出在仓库外的本地留存目录（[证据的存放与复核范围](../testing/README.md) 的「本地留存」一类），实施第 3 步时由仓库内的生成器、差分与模糊对照取代：

- `p1`：pymilvus 3.0.2 的建表、写入、查询、检索、删除、计数、Unicode 边角与错误文案。
- `p2`：原始 gRPC 的主键 / partition key、默认值槽位、写入槽位、输出、400 位与 `1e-400` 的距离、`schema.json` 与 Parquet 的列类型。输出里 `describe dl`、`dl insert`、`dl query` 的报错是探针顺序造成的（`dl` 因默认值无效没有建出来），不是产品行为。
- `p3`：nullable 加默认值、两个 GEOMETRY 字段的比较、与算术混用、HybridSearch、boost、两种 iterator、flush 与 compaction 之后、原始 gRPC 写入 `ARRAY<GEOMETRY>`。
- `hypot_probe`：CPython 3.12.14 的 `math.hypot` 对 libm，固定种子 30 万组，48 056 组不同（16.0%），最大 1 ULP。
- 正则的大小写等价集、空白集合与 `float()` 边角：CPython 3.12.14 上对全部码位穷举。
