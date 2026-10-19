# 0017 分区设计补充调查

- 状态：设计阶段及本记录原调查工具于 2026-10-18 独立评审通过；不是 Rust 分区验收
- 日期：2026-10-17
- 仓库基线：`a01310cbf668ba5676aa3f07d2534199f1caede5`；没有修改产品、版本或参照代码，没有提交或推送
- 参照：v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`
- 对应：[0017 设计草案](../decisions/0017-partitions.md)；前序：[0016 调查](0016-partition-investigation.md)
- 后续：[可见性组件对照](0017-visibility-component.md) 单独记录新增真值、资源与证据边界，未沿用本记录的评审结论

## 本轮实跑

| 输入 | 环境 | 结果与证据边界 |
| --- | --- | --- |
| 原 26 组引擎 + 2 组 key + 49 项 RPC + 11 项路径 | macOS arm64、Arrow 15.0.0 / 25.0.1、SDK 3.0.2 | 两环境各 88 组与原 fixture 逐项一致；包含实际合并、受控 WAL 退出及 snapshot |
| 原 49 项 RPC | macOS arm64、Arrow 25.0.1、SDK 2.6.17 | --verify 通过，与原 fixture 一致 |
| 原 26 组引擎 + 2 组 key + 49 项 RPC | Linux aarch64、Arrow 25.0.1、SDK 3.0.2 | 77 组 --verify 通过；旧 paths 不在此列，大小写事实改用新工具单列 |
| 新 17 组 raw gRPC / 文件系统边界 | macOS arm64：Arrow 15/25 + SDK 3.0.2，Arrow 25 + SDK 2.6.17 | 三环境各独立采集后再用新目录 --verify；全部符合 mac-alias fixture，三份 cases 相同 |
| 新 17 组 raw gRPC / 文件系统边界 | Linux aarch64：Arrow 25 + SDK 3.0.2 | 独立采集及新目录 --verify 均符合 linux-distinct fixture |
| 全局 delta 转存数据原型（control / anchor 两支） | macOS Arrow 15/25、Linux Arrow 25，均 SDK 3.0.2 | 三环境控制组重开复活 id=2；转存组不复活；实际合并为一个 p2 数据文件并再次重开后仍符合预期 |
| 工具自身检查 | Python 3.12 | 13 tests 通过，含破坏性路径约束及归一化边界反例 |

新工具先经 SDK 建 schema/collection，随后 Create / Has / Show / Insert / Flush / Query / Search / Drop / LoadCollection 都使用原始 proto。它证明危险名称确实能穿过服务端，不只引用直接引擎或 SDK 预校验结果。每组独立服务和数据目录，不复用旧数据库。

Linux 使用 manylinux_2_28 aarch64 / AlmaLinux 8.10 容器；数据在新命名卷，源码只读挂载，不把 macOS 绑定目录当 Linux 文件系统证据。实测 p1/P1、NFC/NFD 的设备/inode 身份不同；macOS 本轮目录两对都相同。结论依赖实际卷的别名行为，不能扩大为所有 macOS 或所有 Linux 文件系统。

旧工具的受控退出子进程固定引用其所在仓库的 reference。macOS 15/25 因环境对应已冻结的参照副本，使用同字节的候选归档工具，核对 SHA256 与本仓一致；SDK 2.6 与 Linux 使用本仓工具及相匹配的 reference 路径。两份参照 HEAD 均固定，结束时检查无本地改动。没有修改参照代码来得到结果。

## 新确认的事实

| 情形 | 真实服务结果 |
| --- | --- |
| 绝对路径分区 | Create / Insert / Flush 成功，Parquet 位于 collection 之外；路径仍在本案例可丢弃根目录内 |
| 空名、`.`、`..` | Create 多数成功；Drop 空名/`.` 可删除默认分区文件，重开并加载后只剩空集合；Drop `..` 可删去 collection，之后 LoadCollection 为 100/4 |
| NUL、256 个 ASCII 字符 | Create code 1，但 Has=true、manifest 已登记。超长 errno 在 macOS 为 63、Linux 为 36 |
| 分隔符、反斜杠、Unicode、`_all`、冒号 | 参照接受；其是否属于建议拒绝边界见 0017，不以“都应禁止”代替设计 |
| p1 / P1 与 NFC / NFD | macOS 两对物理同目录，Drop 第二个后第一分区引用文件不再存在，重开并 LoadCollection 后只剩默认 canary；Linux 两对各自独立，第一分区文件及行均保留 |
| 预置符号链接分区 | raw Insert/Flush 写入链接目标；Drop 删除目标内的自有标记文件，随后报 Errno 20。目标在本案例根内，未触碰用户其它目录 |
| 先创建/写入 z，再 a | Show 总为 `_default,a,z`；运行中 Query 的 tag 为 `canary,z,a`，重开并加载后为 `canary,a,z`；等距离 Search 的 id 从 `[1,2,10]` 变为 `[2,1,10]` |
| 交错 Flush z、a、z | 运行中 Query 为 `canary,z,a,z`，重开为 `canary,a,z,z`；等距 id 从 `[1,2,3,10]` 变为 `[2,1,3,10]`。请求反向列举分区不改变该源序 |

顺序由参照 [Collection 的 segment 缓存](../../references/milvus-lite/milvus_lite/engine/collection.py)、[manifest 的遍历和保存](../../references/milvus-lite/milvus_lite/storage/manifest.py) 共同决定：运行中新增缓存项追加，重开从 JSON 的键序重建，而保存使用 sort_keys。仅保留“分区插入顺序”仍不能解释交错 Flush，0017 因此要求独立 source_order。

复核原调查也再次确认：同键的活跃 MemTable 全局覆盖、选定 segment 分区后再去重、删除全局作用、Drop 丢失 delta 后重开复活、default Drop 的先 Flush 副作用和 key 自动 ID 的已持久化前缀覆盖缺陷。本轮没有把这些当作 Rust 已支持的能力。

## delta 转存原型的证明范围

[原型工具](../../tests/compatibility_audit/partition_drop_anchor.py) 先在 p2 写 id=2 并 Flush，再指定 p1 删除并 Flush，关闭后复制自己的种子目录。control 走参照 Drop p1，重开见 id=2；anchor 仅对数据副本操作：把 p1 的 delta **逐字节复制**到 `_default`，使用参照 Manifest API 同次保存“新增默认引用 + 移除 p1”，实际删除自己的旧 p1 目录，然后真实重开、写入更多 p2 行、等实际合并、再重开。anchor 始终不见 id=2，最后 p2 只有一个数据文件。

依据是 delta Parquet 只有主键和 `_seq`，不带 `_partition`；DeltaIndex 从全部分区重建全局删除。原型未增加 schema / manifest 字段、未改 Parquet 内容、未推进 current_seq。Arrow 15 与 25 原本写出的字节不同，但**各自转存前后字节相同**，不声称跨 Arrow 的文件字节相同。

它证明同格式转存可被 3.x 正常读回、合并，支持设计选择；没有验证碰撞、目录 fsync 崩溃窗口、并发读写、旧 snapshot 或 Rust 实现。那些列入 0017 的正式验收，不能拿本原型替代。

## 工具、fixture 与归一化

新 [boundary 工具](../../tests/compatibility_audit/partition_boundaries.py) 每项独立子进程，固定 commit / 模块来源 / Arrow / SDK，工作根必须是仓库及参照外的新目录。破坏性路径先 resolve，必须留在该项自己的根内；符号链接目标也受此约束。绝对路径和 `..` 的试验只影响工具自己创建的可丢弃目录。无联网、pip、默认测试接入或已有数据目录输入。

[boundary fixture](../../tests/compatibility_audit/fixtures/partition_boundaries_v321.json) 有两份明确的文件系统变体，不在验证失败后自动切换。每份 17 组，保存状态、错误、manifest 摘要、外部文件位置、物理别名、Drop 后文件存活、关闭/重开/加载及输出顺序。根路径替换为 `<work>`；`..` Drop 中 Errno 2 指向 data/delta/indexes 哪个子项取决于文件系统目录枚举，仅这一错误的子项统一为 `<removed collection entry>`，原文留在日志。其它状态、输出、错误不放宽，超长 errno 差异保留于平台变体。fixture 是事实快照，不是批准照抄这些缺陷。

| 输入 | SHA256 |
| --- | --- |
| partition_truth.py（旧工具，本轮未修改） | `3eb0dbf9499e1f9272cd55dffb50a16704ce16532ef73c55e819794ec5cefc08` |
| partition_boundaries.py | `28ce8421121099760f7c538d60f0ae352700f97b04a6d965d781e21f52ce4cd4` |
| partition_drop_anchor.py | `124b280a80efb065167914a44a39d3aa3281c62d68164c6fb8b76ad38db37510` |
| partition_boundaries_v321.json | `19b7d498381e323312e69146143031b37459cf3b3f62f78af3ade7fe02bba1b9` |

复现步骤与环境前提见 [工具 README](../../tests/compatibility_audit/README.md#0017-分区边界补测)。上游测试仍按固定 commit 引用，没有复制进仓；只加入本仓调查脚本和有限的合成输入真值。

## 保留的失败、原始记录与未验证项

外部工作目录保留 legacy25/legacy15 的 engine、key、rpc、paths 日志，legacy26-rpc，mac25/mac15/mac26/linux25 的 capture 与 verify，anchor25/anchor15/linux-anchor25；最终采集及验证均来自上表工具哈希。更早 16 组预跑没有重开后的 LoadCollection，只证明重开前事实；保留日志，但不用于证明重开后数据/顺序。最终 17 组明确先记录 released 的 101，再 LoadCollection 并检查数据，未用 release 错误伪装成功读取。

Linux 最初离线环境准备因缺 Arrow 25 wheel 失败，日志保留；显式补齐固定依赖后独立执行调查。准备中的下载不属于离线调查工具执行。报告、运行环境、命令、容器/卷身份及结果在仓库外保留，不把大日志或本机路径写进仓库。

未跑 Linux Arrow 15、Linux SDK 2.6、x86_64/Intel Mac；这轮不是实现或发行验收。没有重跑 Rust/Python 产品全套、正式故障注入、双向 Rust 分区互通、回退矩阵、性能或三组统计，因为没有新增产品分区代码。文档只制定这些后续门槛，不引用历史通过数填补新能力证据。
