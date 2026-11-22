# 文档索引

本目录是用户文档、设计决策、验收与调查记录、发行文档的入口。查某个主题时先在本页找到编号，再读对应文档；进展以决策文档开头的「状态」与验收记录为准，本页不记录进展。

- 用户文档：[使用说明](usage.md)（安装细节、平台与验收范围、命令行与导出、停止与恢复、后台维护与内存、性能）与[兼容性说明](compatibility.md)（支持范围、与官方 3.x 的差异、索引、未实现清单、已知限制、降级）。项目入口是根目录的 [README](../README.md)。
- [decisions/](decisions/)：设计决策，按编号 `NNNN-主题.md`，新决策取下一个编号并在下表加一行。
- [testing/](testing/)：验收与调查记录。证据的分类、谁能复核与记录的写法见 [testing/README.md](testing/README.md)。
- [releases/](releases/)：各预发布版本的发行说明与候选方案，以及[版本台账](releases/consumed-versions.json)。

## 设计决策

| 编号 | 查什么时看 | 验收与调查记录 |
| --- | --- | --- |
| [0001](decisions/0001-refactor-baseline.md) | 为什么以 3.x 而非 2.5.1 为基础；两代架构对比、3.x 的耗时归属；Rust 实现的性能约束 | |
| [0002](decisions/0002-data-dir-compat.md) | 数据目录与 3.x 互通的范围；共享文件的约束（约束 8：分词与 BM25 逐位一致）；索引文件共存规则；跨实现验证矩阵 | |
| [0003](decisions/0003-in-process-server-lifecycle.md) | 进程内服务的生命周期：同路径复用、进程退出时的停止与持久化、fork 后的处理、与 3.x 的差异 | |
| [0004](decisions/0004-m1-scope.md) | M1 范围、阶段性对外差异、验收标准；距离类的默认误差 | |
| [0005](decisions/0005-search-thread-model.md) | 检索的线程模型：RPC 与计算池的分工、计算任务的约束、锁的边界、停服与 fork 下的计算池 | |
| [0006](decisions/0006-m2-scope.md) | M2 范围；3.x 存储层的实测事实与缺陷；与 3.x 的差异；持久化保证（第 19 项）；依赖评估（第 24 项） | [M2 验收](testing/m2-acceptance.md)、[功能与故障证据](testing/m2-correctness.md)、[性能总结](testing/m2-performance-summary.md)、[S1 过滤性能](testing/s1-performance.md) |
| [0007](decisions/0007-m2-storage-engine.md) | 存储引擎结构、取锁顺序、快照与有效位图；WAL、flush、drop 与关闭的提交和失败协议；重复 `_seq` 的平局规则 | |
| [0008](decisions/0008-first-release-scope.md) | 首次发布的最小范围与验收；首发与 3.x 的平台差异（Windows） | |
| [0009](decisions/0009-top-level-dynamic-filter.md) | 顶层动态字段过滤 | |
| [0010](decisions/0010-compaction-and-tombstone-gc.md) | 首发 compaction、正常关闭排空、tombstone GC | [合并验收](testing/compaction-correctness.md) |
| [0011](decisions/0011-committed-delete-state.md) | 独立的已提交删除状态 | 同上 |
| [0012](decisions/0012-cli-server.md) | CLI server 的参数、工作者、信号与停服错误 | [CLI 验收](testing/cli-correctness.md) |
| [0013](decisions/0013-first-release-platform-and-packaging.md) | 首发平台、PID 1、发行物验收；候选阶段决策（RC 只发 GitHub Release、公开地址；候选执行见 0021，Intel Mac 为标准层） | [首发准备验收](testing/first-release-acceptance.md) |
| [0014](decisions/0014-index-definitions-and-filter-expressions.md) | 索引定义、DropIndex 与过滤表达式；删除意图与实施验收矩阵 | [索引定义](testing/index-definitions-acceptance.md)、[过滤表达式](testing/filter-tree-acceptance.md)、[收口](testing/0014-compatibility-closure.md) |
| [0015](decisions/0015-nullable-defaults-and-array.md) | 标量 nullable、默认值与 ARRAY；向量 nullable 延后；真值矩阵与前后兼容 | [调查](testing/0015-schema-investigation.md)、[A 验收](testing/0015-scalar-nullable-acceptance.md)、[A 修复](testing/0015-scalar-nullable-review-fixes.md)、[JSON 写入回归](testing/0015-json-write-regression.md)、[B 验收](testing/0015-array-acceptance.md)、[与 0016 的跨平台验收（含 SDK 临时端口复用的缓存隔离）](testing/0015-0016-cross-platform-acceptance.md) |
| [0016](decisions/0016-query-iteration-and-range-search.md) | Query iterator 与 range search；分区调查结论及冻结边界 | [iterator 验收](testing/0016-query-iterator-acceptance.md)、[range 验收](testing/0016-range-search-acceptance.md)、[分区调查](testing/0016-partition-investigation.md) |
| [0017](decisions/0017-partitions.md) | 显式分区（A：目录安全、按选择集可见性、Drop 保留全局删除）与 partition key（B） | A：[设计调查](testing/0017-partition-design-investigation.md)、[可见性组件](testing/0017-visibility-component.md)、[验收](testing/0017-partition-acceptance.md)、[评审修复](testing/0017-partition-review-fixes.md)；B：[调查](testing/0017-partition-key-investigation.md)、[验收](testing/0017-partition-key-acceptance.md)、[性能](testing/0017-partition-key-performance.md)；[非 key 重复主键 Upsert](testing/0017-non-key-upsert-fix.md)、[跨平台](testing/0017-cross-platform-acceptance.md) |
| [0018](decisions/0018-json-write-normalization.md) | JSON 写入优化：复用已规范化 JSON 构造 WAL | [调查](testing/0018-json-write-investigation.md)、[验收](testing/0018-json-write-acceptance.md)、[诊断工具](../benchmarks/json-write/diagnostics/README.md) |
| [0019](decisions/0019-test-sharding.md) | 候选测试的并行编排与确定性分片 | [试点](testing/0019-pilot-acceptance.md) |
| [0020](decisions/0020-continuous-integration.md) | 持续集成：快速层与对照层、零跳过与汇总判定、`ci-pass` / `compare-pass`、推送方式、重新计数、不纳入 CI 的候选内容；判定层不加 retry；停服路径段错误的风险接受（适用范围与停止条件见调查记录） | [试点](testing/0020-ci-pilot.md)、[停服段错误调查](testing/shutdown-segfault-investigation.md)、[工作流与固定依赖](../.github/ci/README.md) |
| [0021](decisions/0021-release-build-in-ci.md) | 在 CI 中构建与验收发行物：冻结身份、候选结果、版本消费、来源证明、禁止签发后重跑、传输层有限重试（第 11 条）、签发路径未变时演练可选（0039）；0013 的本地流程只作诊断 | [试点](testing/0021-release-pilot.md) |
| [0022](decisions/0022-pre-signing-preservation-gate.md) | 签发前的候选保全门闩（已并入 0021 第 3、13、19、20 条，以 0021 为准） | [试点](testing/0022-gate-pilot.md) |
| [0023](decisions/0023-aliases-rename-truncate-databases.md) | 别名、改名、清空与集合属性（A：清空与改名的私有意图与提交协议）；多数据库（B：数据库注册表、五个数据库 RPC、`db_name` 路由、属性与隔离）；锁与测试矩阵；跨数据库改名返回 UNIMPLEMENTED | [A 验收](testing/0023-a-acceptance.md)、[B 验收](testing/0023-b-acceptance.md) |
| [0024](decisions/0024-group-by-search.md) | 分组检索的候选窗口、offset 切片、空值组与参数真值；两个不复制的 3.x 缺陷 | [验收](testing/0024-group-by-acceptance.md) |
| [0025](decisions/0025-timestamptz-and-timezone.md) | TIMESTAMPTZ 与 `timezone` 属性：UTC 微秒值、ISO 解析与输出、时区优先级与 `fold=0`、`time_fields`、请求时区的两个校验点与错误先后、毒化整数、内置 tzdata（jiff） | [验收](testing/0025-timestamptz-acceptance.md) |
| [0026](decisions/0026-rerank-and-hybrid-search.md) | HybridSearch 与 rerank（RRF / Weighted / boost / decay）：三个协议表面、分数契约、候选窗口、错误先后 | [验收](testing/0026-rerank-hybrid-acceptance.md) |
| [0027](decisions/0027-reference-3.2.2-upgrade.md) | 参照从 v3.2.1 升级到 `1320d26` 的对外差异；flush 字节预算 | |
| [0028](decisions/0028-multi-vector-fields.md) | 多向量字段：按字段的内存范数、写入检查、登记差异、WAL 中 null 行不按截断处理 | |
| [0029](decisions/0029-text-match.md) | standard 分词与 `text_match`：固定 Unicode 表、hybrid 回退、`analyzer_params` 的判定与持久化 | |
| [0030](decisions/0030-sparse-vectors.md) | 稀疏向量与 IP：打包字节的存储与解码、SPARSE_INVERTED_INDEX 的定义与调参项、惰性倒排的缓存与作废、排序键与错误先后、带分区时先全局判版本 | |
| [0031](decisions/0031-bm25.md) | BM25 function 与全文检索：校验与持久化、分词器、度量换算、按 segment 的统计与 float32 打分 | [验收](testing/0031-bm25-acceptance.md) |
| [0032](decisions/0032-arithmetic-filters.md) | 过滤中的算术：pyarrow / Python / hybrid 三条求值路线、资源上限 | [验收](testing/0032-arithmetic-acceptance.md) |
| [0033](decisions/0033-hybrid-dynamic-inference.md) | hybrid 动态列类型推断对 pyarrow 的复刻；Arrow 15 / 25 的版本差异 | |
| [0034](decisions/0034-geometry.md) | GEOMETRY 字段与几何谓词：WKT 接受范围、求值语义、CPython 3.12 的 `math.hypot` | |
| [0035](decisions/0035-function-chains.md) | 公共 function chain：执行位置、解码与校验先后、各算子求值、`order_by_fields` 的冲突判定、投影合同、三项登记差异、2.6 线的测试机制与“改后复制”的边界 | |
| [0036](decisions/0036-jieba.md) | jieba 分词：jieba-rs 与 0.42.1 的差异及包装层、用户词与上限、3.x 目录的打开口径、参照环境的受控构建 | |
| [0037](decisions/0037-dump.md) | `milvus-lite dump` 导出：3.x 的行为与实测、五处 3.x 缺陷的修正（function 输出、ARRAY VARCHAR、批大小 0、写文件失败、非有限浮点）、失败清理与信号、`tests/dump` 的依赖环境与发行阶段 | |
| [0038](decisions/0038-nullable-dense-vectors.md) | nullable 稠密向量：3.x 写入时换成零向量的实测、按真实 Milvus 语义存为 Arrow null 的决策、pyarrow 低于 26 时 3.x 打不开的互通代价、解码三种形态与整批为空、检索排除、紧凑输出、`ref26` 互通环境 | |
| [0039](decisions/0039-release-speedup.md) | 发行提速：签发路径（头部、最后两个 job、签发工具的 import 闭包）相对已发布基线未变时演练可选、基线规则、候选结果格式 2 的 `dry_run_basis`；测试 job 直接取构建产物与已测摘要交叉核对；macOS 按平台限流；差分会话组只测量 | [试点](testing/0039-release-speedup-pilot.md) |
| [0040](decisions/0040-reclaim-superseded-rows.md) | （搁置）回收被已提交新版本或已提交删除覆盖的旧版本行：写删负载下内存增长的定位与 3.x 对照、单文件重写的设计与原型数据、三方写删基线（本实现已低于 2.5.1）、重启条件与重启前须处理的评审事项 | |
| [0041](decisions/0041-hnsw.md) | HNSW 向量索引：3.x（faiss）的行为真值与已登记的差异（AUTOINDEX 仍按 FLAT、`max(ef, 窗口)`、过滤回退）、自写纯 Rust 实现的理由、对齐口径与召回门槛、落盘与生命周期、原型基准数据与据此确定的取值 | |

不依附某个决策的记录：[恢复内存与冷启动](testing/recovery-performance.md)、[批量检索性能](testing/batch-search-performance.md)、[分配器保留内存](testing/allocator-retention.md)（Linux 上维护轮次后的 `malloc_trim`）、[Parquet 行组上限与写入峰值](testing/parquet-row-group-memory.md)、[关闭时显式解锁](testing/explicit-unlock.md)、[存量 FAISS 定义的 load 度量检查](testing/index-load-metric-investigation.md)（rc2 的 load 修复合同）、[子进程与关闭重开用例的隔离](testing/process-test-isolation.md)、[JSON 数字私有键修复](testing/json-private-number-key-fix.md)（`json_text` 唯一入口的来由）。

## 发行

0.3.0rc3 起每个版本号的结局以[版本台账](releases/consumed-versions.json)为准。

| 版本 | 发行说明 | 候选方案 | 验收 |
| --- | --- | --- | --- |
| 0.1.0rc1–rc3 | | | [验收](testing/first-release-acceptance.md) |
| 0.2.0rc1 | [说明](releases/0.2.0rc1.md) | | [验收](testing/0.2.0rc1-acceptance.md) |
| 0.3.0rc1 | [说明](releases/0.3.0rc1.md) | [方案](releases/next-candidate-plan.md) | [验收](testing/0.3.0rc1-acceptance.md) |
| 0.3.0rc2 | [说明](releases/0.3.0rc2.md) | [方案](releases/0.3.0rc2-candidate-plan.md) | [准备核验](testing/0.3.0rc2-preparation.md)、[验收](testing/0.3.0rc2-acceptance.md) |
| 0.3.0rc3 | [说明](releases/0.3.0rc3.md) | [方案](releases/0.3.0rc3-candidate-plan.md) | [验收](testing/0.3.0rc3-acceptance.md) |
| 0.4.0rc1 | [说明](releases/0.4.0rc1.md) | [方案](releases/0.4.0rc1-candidate-plan.md) | [验收](testing/0.4.0rc1-acceptance.md) |
| 0.5.0rc1 | [说明](releases/0.5.0rc1.md) | [方案](releases/0.5.0rc1-candidate-plan.md) | [验收](testing/0.5.0rc1-acceptance.md) |
| 0.6.0rc1 | [说明](releases/0.6.0rc1.md) | [方案](releases/0.6.0rc1-candidate-plan.md) | [验收](testing/0.6.0rc1-acceptance.md) |
| 0.7.0rc1 | [说明](releases/0.7.0rc1.md) | [方案](releases/0.7.0rc1-candidate-plan.md) | [验收](testing/0.7.0rc1-acceptance.md) |

发行流程的命令（流程与前提以 [0021](decisions/0021-release-build-in-ci.md) 为准）：

| 任务 | 命令 |
| --- | --- |
| 演练 / 正式运行（从以冻结提交为最新提交的已推送分支触发；签发路径相对已发布基线有变化时，正式运行引用同一提交含 PID 1 的成功演练，未变时可不引用，见 [0039](decisions/0039-release-speedup.md)） | `gh workflow run release.yml --repo <owner/repo> --ref <冻结分支> -f commit=<完整 SHA> -f version=<版本> -f dry_run=true`；正式运行改为 `-f dry_run=false -f pid1_dry_run=<演练运行 ID>.<attempt>`，不引用演练时省去 `pid1_dry_run` |
| 签发路径判定（只读；触发正式运行前确认是否须演练，也可回放历史版本） | `python3 tests/release/runs.py --repo <owner/repo> basis --sha <冻结提交>` |
| 签发环境配置只读核对（第 19 条；改动 `release-signing` 后、下一次正式运行前） | `gh api repos/<owner/repo>/environments/release-signing`；`gh api repos/<owner/repo>/environments/dry-run-signing` |
| 版本台账校验（第 3 条） | `python3 tests/release/ledger.py docs/releases/consumed-versions.json` |
| 发布准备：核对待发布的七个附件（第 20 条；用候选冻结提交中的工具） | `python3 tests/release/release_set.py --dir <附件目录> --version <版本> --repo <owner/repo> --prepare` |
| 发布后公开下载核验（第 20 条；只在 `main` 上触发） | `gh workflow run public-check.yml --repo <owner/repo> --ref main -f version=<版本>` |
| 签发前保全：保存候选证据集并输出审批行 / 复核已保存目录（[0022](decisions/0022-pre-signing-preservation-gate.md)；在冻结提交的干净 worktree 中运行，演练用 `--mode dry-run`） | `python3 <worktree>/tests/release/preserve.py save --mode release --repo <owner/repo> --run <运行 ID>.<attempt> --version <版本> --root <本地留存根目录>`；`python3 tests/release/preserve.py verify <保存目录>`（复核 0.7.0rc1 及更早版本的目录时，用该版本 tag 中的工具：候选结果格式 2 起读取方只接受格式 2） |
