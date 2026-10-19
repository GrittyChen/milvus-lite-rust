# 0017 A：显式分区实施验收

- 状态：0017 A 的实施与修复已于 2026-10-19 独立复审通过，见[修复与复审记录](0017-partition-review-fixes.md#独立复审收口)；本页仍为初轮 N7 历史验收，本记录随 A 收口本地提交，未推送、未发布。
- 日期：2026-10-19
- 合同：[0017](../decisions/0017-partitions.md)，继承 0007 / 0010 / 0011、0015、0016。
- 仓库基线：`a01310cbf668ba5676aa3f07d2534199f1caede5`，对照已发布 `0.2.0rc1`。
- 参照：milvus-lite v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`，保持只读；SDK 固定 2.6.17 / 3.0.2。
- 开发产物暂沿用 `0.2.0rc1` 版本字符串，只以源码及原生库指纹识别；不是新的发行候选，不表示覆盖既有 Release。

本文的构建身份、计数、性能和证据包对应初轮 N7 历史构建。N8 修复后的源码、产物身份、本轮重跑和未重跑边界统一列在[修复记录](0017-partition-review-fixes.md)，不将本页旧矩阵冒充新构建实测。

## 实施范围

接通 CreatePartition、DropPartition、HasPartition、ShowPartitions、GetPartitionStatistics，以及已有 Insert、Upsert、Delete、count、Get、Query、Search、Query iterator、range search 的分区参数。DescribeCollection 返回真实分区数；Show 仍只返回名称及集合级 load 百分比，不增加分区 ID、独立 load 状态或服务端游标。

partition key 留在 B 单独冻结；LoadPartitions / ReleasePartitions、alias/rename、BM25、真实 ANN、服务端 iterator V2 没有扩展。已发布版本的范围说明仍保留历史含义，不将开发实现写成已经发布的能力。

- 真实可见性组件复用既有 PkMap、不可变列和 COW 位图；先选分区再判版本。全集保持既有快路径，单分区共享本地位图，多分区组合稀疏遮挡；不扫描全库主键判版本，不复制向量，不缓存全部选择组合。
- default-only 状态保留旧表示；首次创建显式分区才移交原主键索引的桶存储。原列、PK 类型及已有活跃/冻结位置保留；移交失败或 panic 封锁该实例并保留 WAL，重开恢复。
- 数据源使用稳定 SourceId，文件映射使用 `(partition, relative_file)`；展示顺序与 ID 分离。运行时保留 segment 顺序，重开遵循输入 JSON 的分区键序；写出的 JSON 仍沿用已有排序编码。
- 多分区 Flush 在同一次 manifest 提交及内存发布中接入全部输出；合并只在单分区内进行，GC 阈值从全部当前分区计算。活跃删除、冻结删除及 committed-delete 分开维护。
- Create/Drop 的数据库独占许可覆盖检查、内部 Flush、文件准备、提交、发布；复用既有 flush 锁和状态锁，不增加锁层级。文件准备、rename/目录同步及清理不持状态锁，状态锁只用于检查及发布。Drop 转存 delta 与移除分区同次提交，replace 后同步不确定时发布新状态、延后清理并经重提交恢复。
- 沿用 manifest format 2、schema、Arrow WAL `_partition`、Parquet 与侧车布局，不引入共享格式。安全差异、未知 WAL 与过期序号保护的精确边界见 0017。

产品源码清单 SHA256：`f330730795ccc73e00c761004b1cccb79339fa1e1a848e689164a8e98b0d8396`。清单含三个 crate 的源码/协议/测试与构建输入，独立于后续验收文档状态，不以未提交的 Git HEAD 冒充构建输入。

## 构建身份

| 产物 | SHA256 |
| --- | --- |
| macOS arm64 完整矩阵所装 wheel | `e71827de7ed9b0a1b8f48842db900fde242c5a4292294f6a09a0b5ed700709fb` |
| macOS arm64 最后打包检查的 wheel | `3c490affcc83241f7faedd18b64d35886d78b74b144cb26e82056acee7790b3d` |
| 两份 macOS wheel 的原生库 | `650f06af8ba46621f24766297d5ba6f60bb818e9d037b493f9c5cc47f1bff5f3` |
| Linux aarch64 wheel | `be27d41a7bfcd387de7c4016dd693c64ec9ccd67ad0cfcbd048b317abc01b105` |
| Linux aarch64 原生库 | `7d617373b35c94749e64e547eaf39fd344c4fee814fb9a0da3b48aaef381e136` |
| 最后检查的 sdist | `a47fad7d0ce1380b4c71c1224e15800aa251c9d8645ac71cefc8d08571c9e861` |

完整矩阵使用第一份 wheel；随后只整理测试收尾、工具和文档，再次正常构建的原生库逐字节相同，未用版本字符串推断等价。两条 SDK 的完整矩阵加载同一原生库。macOS wheel / sdist 内容及二进制检查通过，最低 macOS 为 11.0；Linux wheel 的内容、ABI 标签及 readelf 检查通过，最高 GLIBC 要求为 2.28。

## 软件与平台证据

| 检查 | macOS arm64 | Linux aarch64 |
| --- | --- | --- |
| fmt / Clippy（含 fault-injection） | 通过，无告警 | 本轮执行 workspace / FI；Linux Clippy 未单列 |
| Rust workspace | 339 passed | 340 passed |
| core fault-injection 顶层目标 | 429 passed | 430 passed |
| SDK 3.0.2 Python 全套 | 433 passed | 433 passed |
| SDK 2.6.17 Python 全套 | 433 passed | 未跑 |
| SDK 3.0.2 差分全套 | 340 passed | 新分区差分 15 passed；未跑全套 |
| SDK 2.6.17 差分全套 | 340 passed | 未跑 |
| Arrow 15/25 存储互通 | 195 passed | 未另跑完整互通 |
| 打包及平台工具自检 | 170 passed | 实际 ELF / wheel 检查通过 |
| 既有基准工具自检 | 60 passed | 未另跑 |
| 调查及新基准守卫自检 | 17 passed | 未另跑 |

故障测试按每个 Cargo 顶层 test target 的最终汇总计数，避免受控子进程的两条输出被再次相加。若将整份日志所有 `test result` 直接相加，会得到 431 / 432；不将计数口径差异当作新增用例。历史记录保持原口径。

Linux 使用 manylinux_2_28 aarch64 / AlmaLinux 8.10 的独立源码副本、构建目录和命名卷，未将 macOS 绑定目录当 Linux 文件系统证据。macOS 两对 p1/P1、NFC/NFD 为同一实际目录并被拒绝重复登记；该 Linux 卷上两对身份不同、均可正常登记。结论仅适用于实测卷。容器日志及产物已归档，容器停止，卷保留。

这些是开发切片证据。Linux x86_64、Intel Mac、其它 CPython 组合及完整候选矩阵未在本片重建；下一次正式候选仍按 0013 重建及验收，不能用本记录宣称全平台发布完成。

## 主要鉴别路径

新增[差分](../../tests/differential/test_partitions.py)覆盖两种主键、活跃/segment/混合/重开/合并、重复与未知选择、nullable/ARRAY/动态过滤、Get 的重复与请求顺序、Query iterator、range、多查询输出、Upsert、全局 Delete，以及原始协议状态、输出字段和失败无写入。[额外的 V1 交叉对照](../../tests/compatibility_audit/partition_search_iteration.py)在两条 SDK 各执行 40 项，逐页核对 INT64/VARCHAR、混合/重开、全部/单/组合/未知分区、范围开关、完整条数及 nullable/ARRAY 实体；实际客户端类均为 SearchIterator。错误优先级覆盖 Insert / Upsert 的缺主键、向量长度、ARRAY 转换和未知目标分区。

[核心状态测试](../../crates/core/tests/partitions.rs)和 Db 内部用例固定：

- 活跃覆盖移走后回到已有 segment；Delete 未提交 / 已提交后跨分区重插两支；Drop 新 winner 恢复另一分区旧值；旧读取快照跨 Drop 仍有效。
- 首次启用分区时已经存在的 default-only 冻结代、冻结删除，分别经过旧/新 Flush 发布路径，再重开核对，不能只测试先建分区后写入。
- unknown Drop 在 Flush 前失败且 manifest/WAL 字节不变；Drop `_default` 先完成 Flush，再返回原错误，重开数据保留。
- 全分区删除 GC、真实后台单分区合并、关闭排空；一分区数据被全局较新值遮住时仍保留局部版本。
- 两种先后顺序的共享写许可 / 分区独占许可；八组检查、Flush、replace 前和发布后的确定事件交错；懒打开复用许可，无递归升级。另在实际 rename 后、目录 fsync 前暂停 Create/Drop，证明纯读仍完成且读取旧代；恢复后原子发布，旧快照不变。
- delta 转存 write/file-sync/dir-sync 故障，manifest replace 前后进程退出，以及提交后同步不确定的重提交；失败不复活已删除数据。
- 全分区索引侧车先整体预检再删除；后面的坏侧车不使前面文件先被删除；已 Drop、但 data/delta 被 snapshot 钉住的分区仍参与未钉住侧车清理，同名可重建，钉住数据字节不变。
- 未登记目录中的未知文件不归引擎整体所有：Create 拒绝后，另一个分区 Drop 及重开均不能顺带删除它。

四项独立源副本变异分别将选择改成先看全集、遗漏 delta 转存引用、将 namespace 变更改为共享许可、把磁盘提交移回状态锁；对应测试均明确失败，非编译失败。反证不改工作区，不复用产品构建目录。

## 真值、互通与回退

固定[组件真值](../../tests/compatibility_audit/fixtures/partition_visibility_v321.json)含 14 条直接引擎轨迹的 170 个状态，另有两条原始 gRPC 轨迹；Arrow 15.0.0 / 25.0.1 的真实参照 `--verify` 均通过。真实组件逐项物化核对 1870 项当前读取及 1870 项旧快照；历史隔离模型的 1716 次旧快照计数不代替真实组件。

[新互通用例](../../tests/storage_interop/test_partitions.py)覆盖 Rust、Arrow 15、Arrow 25 三个写入方，WAL / Flush / 实际 compaction，分别复制后由三种读者打开，核对 `_partition` 编码及 Parquet 归属；另覆盖全局 delta 转存及 snapshot 文件字节不变。互通父进程为 SDK 3.0.2，两条 SDK 的请求兼容由各自完整 Python/差分矩阵提供。

[产品边界工具](../../tests/compatibility_audit/partition_product_boundaries.py)在两条 SDK 的 macOS 及 SDK 3.0.2 的 Linux 上验证危险名、链接、超长失败不登记、既有危险 manifest 在清理前拒绝且原文件不变，同库健康 collection 仍可读取。真实目录身份和原始错误保留在仓库外。

[回退工具](../../tests/compatibility_audit/partition_rollback.py)以本片原生库分别产生 WAL / segment 种子，再由已发布 0.2.0rc1 复制后打开：非默认分区明确 UNIMPLEMENTED、受保护文件完全不变；健康 collection 可读；最后一个非默认分区 Drop 后仅默认分区可读，转存删除继续生效。未重复 0015 的 rc1–rc3 类型矩阵，不据此扩大旧版本能力。

## 固定统计

全部为未干预模式；三组身份集合、重复和缺失检查保持开启。

| 样本 | 0016 B | 本片 | 逐项增益 / 退回 |
| --- | --- | --- | --- |
| 参照固定 760 条 | 547 passed | 570 passed | 23 / 0 |
| 上游固定 90 条 | 75 passed | 82 passed | 7 / 0 |
| 调用探针固定 49 项 | 32 成功 | 33 成功 | 1 / 0 |

760 条其余为 185 failure、4 error、1 skipped；失败/错误仍为已登记范围外 UNIMPLEMENTED，skip 仍是原 nullable 向量。原 CreatePartition 首阻断 23 条中有一条推进到 CreateAlias 后继续失败，连同 statistics 共净增 23；不能把接口新增数或诊断首阻断数等同于转绿承诺。调用探针只判成功，不比较结果，不作为存储验收。

## 性能与资源

全部 105 条顺序测量完成：四方三轮、10 万 / 百万行、S1 / batch / 恢复共 72 条；默认及分区模式 15 条；JSON raw / SDK 18 条。软件验收、编译及容器退出后才打开测量门槛，没有并行其它测试或基准。每项逐次核对结果，原生库、脚本、逻辑输入及真值指纹均配对核验。

以下为三轮 p50 的中位数；恢复为三次独立服务启动的中位数。逐轮值及范围保留在证据 JSON。OpenMP / BLAS 设为 1，Rust 保留宿主默认计算池；不将这些延迟数字解释成相同线程数下的执行器效率。

| 项目 | 行数 | rc1 | 当前 | 3.x | 2.5.1 |
| --- | --- | --- | --- | --- | --- |
| query_count (ms) | 100000 | 0.390 | 0.410 | 1661.670 | 0.400 |
| query_filter_limit100 (ms) | 100000 | 0.650 | 0.570 | 87.940 | 0.620 |
| search_filter_integer (ms) | 100000 | 0.830 | 0.780 | 39.370 | 0.840 |
| search_filter_string (ms) | 100000 | 0.720 | 0.730 | 37.350 | 0.900 |
| query_count (ms) | 1000000 | 1.390 | 1.420 | 17684.320 | 2.610 |
| query_filter_limit100 (ms) | 1000000 | 2.770 | 2.810 | 985.860 | 3.450 |
| search_filter_integer (ms) | 1000000 | 4.340 | 4.380 | 383.160 | 23.010 |
| search_filter_string (ms) | 1000000 | 2.970 | 3.100 | 359.570 | 8.910 |
| search_nq1_top10 (ms) | 100000 | 1.470 | 1.490 | 50.530 | 2.980 |
| search_nq100_top10 (ms) | 100000 | 35.450 | 34.870 | 1255.570 | 25.410 |
| search_nq1_top10 (ms) | 1000000 | 5.240 | 5.170 | 488.400 | 23.540 |
| search_nq100_top10 (ms) | 1000000 | 257.460 | 257.970 | 13920.410 | 227.060 |
| ready_s (秒) | 100000 | 0.216 | 0.216 | 0.752 | 1.300 |
| rss_after_load_mib (MiB) | 100000 | 118.500 | 120.800 | 419.200 | 180.600 |
| rss_after_search_mib (MiB) | 100000 | 118.800 | 121.200 | 507.300 | 183.000 |
| ready_s (秒) | 1000000 | 1.510 | 1.523 | 1.780 | 7.833 |
| rss_after_load_mib (MiB) | 1000000 | 852.000 | 849.900 | 1681.600 | 798.600 |
| rss_after_search_mib (MiB) | 1000000 | 852.700 | 850.600 | 1989.700 | 801.900 |

默认路径另以 10 万行二维输入、32 查询批检索，以及每批 5000 行的普通 / JSON / ARRAY 写入和 Flush 核对；下表为 ms，不与高维专项混为同一输入：

| 默认路径项目（ms） | rc1 | 当前 | 3.x |
| --- | --- | --- | --- |
| all/query | 34.276 | 34.204 | 756.043 |
| all/batch-search | 5.265 | 5.229 | 64.686 |
| plain/insert | 3.927 | 3.966 | 25.674 |
| plain/flush | 15.655 | 17.074 | 50.843 |
| json/insert | 180.318 | 175.708 | 132.610 |
| json/flush | 93.014 | 90.284 | 80.189 |
| array/insert | 15.600 | 15.382 | 36.949 |
| array/flush | 22.725 | 25.647 | 63.120 |
| json-raw | 282.718 | 281.411 | 190.909 |
| json-sdk | 447.584 | 447.552 | 361.311 |

16 个分区的读取对照（ms）：

| 选择 | Query 当前 / 3.x | 32 查询 Search 当前 / 3.x |
| --- | --- | --- |
| 全集 | 31.339 / 752.268 | 5.199 / 71.216 |
| 单分区 | 0.876 / 10.187 | 1.754 / 8.306 |
| 偶数分区子集 | 15.340 / 375.826 | 3.468 / 33.574 |

相对 rc1，S1 三轮中位数比值为 0.877–1.051；批检索为 0.984–1.014；恢复就绪时间为 1.001–1.009，恢复 RSS 约 0.998–1.020。默认 JSON / ARRAY 写入及独立 JSON raw / SDK 没有重现此前的写入退化。

普通列 Flush 中位数为 17.074 / 15.655 ms，ARRAY 为 25.647 / 22.725 ms；三轮范围分别为当前 [15.644, 18.190] 对 rc1 [15.487, 18.007]、当前 [21.653, 26.121] 对 rc1 [22.598, 24.402]。方向未逐轮保持、区间重叠，因此不判为稳定回归，也不声称 Flush 每项都完全持平。

百万行 nq=100 仍慢于 2.5.1，独立 JSON 写入仍慢于 3.x；随机百万 128 维独立服务 RSS 也仍高于 2.5.1。本片相对 rc1 没有加重这些既有差距，不将其包装成 0001 全部性能/内存目标已完成。

资源对照另列：实际组件 26 个基本负载全部核对；新增双向遮挡负载达到 256 分区的 65280 条非空关系，合计 35 组。对同一键的增量更新仅修改受影响分区的关联边，不逐次重算全部分区对。百万 128 维恒值种子的三次冷重开，移交既有 PK 索引后的三轮时点 RSS 增量为 0.359 / 0.359 / 0.375 MiB；不复制列/向量。这是增量资源证据，不能用恒值向量或时点 RSS 代替随机数据、独立服务的总体恢复或峰值内存目标。

## 调查中修正的边界

- 最后核对 0017 的 I/O 合同时，将 Create/Drop 的磁盘提交从状态锁内移至锁外，仅在短状态锁内发布。此前 N6 的性能记录因此作废保留，不进入本片最终性能表；没有沿用旧二进制的完整矩阵冒充新构建。
- 全零本地位图省略后，补保留已知源长度；`is_live` 继续区分无效行与未知/越界位置，没有以修改断言绕过。
- 全分区文件系统检测归入既有启动来源入口，测试替身与真实来源使用同一入口；不在替身之外再执行环境判定。
- 未登记目录只清理可识别输出，保留未知文件；快照 data/delta 钉住但不钉住后建索引时，DropIndex 不漏掉已删除的分区。
- 工具的 JSON 读回不使用 HybridExtraList 的自定义整体相等运算，改为独立规范文本，保留 bool/int/float 区别；三实现共同触发的工具初次失败保留，不算产品失败或性能结果。

初次失败、负例、旧构建和最后构建分目录保留；不删除原日志、不自动重试、不覆盖参照 fixture。源码、产物、环境、输入、命令、JUnit、工具和证据清单保存在仓库外；本记录与证据包按最后构建逐项配对。A 的实施与修复现已独立复审通过；提交及版本仍需单独决定，不自动进入 B 或发布。

证据清单 SHA256：`4f9eb0faf2883c7d5411758cdb9ad2d71cf9d40d3098c665520dc796ee866185`；证据包 SHA256：`7dc60f04de4447043ebe974961b50537538ec7b85b38dc57f1285b6673850b80`。证据包仅含文本结果、源清单和实际工具，不含 `.db`、构建缓存；仓库中不提交原始日志或个人路径。
