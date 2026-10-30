# 0023 A：集合管理实施验收

- 状态：A 已按[实施设计](../decisions/0023-aliases-rename-truncate-databases.md#a-实施设计终稿)分七步实施，第 1–6 步与第 7 步的补测批各自独立评审通过；本记录随第 7 步的文档与收口批提交，**未推送、未发布**。B（多数据库）未开始。
- 日期：2026-10-30
- 合同与设计：[0023](../decisions/0023-aliases-rename-truncate-databases.md)，继承 0002、0006 第 14–18 项、0007、0010、0011、0014、0017。
- 仓库基线：已发布 `0.3.0rc3` 之后的 `1026c0854886c70e303b725b4fdb00b98e95ff3a`。
- 参照：milvus-lite v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`，保持只读；SDK 固定 2.6.17 / 3.0.2。
- 本地提交序列（均未推送）：合同 `977fda5`、实施设计 `5047430`、第 1 步 `5c2732b`、第 2 步 `8dbeb58`、第 3 步 `36a05cf`、第 4 步 `486e8b6`、第 5 步 `a154da0`、第 6 步 `14a165a`、第 7 步补测批 `fcb263e`；本批在其上追加测试补强、CI 计数与预构建清单、文档同步与本记录。
- 开发产物沿用 `0.3.0rc3` 版本字符串，只以原生库与 wheel 摘要识别；不是新的发行候选，不表示覆盖已有 Release。

## 结论

范围内的 RPC 与持久化协议均已实施并通过下列验收：别名五个 RPC 与所有集合 RPC 的别名解析、RenameCollection、TruncateCollection、AlterCollection 的集合属性、共享 JSON 的 3.x 键排序写出、对已存在非默认数据库的 UNIMPLEMENTED。两种私有意图（改名、清空）在全部提交点注入进程退出，并在各出口的代表点注入 I/O 错误与 panic（panic 与 I/O 错误共用同一捕获出口），重开后只有「操作前」或「操作后」之一；3.x 在中途目录上读写后，本实现重开仍能收尾。固定统计中原阻断全部解除，无一项退回（见「统计」）。

仍未覆盖的事项见「残余风险与未覆盖」。数据库 RPC 与 `db_name` 非默认库属于 B。

## 实施范围

| 步骤 | 内容 | 提交 |
| --- | --- | --- |
| 1 | `pyjson::dump_file`，schema、alias、manifest 全部按 3.x 的 `json.dump` 参数排序写出；放宽 `schema.json` 名字与目录不同；`require_database` 对已有非默认库返回 UNIMPLEMENTED | `5c2732b` |
| 2 | `CollectionKey` 与 `DatabaseState` 重构，集合表、名字锁、失败缓存按（数据库，名字）索引 | `8dbeb58` |
| 3 | 别名表缓存与两段写、各入口许可后解析、五个别名 RPC、独占许可下的别名写入 | `36a05cf` |
| 4 | AlterCollection；`fs_commit::replace_file`；`timezone` 延后 | `486e8b6` |
| 5 | RenameCollection、改名意图、启动与重开收尾、`lock_pair` | `a154da0` |
| 6 | TruncateCollection、清空意图、重建读视图、清理重试的共享到独占重新入闸、重开收尾 | `14a165a` |
| 7 | 补测（独占变更与合并、进程退出、启动收尾注入、半截清理、继承旧 WAL、3.x 互通退出点）、小加固、CI 预期计数与预构建清单、文档同步、统计与本记录 | `fcb263e` 及本批 |

偏离设计之处全部登记在设计的[对合同方向的细化](../decisions/0023-aliases-rename-truncate-databases.md#对合同方向的细化)与[实施登记的差异与更严格的出口](../decisions/0023-aliases-rename-truncate-databases.md#实施登记的差异与更严格的出口)，并同步 0002、0006、0007、0011、0014、README 与 AGENTS。

## 构建身份

| 产物 | SHA256 |
| --- | --- |
| macOS arm64 开发 wheel（统计、互通补充用例、两条 SDK 线与 1e+999 核对所用） | `07c9c9f57fbcaca32abb65b0d3b19117aa0015775fdf9b4e927d7b5bead2f1ce` |

wheel 由 `fcb263e` 之上的工作区构建；与 `fcb263e` 相比产品源码只有 `fs_commit` 单元测试模块的一处注释，运行时二进制所含代码相同。该 wheel 是未发布的开发产物，属本地留存。

## 测试

在 macOS arm64 本机、CPython 3.12（2.6 线用 3.10）、参照 `43d1257` 上执行，扩展均在最后一次产品源码修改之后重建。

| 套件 | 命令（仓库根目录） | 结果 |
| --- | --- | --- |
| 格式与 lint | `cargo fmt --all --check`；`cargo clippy --workspace --all-targets -- -D warnings`；加 `--features milvus-lite-core/fault-injection` 再执行一次 | 均干净 |
| Rust | `cargo test --workspace` | 406 通过，0 失败，0 ignored |
| Rust 故障注入 | `cargo test -p milvus-lite-core --features fault-injection` | 537 通过，0 失败，0 ignored（含子进程用例打印的 `test result` 行，前后同一口径） |
| 端到端（SDK 3.0.2） | `.venv/bin/python -m pytest tests/python` | 485 通过 |
| 端到端（SDK 2.6.17） | 在仓库外目录用 2.6.17 的 venv 执行同一目录，见 [AGENTS.md](../../AGENTS.md) 常用命令 | 485 通过 |
| 差分子集（SDK 3.0.2） | 带 `MILVUS_LITE_3X_PYTHON` 运行 `tests/differential` 的 `test_truncate`、`test_rename`、`test_collection_properties`、`test_aliases`、`test_index_definitions`、`test_reopen`、`test_scenarios`、`test_drop_displaced`、`test_partitions` | 76 通过 |
| 差分子集（SDK 2.6.17） | 同上，在仓库外目录 | 76 通过 |
| 存储互通 | 带三个解释器环境变量运行 `tests/storage_interop`（环境见[互通 README](../../tests/storage_interop/README.md)） | 248 通过 |
| 发行工具与平台脚本自检 | `.venv/bin/python -m pytest tests/packaging tests/release tests/ci tests/sharding tests/platform` | 886 通过 |
| 工作流静态检查 | `actionlint` 检查 `.github/workflows/*.yml` | 无告警 |

完整差分与互通套件在 CI 中的预期计数随本批更新：[scorecard.py](../../tests/release/scorecard.py) 的 Python 套件 485、差分 431（会话组 118）、互通 248，与 [compare.yml](../../.github/workflows/compare.yml) 的差分、互通 `--expect` 及 [ci.yml](../../.github/workflows/ci.yml) 快速层两条 SDK 线的 485 一致（2.6.17 线的收集数同为 485），互通预构建清单加入两个故障注入测试程序（[0019](../decisions/0019-test-sharding.md)）。计数取自分片工具的收集结果，工作流本身在本批尚未实际运行。

## 差分与预期差异

「参照实测」每一行在两条 SDK 线上对 3.x 做 raw gRPC 与 MilvusClient 对照，包括 `code` / `error_code`、文案、判定顺序、load 状态、属性顺序（进程内与重开后）、磁盘字节：

- `schema.json`、`aliases.json` 与清空后的 manifest 除登记的 `version` 外逐字节相同。
- 预期差异单独断言：跨数据库改名、改名新名为悬空别名或被占用的路径、范围外 schema 上的清空与改属性、`timezone`、非法语法名字、`aliases.json` 解析边界，均为 UNIMPLEMENTED 或 1/1、1/0 零写入，逐项见设计的[实施登记](../decisions/0023-aliases-rename-truncate-databases.md#实施登记的差异与更严格的出口)。

## 故障与崩溃

进程退出覆盖全部提交点（子进程，退出码 88；manifest 钩子 86）；I/O 错误与 panic 在各出口的代表点注入（panic 与 I/O 错误共用同一捕获出口，清空的 staging 清理只有退出点）：

- 别名与 AlterCollection：替换之前、之后（目录同步之前）。
- 改名：意图写入前后、目录改名之前、`collections/` 同步、`schema.json` 替换前后、别名替换前后、意图删除前后，共 10 个退出点；启动收尾的 7 个故障点；改名暂停点上的请求等待；暂停通道失效按「改名未完成」收尾。
- 清空：WAL 迁移前后、意图写入前后、manifest 准备与替换前后、读视图重建、`schema.json`、snapshot、旧文件、`.prev`、意图删除、staging 清理，共 16 个退出点；五种 `wal/` 状态；半截清理（元数据已删、副本与部分旧文件残留、同名不同 id 的 snapshot 保留）；清理未完成时 drop 与改名；继承已提交 WAL 后再清空；清理完成后合并恢复；九类写入入口的独占重试。
- 并发：别名、AlterCollection、改名、清空四类独占变更与进行中合并的先后；清空与改名期间的请求等待。

## 互通

3.x 与本实现交替执行别名、属性、改名、清空，互相打开并比较可见集合、别名、属性（含顺序）、数据、load 状态与 auto_id；另有三组退出点用例：

- 清空在提交点之前退出的 6 个点，由 3.x 打开并继续写入，本实现随后只删除意图。
- 清空已提交、清理未完成的 4 个点，3.x 只写 WAL 就被强杀，本实现补做清理并回放，旧数据不回来。
- 改名的 10 个退出点，3.x 按目录名打开，本实现随后收尾到改名前或改名后。

另外覆盖：没有 manifest 的 partition key 集合清空后两边的分区与逐 id 路由一致；改名前后 auto_id 在两边延续。

### 1e+999 存量定义的核对

[0006 差异表](../decisions/0006-m2-scope.md#与-3x-的差异)登记的溢出数字在重写后的保留方式，用开发 wheel 与真实 3.x 核对：先让 0.3.0rc3 的写法（索引参数 `"M": 1e+999`）出现在 manifest 中，本实现再 insert 并 flush 重写 manifest，结果是该数字仍写为 `1e+999`、不出现 `Infinity`，3.x 随后打开成功并读到 `M` 为 `inf`、行数不变。探针脚本由负责人本地留存。

## 统计

被测为本批开发 wheel；方法同 [0014 收口](0014-compatibility-closure.md)：参照端沿用同一批输入与结果（同一参照 `43d1257`），被测端按同一环境依赖重建、只换本包；760 条参照用例清单、90 条上游子集（上游 Milvus `dcda428`）与 49 项探针固定。前一基线为 0.3.0rc3 的同一统计。原始输出与逐项对照由负责人本地留存。

| 来源 | 0.3.0rc3 | 本批 | 3.x | 新增 / 退回 |
| --- | --- | --- | --- | --- |
| 参照用例 760 | 608 | **621** | 760 | 新增 13，退回 0 |
| 上游子集 90 | 82 | **89** | 90 | 新增 7，退回 0 |
| 探针 49 | 34 | **37** | 43 | 新增 3，退回 0 |

逐项核对合同预期：

- 参照用例：原先首个阻断为 alias / rename / truncate / 集合属性的 14 条中 13 条通过（CreateAlias 3、TruncateCollection 4、RenameCollection 3、AlterCollection 3），另有一条（`test_same_alias_isolated_by_database`）已越过别名、阻断点移到 CreateDatabase，属 B。剩余 138 条失败全部是明确的 UNIMPLEMENTED，另 1 条原有跳过：全文与 analyzer 39、rerank 与 function chain 20、数据库 38（CreateDatabase 18、ListDatabases 14、DescribeDatabase 5、AlterDatabase 1）、TIMESTAMPTZ 16、GEOMETRY 7、grouping 7、HybridSearch 6、算术过滤 4、多向量 1。**没有一条「已接受但结果不同」。**
- 上游子集：原剩余 8 条中属于本范围的 7 条（alias 6、rename 1）全部通过，剩 1 条多向量字段。
- 探针：新增 `rename_collection`、`alias_ops`、`collection_properties`；剩 `databases`（B）、`group_by_search`、`multi_vector_hybrid`、`sparse_vector`、`bm25_full_text`、`text_match`（另 6 项 3.x 同样不成功，成败与上一基线相同）。
- 统计只计首个阻断，数字是诊断样本，不是收益承诺。

## 残余风险与未覆盖

- **B 未实施**：数据库 RPC 共 38 条参照用例与 `databases` 探针仍 UNIMPLEMENTED；`using_database` 与带 `db_name` 的访问对已存在的非默认库仍返回 UNIMPLEMENTED。
- **平台**：全部在 macOS arm64 本机执行。Linux 两个架构、Intel Mac 与 CI 环境未跑；工作流的计数与预构建清单已改并通过静态检查，但本批未实际运行，须在下次互通 CI 或候选前跑一次。
- **断电类故障**：进程退出不丢页缓存，因此「目录同步之前的断电回退」只由目录同步的错误注入与顺序钩子覆盖，没有真实断电实测，同 [0007](../decisions/0007-m2-storage-engine.md) 的既有边界。
- **临时文件写入与文件同步失败**未单独注入：它们与改名同在 `replace_file` 的第一个闭包内、走同一出口，由钩子与改名失败的单元测试证明出口分类，没有逐一注入写入与同步。
- **合并用例的字节断言窗口**：别名与 AlterCollection 的合并先后用例断言 manifest 字节不变，依赖排队的后续轮次不在变更完成后才开始；评审复跑 30 轮（含 CPU 满载 20 轮）未复现，同模式的既有创建用例一直如此。改名分支只断言排队与数据不丢，旧句柄上的取消不在报告里可见。
- **既有、未纳入本切片**：崩溃遗留的 `*.json.tmp.<pid>.<n>` 无清理，单独跟进；`wal/` 为普通文件时两边都无法打开集合，是互通说明而非差异。
- **README 的表述**：README 对应已发布的 `0.3.0rc3`，本批只在「集合管理（开发线）」一条中如实说明尚未随预发布交付；发行说明与 Release 附件不变。

## 证据类别

仓库内可重跑：上述测试命令、差分与互通用例、统计的方法与清单（0014 收口记录）、`scorecard.py` 与工作流中的预期计数。本地留存：开发 wheel、统计与 1e+999 核对的原始输出、环境与探针脚本、各步骤评审时冻结的 diff。以上结果均未推送，第三方在推送之前无法核对本记录所指的提交。
