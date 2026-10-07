# M2 功能与故障验收证据

合同以 [0006「验收」](../decisions/0006-m2-scope.md#验收)与 [0007「验证」](../decisions/0007-m2-storage-engine.md#验证)为准。此处记录测试证据及缺口，不另定义协议；性能基准与里程碑最终验收另行执行。

## 范围

- S1 解锁的上游用例迁移、原过滤表达式恢复与两条客户端线对照。
- 健康状态下的并发写读与提交、受控的故障窗口拒绝，以及在 WAL I/O 后尚未返回的写入退出；按各用例实际观察的提交点与结果核对。
- Parquet 元数据与批次解码 panic、完整列边界校验、真实 I/O 错误，及 collection 拒绝打开前不得清理孤儿文件。

## 已有证据

| 合同条目 | 测试入口 |
| --- | --- |
| 冻结代锁外构建与下一代写入交错、快照稳定、发布失败重试 | `read_snapshot.rs` |
| 同名懒打开互斥、不同名并行、打开持共享许可、关闭等待登记、名字锁回收 | `db.rs::tests` |
| 改名提交点两侧、整库写入封锁、保留 WAL 全部同步后才关闭、显式重试 | `db_persistence.rs` |
| manifest 与 CreateIndex 提交点两侧、data/delta unlink 之间、目录改名两侧、部分 WAL 追加后退出 | `crash_recovery.rs` |
| WAL 每字节截断与部分写入、零长度 EOS、解码 panic 与真实 I/O | `wal_recovery.rs`、`wal.rs` |
| 数据源、主键引用、有效位图的逐行参考模型 | `collection.rs::tests` |

## 本次新增证据

- [adapter 清单](../../tests/python/adapter/INVENTORY.md)：迁移全部 40 条待复用用例，恢复此前绕过过滤限制的表达式；HNSW→FLAT 的授权替换仍按依赖逐条登记。
- [concurrency_faults.rs](../../crates/core/tests/concurrency_faults.rs) 的压力用例先收到一次 flush 故障，再启动四个写者、Query/count/Search 与提交线程；后续 recommit 时机不受该用例控制；写者全部返回后，在读与提交可能仍在途时退出。该用例只核对记录的已确认写入的恢复；三个种子改变数据，不代表三个受控交错，也不证明写入期间经过拒绝分支。
- 同文件的受控用例等待写者成功追加后，才从另一线程注入 flush 提交后故障；flusher 保持待重试状态，Insert/Upsert/Delete 三次都须返回节流拒绝，拒绝计数必须为 3，模型不更新。显式重试后，用 File 层钩子分别暂停完整批与 ENOSPC 半截批，两个 API 都尚未返回时退出。恢复集合必须包含全部确认行，最多包含记录的完整在途候选；确认值逐行匹配，拒绝与半截批不得复活。两种用例的证据不能互相替代。
- 同一文件固定状态写锁内 Mutation/Freeze/Publish/CreateIndex、drop 同步与 recommit 的 panic 的 collection 隔离、旧快照、拒绝后续排队操作及重开结果。WAL 已追加后发生实现缺陷的 panic，按 0006/0007 的单列异常协议验收，不套用「错误写入一律不存在」。自动冻结失败之前已成功写入的 RPC 仍成功，之后封锁该 collection。
- Flush 准备阶段与锁外写盘 panic 均按实际提交点分类；manifest panic 按实际 replace 状态分类，Flush 与 CreateIndex 使用同一提交边界；WAL 清理 panic 作为已提交后的清理故障，不阻断后续写入。create/drop 改名之前的 panic 进入错误回滚，别名维护的真实序列化、写入与 rename 整段受 panic 边界保护；rename 后注入会实际改变 aliases 文件，drop 的错误仍使用「目录已移走、元数据待清理」语义。关闭 panic 停止 manifest 提交、同步保留 WAL、完成关闭闸门；重入关闭不挂起。暂停持在途许可的 Flush，再由第二个进程探测 LOCK，关闭只有在该 Flush 完成后才释放 LOCK。
- Parquet 的元数据和首批/后续批 panic 均拒绝整个文件，不返回已读批前缀；每列在转换之前经过 `validate_full`，会增加打开期的全列扫描；开销留待下一步基准测量。读取失败不登记 collection、不清理孤儿，真实 I/O 下次访问可重试，确定的解码失败仍沿用判定缓存。
- 文件操作记录只在对应同步、replace、unlink 成功后产生，固定继承文件与新文件、manifest tmp、原 WAL 的同步先于 replace，及 unlink 后目录同步与 data/delta 顺序。冻结 WAL 的 Data/Delta/目录同步在 I/O 错误及中途 panic 时均保留全部 File，最后一次同步成功之后才统一清空；两类构建共用同一个同步函数。它证明本实现调用顺序，不替代 Linux strace 或真实掉电设备。
- debug 顺序检查覆盖五层锁、三个查表叶子锁与 aliases 锁，反向、重入、守卫提前释放均有断言；别名读改写入口另检查具体 aliases 锁身份，不以任意 rank-6 锁代替。关闭闸门与 WAL 文件锁保持结构性的叶子性质。按负责人确认保留独立 aliases 叶子锁，保护完整读改写事务；两目标并发删除保留第三个存活别名。前置别名校验之后释放该锁，再取得 flush / 状态锁；提交后的别名事务只向叶子方向取锁，临界区内不再取得其他有序锁或查表锁。

受控交错不等于压力测试，进程退出不等于真实断电。Linux/Windows、真实断电与性能数据不由上述测试推断。

## 执行记录

2026-10-08，macOS；基线为 `48d17c0` 之后的本次工作区改动，参照为 v3.2.1 / `43d1257`。下表只记录本次执行，不代表后续版本的结果。

| 检查 | 结果 |
| --- | --- |
| workspace 默认测试、core fault-injection 全套 | 全通过，0 ignored；新故障组合文件 18 条 |
| 两套 Clippy `-D warnings`、fmt 与 diff 检查 | 通过 |
| 本实现 + pymilvus 3.0.2 / 2.6.17 | 两条线分别 Python 232、差分 79、存储互通 52，全通过 |
| 复制后的 adapter 对真实 3.x，两条 pymilvus 线 | 各 179 passed |
| 40 条新增用例本体与上游 AST 核对 | 除授权的索引替换外一致 |
| INVENTORY 汇总、逐条状态、实际定义 | 327 条清单、165 个用例函数，收集 179 条；待复用 0 |
| 新故障组合文件连续三轮 | 本轮每轮 18 条通过；数据种子不冒充受控交错 |
| 受控变异 | 取消毒化、Parquet panic 逃逸、错分 replace 后故障、省略 unlink 后目录同步、取消锁序检查均被抓住；源码恢复后全套通过 |
| 发布构建检查 | 默认扩展没有新增故障注入或 debug 顺序检查的标记 |

本轮未执行 Linux strace、Windows、真实断电设备或性能基准；里程碑最终验收另行推进。进程崩溃测试只验证操作系统仍运行时的恢复，文件操作记录只验证调用顺序。

### 本轮复审修复

- 并发覆盖改为逐用例界定，新增强制拒绝与 API 未返回窗口的独立恢复模型；拒绝计数与在途哨兵必须触发。
- 负责人确认保留原 aliases 叶子锁设计；别名提交的序列化、真实写入、rename 和目录同步整段受 panic 边界保护，rename 后的文件变化独立断言。
- 冻结 WAL 同步不逐个取走 File，中途 panic 保留全部句柄，明确按提交点前错误重试；统一两类构建的同步路径。drop 的正常与目录已移走分支、recommit 均在状态变更边界内；普通 I/O 错误仍沿用原有重试与回滚语义。
- test_offset 只清理未用 import 与空节标题；保留上游的 all_rows 绑定及其首次 Query 调用，避免修改授权替换之外的用例本体。

本轮复审修复完成后：workspace 与 core fault-injection 全套、两套 Clippy、fmt 及 diff 检查通过；两条 pymilvus 线仍各 Python 232、差分 79、存储互通 52 通过，均使用重新构建的扩展或 wheel。新增反证将拒绝操作写进模型、让半截在途批复活、取消真实别名写入的 panic 边界、取消 aliases 事务锁，四种变异均失败；实验源码恢复后重新运行。

### 注释与证据口径收尾

- 七个副本文件头只声明实际做过的替换；server_manager 兼容副本恢复上游 importorskip 改直接导入的说明，不改用例本体。
- 关闭及正常/被移走目录 drop 的 WAL 同步共用 sync_file，包含活跃 writer 与冻结代原 File 的 trace 断言；记录中的原路径仅标识原句柄，不表示追踪被移走的路径或创建原目录。别名文件自身的同步仍不在该 trace 验证范围内。
- 用例串行互斥使用 parking_lot 的无中毒语义：捕获一次用例 panic 后仍能取得同一锁，保护的仅是测试隔离，不复用失败用例的引擎状态。

收尾建议修正后重新构建并执行：Rust workspace 与 core fault-injection 全套、两套 Clippy、fmt 和 diff 检查通过，18 条组合用例连续三轮通过；两条客户端线仍各 Python 232、差分 79、存储互通 52 通过。新增 debug 检查只核对已持有的具体 aliases 锁，不改变当前事务边界；没有修改上游副本用例本体。

终审后的导入与文件头清理仅删除未用导入和不实说明：22 个副本均通过 `py_compile`，去除顶层 import 后的全部 AST 与清理前一致，165 个用例函数保持不变；两条客户端线的全部 179 个收集项逐项一致，adapter 各 179 passed。本次未改引擎代码，沿用上表的 Rust、差分与存储互通证据。

## 单独登记的维护项

- WAL 规范文件名的格式化逻辑重复：[WalKind::file_name](../../crates/core/src/storage/wal.rs) 是写入入口的格式化方法，[flush](../../crates/core/src/storage/flush.rs) 与 [recovery](../../crates/core/src/storage/recovery.rs) 在扫描时又各自拼接 `wal_data_{number:06}.arrow` / `wal_delta_{number:06}.arrow`，用于排除非规范文件名。该重复在本次改动前已存在，当前三处格式一致，不构成文件名冲突或数据正确性缺陷；风险在于以后修改命名规则时，写入、清理和恢复的识别规则可能漂移。后续维护可复用现有格式化方法，并以非规范文件名、六位以上编号及 data/delta 顺序用例核对边界；本切片只登记，不修改命名与扫描逻辑。
