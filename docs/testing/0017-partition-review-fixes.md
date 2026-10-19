# 0017 A：实施评审修复与补验

- 状态：2026-10-19 独立复审通过，限 0017 A 的实施与三项修复；本记录随 A 收口本地提交，未推送或发布。
- 日期：2026-10-19。
- 合同：[0017](../decisions/0017-partitions.md)，继承 0007 的提交与 WAL 清理、0014 的启动来源及可信缓存边界。初轮 [N7 验收记录](0017-partition-acceptance.md)及其证据包保留为历史，不追改原始日志。
- 仓库基线仍为 `a01310cbf668ba5676aa3f07d2534199f1caede5`；参照仍为 v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`，保持只读。开发版本字符串未切换，不构成发行候选。

## 核实与最小修复

三项新增回归均先在未修复产品上实际失败，再在修复后通过。不是只采信评审日志。

| 阻断 | 实际失败与原因 | 修复及保留边界 |
| --- | --- | --- |
| P1：缺失目录的别名 | 空 collection 的 `_default` 未物化；准备 `_DEFAULT` 后两名首次指向同一 dev/ino，旧预检只看到了两个不存在的路径 | `partition_paths::prepare` 物化后重新校验 manifest 全部名称、额外名称和 pins；只回收本次新建的空目标并同步父目录。Linux 上物理不同的名字仍合法；复用目录、未知文件及钉住文件不删除 |
| P1：Drop 遗留已提交 WAL | manifest replace 后退出，恢复滤掉已提交行，空 MemTable 使普通 Flush 不冻结；移除分区后旧 WAL 仍引用该分区 | Drop 非默认分区前检查 `retained_old_wal`，必要时冻结空恢复代并走已有同步、提交、清理协议。失败阻止移除分区；不忽略未知 WAL，不改变共享格式 |
| P2：非默认索引意图 | collection 与实际 p/indexes 均为 APFS，默认目录缺失却令基础 `BootContext.local` 为 false | 基础判定只检查 collection；意图入口继续检查每个实际文件父目录，写入与重开统一。启动来源、UUID 和本地文件系统白名单保持不变 |

空 WAL 的额外排空限定在 DropPartition：最初试过将普通空 Flush 一起排空，完整故障套件中的 `index_recommit_does_not_delete_inherited_wal_before_first_generation_publish` 揭示这会改变 0007 已有合同，已撤回该扩大范围的实现。最终该原用例全绿，原测试与断言没有修改。

## 回归与二、三阶影响

| 边界 | 本轮证据 |
| --- | --- |
| 已登记但缺失目录 | Rust 组件覆盖默认及 Unicode 名称、manifest 与 pins 两种来源；Db 覆盖默认、显式大小写和 Unicode 名称，拒绝前后 manifest 不变、可重试合法名称、写入及重开；原始 gRPC 增加同组三项 |
| 已存在目录与复用 | 复用原有路径、snapshot、未知文件与符号链接测试；新增清理仅 `remove_dir` 空目标，无递归删除，不扩大既有清理所有权 |
| 空恢复代及新写入 | data、delta、混合三种 WAL；每种覆盖立即 Drop、新活跃写入、unlink 错误、unlink panic、提交前 panic、replace 后目录同步错误，共 18 组 |
| Drop 成功返回后崩溃 | 每组先在 Flush replace 后退出，再在 Drop 成功后直接 `process::exit(0)`；不调用正常关闭。每次重开核对 p 已移除、默认分区 id=9 可读，已提交删除不复活，WAL 已排空 |
| 失败及重试 | 注入错误后 p 仍登记；同实例重试完成，再直接退出并重开。已有故障套件补验普通 Flush、重提交、关闭、快照、毒化及提交两侧的其他路径 |
| 真实启动及文件系统计算 | 单元测试不设置完整 BootContext 替身，实际读取内核来源并检查 collection / p/indexes；默认目录有无两个对照、缺失实际目录和缺失 collection 均校验；仅改 marker 的历史 boot UUID 模拟另一次启动，检查真实当前路径是否允许清理 |
| 实际 Db 索引故障 | 用评审同一探针重新构建，在 p 独有侧车及附加空默认目录两组，真实持久化标记均为 `auto_recovery=true`，模拟可信不同启动后的恢复均成功。该额外探针的启动模拟使用已有替身；上述单元测试另负责不绕过真实路径判断 |

历史 `auto_recovery=false` 标记不自动升级：当前路径可信不能证明标记创建时的缓存边界可信。旧标记继续按 0014 的人工恢复协议处理，不能为修复此判断错误而放宽未知历史状态。

本轮没有实际重启宿主系统；进程退出测试不替代断电保证。Linux 与 macOS 按实际路径身份分支断言，不根据操作系统名称预设大小写结果。

## 检查 / 写入顺序与扫描

Create 的形态族检查包括名称、真实根及子目录、已存在目标中的未知条目、最终文件链接、当前及 pinned 命名空间物理身份；可预先判定的错误仍在物化前拒绝。目录不存在时无法只凭字符串判断实际别名，因此在物化后补查，并在任何 manifest 准备及提交之前拒绝。能力族沿用目标枚举、逐级创建及目录同步的实际错误传播；不以权限预检替代 I/O 结果。失败清理只移除本次创建且仍为空的目录；unlink 或父目录同步错误与原错误一起返回。

Drop 仍先检查未知目标（不 Flush）、对默认目标先普通 Flush 后拒绝；仅已登记非默认目标进入额外空恢复代排空。该阶段不移除分区引用、不复制 delta；全部成功后才进入既有路径及索引意图检查、delta 转存、manifest 提交与内存发布。清理失败不返回成功；不修改 0007 的持久化顺序。

扫描范围为全部 0017 实施变更及直接调用链，重点复核下列模式与出口：

- `prepare|validate|create_dir_synced|remove_dir`：Db Create、分区准备、恢复、索引文件检查、Flush 的继承同步与 Drop 转存；新建、缺失、复用、pins、别名及链接。
- `retained_old_wal|freeze_active|cleanup_committed_wal|cleanup_wal|has_wal`：恢复生产标志、runtime 发布及复位、普通 Flush、索引重提交、关闭与 Drop；空/非空、失败、重试、直接退出。
- `boot_context|local_fs|_default/indexes|auto_recovery`：标记持久化、重开检查、路径解析、实际目录同步和既有故障替身；可信、未知来源与未知文件系统继续 fail-closed。

## 新构建身份与执行结果

相对 N7 的产品逻辑仅改四个文件：`db.rs`、`partition_paths.rs`、`runtime.rs`、`index_platform.rs`；`index_intent.rs` 只增加不替换真实路径判断的单元测试。另增加 Rust / 原始 gRPC 回归及同步文档。没有新增生产依赖、锁层级、数据格式或 RPC。

产品输入清单含 124 项，SHA256 为 `0259f6a9aaf06a2a5a46ecddc5d16bc295634bdabff5b4d778042df6e9316086`；实际测试后再次核对全部输入未变。

| 产物 | SHA256 |
| --- | --- |
| macOS arm64 wheel | `ae00154d487b14a9d3f1dd407be2dd0574cde67d809c606bb71daf3aca89da20` |
| macOS 原生库（两条 SDK 实际所装） | `c7806f8f418c446887fc1cc6f3ea99d3ab4c29303c996e531c4f1d1e9b0d9a4c` |
| Linux aarch64 wheel | `de352f2db3ea0d186602516d58bf5bc1493c82884d2d82aeb2d266d49807d14c` |
| Linux 原生库 | `c403a9d30281d005af8532627003c65c2c11926aaa181963de3a0c48a57334ba` |
| sdist | `df2cd6800f7ff391f3e304e7a9b83915e8d03d2224522eb7d3c14f6cfbc51496` |

| 本轮重跑 | macOS arm64 | Linux aarch64 |
| --- | --- | --- |
| Rust workspace | 342 passed | 343 passed |
| core fault-injection | 434 passed | 435 passed |
| Python 全套，SDK 3.0.2 | 436 passed | 436 passed |
| Python 全套，SDK 2.6.17 | 436 passed | 未重跑 |
| 差分全套，SDK 3.0.2 / 2.6.17 | 两条 SDK 各 340 passed | 未重跑全套；分区子集 SDK 3.0.2 为 15 passed |
| Arrow 15 / 25 双向互通 | 195 passed | 未重跑 |
| 产品目录边界工具 | 两条 SDK 各 13 组通过 | SDK 3.0.2 的 13 组通过 |
| 新增缺失目录别名原始 gRPC | 两条 SDK 全套中各包含 3 项 | SDK 3.0.2 全套中包含 3 项 |
| WAL / segment 回退到已发布 0.2.0rc1 | 两支均拒绝非默认分区且文件不变、健康 collection 可读、Drop 后默认数据及全局删除可读 | 未重跑 |
| 工具自检 / packaging 与 platform 自检 | 17 / 170 passed | 未重跑 |
| wheel / sdist 检查器 | 内容、arm64、最低 macOS 11.0 通过 | 内容、aarch64、readelf GLIBC 上限 2.28 通过 |
| fmt / Clippy（workspace、all-targets、fault-injection，`-D warnings`） | 通过 | 以实际 Rust 构建及测试覆盖；未另跑 Clippy |

Rust 数量按每个独立 harness 的最终结果汇总，不将父进程日志中嵌套的 child 输出再次相加。Python 全套与差分分进程运行；环境固定 SDK 版本，先安装本表 wheel，再核对实际原生库 SHA256。两条 SDK 的 Python 全套各七条警告保留，没有以 skip、忽略或放宽断言处理。

复现、最终命令日志、实际探针源码、产品差异、输入和产物清单保存在仓库外；本轮证据清单含 52 项，SHA256 为 `96074e0c31c3a3c683133dd0c57825fc67b59bb7979402525e01f0e17217a82e`；文本证据包 SHA256 为 `9ee629ef73ce83af7a04d56cac6b5838c40ff25d1b624ca498fe4855c8d9cd0e`，不含数据库、wheel 或构建缓存。

## 证据边界

本轮修复不涉及过滤、检索、可见性算法、列编码或共享存储格式。N7 的 105 条性能、固定兼容统计和资源测量保留为旧构建历史；本轮不重新宣称性能改善或完成 0001 的全部目标。Linux x86_64、Intel Mac、全 CPython 与正式发行候选矩阵仍须按 0013 在最终候选重建，不能拿本轮开发补验代替。


## 独立复审收口

2026-10-19 独立复审结论为通过，限 0017 切片 A 的实施与修复。复审覆盖全部 58 个变更文件及直接关联调用链，确认目录身份、空恢复代 WAL 排空和实际索引目录判断三项阻断闭环，完整范围复扫及二、三阶检查未发现新的阻断。该结论不扩展到尚未冻结的 partition key 切片 B 或最终发行验收。

独立重跑：Rust workspace / fault-injection 为 342 / 434；SDK 3.0.2 和 2.6.17 的 Python 全套各 436；SDK 3.0.2 完整差分 340；Arrow 双向互通 195；工具自检 17。fmt、全 workspace / all-targets / fault-injection Clippy 及 diff 检查通过。互通初次因参照解释器指向不同 checkout 被环境守卫拒绝；改用本仓固定参照 checkout 后通过，不将环境准备失败计为产品失败，也不隐去该次日志。

Linux 及 SDK 2.6 完整差分本轮仅核对 N8 归档；性能、固定统计仍为 N7 历史结果。真实内核重启、断电及最终发布平台矩阵没有新增实测证据。独立复审核对 52 项补验证据与 124 项产品输入哈希，源码未变；收口只更新文档，不重建已核验产品，也不重跑没有被本次文档变更影响的产品套件。

独立复审的 JSON 记录及 17 份日志已另存仓库外文本归档，不包含数据目录或构建产物。复审记录 SHA256：`6c5d08c0701d0f667c98948f062628ede356311943048261f257192b1550a39c`；归档 SHA256：`b2f7794a0118c57b1afedc6cc6312e9086c87f4051e8f207c86a62b917023003`。原 N7 / N8 证据和指纹保持原身份，不将本次文档收口冒充新的产品构建。
