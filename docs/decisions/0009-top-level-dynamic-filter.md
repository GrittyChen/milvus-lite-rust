# 0009 顶层动态字段过滤

- 状态：顶层范围已由负责人确认，实现经范围内复审通过；首次发布授权独立确认
- 日期：2026-10-09
- 参照：v3.2.1 / `43d1257` 的 search/filter 与 search/assembler.py
- 前置：[0008 首发范围](0008-first-release-scope.md)、[0007 读快照](0007-m2-storage-engine.md#读)、[0006 有效位边界](0006-m2-scope.md#与-3x-的差异)

## 功能与拒绝边界

- 沿用完整解析器与 S1 六种标量比较、and、括号、顶层主键 in；未知字段在 enable_dynamic_field=True 时绑定到动态键。
- 新增动态键与字面量的六种比较（含字面量在左）以及裸动态字段 `in`；可以与 S1 比较、其他动态比较/in 用 and 组合。
- `$meta["key"]` 支持顶层比较，也可访问不适合裸标识符的键。参照解析器不允许它直接放在 in 左侧，故该写法仍返回相同语法诊断；使用 `key in [...]`。
- 缺失和 JSON null 按参照的三值逻辑处理，不能把 `!=` 当成匹配缺失值。已声明 schema 字段优先；禁用动态字段时未知裸名字/显式 meta 访问报对应诊断，保留保留字段错误。
- 不接入嵌套路径、or/not、not in、LIKE、is null/exists、JSON/数组函数、时间或全文求值。非主键 schema 字段 in、主键 in 与 and 的组合仍属范围外；语法完整检查后显式 UNIMPLEMENTED，范围外语义不补做。
- Search、Query（含分页）、count、Delete 共享绑定与求值，主键顶层 in 的顺序、重复和转换快路径不改。

## 类型与求值协议

参照的 hybrid 先将所有引用的动态键按名字排序并推断整批类型，再尝试 Arrow 求值。异构值、不支持的类型配对或安全转换失败使整条表达式转入 Python 路径；真实整数溢出不在其 catch 清单内，必须向上传递。因此不能统一使用逐行 Python 比较，也不能给每个 Rust 小块独立选路。

本实现复用已有 SourceId，把一个 segment 或 memtable 的全部活行组成同一次求值输入。已提交源按物理行号、内存代按既有位次排列；Rust 小块合并和 Parquet 批切分不改变类型推断或错误顺序。仅物化表达式引用的标量列与动态标量列；向量、JSON 列与整个 meta 对象不深拷贝，meta 仍借用不可变快照。临时引用、排序与列缓冲区按一个数据源构建，不缓存到 collection。

同质标量使用现有列式核和位图；成员列表沿用 Arrow 的类型转换与负零按位查找。混合批使用 Python 的精确整数/浮点比较、布尔数值等价、跨类型比较及 Kleene and；Python and 遇到 False 会短路，但 Unknown 仍继续。数组/对象的 in 查找按参照报告 unhashable type，失败 Delete 不得改变序号、WAL 或行集。整数不能先无条件转成 f64，宽整数与 2^53/2^63 的比较必须保持精度。

纯 S1 路径继续逐个验证原有效位上的所有 and 操作数，不因累积掩码为空或 limit 已满足而屏蔽错误。含动态字段的表达式选路与整条 and 行为按上述 hybrid 合同，不把纯 S1 的转换报错规则强加给 Python 路径。整个求值阶段不做 I/O；只允许与参照对应的类型/安全转换失败改走 Python，绑定损坏、整数溢出和其他真实错误不得吞掉。

沿用 0006 的只对活行求值边界：失效旧行不参与类型推断、转换或成员查找，避免旧版本控制当前读取。3.x segment 中失效物理行仍可能影响 backend/报错，该差异扩展到动态过滤并在 0006 登记。旧快照仍钉住原列与有效位；分组和筛选在状态锁外，不改变锁、计算池、WAL、manifest、Parquet、提交点或关闭协议。

## 验证要求与当前证据

- [组件真值生成器](../../tests/differential/generate_dynamic_filter_fixtures.py)直接调用真实参照的 parse/compile/evaluate，记录同质/异构、null/缺失、布尔/数值/字符串转换、宽整数、负零、列表/对象、合成列冲突、错误优先级与 meta 访问的掩码或错误原文；--verify 必须逐条一致。
- [组件单测](../../crates/core/src/filter/dynamic.rs)逐条核对语料及原有效位边界；[生命周期组合](../../crates/core/tests/dynamic_filter.rs)覆盖一个源的多个块、分割 Parquet 重开、不同源独立选路、旧快照与失败 Delete 的 WAL/manifest/行集不变。
- [真实 RPC 差分](../../tests/differential/test_filter_dynamic.py)验证 Search/Query/count/Delete、分页、发布、重开、partial upsert、NULL、混合类型、错误以及范围外拒绝；适配器新增上游原样 TestDynamicField，状态见 [INVENTORY](../../tests/python/adapter/INVENTORY.md)。
- 首发验收需两条 SDK 的当前构建端到端/差分、格式/Clippy、默认与故障注入 Rust 全套，并核验既有完整语法语料。存储格式未改，但通过互通读取同一动态值目录验证结果；不得沿用旧产物冒充新构建。

本切片不声明动态过滤性能提升，也不视为全部过滤或首次发布已经完成。类型绑定、临时列、source 级引用排序的开销尚未量化；新增路径没有过滤索引、表达式缓存或新后台任务。

### 初审前构建的实际验证（2026-10-09）

开发扩展与独立 wheel 的 native SHA256 均为 `c75029c45dc785bddb1947c703541c391165fca3f63029484f439ac6c3ef7c40`。两条 SDK 顺序执行，重新核对该产物；源码、语料与参照目录均未在测试中回退或注入变异。

| 验证 | 结果 |
| --- | --- |
| workspace 默认全套 / core fault-injection 全套 | 全绿；core lib 141 / 162，新生命周期组合 3 条通过 |
| 默认 workspace / core FI Clippy `-D warnings`、fmt、Git whitespace | 通过 |
| 顶层动态组件真值 | Arrow 25 与 Arrow 15 各 3282 条一致 |
| 完整语法真值 | 7791 条通过 |
| pymilvus 3.0.2 | Python 233 / 差分 106 / 存储互通 64 passed |
| pymilvus 2.6.17（独立 wheel，仓库外执行） | Python 233 / 差分 106 / 存储互通 64 passed |
| benchmark/platform 脚本自检 | 41 passed，不视为新平台或性能测量 |
| adapter 函数体 AST | 166 个用例中 162 个与上游一致，其余 4 个仍是已登记的 HNSW→FLAT；新增副本原样一致 |

互通新增四条：两方分别写出的已 Flush 与保留完整 WAL 目录，由另一方经真实 RPC 读取并求值。写入形态由 WAL 文件集合先断言，再核对动态比较、in、null/count 和过滤检索。本轮没有复测各历史性能表、Linux/Windows、真实断电或大型并发负载；其旧证据与限制仍分别保留。

### 评审修复：跨类型大小比较的 Unknown

初审前构建把“类型不可比较”和数值 NaN 都表示为没有排序结果，继而把大小比较统一转为 False。对 `x < 'c' and y in [1]`，x 为整数、y 为列表时，错误的 False 屏蔽了后面的 unhashable 错误；同源另一行仍可能匹配，导致 Delete 成功改变行集。该问题违反本决策的 Kleene 合同。

修复仅在 Python 比较的类型配对分支区分两者：跨类型 ==/!= 仍为 False/True，大小比较为 Unknown；数值 NaN 的 == 与大小比较仍为 False，!= 仍为 True。没有改 Arrow 选路、成员查找、纯 S1 或 collection 的快照与提交路径。

原组件语料只比较最终单条件掩码，Unknown 和 False 都被折叠成不匹配，没有覆盖“大小比较不可比较 + 后续操作数报错”的组合。新增 288 条双键、六操作符、双方向及类型/NULL组合，原 3282 条保留；旧实现实际产生 104 条分歧，修复后的 3570 条分别与 Arrow 15/25 真值一致。独立单测同时固定数值 NaN 与跨类型顺序比较，防止将所有无序情况一并改为 Unknown。

真实 RPC 回归在 MemTable/已提交两种状态下核对 Query、Search、count、Delete 的错误与失败后完整行集；生命周期反证固定 WAL 字节、manifest 缺失状态、没有新增 delta WAL，且后续 insert+flush 的 current_seq 为 3、重开三行仍在。旧产物的实测 Query 返回 id 2、Delete 删除该行；修复产物与参照均报 unhashable type，并保留两行。上述旧失败记录保留，不以新通过记录覆盖。

修复后重新构建，开发扩展与 wheel native SHA256 一致为 `9f45ef1c669672cac5cff8de80cda6b8103dc9a206df78ac5fb2baeb2ee7dc2f`。本轮实际结果：workspace 全套（core lib 142）、core FI 全套（lib 163、动态生命周期 4 条）、两套 Clippy `-D warnings`、fmt/whitespace 均通过；两个 Arrow 版本各 3570 条动态真值、完整语法 7791 条通过。pymilvus 3.0.2 / 2.6.17 顺序运行，分别各 Python 233 / 差分 116 / 互通 64 passed，无跳过或断言放宽。历史性能、Linux/Windows、真实断电与动态过滤性能本轮仍未重测。

扫描包括 compare 的全部普通比较/in 调用点、int_float_cmp 与其他返回 None 的类型选路分支、evaluate_python 的 False/Unknown 分支，以及共享过滤出口到 query_for_delete→mutate_handle 的调用链。数值 NaN、NULL、不可比较类型、相等/不等与双方向大小比较分别核对；字段绑定的两处内部不变量错误映射及不可达 parser 分支没有可达新证据，保持本轮范围外的已接受残余项。
