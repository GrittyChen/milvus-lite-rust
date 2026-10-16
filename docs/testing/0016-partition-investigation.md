# 分区收益与参照语义调查

- 状态：调查事实归档；未纳入 0016 A/B 的独立评审，不代表分区设计定案或实现获准
- 日期：2026-10-16；调查对象为只读参照 v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`
- Rust 基线：0015 B3，原生库 SHA256 `283da7afb42a0c9be412f1c3eaba049186e05fd15ed93a09b337d47f7507e7b8`
- 结论：先按 [0016](../decisions/0016-query-iteration-and-range-search.md) 做 Query iterator，再做 range search；分区需独立确认缺陷处理和存储设计。本轮没有加入分区产品能力

## 收益估算

固定 [62 条分区样本](../../tests/compatibility_audit/manifests/partitions.txt) 来自 760 条原始样本的 B3 首阻断：partition key 38、CreatePartition 23、GetPartitionStatistics 1。清单只保存身份，不复制上游测试。

| 运行 | 参照 | Rust B3 |
| --- | --- | --- |
| 原始输入 | 62 passed | 62 failed |
| 仅移除客户端 is_partition_key 声明 | 58 passed / 4 failed | 35 passed / 27 failed |

按逐条身份配对，**34 条干预后两端通过**，4 条参照不再通过，24 条无干预。不是 35 条收益，更不是承诺补齐分区后 62 条转绿。4 条参照失败中，3 条检查 Describe 中的 partition key 属性；另一条检查 key 集合禁止手建分区，移除属性后参照允许，而 Rust 的 UNIMPLEMENTED 被宽泛的预期异常捕获，单边通过不能计入收益。Rust 其余 27 条仍有 23 CreatePartition、1 statistics 及 3 属性检查失败。

观察钩子只记录 API、阶段、partition 参数、返回/异常，不修改数据和断言；原始运行、干预运行分开保存。初次参照运行因解释器缺 pytest-timeout 收集失败，已保留失败日志；上表使用补齐依赖的独立成功运行，不把收集失败当产品结果。

## 真值矩阵

[调查脚本](../../tests/compatibility_audit/partition_truth.py) 只在新建的仓库外目录起本机服务或直接调用参照引擎；固定源码和模块来源，正常退出、受控 WAL 退出、Flush、实际 compaction、重开与 snapshot 分别覆盖。捕获 [fixture](../../tests/compatibility_audit/fixtures/partitions_v321.json)；大日志与物理数据目录不入仓。

| 范围 | 数量 | 复核 |
| --- | --- | --- |
| 直接引擎生命周期 | 26 组 | Arrow 15.0.0 / 25.0.1 的全部结果相同 |
| 路径与文件系统别名 | 11 项 | 两条 Arrow 结果相同；大小写别名为本机文件系统证据 |
| partition key 路由及分桶退出 | 2 项 | 两条 Arrow 结果相同 |
| 原始 RPC 与 SDK | 49 项 | pymilvus 2.6.17 / 3.0.2 的全部结果相同 |

以上是分区调查，不是 Rust 分区验收。直接引擎的危险名称没有逐一经 raw gRPC 重跑；服务端 CreatePartition 将名称原样传入引擎的代码链已检查，二者证据类型不混称。

## 确认的边界与缺陷

| 行为 | 实测结果 | 对后续设计的影响 |
| --- | --- | --- |
| 同一主键先写 p1 后写 p2，均在 MemTable | p1 无行；全集只见 p2 新值 | MemTable 按主键全局覆盖 |
| p1 旧值先成 segment，再写 p2 | p1 查询仍见旧值；全集只见新值；分区计数之和可大于总数 | 去重发生在选定分区的候选内，不能只给全局 latest_data 加分区筛选 |
| 多 segment 实际合并、重开 | 旧值在 p1 仍可见 | compaction 不能回收仅在全集被遮住、分区仍可见的行 |
| 对仅在 p2 的主键指定 p1 删除 | 返回删除 1，全集也被隐藏 | 本地与已提交删除按主键全局生效，并非分区隔离 |
| 上述数据已有 segment，删除落 p1，随后 Drop p1 | 活跃缓存中仍隐藏，重开后 p2 行复活 | DeltaIndex 缓存和文件生命周期不一致；不能静默复制或修正 |
| Drop _default / Drop unknown | 前者先 flush 待写数据再报 code 6；后者报错前不 flush | 失败路径也有可观察的写入副作用 |
| snapshot 钉住文件后 drop / 同名重建 / restore | 恢复旧数据与 nullable ARRAY | 名称复用必须考虑钉住文件 |
| 空、`.`、`..`、相对越界、绝对路径、分隔符等名称 | 多数被接受；绝对路径 Flush 确认写出 collection 之外 | 目录安全缺陷；实测路径全部限制在可丢弃调查根目录内 |
| 含 NUL 的名称 | 建目录失败，但 manifest 已登记 | 错误发生于持久化登记之后 |
| p1 / P1 在大小写不敏感文件系统 | 指向同一目录；Drop P1 删除 p1 的文件；缓存仍可读，重开丢行 | 文件系统别名可造成数据丢失，不能只检查字符串不同 |
| partition key | 固定 16 桶，MD5(str(value)) mod 16；包含 _default 共 17 个名称 | NULL / 默认值及实际路由须冻结，不能依赖声明 num_partitions |
| auto_id 分桶后仅首桶提交就进程退出 | 重开后重新分配 ID，覆盖已有 ID；累计 6 条实际只剩 5 条 | 再次证明 [0002 约束 4](../decisions/0002-data-dir-compat.md#共享状态的语义约束) 的既有缺陷 |

ShowPartitions 按名称排序、过滤未知请求名，百分比来自整个 collection 的 load 状态。LoadPartitions / ReleasePartitions 在参照仍为 UNIMPLEMENTED；实现分区也不能因此宣称支持独立加载。空表达式 count(*) 忽略分区和 load；非空表达式先检查 load、再按选定候选计数。未知分区统计返回 200/200，未知集合仍为 100/4。

建议后续独立冻结：名称必须安全地映射到 collection 下单个目录，并检测实际文件系统别名及被 snapshot 钉住的名称；写入 manifest 前完成校验。可见性需同时覆盖全局 MemTable、按选择集去重、全局删除与按分区文件回收。这些只是待确认的分区方案，本轮没有修改参照、添加 Rust 分区或引入格式。

## 证据保留

仓库外调查目录的主要身份为 engine25-final / engine15-final、paths25 / paths15、key25 / key15、rpc30 / rpc26；固定样本运行身份为 partition-reference-verified、partition-original-rust、partition-no-key-reference、partition-no-key-rust，配对结果为 key-comparison.json。复现命令见工具 README。原始失败的环境准备与探针日志也保留，不用于填补成功证据。
