# 0017 可见性组件对照

- 状态：历史隔离组件记录，生成时未接通产品分区；后续 A 实施与修复已独立复审通过，见[收口记录](0017-partition-review-fixes.md#独立复审收口)
- 日期：2026-10-18；仓库基线 `a01310cbf668ba5676aa3f07d2534199f1caede5`
- 参照：milvus-lite v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`
- 前置：[0017 设计](../decisions/0017-partitions.md) 的设计阶段及原调查工具已评审通过；本记录不把该结论扩展到新增组件

## 评审建议的核实与处理

独立复核 [MemTable.apply_insert](../../references/milvus-lite/milvus_lite/storage/memtable.py) 的 delete_index.pop，以及 [DeltaIndex](../../references/milvus-lite/milvus_lite/storage/delta_index.py) 的独立保留；直接引擎及原始 gRPC 的固定结果如下，两条 SDK 均一致：

| 输入 | p1 单分区在重插后 | 全集 |
| --- | --- | --- |
| p1 旧值 Flush → Delete 不 Flush → p2 同键重插 | 旧值重新可见，之后 Flush / 合并 / 重开仍可见 | p2 新值 |
| p1 旧值 Flush → Delete 已 Flush → p2 同键重插 | 旧值仍隐藏，之后 Flush / 合并 / 重开不复活 | p2 新值 |

没有改变全局删除语义，也没有扩展 partition key。活跃 tombstone、冻结待提交删除和 committed-delete 必须分开，不能把读取状态实现为永不回退的统一删除上限。

核对 [0007 锁序](../decisions/0007-m2-storage-engine.md#取锁顺序) 与 [Db create/drop](../../crates/core/src/db.rs) 后，0017 已明确数据库**独占许可**覆盖 Create/Drop 的检查、Flush、文件准备、提交及发布，不从共享升级；内部 Flush 不再次递归取共享许可。`flush_lock` 只串行维护，不能挡住普通 Insert/Delete。未来接线用确定事件核验两个方向的交错及各退出路径；本轮没有伪称已测试尚未存在的分区 RPC 许可实现。

## 输入、组件与覆盖范围

[真值工具](../../tests/compatibility_audit/partition_visibility_truth.py) 生成 [固定 fixture](../../tests/compatibility_audit/fixtures/partition_visibility_v321.json)：14 条直接引擎轨迹，共 170 个状态，每个状态 11 组 adapter 可达的选择参数；另有 Delete 未 Flush / 已 Flush 的两组原始 RPC 轨迹。INT64/VARCHAR 各覆盖活跃覆盖、segment 遮挡、活跃 winner 被其它分区覆盖后回退旧 segment、Drop 最新分区、确定种子的 48 步混合写删、实际合并和重开。选择含默认分区、单/多分区、全集、重复名、未知名及混合未知名。

物理输入独立从全部 segment.table、MemTable 主键位置及原 batch、active delete_index 和 committed DeltaIndex 取得，不用期望 Query 结果反推有效候选。期望由真实 Query 单独产生；不替换参照函数、不修改参照代码。模型输入中的 MemTable 候选已按参照全局主键表排除被覆盖槽位，这是输入边界；模型不是 WAL 回放或 MemTable 实现。

[Rust example](../../crates/core/examples/partition_visibility_probe.rs) 在写侧更新候选和位图；全集/单分区共享 COW 位图，子集按实际存在的局部位与稀疏遮挡字组合，读取不遍历主键判版本。SourceId 与来源顺序独立；候选移除能回退同分区旧行，active 删除移除会重算相关位。每步持有上一阶段的所有选择快照，更新后核对其内容不变。

第一轮对照暴露了直接引擎 `partition_names=[]` 的内部路径差异：segment 被视为全选，MemTable 却不贡献候选；gRPC adapter 将空 repeated 字段转成 `None`，因此客户端不走此路线。没有改掉原始观察或把错误结果当作公开语义：fixture 每步另存 direct_empty_diagnostic，--verify 仍逐项核对；组件 queries 固定为真实 adapter 可达的 `None` / 非空列表，原始 RPC 的空列表也实际核对。首次模型失败日志及旧输入保留在仓库外，不能称作同一输入重试后通过。

## 实跑结果

| 检查 | 本轮结果 |
| --- | --- |
| 真值 --verify | macOS arm64：Arrow 15/25 + SDK 3.0.2、Arrow 25 + SDK 2.6.17；Linux aarch64：Arrow 25 + SDK 3.0.2；四环境各 16 组、170 状态，完整 fixture 相同 |
| Rust 组件对照 | macOS 原 example、Linux 同字节源码独立编译的模型，各 1870 次当前读取、1716 次旧快照核验通过 |
| Rust 组件单测 | macOS / Linux 各 4 passed：活跃/冻结/已提交删除分离、冻结状态提交/失败输入、Drop 后回退与旧快照、同号来源顺序变化 |
| 变异反证 | 独立外部项目的原模型通过；3 个错误模型全部失败：活跃删除只增不减、忽略 committed-delete、忽略子集遮挡；失败分别落在对应固定输入，不以编译失败计数 |
| 隔离资源诊断 | macOS / Linux 各 26 项，所有读取计数通过；binary 身份见下表 |
| 代码与既有 Rust 回归 | fmt、workspace Clippy `-D warnings`；workspace 313 passed；core fault-injection 的 397 个顶层用例通过，另有 2 次受控子进程内验证，日志共 399 次通过；组件的 4 项另列 |
| 工具自检 | 14 passed，含两条删除真值的结构/结果断言；git diff --check 通过 |

Linux 使用原调查的 aarch64 manylinux_2_28 / AlmaLinux 8.10 容器及命名卷；参照源码只读。模型使用相同源文件、Rust 1.98.1 及离线已缓存的 serde/serde_json 在独立小项目编译，**不是 Linux workspace / wheel 验收**；Cargo.lock 与日志单独归档。两份参照 checkout 结束时 HEAD 固定且无本地改动。

冻结代测试是受控状态输入：验证新活跃插入不会清除别代删除，发布失败保留、成功移交 committed；没有模拟磁盘故障或真实并发 Flush。故障套件属于既有存储回归，不能替代未来新分区提交点的故障矩阵。顶层数量用 --list 的身份复核，避免把嵌套子进程输出误当额外独立用例。

## 资源结果与工程边界

命令和方法见 [资源工具 README](../../benchmarks/partition-visibility/README.md)。RSS 表示 loaded 减 empty 的同步点差额，包含索引、行式调查载体及分配器保留量；读取时间是 100 次“位图快照 + count”的平均值，单轮诊断，没有向量和 RPC。下表均为 INT64，完整 26 项还包含 VARCHAR 和其它规模。

| 分区 / 物理行 / 重叠 | 关系 / 非零遮挡字 | macOS RSS 增量 MiB / 读取 µs | Linux RSS 增量 MiB / 读取 µs |
| --- | --- | --- | --- |
| 16 / 65536 / 全重复 | 120 / 7680 | 7.83 / 4.5 | 5.95 / 4.4 |
| 256 / 65536 / 不重复 | 0 / 0 | 25.59 / 30.3 | 11.34 / 30.1 |
| 256 / 65536 / 全重复 | 32640 / 130560 | 21.28 / 242.1 | 14.81 / 238.1 |
| 256 / 65536 / 只共享一个键 | 32640 / 32640 | 36.52 / 212.0 | 19.88 / 211.9 |
| 16 / 1048576 / 不重复 | 0 / 0 | 330.97 / 5.0 | 211.01 / 5.5 |
| 16 / 1048576 / 全重复 | 120 / 122880 | 130.83 / 50.6 | 94.51 / 51.5 |

第一版模型每键各自分配候选 HashSet，并缓存另一份 winner；百万行无重复键的 macOS 增量约 653 MiB，不能据此通过资源门槛。已改为单候选内联，仅多候选扩展数组，移除重复 winner 缓存；最终同项约 331 MiB。初始和最终数据均保留；这不是正式轮换基准，不作产品性能声明。

位图机制符合两条结构约束：无跨分区重复键没有成对关系；子集遍历实际存在的关系，不为不存在的分区对做空查询。但 P 个分区共享一个键也会产生 P(P−1)/2 条关系，不能把“稀疏”说成普遍 O(P) 空间。高重复场景构建仍有成本：256 分区全重复的首次载入约 0.7 s；16 分区百万行全重复约 1.1 s。它们包括整帧输入建模，不是增量 Insert 延迟。

**工程门槛仍需保留。** 本模型按发生变化的源输入更新索引，对受影响主键重算候选与关系；不应把整帧替换、每键全关系重算或行式载体直接搬入产品。下一步先将候选变更收敛到既有列式源与 PkMap，按变化分区更新相关遮挡边，并测实际更新成本、快照 COW 峰值和百万行整库 RSS，再接显式分区 RPC。不能把这里的模型 RSS 加上向量大小就宣称正式引擎已达 0001 的约 0.8 GB 目标，也不能用有限的 256 分区样本证明任意分区数无资源问题。

## 身份、保留与未执行项

| 输入/产物 | SHA256 |
| --- | --- |
| Rust 模型源 | `8fc103df72ecbeb99c8b9ac83ef83f18613e59bb4412820e9d8c7c356bdc7a67` |
| 真值工具 | `b80c86b84fb93848037e20593e8799afe4d2a2773d581cbb2cbac6285219c975` |
| 可见性 fixture | `ca5de3630a63e3860660d3e26c563dcca705d36dd0f1a12f81e799cc3313ba44` |
| 资源驱动 | `8556eafc55e4a37a234f4dc5614d0ca816a3280696367c90989b4af633b39028` |
| macOS example | `9acb2cd2850f070d1b87c4b498908976ebd9171c8bd31c569bf5cedbdc29357e` |
| Linux 独立模型 | `d83ab1072714fdc29b08ca5096be561cb6883864c61dfed74b042719f2a9f441` |

仓库外保留四环境 accept 目录及完整 fixture/metadata、model-accepted-input 与 linux-model-accept、component-tests-final、workspace、fault-injection、resources-final/linux-resources、mutations-final 的源码/构建/实际失败日志。初次 Python 候选采集误读已剥离的 `_partition`、直接引擎空列表模型失败、两次 Clippy 诊断、Docker 未启动的环境失败也原样保留，没有计入最终成功证据。

未跑产品 Python/差分/互通、分区回退、三组统计、正式性能轮换、分区故障矩阵、Intel Mac/x86_64；未新增产品 module/RPC、依赖、格式或锁，版本与发行物不变。新工作未提交或推送；partition key 继续单独冻结。
