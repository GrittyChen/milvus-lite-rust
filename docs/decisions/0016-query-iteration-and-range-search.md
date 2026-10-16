# 0016 Query iterator 与 range search

- 状态：切片 A 已独立评审通过并收口；随后推进切片 B，分区继续延后冻结设计
- 日期：2026-10-16
- 参照：milvus-lite v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`；pymilvus 2.6.17 / 3.0.2
- 前置：[0014](0014-index-definitions-and-filter-expressions.md)、[0015](0015-nullable-defaults-and-array.md)
- A 实现证据：[验收记录](../testing/0016-query-iterator-acceptance.md)；负责人独立评审通过（2026-10-16）
- 调查：[分区调查](../testing/0016-partition-investigation.md)；工具：[compatibility_audit](../../tests/compatibility_audit/README.md)

## 依据与切片

0015 A、B 已分别评审通过。B3 的固定样本为 531/760、75/90、调用成功 29/49；剩余首阻断中，分区 62、Query iterator 8、range search 8（其中 2 条是依赖范围检索的 search iterator）、alias / rename 各 3。首阻断不能当作补齐后的收益承诺。

分区调查发现目录名可越出 collection、文件系统大小写别名可导致丢数据，以及跨分区相同主键在 MemTable、segment、删除与重开后的可见性差异。它需要独立冻结正确性和兼容差异，不能作为几个 RPC 的翻译直接加入。因此先完成只扩展读取路径的两个切片：

| 切片 | 范围 | 交付门槛 |
| --- | --- | --- |
| A | 普通 Query RPC 承载的客户端 query iterator；INT64 / VARCHAR 排序、分页、过滤、输出、两条 SDK | 参照真值、差分、生命周期和并发快照测试、原有读取回归、固定统计、独立验收记录 |
| B | radius / range_filter、相应的客户端 search iterator V1 | 另行冻结范围边界、有限候选窗口、距离阈值与舍入顺序、错误优先级，再实现和验收 |

继续保持 0.2.0 开发线；实施与验收阶段未切版本、提交、打 tag 或发布；收口后仅作已评审内容的本地提交，不推送、不打 tag、不发布。最终候选仍走 [0013](0013-first-release-platform-and-packaging.md)。BM25 继续独立里程碑，真实 ANN 仍由规模驱动。

## A：冻结合同

参照 Query 分派及引擎排序由上游 `a8d95b4` 引入，目的是避免客户端用末尾主键续读时跳行或重复。读源码、历史和真实服务的结果一致；两条 SDK 的 10 组数据 × 28 项真值共 280 项逐项相同，保存在 [fixture](../../tests/compatibility_audit/fixtures/query_iteration_v321.json)。

1. query_params **出现 iterator 键即生效**，不解读其值；`true`、`false`、`0`、任意文本均相同。`false` 是 SDK 执行 offset seek 时的实际请求，不能当作普通 Query。
2. iterator 绕过 `id in [...]` 的 Get 快路径；先用已发布有效位图去重、过滤，再按实际主键升序，最后应用 offset / limit。INT64 用数值序，VARCHAR 用 Unicode 字符序（合法 UTF-8 字节序与之相同），不按数字字符串或物理位置排序。
3. 不带 iterator 键的普通 Query 继续保留源顺序，主键 IN 快路径继续保留请求次序及重复主键、忽略分页的原有行为；Delete 使用的读快照路线也不改排序。
4. 保留 3.x 分页的边界行为：追加第一行后才检查 offset + limit，因此零或负 limit 仍可能返回一行；负 offset 沿用 Python 切片规则。错误优先级沿用当前普通 Query 的 load、语法/绑定、范围外参数与输出路径；只撤销原先对 iterator 的明确拒绝。
5. count(*) 仍先于 iterator 分派；空表达式直接取全局计数，即使已 release；有表达式仍检查 load。nullable / ARRAY 编码沿用 0015，不新增输出协议。
6. 没有索引定义的集合按参照始终自动加载，release 不生效；有索引定义的 released 集合仍先报 101，再检查坏表达式。每个 RPC 取得独立不可变读快照。请求之间可观察到新写入/删除，与 3.x 相同；服务端不保存游标、不提供跨 RPC MVCC、不伪造 session_ts。客户端负责构造主键续读条件与关闭自己的 iterator。

## 实施边界

引擎在既有 Query 的行引用集合上增加按主键比较的可选排序，字符串按借用比较，不为每次比较复制字符串。过滤继续列式，直接消费已发布 live bitmap；不新增逐行全库有效性判定，不复制全部行或向量。临时行引用空间仍为 O(匹配行数)，排序 O(匹配行数 × log 匹配行数)；这不是常数内存的服务端游标。

gRPC 只选择引擎读取路线；解析仍在状态锁外，排序、物化与编码仍在释放 collection 状态锁后。不新增 module、依赖、锁、后台线程或服务生命周期状态；不改变 WAL、Parquet、schema.json、manifest 与任何数据格式。

验收覆盖 INT64 / VARCHAR（含负数、非 ASCII）、活跃 / 混合 / segment / 重开 / 实际合并、三条过滤路线、父 null 与 ARRAY 投影、零/负分页、原始 proto 与 SDK、并发旧快照、普通 Get/Query/Delete 回归。运行 fmt / Clippy、workspace、故障注入、两条 SDK 的 Python / 差分、Arrow 15/25 互通及三组固定统计。故障注入与互通是既有路径回归，不声称本片新增持久化协议。

性能分别测普通读取相对 B3 的回归、较大匹配集的 iterator 首批/完整分页；顺序轮换，逐次校验结果。只评估回归和资源边界；不据此作相对 2.5.1 / 3.x 的加速声明。平台证据分层沿用 0013。


## 下一切片

按已确认顺序推进 range search；先冻结候选窗口、公共距离阈值和客户端 V1 真值，再独立实施与验收。
