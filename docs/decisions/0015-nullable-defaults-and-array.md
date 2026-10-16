# 0015 标量 nullable、默认值与 ARRAY

- 状态：切片 A、B 均由负责人评审通过并收口（2026-10-16）。发布线保持 0.2.0 开发线，本轮不切版本或发布
- 日期：2026-10-16
- 参照：milvus-lite v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`；pymilvus 2.6.17 / 3.0.2
- 前置：[0002](0002-data-dir-compat.md)、[0007](0007-m2-storage-engine.md)、[0013](0013-first-release-platform-and-packaging.md)、[0014](0014-index-definitions-and-filter-expressions.md)
- 调查证据：[schema 调查记录](../testing/0015-schema-investigation.md)；离线工具与复现步骤：[compatibility_audit](../../tests/compatibility_audit/README.md)
- 实现证据：[切片 A 验收](../testing/0015-scalar-nullable-acceptance.md)、[切片 B 验收](../testing/0015-array-acceptance.md)；不替代最终候选平台验收
- 负责人评审后修复：[修复与重验记录](../testing/0015-scalar-nullable-review-fixes.md)
- JSON 写入性能合同修订与修复：[回归专项记录](../testing/0015-json-write-regression.md)

## 依据与范围

0014 的固定样本为参照 760 / Rust 375 passed，上游 90 / 75 passed，调用探针成功 43 / 25 项。剩余 384 条明确 UNIMPLEMENTED 中，schema 首个阻断 189 条：nullable 120、ARRAY 42、默认值 3、TIMESTAMPTZ 16、GEOMETRY 7、多向量 1；另有 1 条原 nullable 向量跳过。本轮使用已发布 rc3 正式 wheel 重跑三组原始统计，结果保持相同，不能把样本通过比例当作全功能兼容率。

原始 schema 观察中，120 条 nullable 失败均涉及已有标量类型，未发现其中必须先接入 nullable 向量。移除客户端标量 nullable 后，124 条调查样本中参照 63 条不再通过，两端通过 55 条，另有 2 条继续停在 CreatePartition。两端通过仅是诊断：干预改变输入语义，不能承诺正式实现新增通过 55 条，也不能把未通过部分都归因于新能力缺口。完整分类与其他模式见调查记录。

分两个实现切片，分别评审收口：

| 切片 | 纳入 | 不随切片扩展 |
| --- | --- | --- |
| A | 已有 BOOL、INT8/16/32/64、FLOAT、DOUBLE、VARCHAR、JSON 的 nullable 与默认值；建表、Describe、读取存量 schema、Insert、普通/partial Upsert、过滤和结果输出、WAL/segment/恢复/合并 | nullable 向量、向量默认值、ARRAY、新的标量类型、schema 变更 RPC |
| B | ARRAY 的 BOOL、INT8/16/32/64、FLOAT、DOUBLE、VARCHAR 元素，父列空值与合法的元素空值、element_type / max_capacity、写入/读取/数组表达式、存量定义中的默认值及双向互通 | 新增真实 INVERTED/ANN、嵌套 ARRAY、多向量、TIMESTAMPTZ、GEOMETRY；不能由内部支持推导 SDK 可声明数组默认值 |

nullable 主键仍按参照拒绝。TIMESTAMPTZ / GEOMETRY 的默认值随类型延后。AddCollectionField 与 AlterCollectionSchema 在参照服务中未实现，继续 UNIMPLEMENTED；不扩大已有 AlterCollection 的能力。

## nullable 向量：调查结论与延后

正常 Collection.insert 先构造 WAL batch，其中把 None 改成零向量；同一个 batch 随后进入 WAL 和 MemTable。不能把这段注释解释为实现了可靠的“零向量识别 null”协议。

本轮在 Arrow 15.0.0 / 25.0.1 上直接调用真实引擎，以 None、真实零向量、非零向量三个对照写入。首次 MemTable、子进程直接退出后的 WAL 重放、Flush 后 segment、实际 compaction 后，原 None 都是可读取、可检索的零向量；segment 的 vector_null_mask 为 None。正常入口并没有等到 WAL 重放才丢失空值信息。直接构造的 Arrow null 则是另一路径，不能与上述结果混为一谈。

**本决策延后 nullable 向量实施。** A/B 中仍明确拒绝其新建与存量 collection，不为追求一致的空值语义引入私有磁盘标记，也不把真实零向量判为 null。后续向量切片独立验证 raw gRPC、两条 SDK、Arrow null 来源、全部生命周期与度量，再按规则 2 对缺陷行为或差异作明确决定。上述实测是限定范围的引擎证据，不是向量协议的全量验收。

## 四项先冻结的边界

### 缺失、显式 null、默认值与 partial Upsert

默认值的“存在”独立于 Python 真值：False、0、0.0、空字符串与空容器都不能当作未设置。参照在记录校验前填充缺失或 None 的默认值；无默认值时，缺失/None 能否保留为空由 nullable 决定。schema.json 中 default_value 的 null 表示未设置，保持参照语义。

JSON 默认值沿用 [0004](0004-m1-scope.md#与-3x-的差异) 已登记的任意 JSON 值规则：顶层数值/布尔/字符串正确保存并读回。参照在数值/布尔上可能因 Arrow 字符串列报错，字符串可能丢失引号；本切片不撤销既有差异，也不把这些组合的差分当成普通精确一致用例。非有限数值与 binary 原始默认值暂不支持，CreateCollection 明确返回 UNIMPLEMENTED；不把它们转换成 null 或写出 3.x 不可读的 schema。它们须在专项中扩展，原始错误优先级与参照可能不同。

schema 中数值形式的整数字面量超过 CPython 默认 4300 位限制时同样明确拒绝，避免写出参照无法解析的共享 schema；字符串不按该数值规则拒绝。Describe 的默认值转换复用固定 Unicode 15 十进制分类，保留 Python 的数字字符串限制；protobuf 整数槽位越界报错，不能使用请求参数的历史饱和规则。

保留“字段未提供”与“已提供且为空”的区分，直到引擎决定写入规则。普通 Upsert 按替换记录验证；partial Upsert 的未提供列与显式空值分别冻结，不直接沿用 Insert 的填充算法。SDK 可能将缺失转换成协议空值，必须同时观察客户端实际发出的请求，不能由调用参数推导服务收到的字段集合。

本轮 INT64 协议探针覆盖无默认值、默认 5、默认 0；两条 SDK 在 Insert 的缺失/None 情况下都发送 valid_data=false，默认值由服务端填入。该观察不推广到其它类型、批次或 SDK 版本。

两条 SDK 的 18 项 Upsert 对照也一致：原 x=7 时，普通 Upsert 的缺失列变为 null 或默认值，partial Upsert 的缺失列保留 7；显式 None 在 partial Upsert 中仍清为空或填默认值，显式 0 保留 0。完整请求、返回值和参数组合保存在调查证据中。

### 原始 gRPC 与输出

标量 valid_data 接受参照的紧凑与完整布局，校验行数、掩码长度与有效值数量；省略列与空列不混同。保留列解码、长度/valid_data 检查、逐行业务校验的错误优先级；不能用 false 掩码跳过参照仍会执行的解码错误。

**手写 proto 的坏 JSON 拒绝边界。** 沿用 0004“畸形 JSON 明确报错”的差异，本切片也拒绝被同名后列覆盖的坏 JSON，以及完整标量布局中被 false 掩码遮住的坏 JSON。相较 rc3，同名覆盖不再让前列坏值被接受；带标量空值掩码的请求则从 rc3 的不支持边界进入引擎字段校验，坏 JSON 报 code 6。这样防止覆盖、掩码或默认值隐藏畸形输入，同时保留主键、向量、逐行和 schema 字段顺序的错误优先级。pymilvus 会自行编码合法 JSON，这些畸形内容只能由手写 proto 请求构造；不将参照在后列顶层数值上偶发的 Arrow 错误当作已校验前列坏 JSON 的证据。

Query/Get/Search 输出的值数组与 valid_data 必须对齐，包括空结果、全 null、多查询、多块拼接、投影与分页。JSON 字段的 Arrow 空值、JSON 文本 `null`、动态字段的缺失/null 独立测试。新默认值不能放在 Python 胶水或 adapter 中填充；adapter 只解码/编码，业务规则留在 core。

### 编码、恢复与前后兼容

沿用 3.x 的 schema.json、Arrow IPC WAL 与 Parquet 编码，**不引入新格式、不增加私有字段、不改变现有格式版本号**。新能力只扩展已经存在的 nullable / default_value / element_type / max_capacity 的支持范围；旧数据无需迁移。

标量空值使用 Arrow validity，不用 0、False、空字符串或 JSON `null` 文本充当空值。ARRAY 使用 list<elem>，父 list 的 validity 与元素 validity 分开；空数组、空父列、含空元素的数组不同。Parquet 保留 ARROW:schema 与既有 snappy 设置。schema、列长度、类型或空值约束不符时明确报错，不能用占位数据静默修复历史目录。

互通同时覆盖 3.x 写 → 新 Rust 读、新 Rust 写 → 3.x 读、双方交替写入，Arrow 15/25、未 Flush WAL、已提交 segment、覆盖/删除、合并与旧快照。新 Rust 仍可读 rc1–rc3 的合法目录；新 Rust 写出的旧能力目录仍可回退读取。使用新属性的 collection，旧包应在实际访问时明确拒绝，不能误读或修改其持久化内容；健康 collection 仍可用。服务成功启动不是懒打开 collection 成功的证据。

本轮已用参照生成的目录预检三份旧 wheel；它不能替代实现后由新 Rust 实际写出的回退矩阵。回退在副本上进行，分别比较受影响 collection 的 schema、manifest/.prev、WAL、data/delta、索引与 snapshot 文件名和内容；健康 WAL collection 正常关闭产生 Flush 是允许的。

### 空值过滤：三条整表达式路线

typed 列出现 null 后，仍按 0014 的整表达式路线执行，不改变静态路由，不将所有操作统一套为 SQL 三值逻辑，也不把动态字段缺失规则直接套在 typed null 上。

本轮真实组件探针的例子，x=[None,7,0,None]：

| Arrow 路线表达式 | 掩码 |
| --- | --- |
| x is null | T F F T |
| x in [7] | F T F F |
| x not in [7] | T F T T |
| not (x == 7) | F F T F |

因此不能在所有叶子结果上机械地与列 validity 求交。Arrow/hybrid/Python 的 is null/exists、not、比较、in/not in、and/or、字段比较、全 null、整树回退和 Python 短路分别冻结。三路线组件输入、实际 backend、结果掩码或错误阶段/类型/原文作为真值；Arrow 15/25 不同的错误按已有登记机制处理，不能宽松放过。

范围包括 active/frozen/committed/重开、多源与多块、覆盖后的实际源选择、普通扫描及存量 INVERTED 定义。沿用 0014 的“标量索引定义按扫描执行”差异，不据此宣称与参照真实倒排计划所有结果相同。Query/count/Search/Delete 均验证；Delete 必须在全部求值成功后才分配序号和写 WAL，后一源失败不能留下部分删除。

沿用已登记的 2^63 安全边界与架构差异；新增空值不能成为放宽错误码、数值或返回结构比较的通用例外。

ARRAY 的父 null 与空数组仍按各表达式的实测处理。例如组件输入 [None,[],[7],[None,7]] 时，array_length(x)==0 在 Python 路线匹配前两行；不能自行把所有 null 函数结果改为 Unknown。

## ARRAY 的实施真值边界

固定参照的 336 项 schema/default/输入、152 项 Arrow 元素转换、768 项过滤组件真值在 Arrow 15/25 一致；由 [ARRAY 真值生成器](../../tests/differential/generate_array_fixtures.py) 离线核验，业务真值复用本仓 schema 调查工具并冻结其哈希。不能将标量字段校验直接应用到元素：参照先检查全部记录的外层数组形状、容量及其它业务字段，再由 Arrow 转换元素。INT 元素接受可表示的浮点数并截断，BOOL 不接受整数，FLOAT/DOUBLE 接受 bool；超出精确整数范围和窄整数范围按参照报错。元素 null 合法，子列 nullable 与父列 nullable 独立。

- gRPC 解码按每个 ScalarField 实际 oneof 取值，ArrayArray.element_type 不代替 schema 类型；缺失或不支持的行槽位按参照得到空数组。valid_data 只描述父列。紧凑与完整布局中，被 false 父掩码遮住的普通数组元素不会做 Arrow 转换。
- 元素 null 可经引擎和存量目录保存、参与过滤；Query/Get/Search 若实际投影含 null 元素的数组，参照编码失败并报 code 1。本实现保留相同错误，不将元素 null 编成零值。父 null 与空列表可正常输出；不投影该数组的过滤/count 仍可执行。两条 SDK 的正常数组槽位无法直接声明元素 null，互通用真实引擎入口覆盖。
- ARRAY 的 max_capacity 限制外层元素数量；参照不校验 VARCHAR 元素长度，CreateCollection 的 ARRAY max_length 参数被忽略，Describe 不回显被忽略参数。存量 schema 的 max_length 元数据仍保留。列表默认值按存量 schema 处理，Describe 的 ValueField 无法表达数组，沿参照省略；不据此宣称 SDK 可建列表默认值。
- 静态整表达式路线的数组整体比较/非空 in 会返回 Arrow 无内核错误；空 in/not in 与 Python/hybrid 回退分别遵循真值。错误里的子字段名在 MemTable/WAL 是 `item`，提交为 segment 后为 `element`，按 PyArrow 15/25 的 Parquet 读取口径保留。该信息仅在内存用于诊断，不增加磁盘字段。
- Python 数组成员判断保留同一 NaN 元素的对象身份规则：`array_contains(x,x[0])` 可为真，而 `x[0] == x[0]` 为假；不会把全部 NaN 一概视为相等。
- ARRAY 不接受新建 INVERTED；其它标量的存量 INVERTED 定义与数组谓词组合仍按已登记扫描路径执行。元素 JSON、嵌套 ARRAY、向量元素不在本切片。

## 实施设计

### core 的列与输入

采用“现有稠密类型值列 + 可省略 validity”的列容器，将 ColumnData 的类型值与 validity 封装在同一对象中，让快照、输出、复制、append/take 和 Arrow 转换都携带空值信息。具体以只在实际含 null 时出现的 Nullable 变体包裹现有稠密值列和有效位，禁止嵌套包裹；全部有效列保留原变体，不增加间接读取。避免在 Chunk、Rows、WAL 转换之间用互不约束的平行掩码表。主键和向量在本阶段始终全有效。

- validity=None 表示该列全部有效；声明 nullable 但没有实际空值也不分配持久的空值位图。首次实际空值才物化掩码，长度与值列行数相同，尾位清零。
- null 槽的类型占位值不具有业务含义，不可被过滤、投影、主键、索引预检或文件转换读成真实值。空值判断使用统一的列视图，不在不同消费者重复推断。
- 输入同样保留 validity 与字段缺失状态；协议展开紧凑布局后由引擎进行默认值与类型校验。不会在 core 引入 protobuf 或 Python 对象。
- 默认值保留与既有 Python/JSON 值规则一致的类型信息，不用字符串或 f64 统一承载整数、布尔和 JSON。不得因“更早发现错误”改变参照在建表/写入阶段的错误优先级。
- ARRAY 在 B 中增加有类型的 offsets/元素列，并携带独立的父/元素有效性；不把数组降级为 JSON 文本，不提前加入嵌套数组框架。

这一选择比每行 Option<T> 更能保留现有连续列的布局和快路径，但涉及多个消费者，必须以编译约束和反证测试防止掩码遗漏。全部有效列的过滤与距离计算继续走现有连续访问，不为支持 nullable 在查询时引入逐行全库有效性扫描。

### 快照、WAL 与合并

空值位图与值列一起冻结并由不可变快照持有；可变构建阶段只修改私有缓冲。沿用 0007 的写入许可、锁序、快照与文件钉住、WAL-first 和提交后清理协议，不增加维护线程或新锁，也不改 schema 不可变合同。

Flush/compaction 的行选择同时选择值与 validity，删除有效位与列空值位不能混用。恢复检查 Arrow null 并恢复内存列；JSON 列仍保持原有文本与 pyjson 规则，$meta 不随本切片重写。损坏列、解码失败和 I/O 失败沿现有失败协议上报，不能用空值填充来恢复。

## 真值与验收矩阵

以下每行均要求 A/B 分别保留独立验收记录；调查阶段的通过不记为新产品能力通过。

| 证据 | 必须覆盖 |
| --- | --- |
| schema / 输入组件 | 类型×nullable×默认值有无/零值×缺失/None/合法/非法输入；错误阶段与原文；默认对象不跨记录污染 |
| raw gRPC / SDK | 紧凑/完整/全 null/无 mask/缺列/错误长度；实际发送请求；SDK 2.6 与 3.0；普通与 partial Upsert |
| 过滤差分 | 三条整树路线与上述操作符，INVERTED 定义下扫描，Query/count/Search/Delete，失败删除的 WAL/序号/行集不变 |
| 生命周期差分 | active、frozen、已 Flush、重开、覆盖/删除后、实际 compaction 后；Get 与 Search 输出的值和掩码 |
| 双向存储互通 | Arrow 15/25；双方写读和交替写；WAL 与 segment；snapshot 钉住、删除、合并后空值保持 |
| 故障注入 | 带 null 的 WAL 追加前后失败/残批；Flush 提交前后及删 WAL 前崩溃；compaction 提交前后退出；重开后值与 validity、序号、删除证据与健康 collection |
| 旧版本回退 | 已发布 rc1/rc2/rc3 wheel；新 Rust 写出的 WAL/segment/合并目录；旧能力正常，新属性明确拒绝且受保护文件名/内容不变 |
| 工具与构建 | core/grpc/py 相关测试、默认与 FI、fmt/Clippy；不增加正式 wheel 测试入口；最终候选按 0013 重建验收 |
| 性能回归 | 同机 rc3 与候选独立 wheel/venv，S1 过滤、批量检索、恢复 RSS；旧快路径与全有效 nullable；另报告不同 null 密度的限定场景 |
| 含 JSON 字段的写入（2026-10-16 修订） | Insert 普通非空 JSON 列；预序列化 raw gRPC 与 pymilvus 端到端分别计时，三轮轮换 rc3/变更前/变更后/3.x，核对全部写入结果；恢复到 rc3 的回归基线，不扩大到既有跨实现写入差距 |

故障注入必须触达既有受控边界，不能只 kill 一个空闲进程。进程崩溃结果不推导断电保证。至少用反证拦住：丢弃输入 validity、把 null 当零/空串、not in 错误屏蔽 null、仅回退一个叶子、合并遗漏掩码、输出掩码长度错位、失败 Delete 仍分配序号。变异使用仓库外副本与独立 CARGO_TARGET_DIR。

性能复用三个专项的规模、数据布局与轮换口径，对不含 null 的 rc3 场景比较前后 p50 / RSS 与轮间范围；出现稳定退化先定位，证据不足时补采样。新增 null 场景没有 rc3 功能基线，不宣称相对加速。若作性能提升声明，另按规则 7 补同机 2.5.1 / 3.x 对照。

含 JSON 写入是负责人复审后单独提出并授权实施的合同修订，原 S1/批量检索/恢复矩阵不足以覆盖它；不将本次发现追溯改判为旧合同的阻断。复现方法见 [JSON 写入专项](../../benchmarks/json-write/README.md)，修复与限定证据另行记录。空值探测仅识别首尾 JSON 空白包围的 `null`，实际语法校验、规范化和被覆盖/掩码值的校验保持原边界。

## 版本与执行顺序

负责人于 2026-10-16 确认固定 0.1.0 的功能范围为 rc3 已交付能力，新 schema 功能进入 0.2.0 开发线；0.1.0 是否达到正式发布条件，由试用稳定性与既定门槛独立决定，不为版本号收口自动转正式版。本次不切换版本、不发布。

不默认将 A/B 功能加入 rc4；如后续改变发布线，显式修订本决策与 0013 的范围和 RC 收口条件。

1. 统计工具整理为独立逻辑变更，完成原始基线、schema 使用分类和受控干预。
2. 评审本决策与真值矩阵，冻结列表示、标量/ARRAY 边界、向量延后与目标发布线。
3. 实施 A，完成差分、互通、FI、两条 SDK、回退和性能记录；评审收口。
4. 实施 B，完成同等级证据；nullable 向量仍为独立切片。
5. 重跑三组固定统计与全部相关门槛，再按目标版本走 0013 的候选/发布流程。版本、tag、发布仍分别授权。

工具不会默认联网、进入默认测试或将外部测试源码复制入仓；文档与提交仅使用仓库相对路径、固定公开 commit 与证据哈希。原始数据、日志和目录留在仓库外。
