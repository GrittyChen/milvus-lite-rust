# 0023 B：多数据库实施验收

- 状态：B 已按[实施设计](../decisions/0023-aliases-rename-truncate-databases.md#b-实施设计评审稿)分五步实施，设计与第 1–4 步、第 5 步的补测批各自独立评审通过；本记录随第 5 步的文档与统计批提交，**未推送、未发布**。
- 日期：2026-11-02
- 合同与设计：[0023](../decisions/0023-aliases-rename-truncate-databases.md)，继承 0002、0006 第 14–18 项、0007、0017。
- 仓库基线：A 收口后的 `a8be970`（含 N2 的 `369c1fa`）。
- 参照：milvus-lite v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`，保持只读；SDK 固定 2.6.17 / 3.0.2。
- 本地提交序列（均未推送）：设计 `075a129`、第 1 步 `71d8661`、第 2 步 `e0809a4`、第 3 步 `9524835`、第 4 步 `f136ded`、第 5 步补测批 `bb828d0`；本批在其上追加复制的参照用例、并发补测、文档同步、计数与本记录。
- 开发产物沿用 `0.3.0rc3` 版本字符串，只以原生库与 wheel 摘要识别；不是新的发行候选，不表示覆盖已有 Release。

## 结论

数据库的五个 RPC（CreateDatabase、DropDatabase、ListDatabases、DescribeDatabase、AlterDatabase）、所有 RPC 的 `db_name` 与 `dbname` 元数据路由，以及别名、改名、清空与集合属性按数据库隔离均已实施，并通过下列验收：3.x 与本实现在同一布局上逐字节比较属性文件与目录形状，库的创建、删除、属性替换以及非默认库内的改名、清空、别名写入在各提交点注入进程退出与 I/O 错误，重开后只有「操作前」或「操作后」；两个实现交替创建、修改、删除数据库后互相打开。固定统计中原阻断全部解除，剩余 7 条参照用例是 `timezone` 的预期 UNIMPLEMENTED（取舍 1），无一项退回（见「统计」）。

仍未覆盖的事项见「残余风险与未覆盖」。

## 实施范围

| 步骤 | 内容 | 提交 |
| --- | --- | --- |
| 设计 | 调查补充（3.x 的目录形状、属性与删除边界）、对合同的细化、模块与锁、生命周期协议、登记差异、测试矩阵 | `075a129` |
| 1 | 数据库注册表（叶子锁，打开时枚举 `databases/`，含非默认库的 N2 清扫与改名收尾）、`Db.path` 单源化、`require_database` 查表；已有的非默认库开始按 3.x 使用 | `71d8661` |
| 2 | `database_properties` 模块；ListDatabases、DescribeDatabase、AlterDatabase | `e0809a4` |
| 3 | CreateDatabase（staging、精确名字核对、发布与封锁） | `9524835` |
| 4 | DropDatabase（整体改名进 staging 为提交点） | `f136ded` |
| 5 | 补测（非默认库内的管理意图进程退出、并发、3.x 属性字节样本）、staging 常量单源、复制的参照用例、文档同步、统计与本记录 | `bb828d0` 及本批 |

相对设计文本的出入与登记的差异见设计的[B 实施登记与验收口径](../decisions/0023-aliases-rename-truncate-databases.md#b-实施登记与验收口径)，并同步 0002、0006、0007、README 与 AGENTS。

## 构建身份

| 产物 | SHA256 |
| --- | --- |
| macOS arm64 开发 wheel（统计与 2.6 线复验所用） | `70d2dfdd9ed1030ff8a63f18d60b03ec33e9d405cbefe4bb082007e7b6f68178` |

wheel 由 `bb828d0` 之上的工作区构建；与 `bb828d0` 相比本批没有产品源码改动（只增加测试与文档），运行时二进制所含代码相同。该 wheel 是未发布的开发产物，属本地留存。

## 测试

| 套件 | 结果 |
| --- | --- |
| `cargo test --workspace` | 438 通过（设计前 411，B 新增 27，23 个 `databases.rs` 用例 + 5 个 `database_properties` 单元用例 − 1 个移除的 `db_persistence` 用例） |
| `cargo test -p milvus-lite-core --features fault-injection` | 585 通过（含子进程用例重复打印的汇总行，口径同前） |
| fmt、clippy（默认与 fault-injection 两套构建，`-D warnings`） | 干净 |
| Windows 交叉检查（`x86_64-pc-windows-msvc`，默认与 fault-injection，`--all-targets --locked`） | 干净 |
| Python 端到端 `tests/python`（pymilvus 3.0.2） | 516 通过（含复制自参照实现的数据库用例 17 个函数 29 条，加两个按数据库隔离的用例） |
| 同一套件，pymilvus 2.6.17（CPython 3.10，同一 wheel，仓库外运行） | 516 通过 |
| 差分子集（rename、collection_properties、aliases、index_definitions、reopen、scenarios、drop_displaced、partitions、truncate、databases） | pymilvus 3.0.2：83 通过；pymilvus 2.6.17：83 通过。完整差分收集数 438（会话组 118）由分片计划核对，完整运行由 CI 的 Compare 复核 |
| 存储互通 `tests/storage_interop`（Arrow 15 / 25 两个参照环境） | 250 通过 |
| `fixtures/generate.py --verify`（真实 3.2.1） | 通过，含 `database_properties_3x.json` |
| `tests/release`、`tests/ci`、`tests/sharding`、`tests/packaging` 自检 | 871 通过 |

预期计数随新增用例同步：差分 431→438（会话组 118 不变）、互通 248→250、Python 485→516，涉及 `compare.yml`、`ci.yml`、`scorecard.py`、候选自检夹具、分片 README、打包 README、AGENTS 与 0020（仅差分／互通计数）。

## 差分与预期差异

[test_databases.py](../../tests/differential/test_databases.py) 的 7 条用 raw gRPC 与 MilvusClient 对 3.x 逐项对照：

- 读：ListDatabases（含隐藏目录与符号链接目录、普通文件不是数据库）、DescribeDatabase（`default`、空名、元数据路由、不存在、非法名与 256 字节名）、`using_database` 与库内集合 / 别名的隔离。
- 属性：Alter 的合并（空 key 丢弃、同名保留首位取末值、先删后设移到末尾、无变化不写、默认库首次有效修改才建文件）、进程内顺序与重开后键序、未排序文件读回按文件顺序，每次写入后两边的 `database_properties.json` 逐字节相同。
- 创建与删除：判定顺序与文案（`default`、空名、已存在、非空、不存在、非法名）、元数据被忽略、成功路径的目录形状与 staging 逐一对照、带残留目录与损坏元数据的库照常删除、重开后一致。
- 预期差异单独断言：属性文件无法解析与非字符串值、写 `timezone`（UNIMPLEMENTED 且零写入，判定在存在性之后）、被普通文件占用的 Create、只差大小写的数据库名（大小写不敏感文件系统上 3.x 命中，本实现报不存在）、符号链接形式的空库。

## 故障与崩溃

[databases.rs](../../crates/core/tests/databases.rs) 的子进程与故障注入用例（均需 `fault-injection`）：

- CreateDatabase：改名之前（Error、Panic）状态不变且不封锁；改名之后的三个同步点（Error、Panic）库已发布并封锁写入，重开后完整可用；名字核对失败回滚；核对失败且改回也失败时封锁、重开后库存在；改名前后进程退出，重开只有「未创建」或「已创建且完整」，staging 残留被清空。
- DropDatabase：改名之前失败状态不变；提交点之后的三个同步点（Error、Panic）库已不存在并封锁写入，重开后 staging 清空；改名前后进程退出，重开只有「未删除」或「已删除」。
- AlterDatabase：替换之前失败时缓存与文件都不变，替换之后目录同步失败时缓存随磁盘更新并封锁写入；替换前后进程退出，重开只见替换之前或之后的完整文件，无临时文件残留。
- 非默认库内的管理意图：改名（目录改名前、`collections/` 同步后、别名改指后）、清空（意图写入后、读视图重建后、删除旧文件时）、别名写入（替换前后）共 8 个退出点，重开后库根的改名意图在打开时收尾、集合目录的清空意图在首次访问时收尾；变异（只处理默认库）使用例失败。
- 启动清扫：非默认库根的 `aliases.json`、`database_properties.json` 与改名意图的 `*.tmp.<pid>.<n>` 残留在打开时清除，3.x 的固定名与近似名保留；默认库根同样（`data_dir` 的用例）。
- 并发：[设计测试矩阵「并发」行的覆盖口径](../decisions/0023-aliases-rename-truncate-databases.md#b-实施登记与验收口径)逐项列出用例、结构性保证与已知行为。

## 互通

`test_cross_implementation.py` 的 `test_databases_created_by_one_implementation_open_and_change_in_the_other` 在两个先手方向各跑一遍：一方创建带属性的库并在默认库与 `d1` 内建同名集合与别名，另一方读取库列表与属性、经别名读写、修改与删除属性、再创建一个库并删除对方创建的空库，最后核对落盘字节、库列表与 staging 为空。库生命周期的进程退出点由上面的子进程用例固定；3.x 打开这些退出点留下的目录依赖同一个目录布局，由交替用例与差分的逐字节比较提供证据，没有单独把子进程用例的退出点交给 3.x 打开（不同于 A 的 `test_management_crashes.py`）。

## 统计

被测为本批开发 wheel；方法同 [0014 收口](0014-compatibility-closure.md)：参照端沿用同一批输入与结果（同一参照 `43d1257`），被测端按同一环境依赖重建、只换本包；760 条参照用例清单、90 条上游子集（上游 Milvus `dcda428`）与 49 项探针固定。前一基线为 A 收口时的同一统计。原始输出与逐项对照由负责人本地留存。

| 来源 | A 收口 | 本批 | 3.x | 新增 / 退回 |
| --- | --- | --- | --- | --- |
| 参照用例 760 | 621 | **652** | 760 | 新增 31，退回 0 |
| 上游子集 90 | 89 | **89** | 90 | 新增 0，退回 0 |
| 探针 49 | 37 | **38** | 43 | 新增 1（`databases`），退回 0 |

逐项核对合同预期：

- 参照用例：原先首个阻断为数据库 RPC 的 38 条中 31 条通过（CreateDatabase 12、ListDatabases 14、DescribeDatabase 5，AlterDatabase 0；CreateDatabase 另有 6 条、AlterDatabase 1 条转为 `database timezone property` 的 UNIMPLEMENTED），其余 7 条是取舍 1 预期仍不通过的用例：`test_create_database_with_timezone_property`、`test_alter_database_timezone_property`、`test_drop_database_timezone_property`、`test_alter_missing_database_properties_fails_without_changes`、`test_drop_missing_database_properties_fails_without_changes`、`test_create_duplicate_database_fails_and_database_remains`，以及 `test_milvus_client_timestamptz_database_timezone_default`（TIMESTAMPTZ 切片）。与合同「范围内 51 条中 7 条预期仍不通过」一致。
- 上游子集：A 收口后剩 1 条（多向量字段），本批不变。
- 探针：`databases` 通过；剩 `group_by_search`、`multi_vector_hybrid`、`sparse_vector`、`bm25_full_text`、`text_match`。
- 统计只计首个阻断，数字是诊断样本，不是收益承诺。

## 残余风险与未覆盖

- **平台**：全部在 macOS arm64 本机执行。Linux 两个架构、Intel Mac 与 CI 环境未跑；本批改了工作流的预期计数（差分 438、互通 250、Python 516），须在下次 Compare 与 CI 运行时确认。被普通文件占用时 3.x 的 `ENOTDIR` 来自 `rename(2)` 的 POSIX 语义，未在 Linux 上实测。
- **staging 与目标目录须在同一文件系统**：本批在 macOS 上用第二个 APFS 卷实测（两次测量的原始输出本地留存），见[登记](../decisions/0023-aliases-rename-truncate-databases.md#b-实施登记与验收口径)：3.x 的 CreateDatabase 同样失败，但库内集合的 create / 改名 / drop 与 DropDatabase 原地成功；本实现的 CreateDatabase、库内集合 create、已存在集合的 drop 与清空、DropDatabase 以存储错误失败、状态不变，已存在集合的改名成功（改名不经 staging）。3.x 的清空未测。不支持把 `databases/` 或其中的库放在其他文件系统。Linux 未实测。
- **迟到读**：见上述登记，是已知行为，不是缺陷。
- **运行期外部变更**：运行期由外部手工创建或删除的 `databases/<db>` 重开后才可见；数据目录由 LOCK 独占，这是 0002 约束 10 的前提。
- **断电类故障**：进程退出不丢页缓存，因此「目录同步之前的断电回退」只由目录同步的错误注入与顺序钩子覆盖，没有真实断电实测，同 [0007](../decisions/0007-m2-storage-engine.md) 的既有边界。
- **库生命周期与合并**：没有单独的合并并发用例，依据见设计的并发覆盖口径（共享 / 独占许可的结构性保证，Drop 要求库内无集合）。
- **跨数据库改名与 `timezone`**：保持 UNIMPLEMENTED（取舍 1、4），不属于本切片。
- **README 的表述**：README 对应已发布的 `0.3.0rc3`，本批只在「集合管理与多数据库」一条中如实说明尚未随预发布交付，并写明已知差异与降级后的行为；发行说明与 Release 附件不变。

## 证据类别

仓库内可重跑：上述测试命令、差分与互通用例、统计的方法与清单（0014 收口记录）、`scorecard.py` 与工作流中的预期计数。本地留存：开发 wheel、统计的原始输出与环境、探针脚本与 3.x 原始输出、跨文件系统的实测脚本与输出、各步骤评审时冻结的 diff。以上结果均未推送，第三方在推送之前无法核对本记录所指的提交。
