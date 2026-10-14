# 0014 切片 A：索引定义与 DropIndex 实施验证

日期：2026-10-14。实现依据为 0014 的冻结设计；对照为 milvus-lite v3.2.1 / `43d1257`。本节是开发工作区的验证记录，不是 rc3 发布验收，未改变版本、tag 或已发布的 rc2 发行物。

## 实现与证据

- 创建接受 HNSW、HNSW_SQ、IVF_FLAT、IVF_SQ8 及标量 INVERTED；定义按 3.x 保存，检索仍为 FLAT，过滤仍为扫描。IVF_PQ、DISKANN 等类型继续拒绝，load 检查保持 rc2 行为。
- CreateIndex / DropIndex 共用 Db 写入闸门、flush 锁及文件预检。snapshot 定义冲突、目录链异物、删除 ACL、manifest 的删除保护与 tmp 继承 ACL 在删除前拒绝。
- manifest 准备与替换分离；tmp 从 create_new 成功开始归本次操作所有，失败与 panic 按提交点清理。`prev_copy_error` 仍非阻断，`AfterReplace` 发布已提交定义并保留完整重新提交协议。
- 意图版本 1 在删除前持久化；部分删除、同步失败或同步前进程退出保留封锁。清理待完成保留已知安全的实例状态，不能用它放行不同的磁盘标记；清理失败不改变原业务错误类别或已提交 RPC 结果。

核心矩阵见 [index_changes.rs](../../crates/core/tests/index_changes.rs)，覆盖两个真实 Db 入口的准备、删除、提交与清理出口。包含同启动内重开、可信启动变化与未知来源、损坏标记、并发 load、组合故障、所有钉住 snapshot 的比对、版本溢出及文件操作顺序。启动覆盖入口仅在 fault-injection 构建存在，模拟 UUID 变化不作为实际重启或断电证据。

真实 3.x 互通见 [test_index_storage_interop.py](../../tests/storage_interop/test_index_storage_interop.py)：

- Rust 新建四种 ANN 定义，3.x 能构建 `.idx`、load 并检索，Rust 再次打开仍按精确结果执行。
- 真实侧车的删除、同类型换 metric、未钉住旧文件清理，以 3.x 检索距离验证；snapshot 钉住冲突时拒绝、相同时保留，3.x 能恢复 snapshot。
- `M="abc"` 的现成侧车可由 3.x load；Rust 删除之后在 manifest 替换前故障，定义仍在，3.x 重建报错，Rust 仍能 load，固定已登记的可用性影响。
- 只有 WAL 的 DISKANN / INVERTED 定义定向 DropIndex 后，两侧 load 不同，固定额外 flush 的登记差异。
- 私有最终标记由 3.x 原样保留，snapshot restore 不复制；Rust 未知启动依据只拒绝该 collection 的索引变更，读写与 load 正常。

参考 ANN 的量化距离不要求等于精确基准；HNSW_SQ 实测可能返回大于 1 的 COSINE 分数。互通测试要求参考侧正确构建、加载和返回合法结果，Rust 的精确距离另行严格验证，符合 0014 的登记差异。

## 实跑结果

macOS arm64，CPython 3.12.14 / pymilvus 3.0.2，以及隔离的 CPython 3.10.21 / pymilvus 2.6.17，均安装本工作区构建的 abi3 wheel。测试与发行物在仓库外，安装命令和公开 rc2 附件未替换。

| 检查 | 结果 |
| --- | --- |
| Rust workspace 默认 | 272 passed |
| core fault-injection | 354 passed |
| grpc all-features | 22 passed |
| Python 端到端，两条 SDK | 各 277 passed |
| 与真实 3.x 的全量差分，两条 SDK | 各 131 passed |
| 存储互通，两条 SDK | 各 96 passed |
| 打包 / 平台脚本自检 | 170 passed |
| 连同基准脚本自检 | 227 passed |
| fmt、Clippy 默认 / all-features、locked metadata | 通过 |
| 实际 wheel / sdist 检查与原生库检查 | 通过 |

Linux arm64 的 AlmaLinux 8.10 / manylinux_2_28 容器，以独立源码副本、独立 target、Rust 构建并行数 4 核对目录与故障矩阵；权限测试子进程从 root 降权为普通用户，避免特权绕过只读目录拒绝，并实测粘滞位目录中的外来所有权拒绝。最终目录 / 故障矩阵为 22 passed（macOS 对应 23 passed，另含保护标志与 ACL）。此证据覆盖 Linux 专用实现与新入口，不代替 rc3 全平台矩阵。

两项隔离源码变异各被对应测试拦下：忽略意图封锁会错误允许新定义提交；删除后省去 indexes 目录同步会违反 trace 中「最后一次 unlink → 删除同步 → manifest replace」的顺序断言。变异在仓库外的源码副本和独立 target 中执行，未改动工作区。

## 评审修复验证（2026-10-14）

独立核实了五种 FAISS 类型（AUTOINDEX、HNSW、HNSW_SQ、IVF_FLAT、IVF_SQ8）的 BM25 / NONE：以修复前工作区 wheel 写入 300 行已提交数据及索引定义，真实 3.x / `43d1257` 重开后均在 load 返回 code 1、`unsupported metric '<metric>'; expected COSINE / L2 / IP`。FLAT / BRUTE_FORCE 的四个对照组合均能 load。因此在现有 metric 枚举校验之后统一拒绝前十个组合，返回 UNIMPLEMENTED；五种类型的 COSINE / L2 / IP，以及 FLAT / BRUTE_FORCE 原有 metric 集合保留。AUTOINDEX 限制的发布行为变化已登记在 0014，未改已发布 rc2 发行物。

snapshot 引用在公共读取处统一规范化，合并 pins 与逐份 manifest 比对使用相同路径。新增反斜杠引用测试分别拦住误删与漏比对；真实 3.x 侧车、历史路径形态的 snapshot 恢复和同定义重建也已互通核验。

替换权限预检只保护实际侧车删除。Linux 子进程保留真实 root 凭据、将有效 UID / GID 改为普通用户：无旧文件可删时 FLAT、AUTOINDEX、INVERTED 的创建与 DropIndex 都成功；有文件可删时仍在预检拒绝，manifest、旧文件与意图状态不变。同一测试在修复前独立源码 / target 上失败于首次 FLAT 创建，证明此前的回归及本次修复。未实跑 AT_SECURE / file capabilities 环境，代码中无待删文件时完全跳过该凭据预检。

安全清理失败明确提示在当前实例内重试；标记内容或形态不符时保留封锁，提示确认系统重启后的人工恢复，不给重试提示。故障注入核对两种信息及后续状态。新增 Python 代码与其子进程脚本按常规排版展开，0004 / 0006 改为链接 0014。

修复前的 metric 单测、两条 snapshot 反例和 Linux 凭据反例均失败；修复后均通过。完整复跑结果如下，沿用上节两个 SDK 环境：

| 检查 | 结果 |
| --- | --- |
| Rust workspace 默认 | 274 passed |
| core fault-injection | 356 passed，含 2 个子进程结果；顶层套件 354 passed |
| grpc all-features | 22 passed |
| Python 端到端，两条 SDK | 各 287 passed |
| 与真实 3.x 的全量差分，两条 SDK | 各 131 passed |
| 存储互通，两条 SDK | 各 107 passed |
| 打包 / 平台脚本自检（与 Python 端到端合跑） | 170 passed，合计 457 passed |
| Linux arm64 manylinux_2_28 目录 / 故障矩阵 | 25 passed，包含不同凭据子进程及其父进程断言 |
| fmt、Clippy 默认 / all-features、实际 wheel / sdist 检查及原生库检查 | 通过 |

上节 fault-injection 的 354 同样包含 2 个子进程结果，按顶层套件计数为 352；本轮新增 1 条 metric 单测和 1 条 snapshot 集成测试。存量 FAISS 定义的 BM25 / NONE load 检查仍为 rc2 的历史缺口，已在 0002 单独记录，本轮未更改 `unbuildable`。

## 边界

未实跑系统重启、真实断电、新版本的 Linux x86_64 或 Intel Mac 全套验收；MSRV / sdist 解包构建和全平台发布矩阵在 rc3 候选阶段按 0013 执行。overlayfs、共享或未知文件系统关闭自动恢复，人工恢复与实例内清理重试按 0014 / README 说明处理。compaction 的祖先目录检查历史问题仍单独记录，切片 B 未实施。
