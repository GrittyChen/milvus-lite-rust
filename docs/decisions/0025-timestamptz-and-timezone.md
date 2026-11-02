# 0025 TIMESTAMPTZ 与 timezone 属性

- 状态：设计评审通过（r2）；第 1 步（类型与存储）已实施，第 2–4 步待做
- 日期：2026-11-02
- 参照：milvus-lite v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`；pymilvus 3.0.2 实测，2.6.17 经源码核对（`timezone`、`time_fields` 参数与 `DataType.TIMESTAMPTZ` 同样存在，实施阶段补测）
- 前置：[0014](0014-index-definitions-and-filter-expressions.md)（过滤语法与 ISO / INTERVAL 字面量的语法校验）、[0015](0015-nullable-defaults-and-array.md)（nullable 与默认值）、[0023](0023-aliases-rename-truncate-databases.md)（集合与数据库属性；其取舍 1 把 `timezone` 留给本切片）、[0024](0024-group-by-search.md)
- 调查证据：探针脚本（MilvusClient 269 个场景、原始 gRPC 90 个场景、表达式语义 42 个场景、校验时机与错误优先级 39 个场景、落盘形态）、3.x 原始输出与 jiff 原型（固定 jiff 0.2.38 / jiff-tzdb 0.1.9，含 29 组夏令时样本的输出）由负责人本地留存，实施阶段整理成差分场景表与 fixture

## 依据与范围

0024 合入后的缺口统计中，TIMESTAMPTZ 挡住 16 条参照用例，数据库 `timezone` 属性挡住 7 条（含 1 条 TIMESTAMPTZ 用例），合计 23 条；另有 3.x 的引擎层用例与 `AlterCollection` / `AlterDatabase` 的 `timezone` 取值。当前仓库对此全部返回 UNIMPLEMENTED（类型、属性、ISO / INTERVAL 字面量求值）。

范围：

1. 类型：`TIMESTAMPTZ` 标量字段（含 nullable、默认值），写入、读取、检索输出、Upsert / Delete / 过滤。
2. 过滤：`ISO '...'` 与 `INTERVAL '...'` 字面量的求值，时间戳的比较、`in`、`is null`，时间戳与区间的加减。
3. 时区：请求 `timezone` 参数、集合 `timezone` 属性、数据库 `timezone` 属性，以及它们对「没有偏移的时间字符串」的解释；`time_fields` 输出。
4. 持久化：与 3.x 双向互通（`schema.json`、WAL、Parquet、INVERTED 索引定义）。

不在范围内：一般数值算术（`id % 2`、`n + 1` 等，仍为 UNIMPLEMENTED，只放开「时间戳 ± 区间」）、ARRAY 的 TIMESTAMPTZ 元素（3.x 能建表，本切片保持 UNIMPLEMENTED，见差异）、函数链与 rerank 里的时间衰减、分组（3.x 对 TIMESTAMPTZ 报类型错误，沿用现有文案）。

## 参照行为（3.x 实测）

### 数据表示与落盘

| 项 | 3.x |
| --- | --- |
| 内部值 | UTC Unix 微秒的 int64 |
| `schema.json` | `"dtype": "timestamptz"`，`default_value` 是微秒整数，集合属性 `properties.timezone` 原样保存（`"utc"` 之类大小写不规范的取值也原样保存） |
| WAL（Arrow IPC） | `timestamp[us, tz=UTC]`，非 nullable 字段带 `not null` |
| Parquet | 同上，`timestamp[us, tz=UTC]`；默认值在写入时已填入数据；`DescribeCollection` 回显默认值（`default_value.timestamptz_data` 为微秒） |
| 索引 | 允许 `INVERTED`，manifest 里和其他标量字段一样 |
| 保留字 | `iso`、`ISO`、`interval`、`INTERVAL` 不能作字段名（报 6） |
| 不允许 | 主键、分区键（客户端先拒绝）、分组字段（`group_by_field 'ts' has type TIMESTAMPTZ which is not supported for group_by`） |

### 写入

pymilvus 把时间值作为字符串（`string_data`）发送；原始 gRPC 还可以用 `timestamptz_data` 发整数微秒。

- 带偏移的 ISO 8601 字符串按 CPython 3.12 `datetime.fromisoformat`（前后空白与 `Z` 先处理）解析，偏移含 `+08`、`+0800`、`+08:00:30`；多于 6 位的小数**截断**到微秒；周日期、紧凑格式可用，`24:00`、`60` 秒、小写 `z`、年 0 与 10000 报 6 `TIMESTAMPTZ value '...' is not a valid ISO 8601 timestamp`。这一套语法校验 0014 已按 CPython 实现并做过模糊对照（[temporal.rs](../../crates/core/src/filter/temporal.rs)），本切片复用同一个扫描器并让它返回数值。
- 没有偏移的字符串：集合 / 数据库 / 请求都没有配置时区时报 6 `TIMESTAMPTZ value must include a timezone offset or use a configured timezone`；有时区时按该时区的**本地时间**解释（见下）。
- 换算后超出 `0001-01-01` 到 `9999-12-31T23:59:59.999999` 报 1 `date value out of range`（含带偏移的边界值，如 `0001-01-01T00:00:00+01:00`）。
- 原始 gRPC 的 `timestamptz_data` 整数**不做范围校验**：`2^62`、`INT64_MAX` 等都能写入，之后任何读到这些行的查询（包括带过滤的）整体报 1 `date value out of range`，**3.x 自己的 flush 也会失败**（毒化值只能留在 WAL），等于一次写入毒化整个集合。本实现不复制，见差异。`long_data` 槽被忽略（`FieldData 'ts' has 0 rows`）。
- 字符串放进 VARCHAR 槽（`FieldData.type = 21`）：3.x 由引擎按 schema 解析，带偏移的 ISO 字符串写入成功（与 type 26 的 `string_data` 结果相同）；无偏移、非 ISO、空串报 6 `field 'ts' value '...' does not match dtype DataType.TIMESTAMPTZ`（不是 type 26 路径的文案），带偏移越界报 1 `date value out of range`；空值行按 `valid_data` 跳过。本实现在引擎的类型检查处接受并解析 `Str`，文案与错误码同 3.x。已配置时区后无偏移字符串经该槽的行为留到第 3 步实测核对。
- 默认值：创建集合时按集合 `timezone`（其次数据库）规范化成微秒存进 schema；naive 默认值且没有时区时创建失败（6）；写入时 `None` 与缺省都填默认值。**整数默认值创建时不查范围**：`MAX+1`、`MIN-1` 等 3.x 创建成功，`DescribeCollection` 回显原值，含它的目录必须能被本实现打开；字符串默认值换算后越界仍在创建时报 1。使用越界默认值的写入见差异。

### 读取与输出

- 响应的 `FieldData.type = 26`，值放 `string_data`，格式是 UTC 的 ISO 字符串：以 `Z` 结尾，微秒为 0 时省略小数，否则固定 6 位（`2025-01-02T12:30:00.500000Z`）；空值走 `valid_data`。
- `time_fields`（逗号或空白分隔，不区分大小写，空段忽略，可重复，顺序保持）：该字段改为输出 `Array<Int64>`（`type = 22`，元素 int64），按有效时区取 `year / month / day / hour / minute / second / microsecond`；空值整行 `valid_data = false`。`time_fields` 为空串或只有分隔符时按普通输出；不是 TIMESTAMPTZ 的输出字段不受影响。
- 有效时区的优先级：请求 `timezone` → 集合 `timezone` → 数据库 `timezone`；`time_fields` 的默认时区是 `UTC`（与过滤里「没有时区则报错」不同）。

### 校验时机与错误优先级（3.x 实测，39 个场景）

请求 `timezone` 与 `time_fields` 不是在请求入口统一校验，而是在两个固定位置：

1. **过滤编译处**：表达式**非空**时，先校验请求时区（坏名字报 6），再解析表达式。所以 `id >` 加坏时区报时区错误；空表达式不经过这里。
2. **响应编码处**（检索 / 查询执行之后）：Query 与 Search 构造响应时再校验一次有效时区，**即使表达式里没有时间字面量、结果为空**也会报 6；`count(*)` 不构造响应，因此 `count(*)` 加空表达式加坏时区**成功返回计数**，带非空表达式时才在第 1 处报错。

完整先后（越靠前越先报）：

| 路径 | 顺序 |
| --- | --- |
| Query | 集合不存在（100）→ 未加载（101）→ 非空表达式时：坏时区（6）→ 表达式语法 / 类型错误（6）→ 执行 → 响应编码处：坏时区（6）→ 编码非空 TIMESTAMPTZ 值时的坏 `time_fields` 组件（6） |
| Search | 集合不存在（100）→ 未加载（101）→ 分组字段（6，0024）→ `anns_field`（6）→ 非空表达式时：坏时区（6）→ 表达式错误（6）→ 度量名（1）→ 执行 → 响应编码处：坏时区（6）→ 坏 `time_fields` 组件（6） |

要点：集合不存在与未加载永远最先；表达式非空时，过滤编译处的坏时区先于表达式错误，也先于度量名（1）；表达式为空时，度量名错误（1）先于响应编码处的坏时区（`bad metric + bad tz`、空表达式报 1）；分组字段与 `anns_field` 错误（6）先于任何坏时区。

`time_fields` 的组件名**只在真正编码一个非空 TIMESTAMPTZ 值时**才校验：`weekday` 在零行结果、整列为空的结果、时间戳字段不在 `output_fields` 里、空集合上都**成功**，只有输出一个非空时间戳值时才报 6 `unsupported time_fields component 'weekday'; supported: year, month, day, hour, minute, second, microsecond`。`count(*)` 不受 `time_fields` 影响（坏组件也成功）。

适用范围：Query（含 QueryIterator 的 Query RPC，行为相同）、Search、`count(*)`、按主键取值；Delete 不带请求 `timezone`，只用集合 / 数据库时区；HybridSearch 的 `time_fields` 随 HybridSearch，不在本切片。

### 过滤

| 构造 | 3.x |
| --- | --- |
| `ts >= ISO '...'`、`==`、`!=`、`<`、`<=`、`>`、`ISO '...' < ts`、字段与字段比较 | 按 UTC 微秒比较；`ISO` 不区分大小写，引号可以单或双，`ISO'...'` 可以连写 |
| `ts in [ISO '..', ...]`、`not in` | 支持；列表里混入整数、放 `INTERVAL` 报 6 |
| `ts + INTERVAL 'P1D'`、`ts - INTERVAL ...`、`INTERVAL ... + ts`、`ISO '..' + INTERVAL ..` | 返回时间戳；其他组合（时间戳减时间戳、区间加区间、时间戳加整数、一元负号）报 6 `arithmetic '+' requires numeric operands, left side is ...` |
| 时间戳与整数 / 浮点 / 字符串 / 区间比较 | 报 6 `comparison '>' between incompatible types ...` |
| `ts like`、`is null`、`is not null` | `like` 报 6；空值判断正常；比较遇空值为 NULL（不命中，`not` 之后也不命中） |
| 动态字段 / JSON 路径与 `ISO` 字面量比较 | 不报错，结果为空 |
| 字面量与字面量比较 | 允许（`INTERVAL 'P1D' > INTERVAL 'P2D'` 恒为假） |
| 区间语法 | `P[nW][nD][T[nH][nM][nS]]`，**不区分大小写**，数字可以带小数；`P1DT`、`P1.5D` 合法，`PT`、`PT0S`、`P-1D`、`P1Y`、空串报 6 `unsupported INTERVAL ...`，`P0D` 例外合法；微秒数按浮点计算后**向零截断**（`PT1.9999999S` = 1999999 µs） |
| 算术溢出 | 时间戳与区间都是 int64 微秒，加减按 **int64 回绕**（Arrow 非检查运算）；区间的微秒数超出 int64（`P106751992D` 以上）报 1 `Python int too large to convert to C long` |
| 含越界整数微秒的行 | 见上，整个查询报 1 `date value out of range`；对象是原始 gRPC 写入的脏数据 |

### 时区解析

- 时区名交给 Python `zoneinfo` 的 `ZoneInfo`，数据来自**运行平台的系统 tzdata**：macOS 上路径不区分大小写（`asia/shanghai` 可用），Linux 区分；`posixrules`、`Factory`、`localtime` 等是否可用同样取决于平台。
- 报错分类：名字找不到 → 6 `unknown timezone '...'`；绝对路径、不规范路径（`Asia/Shanghai/`、`America//New_York`）、越出 TZPATH（`../etc/passwd`）→ 1，文案是 zoneinfo 的 `ZoneInfo keys may not be absolute paths, got: ...` 等三种；指向非 TZif 文件（`zone.tab`）→ 1 `Invalid TZif file: magic not found`；非字符串（整数）→ 1 `Unexpected error, message=<...>`。属性里的 `timezone` 为空串或不是字符串 → 6 `timezone property must be a non-empty string`。
- 校验后**原样保存**取值：`"utc"` 保存为 `utc`，不规范化为 `UTC`。
- 没有偏移的本地时间按 `datetime.replace(tzinfo=ZoneInfo)` 再转 UTC，即 Python 的 `fold=0` 规则：**重复的本地时间取第一次出现**（美国夏令时结束时的 `01:30` 取夏令时一侧），**不存在的本地时间沿用变换前的偏移**（`2025-03-09T02:30` 在 New_York 得到 `07:30Z`）。历史偏移（含秒，如 New_York 1883 年前的 `-4:56:02`）、遥远未来（按 POSIX 规则尾巴）、`Lord_Howe` 的半小时夏令时、`Etc/GMT±N` 都与 tzdata 一致。换算后越界（如 New_York 的 `9999-12-31T23:59:59`、Tokyo 的 `0001-01-01T00:00:00`）报 1 `date value out of range`。
- 属性变更：`AlterCollection` / `AlterDatabase` 改 `timezone` 只改变「以后」对无偏移字符串的解释与 `time_fields` 的默认时区，**已存数据不变**；数据库时区改了立即影响该库已打开的集合。删除属性后恢复为未配置。

## 设计

### 类型与存储（第 1 步）

- `core::DataType` 增加 `Timestamptz`。列沿用 `ColumnData::Int64`（UTC 微秒），类型差异只体现在 schema：写入解析、输出编码、过滤类型检查、`schema.json` 与 Arrow 映射。这样 Upsert / Delete / 合并 / 主键之外的所有列处理都不用改。
- `storage::schema_file`：`dtype` 认 `timestamptz`；`default_value` 以微秒整数存取；`storage::arrow_data`：`Timestamp(Microsecond, Some("UTC"))` 与 Int64 列互转，读取时也接受 `tz` 为 `UTC` 的 3.x 文件，写出时与 3.x 完全一致（`ARROW:schema` 元数据含该类型）。
- gRPC：`schema.rs` 识别类型 26 与默认值的 `timestamptz_data` / `long_data` / `string_data`；`insert.rs` 解码 `string_data` 与 `timestamptz_data`（`long_data` 槽按 3.x 视为空）；`output.rs` 输出 ISO 字符串；描述集合时类型为 26，默认值回显 `timestamptz_data`（微秒）。
- 整数微秒（`timestamptz_data`）在解码处校验到 `[0001-01-01, 9999-12-31T23:59:59.999999]`，越界报 1 `date value out of range`，**整批拒绝、零写入**，不复制 3.x 的「写入成功、读取全体失败」。
- 读取 3.x 已经写出的越界整数（毒化目录）：输出换算处对同一范围做检查，越界报同文案的 1，不 panic、不返回截断值；该报错与 3.x 一致（3.x 对这类目录本来就整体失败）。互通用例用 3.x 经原始 gRPC 写出的毒化 fixture 钉住。
- INVERTED 索引定义接受该类型（与其它标量一致，按扫描执行）；保留字 `iso` / `interval` 加入字段名检查。
- `ARRAY<TIMESTAMPTZ>`：把 `Timestamptz` 加入 [schema.rs](../../crates/core/src/schema.rs) 里 ARRAY 元素的拒绝列表（现为 Array / Json / FloatVector），报 `EngineError::unsupported`（UNIMPLEMENTED，文案 `ARRAY of TIMESTAMPTZ ...`）；不拒绝的话枚举新增后会被接受，之后在数据层失败。create / insert 各有测试钉住。

### 过滤（第 2 步）

- 解析阶段把 `ISO` 字面量算成 UTC 微秒、`INTERVAL` 算成微秒（浮点公式与向零截断同 3.x）；无偏移的 ISO 先走第 3 步给出的时区，未配置时给出 3.x 的 6 错误。
- 语义检查增加时间戳 / 区间两个类型：比较与 `in` 只允许同类型；加减只放开「时间戳 ± 区间」「区间 + 时间戳」；其余算术保持现有 UNIMPLEMENTED，但用到时间戳 / 区间的错误组合先报 3.x 的类型错误（与 3.x 的先后顺序一致，由差分钉住）。
- 求值：列是 Int64 微秒，加减用 `wrapping_add` / `wrapping_sub` 复刻 int64 回绕；区间微秒超出 i64 报 1 `Python int too large to convert to C long`。动态字段 / JSON 路径与时间戳比较不命中。

### 时区（第 3 步）

#### 时区库

- 新依赖 **jiff 0.2.38**（`default-features = false`，只开 `std` 与 `tzdb-bundle-always`；传递依赖只有 `jiff-core` / `jiff-tzdb 0.1.9`，`jiff-static` 不参与，无构建脚本、无 C 代码，MIT / Unlicense；内置数据约 276 KB）。版本写死在 `Cargo.toml`，升级须重跑 fixture。
- 在原型（已归档，固定 jiff 0.2.38 / jiff-tzdb 0.1.9）里对 3.x 的 29 组夏令时 / 历史 / 未来 / 半小时 / 极端日期实测，`TimeZone::to_zoned` 的 compatible 消歧与 Python `fold=0` **逐项一致**。
- 名字大小写不敏感是 jiff-tzdb **当前实现**，不是文档保证（文档只保证 UTC / Etc / Unknown）；因此封装在一个 `timezone::resolve(name)` 函数里，并加大小写单元测试，升级 jiff 时由测试暴露变化。
- **范围边界**：jiff `Timestamp` 的范围是 `-009999-01-02T01:59:59Z` 到 `9999-12-30T22:00:00.999999999Z`，比 Python 的 `[0001-01-01, 9999-12-31T23:59:59.999999]` 的**高端少约 26 小时**，低端多出年 0。用 jiff 换算失败或结果落到 Python 范围外时：本地时间在高端附近（`9999-12-29` 起）取 `9999-12-30T22:00Z` 处的偏移补算（该处没有偏移变换），再在 i128 微秒上自行检查 `[0001, 9999]`，越界报 1 `date value out of range`。例：`Asia/Tokyo` 的 `9999-12-31T23:59:59` 对应 UTC `14:59:59`，在 Python 范围内、超出 jiff 上界，是补算必须覆盖的样本，与 `Pacific/Kiritimati`（`+14`）、`America/New_York`（越界）、`Asia/Tokyo` 的 `0001-01-01`（越界）一起进 fixture。
- 时区名校验：名字找不到 → 6 `unknown timezone '...'`；复刻 zoneinfo 的三种路径格式错误（1，文案同 3.x）；其余 3.x 因平台文件而异的情形（非 TZif 文件、`posixrules`、`localtime`）不复刻，见差异。

#### 时区解析的归属与锁

有效时区 = 请求 `timezone`（非空）→ 集合 `properties.timezone` → 数据库 `database_properties.json` 的 `timezone`。解析由 **gRPC 层在集合锁之外**发起，经一个很小的 core 类型传入引擎：

- core 增加 `TimezoneSource`：持有请求值、集合属性值（取自读快照 / schema 副本，不再回头锁集合）与一个闭包 `database: &dyn Fn() -> Result<Option<String>>`；API 把两件事分开：`validate_request()` 只校验请求值（过滤编译处与响应编码处调用，不触碰集合或数据库属性）；`for_naive()` / `for_time_fields()` 才按「请求 → 集合 → 数据库」解析有效时区，**只有这时才可能调用数据库闭包**。没有 `time_fields`、没有无偏移字符串的 Query 因此在数据库属性损坏时仍然成功，坏请求时区仍报 6（有对应用例）。闭包由 `Db` 实现：取 `DatabaseState` 的叶子锁（0023 B 的注册表叶子锁，取完 `Arc` 即释放），读缓存的属性；缓存未命中时加载文件，失败不缓存。叶子锁**从不**在持有集合读锁、写锁或写许可时获取，因此不引入新的锁序边。两把叶子锁的次序固定：先取数据库**注册表**叶子拿到 `Arc<DatabaseState>` 后立即释放，再取该状态的**属性**叶子；持有其中一把时不得获取另一把，debug 锁序检查覆盖这一点。
- **Query / Search**：`db.collection()`（100）→ 读锁内取快照后释放 → 引擎 `require_loaded`（101）→ 分组 / anns（Search）→ 过滤编译处：表达式非空时 `effective()` 的**请求值校验**先于解析 → 度量 → 执行 → 响应编码处再次 `effective()`（这是 3.x 的第二个校验点；`count(*)` 空表达式既不经过过滤编译也不经过响应编码）。`time_fields` 默认时区在编码一个非空时间戳值时才向下解析到集合 / 数据库 / `UTC`。
- **Insert / Upsert**：`db.collection()`（100）→ 取集合属性值（读锁内复制后释放）→ 数据库闭包（叶子锁）→ **gRPC 解码处**把 `string_data` 的时间字符串换算成微秒（3.x 同样在 `fields_data_to_records` 里先于引擎校验解析，错误顺序因此不变）→ 之后才进入 `Db::mutate`（写许可、集合写锁）。解码到写锁之间集合被删除重建、schema 变化时，`mutate` 内既有的批校验照常拒绝。
- **Delete（表达式）**：同样在 `Db::delete_expr` 进入集合锁之前解析集合 / 数据库时区（3.x 的 Delete 不带请求时区）。
- **CreateCollection**：naive 默认值的时区取请求属性里的 `timezone`，其次数据库闭包；在已持有的命名空间锁 / 独占许可之内调用叶子锁（叶子锁最后取，无反向边），规范化后写进 schema。
- **AlterDatabase / DropDatabaseProperties** 更新叶子锁里的缓存（先写文件、再换缓存，沿用 0023 B 的 `Unchanged` / `Unconfirmed` 处理）；下一次请求的闭包读到新值，所以数据库时区的改动对该库**已打开的集合**立即生效。**不引入**以不含时区的表达式文本为键的编译结果缓存（3.x 正是因为这个缓存要在改库时区时清空）。
- **数据库属性不可读**（文件损坏、I/O 错误）：只有真的需要数据库时区的操作（无偏移字符串、`time_fields` 的默认时区、naive 默认值）报 1/0 存储错误，**零写入**（Insert 在解码处失败，尚未取写许可）；不需要时区的操作照常，沿用 0023 B「集合操作不读属性」的立场并只在需要时读。需要在 0006 的「只有 Describe / Alter 读」一行和 0023 取舍 1 里同步这一变化。
- 测试：debug 锁序检查下的并发用例（并发 AlterDatabase 改时区 × Insert / Query / Search / Delete / CreateCollection，断言无死锁、每次解析都看到完整的改前或改后的值；同一请求的过滤编译与响应编码各解析一次，其间允许看到不同的值，与 3.x 的每次读取当前属性一致，不做按请求记忆）；损坏属性文件下的 naive insert / filter / `time_fields` / 默认值用例。

#### `time_fields`

Query、Search 输出支持（Hybrid 随 HybridSearch）；非 TIMESTAMPTZ 字段上的 `time_fields` 不生效；组件名只在编码非空时间戳值时校验（见「校验时机」）。

### 属性 RPC（第 3 步）

- CreateCollection / AlterCollection / DropCollectionProperties / CreateDatabase / AlterDatabase / DropDatabaseProperties 的 `timezone`：非空字符串且在时区库内才保存（原样保存，不规范大小写）；错误文案与 3.x 相同；移除 0023 登记的两个 UNIMPLEMENTED 守卫（`schema.rs` 的集合属性、`db.rs` 的 `database_timezone_unsupported`）及 search / query 的 `timezone` 守卫。
- 创建集合时 naive 默认值按有效时区规范化后写进 schema；已存集合的 schema 不因之后改时区而变。
- **需要同步改写的既有断言**（移除守卫后会失败）：[collection_properties.rs](../../crates/core/tests/collection_properties.rs)、[db.rs](../../crates/core/tests/db.rs)、[databases.rs](../../crates/core/tests/databases.rs) 中固定 `timezone` 为 UNIMPLEMENTED 的用例，[test_grpc_translation.py](../../tests/python/test_grpc_translation.py) 里三处同类断言，以及 adapter 副本里因 `timezone` 范围外的 INVENTORY 行。新语义：坏时区 6 且零写入；合法值可保存、展示、删除；已有的 3.x 目录里带 `timezone` 的属性现在可读写。同步改写 0023 取舍 1 与已登记差异、0006 对应行、README 的「`timezone` 仍为 UNIMPLEMENTED」两处与 AGENTS 索引。

### 持久化兼容（规则 8）

- **新格式**：`schema.json` 出现 `timestamptz`，WAL / Parquet 出现 `timestamp[us, tz=UTC]`。这是 3.x 自己的格式，本切片只是开始读写它，数据目录与 3.x 双向互通（新增互通用例与 fixture，两个方向都验证）。
- **降级**：0.3.0rc3 及更早的本实现读到 `timestamptz` 的 `schema.json` 会因未知 `dtype` 拒绝打开该集合（`deny_unknown_fields` / `known_dtype`），数据保留；README 已有的降级说明里补这一点。
- **时区名与 3.x Linux**：本实现接受并原样保存大小写不规范的名字（如 `utc`），3.x 在 Linux 上的 `ZoneInfo` 会拒绝；互通 fixture 一律使用规范名，README 与验收记录说明：用非规范大小写名字写入的数据目录搬到 Linux 上的 3.x 可能无法使用。

## 与参照的差异（预案，评审通过后正式登记）

| 项 | 3.x | 本实现 | 理由 |
| --- | --- | --- | --- |
| 原始 gRPC 写入越界的整数微秒 | 写入成功，之后读到这些行的任何查询整体报 1，自身 flush 也失败 | 写入时报 1 `date value out of range`，整批拒绝、零写入 | 一次写入毒化整个集合不应被复制 |
| 使用越界整数默认值的写入（默认值 `MAX+1` / `MIN-1` 等，创建与描述两边一致） | 写入成功，之后读取整体报 1，flush 失败 | 该写入报 1 `date value out of range`、零写入；显式给值的写入照常 | 同上，是同一毒化的另一入口；打开含该默认值的 3.x 目录不受影响 |
| 超出 int64 的 JSON 整数默认值 | 经 protobuf 的 int64 字段无法产生，只可能来自手改的 `schema.json` | 创建 / 打开报 1 `Python int too large to convert to C long` | 不可达的输入，沿用 Python 的文案 |
| 时区库 | 运行平台的系统 tzdata，路径不区分大小写与否随平台，`posixrules` / `localtime` 等平台文件可用与否随平台 | 内置 tzdata（jiff），名字大小写不敏感（与 macOS 一致、含 Linux 上 3.x 拒绝的大小写变体），不含 `posixrules` / `localtime` | 不依赖运行环境，同一个数据目录行为确定；tzdata 版本不同时个别历史 / 未来规则可能不同，已知并登记 |
| 找不到的时区名 | 找不到 6；指向非 TZif 文件 1；其余路径错误 1 | 找不到（含 `zone.tab` 等）6；三种路径格式错误 1（文案同 3.x） | 非 TZif 文件的报错依赖平台文件，不复刻 |
| 高端边界附近（`9999-12-29` 起）无偏移时间的换算 | 由 zoneinfo 精确换算 | 用 `9999-12-30T22:00Z` 处的偏移补算（该处没有偏移变换），结果与 3.x 一致，越界同样报 1 | jiff 的 `Timestamp` 上界比 Python 低约 26 小时 |
| ARRAY 的 TIMESTAMPTZ 元素 | 能建表（pymilvus 无法写入元素，数据为空） | 创建 UNIMPLEMENTED；打开含该字段的 3.x 目录时，访问该集合 fail-closed 为带明确文案的 UNIMPLEMENTED（不是 100），其他集合不受影响，互通用例钉住 | 范围外，需要时单独做 |
| 区间微秒超出 int64 | 1 `Python int too large to convert to C long` | 同 3.x | 复刻 |
| 数据库 `database_properties.json` 不可读时的 naive 时间字符串 / `time_fields` / naive 默认值 | 数据库打开时已经读过该文件，损坏的库根本打不开 | 只有这些操作报 1/0 存储错误，零写入；其余操作不读该文件 | 0023 B 已登记「集合操作不读属性」，只对需要时区的操作例外 |
| 读取 3.x 毒化目录（越界整数微秒） | 任何读到这些行的查询整体报 1 | 同样报 1 `date value out of range`，不 panic | 一致；本实现只是不再能写出这样的目录 |

## 测试矩阵

- **解析与换算单元**：ISO 值解析（沿用 0014 的 CPython 模糊对照，扩展为返回数值，对照 `datetime.fromisoformat` + `astimezone`）；区间浮点公式与向零截断；`fold=0` 规则的夏令时 / 重复 / 不存在 / 历史 / 未来 / 极端样本，用 Python 生成的 fixture（含 `--verify` 脚本，沿用 0023 B 属性字节样本的做法），**必含** `Asia/Tokyo` 的 `9999-12-31T23:59:59`、`Pacific/Kiritimati`、`America/New_York` 的 `9999-12-31`、`Asia/Tokyo` 的 `0001-01-01`；时区名大小写与伪时区名的单元测试（`utc`、`ASIA/SHANGHAI`、`Factory`、`posixrules`、`localtime`、三种路径格式错误）。
- **引擎**：类型的写入、读取、Upsert、Delete、nullable、默认值（含 `DescribeCollection` 回显 `timestamptz_data`）、重开、合并、WAL 恢复；过滤的比较 / `in` / 算术 / 空值 / 类型错误；int64 回绕与区间越界；请求与属性时区的优先级；数据库时区改动立即生效；`ARRAY<TIMESTAMPTZ>` 在 create 与 insert 上的 UNIMPLEMENTED（文案沿用现有 `ARRAY element type TIMESTAMPTZ is not supported ...`）；主键 / 分区键 / 分组字段 / 保留字 / `INVERTED` 定义；越界整数整批拒绝零写入；读取毒化数据报 1 不 panic；损坏的数据库属性文件下的 naive 写入 / 过滤 / `time_fields` / 默认值（1/0、零写入）；debug 锁序下并发 AlterDatabase 的用例。
- **差分（真实 3.x 对本实现，两条 SDK）**：探针脚本的 269 + 90 + 42 + 39 个场景整理成场景表，覆盖：
  - 写入取值形态、过滤字面量与运算、请求时区矩阵（含坏名字、空串）、夏令时 / 历史 / 极端时区、集合与数据库时区属性及其对已有集合的影响、`count(*)`、搜索与按主键取值、类型限制；
  - **校验时机与错误优先级**（本稿新增）：`count(*)` 空表达式 + 坏时区成功、非空表达式时失败；集合不存在 / 未加载先于坏时区；坏时区先于坏过滤；不存在的输出字段被忽略（不构成错误）；分组 / anns / 度量名与坏时区的先后；空结果与空集合上的坏时区；`time_fields` 坏组件在零行 / 整列为空 / 字段不在 `output_fields` / 空集合 / `count(*)` 上成功，输出非空时间戳值时报 6；Query 与 Search 两条路径各一份；
  - 凡是依赖平台 tzdata 的场景（大小写、伪时区名）不进差分，由引擎单元覆盖。
- **互通**：本实现写、3.x 读与 3.x 写、本实现读两个方向，含默认值、属性时区（规范名）、nullable、`INVERTED` 索引定义；`schema.json` 字节样本；3.x 经原始 gRPC 写出的越界整数毒化目录（读取报 1，不 panic）。
- **QueryIterator**：走 Query RPC，`time_fields` 与普通 Query 相同，用一个迭代器用例钉住；Hybrid 的 `time_fields` 不在本切片。
- **参照用例**：移入 `test_grpc_timestamptz_milvus_compat.py` 的 16 个用例与 `test_grpc_database.py` 里随 `timezone` 阻断的 6 个用例，INVENTORY 与计数同步；同步改写既有的 UNIMPLEMENTED 断言（见「属性 RPC」）。
- **预期计数**：差分、互通、Python 套件的新增数量在各步骤同批同步所有固定点。

## 实施步骤

1. **类型与存储**：`Timestamptz` 数据类型、`schema.json` / Arrow / WAL / Parquet、gRPC 编解码与输出（含默认值回显）、`INVERTED` 定义、保留字、`ARRAY<TIMESTAMPTZ>` 拒绝、整数范围校验与毒化读取、互通 fixture 与用例；此时只接受带偏移的字符串，无偏移仍按「没有配置时区」报错。
2. **过滤**：ISO / INTERVAL 求值、类型检查、`in`、加减与回绕、错误顺序。
3. **时区**：jiff 依赖、时区解析与 `fold=0` 换算及边界补算、`TimezoneSource` 与锁边界、请求 / 集合 / 数据库时区的优先级与两个校验点、无偏移字符串（写入、默认值、过滤字面量）、`time_fields`、属性 RPC（集合与数据库）与既有断言的改写、并发与损坏属性文件的测试。
4. **差分、参照用例、文档与计数**：探针转差分，复制参照用例，0006 差异登记（含「只有 Describe / Alter 读」一行与 0023 取舍 1 的同步），README、AGENTS（依赖、索引）、验收记录与固定统计。

每步写完停下评审，评审通过且授权后才提交。**第 4 步的差分与验收完成之前不推送 main**（第 1–3 步各自带有引擎与互通测试，但对外行为的差分作证在第 4 步）。推送另行授权。

## 已确认的取舍

负责人在设计评审中对六项的结论：

1. **时区库**：采用 jiff 内置 tzdata，版本固定为 0.2.38 / jiff-tzdb 0.1.9，原型与 29 组输出已归档。
2. **时区名大小写**：大小写不敏感地接受，属性原样保存；封装在 `timezone::resolve` 并加单元测试，Linux 上 3.x 的差异已登记。
3. **越界整数微秒**：写入整批拒绝、零写入（1 `date value out of range`）；读取已有的 3.x 毒化数据报同文案的 1。整数默认值创建时不查范围（对齐 3.x，保证含越界默认值的 3.x 目录可打开），使用它的写入同样拒绝。
4. **`ARRAY<TIMESTAMPTZ>`**：保持 UNIMPLEMENTED，在 schema 校验处显式拒绝并钉住错误码与文案。
5. **数据库时区属性**：按需读取，归属、锁边界、错误码与零写入见「时区解析的归属与锁」。
6. **不拆分**：保留一个 0025、四步评审提交；第 4 步完成前不推送 main。
