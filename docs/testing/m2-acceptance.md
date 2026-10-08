# M2 最终验收记录

日期：2026-10-08。合同为 [0006「验收」](../decisions/0006-m2-scope.md#验收)，协议以 [0007](../decisions/0007-m2-storage-engine.md) 为准；本记录不另定义功能面，也不代表首次发布验收。当前验收候选已通过两份独立复审，性能目标只部分达到。

## 版本与范围

- 已提交并推送的基线：`940b3e0`（S1 上游用例复用）、`06f6d66`（并发与故障协议）。本轮存储修复落在 `4acaab8`，差分连接所有权落在 `fca67f5`；基准与最终验收证据作为第三个逻辑提交记录，不以本轮候选的身份替换历史测量版本。
- macOS：Apple M2 Pro，10 核、32 GiB，macOS 27.0.1，Rust 1.98.1；Python 3.12.14 / pymilvus 3.0.2，另以 Python 3.10 的独立 wheel 环境验证 pymilvus 2.6.17。
- Linux：Docker Desktop 29.7.2，ARM64 Linux 7.0.12-linuxkit，Rust 1.99.0，Python 3.13，pymilvus 3.0.2；容器内 overlayfs，非 root 用户。镜像 `rust@sha256:24e632c09342c20abf8312cf4f61430a911c01ed3a5e4c02b87292b1c39c5273`。源码复制到容器，不以 macOS 的共享挂载模拟 Linux 文件系统。
- 参照：v3.2.1 / `43d1257`；基准环境安装包的 121 个 Python 源文件与该 checkout 逐字一致。互通覆盖 pyarrow 15.0.0 与 25.0.1。参照 checkout 全程只读。
- Windows 留到首次发布前；真实断电、macOS ENOSPC 磁盘镜像不由进程退出或 File 层错误注入替代。已有 JSON/`$meta`、损坏 WAL 与目录形态差异仍按 0002/0006 登记的边界验收。

## 合同与证据映射

| 0006 验收项 | 证据与当前状态 |
| --- | --- |
| 1. 重开、删除、upsert、S1 与专门场景差分 | 两条客户端线各 80 passed；度量分叉两侧显式值、auto_id 续号、NotLoad、limit 顺序及 compaction 文件数控制均沿用已固定用例；本轮连接所有权回归另见下文 |
| 2. 上游用例复用 | INVENTORY 的 327 条逐项清单：119 已复制、46 改后复制、待复用 0；165 个函数展开为 179 条；Python 全套两线各 232 passed |
| 3. 共享目录与 pyarrow 下限 | 两线 storage_interop 各 60 passed（原 52 + 真实 data-only 目录八条入口/读者组合）；覆盖双向/交替写入、故障后另一侧读取、WAL 截断、JSON 字节、共享索引定义及 fail-closed；非默认分区、snapshot RPC、compaction 写入等范围外部分不据此宣称完成 |
| 4. 崩溃、错误、panic、系统调用顺序 | macOS/Linux workspace 与 core fault-injection 全绿；core lib 128/149，Db 集成 10 条，组合故障 18 条；Linux strace 成功调用独立核对通过，细节见下文 |
| 5. 有效位图与读取映射模型 | visibility 的随机 `_seq` 模型、collection 的逐行发布模型、读快照在写入/发布失败/重开后的断言全通过；进程退出结果由独立参考模型核对 |
| 6. 并发 | 并发写/删/flush/读、受控拒绝与未返回窗口、同名首次打开与名字锁回收全通过；不将三个数据种子冒充三个受控交错 |
| 7. 基准 | 三方两规模各三轮，54 条结果已收齐；性能目标部分未达到，结果与口径写入 0001；修后 Rust 增量复测单独列出 |
| 8. 构建与静态检查 | 默认及 fault-injection 的 fmt/Clippy、release 扩展构建与依赖图核对；三目标图未发现 cc/cmake/bindgen/protoc 发行包，prost-build 通过 compile_fds 使用纯 Rust protox 结果，不调用外部 protoc |

本轮只扩大验证证据，不把已登记的范围外功能或 Windows/真实断电归入通过结论。

## 本轮发现并修复的缺口

### 继承目录的同步链

新增回归在修前明确失败：首次提交前没有 `collection/partitions/` 的 SyncDir 事件。父目录 owns `_default` 的目录项，仅同步 `_default` 及其子目录不能证明该目录项已持久化。

修复为完整同步现有 partitions 链，并将 metadata 的真实 I/O 错误向上传递；目录是否必需按各自引用判断：data/ 与 delta/ 分别由 data_files、delta_files 决定，两者无引用时共同祖先可以未创建。相对目录的空父路径按当前目录同步。新增回归固定完整祖先、相对路径、错误目录类型及 BeforeReplace 分类；既有空表 CreateIndex、重试与关闭用例继续通过。落盘格式、发布点、WAL 同步与清理协议不变。

后续评审发现上轮把「无引用」错误地按整个 collection 判断，导致真实 3.x data-only 目录首次 CreateIndex 失败。本轮新增单测与 Db 入口回归先复现旧错误，再固定 schema-only/data-only/delta-only/data+delta 四种形态；同步函数不创建未被引用的目录，缺失被引用文件仍报 BeforeReplace。正常空代写盘继续沿用原有先创建 data/delta 后同步的顺序；没有活跃写入或残留 WAL 的 Db Flush 可以不产生提交。互通回归直接由 3.x 生成 data-only 目录，对两种读者固定四条入口及 get/count/search、文件字节与 manifest 引用。

检查清单：默认分区与安全相对路径、引用文件/Schema 的存在性及普通文件类型、collection 到数据库根上级的祖先、各分区子目录是否被自身引用以及存在时的真实目录类型。上述判定和实际 open/metadata/sync 错误都在 manifest replace 之前处理；同步函数没有 create/rename/unlink，权限不以 access 预检代替真实操作结果。祖先的四层来自固定布局 collection→collections→数据目录根→根的上级，不是任意数据目录路径深度的上限。

### Linux 大小写断言

原用例把 APFS 上同一物理目录的结论写死到全部平台。修后按实际卷是否把 `case` 映射到已存在的 `Case` 分支：不敏感卷必须拒绝第二次创建；敏感卷必须能创建两个名字，删除其中一个不影响另一个。Linux 与 macOS 都实际运行，不用 skip 或仅按 OS 名猜测文件系统行为。

### 差分连接的所有权

首次双线复跑均为 78 passed / 1 error，错误来自 3.x 的 `Too many pings`，不是结果断言。SDK 默认共享池在 close 后保留闲置 channel，并以 10 秒 keepalive 发送 ping；长时间执行独立重开场景后，回到 session 服务时复用了被拒绝的旧连接。

差分 fixture 和 ReopenPair 改为 `dedicated=True`，让测试拥有并立即关闭自己的连接；fixture 用 ExitStack 清理构造失败或 teardown 异常时的已建连接，ReopenPair 记录部分构造的客户端以便清理。新增断言核对真实 SDK 管理器中的专属连接身份，避免只验证参数字面量。业务代码、服务端 keepalive 与默认共享池未改；端到端套件继续覆盖普通 MilvusClient。

本轮另用两条 SDK 对仅 Rust 服务做默认共享连接探针：闲置 65 秒后直接调用旧 stub 的 GetVersion（不经过 SDK 重试）成功，再连续建立并关闭五个默认客户端，业务调用成功且共享 handler 相同。SDK 的 idle health check 可主动重建 channel，因此不以 channel 对象始终相同作为断言。此场景未复现原参照侧的 Too many pings；不据此断言所有长期会话或所有后端都不存在 ping 约束。

## Linux LOCK 与 strace

执行入口见 [tests/platform](../../tests/platform/README.md)。Rust 持锁→3.x 拒绝、3.x 持锁→Rust 拒绝均通过；对应 flock 被拒绝，POSIX record lock 的 lockf 仍可独立取得，停止持锁者后另一方重新打开成功。

strace 使用 `-ff -ttt -yy` 合并线程，分析器只统计成功调用，共 81 条。断言继承及新文件、schema/manifest tmp、原 WAL、目录祖先（含 partitions）在 replace 前同步，replace 后 collection 同步，data→delta unlink，及各自 unlink 前同步 WAL、unlink 后同步 wal/。分析器删除任一必要事件的反证均失败。证据覆盖指定的真实提交路径，不推断全部调用或实际掉电。

## 性能证据与未达目标

主三方表对应已提交版本 `06f6d66`，脚本 SHA256 为 `00947bbcd41dcdce36ef8ddea529309e11b60386b3411a7662bb6b39d0893d5a`；原始数据和库在仓库外。修后 native 构建另以相同冻结脚本进行三轮增量复测，不把主表冒充修后精确数据。

这是历史测量身份；当前脚本已继续修正，不能直接复算出同一值。源码恢复补丁及独立复算方法见[基准 README](../../benchmarks/baseline/README.md#历史测量脚本的指纹复核)，本轮实际恢复得到上述完整 SHA256。`b79c01cd` 仅标识当时 native 输入，后续修订不复用该构建标签；当前构建输入的两次摘要一致，工作日志不参与。

修后构建 `06f6d66+b79c01cd` 的 18 条结果已收齐，同一冻结脚本、两规模各三轮，召回仍全为 1、m2/reopen 为 N−1000 行、memory 为 N 行。百万行中位数：写入 54648 行/秒、nq=1 5.23 ms、nq=100 273.33 ms、过滤 count 6.01 ms、重开 1.11 秒、RSS 1222.4 MiB、持续写入检索 p99 10.26 ms。全部范围及与原主表的边界见 [0001](../decisions/0001-refactor-baseline.md#同步修复后-rust-增量复测)。

截至主表：单条 FLAT、写入、点查、删除及并发吞吐达到比较目标；批量检索、S1 过滤/计数、百万行冷启动与内存未全部达到。空表达式 count 与过滤 count 不能混为同一目标。百万行持续写入的 p99 同时列出实际写入速率与样本数，2.5.1/3.x 的样本分别约 83/22，经验 p99 接近最大值，不能视为高精度尾延迟估计。

这份主表是历史快照，后续列求值与稠密位图优化的测量结果见 [S1 性能专项](s1-performance.md)；该专项只覆盖其声明的场景，原表和历史未达结论保持原样。

一次独立新进程探针中，Rust 百万行服务从启动约 25 MiB、load 后约 1362 MiB、检索后约 1363 MiB，行数正确。这表明超额 RSS 在加载时已出现；恢复路径在原 DecodedData 尚存活时克隆列到 ChunkData，会形成额外峰值。活跃数据、重复主键索引和分配器保留各占多少仍须堆分析，本轮不据此宣称已定位全部内存原因。

性能不设 M2 硬门槛；未达到目标如实保留，由负责人决定后续优化顺序。本报告不将性能目标部分未达写成已达标，也不弱化 fsync、校验或召回换取数字。

## 本轮评审修复复核

- 原全表空引用条件导致 data-only 的首次索引失败，新稀疏形态单测及 Db 回归在修前失败、修后通过。同步共同入口按自身目录引用决定是否允许缺失；CreateIndex 与空代提交均核对，不把合法无提交 Flush 误判成新建目录的证据。
- 两线重新构建后的 Python/差分/互通分别为 232/80/60 passed；macOS/Linux Rust 全套、两种 feature 的 Clippy、fmt 与 diff 检查通过，Linux 敏感卷的两个名字分支实际执行。基准与 strace 分析器自检 8 passed。
- 新真实互通八条按两种读者分别固定 index-first、query-first、empty-flush、close-only；不以先写入制造 delta 目录规避回归，不删改原 data 文件，不伪造新目录来满足索引提交。
- 析构 ExitStack 仍保证客户端 close；清理某个 collection 失败可能中断后续 drop 的既有语义本轮不改。祖先类型受引用文件/schema 路径检查与提交操作约束，同族 create_dir_synced 的父路径递归重检、WAL 保留同步的 read_dir 前置校验均已核对，未引入全局文件系统重构。
- 2.5.1 独立 server/memory 分支以 2000 行现场重跑，行数为 2000、服务端 RSS 有效；参照 checkout 为 43d1257，121 个源文件仍逐字一致。此冒烟不替代主表三轮性能数据。
- 扫描模式为 `sync_inherited|inherited_synced|is_dir|symlink_metadata|data_files|delta_files`（core/相关参照存储实现）、`dedicated|get_stats|keepalive|ping`（差分及两 SDK）、基准 phase/suffix 与原始指纹；覆盖 schema-only、data-only、delta-only、双目录、引用缺失、错误类型、真正 I/O、两提交入口和正常关闭。
- 本轮未重跑完整性能矩阵：原始表及 native 输入身份均作为历史快照保留，当前脚本的预热说明、目录隔离与源码恢复补丁不反写历史数值。Windows 与真实断电仍未验证。
