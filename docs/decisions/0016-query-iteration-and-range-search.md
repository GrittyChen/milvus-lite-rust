# 0016 Query iterator 与 range search

- 状态：切片 A、B 均经独立评审通过并收口；分区继续延后冻结设计
- 日期：2026-10-16
- 参照：milvus-lite v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`；pymilvus 2.6.17 / 3.0.2
- 前置：[0014](0014-index-definitions-and-filter-expressions.md)、[0015](0015-nullable-defaults-and-array.md)
- A 实现证据：[验收记录](../testing/0016-query-iterator-acceptance.md)；负责人独立评审通过（2026-10-16）
- B 实现证据：[验收记录](../testing/0016-range-search-acceptance.md)；独立评审通过并收口（2026-10-16）
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

## B：冻结合同

当前源码调查显示，3.x 在 range search 时先取有限近邻窗口（至少 `5 × (topk + offset)`），再以公开、尚未舍入的距离过滤；L2 为 `range_filter <= distance < radius`，IP/COSINE 为 `radius < score <= range_filter`。它不是全量范围扫描。以下实测冻结了阈值、缺失/空参数、offset 与客户端 V1 分页；V2 服务端 iterator 继续范围外。

参照源码及真实服务的 15 组生命周期、1100 项结果在两条 SDK 下逐项相同，冻结为 [range fixture](../../tests/compatibility_audit/fixtures/range_v321.json)。采用以下边界：

1. radius / range_filter 优先取 params 对象中的非 null 值，再回退顶层最后一个同名键；两者均缺失/null 时沿用普通检索，不扩展候选窗。布尔、数值和数值字符串按 Python float 转换。JSON 指数溢出为 infinity，巨大整数转 float 报 code 1；复用 0015 已实现的数字转换。非严格 JSON 容器及其它 0004 已登记的参数差异仍按原范围拒绝或保留，不在本片引入通用 Python JSON 解析器。
2. 普通过滤和去重完成后，取得最近 `5 × (topk + offset)` 个候选，再做范围判断、截断至 topk + offset，最后跳过正 offset。候选窗不为正时为空。负 topk / 正 offset 也必须保留真实候选窗是否为空，不能因无输出而改变数据过滤的错误优先级。原有非 range 路径保持原样。
3. 公共距离在 float64 中比较：L2 是内部 float32 开方距离转 float64 后平方，COSINE 是 1 减内部 float32 距离，IP 是内部距离取负。不能拿原始平方距离、余弦相似度或响应中舍入后的 float32 代替。NaN 作为任一有效边界时没有匹配；数据 NaN 也不能通过有效的范围比较。
4. 上述窗口是参照可观察行为，保留。例如 L2 位置 1..32，topk=1、range_filter=36 时先取最近 5 条后全部被排除，返回空；topk=2 时取最近 10 条，再返回 6、7。不能在扫描阶段提前丢弃范围外点，从而把它变成全量范围检索。
5. 保持 placeholder → topk → radius → range_filter → group_size → offset → round_decimal 的解码错误顺序；解码失败先于 load，成功解码后 released 仍报 101。过滤路线、度量/维度检查与投影沿用引擎既有顺序，范围判断在排序后、物化和响应舍入前。覆盖多查询的 topks、分数、nullable / ARRAY / 动态字段输出，失败不得返回部分字段。
6. search_iterator 只接通参照真实支持的客户端 V1；V2 探测沿用不返回 V2 结果字段后的客户端回退。不添加服务端 token、游标、MVCC 或跨 RPC 状态。V1 的 batch=1 宽度归零后取 0.05，最多约 21 次线性探测，可能跨不过较大的距离间隙而提前结束；已在两条 SDK 的真实 3.x 上复现（本例 L2 返回 2/32，batch=7 返回 32/32）。这属于既有客户端算法限制，本片不修改 SDK，不承诺任意数据与批量下 V1 都会遍历全部记录。

实施复用现有 TopK、计算池及读快照：扩展全局候选数，合并后在候选向量上原地保留范围内条目，再物化最终行。引擎新增与协议无关的 SearchRange 输入，普通 SearchRequest 和已有调用方保持兼容。无新锁、module、依赖、存储格式或写入协议。

验收除上述真值与差分外，包含多批计算任务、旧快照、NaN、有限窗反证、两条 SDK 全套、Arrow 15/25 互通、既有故障注入与固定三组统计。性能对照普通 Search 的 A 基线回归，并补充范围检索、客户端 V1 与官方实现的同机结果；每轮校验，平台证据仍按 0013 分层。分区、group_by、rerank/function chain、稀疏向量及真实 ANN 不随本片展开。

### 阈值附近的数值差异（负责人确认）

128 维、seed 1927 的实测中，同一行的 Rust 公共距离为 `90.59801781004353`，3.x 为 `90.59799965532966`；radius 为 `90.5980087326866` 时，命中数量为 19 / 20。这是 SIMD 与 NumPy 求和顺序产生的既有分数误差向范围判断传播，差值 `1.82e-5` 在 0004 的 L2 预算约 `9.06e-4` 内；仅允许分数误差和 topk 截断换位的旧合同未明确授权阈值成员数量差异，因此单独确认后登记。

实现负责人决定沿用既有误差预算，不要求逐位模拟依赖平台 BLAS 的求和，不给服务端范围比较添加 epsilon。对有限分数和有限阈值：以 3.x 的未舍入公共距离 d、相应行的 0004 误差预算 t 为准，只有 `|d - threshold| <= t` 的行允许两端范围资格不同；带外的资格必须一致。半径的开闭比较符、每端按自身未舍入距离严格判断、候选窗和结果上限仍必须正确；这不允许任意漏行，也不扩大分数误差。有限候选窗截断处的并列/近邻换位仍按 0004 原规则。

影响：阈值来自精确浮点值或落在误差带内的 range search，及依赖这些阈值的客户端 V1，可能有成员/数量差异；普通 Query 与标量过滤不受影响。固定 [边界输入](../../tests/compatibility_audit/fixtures/range_boundary_v321.json) 和独立差分用例分别检查两端自身的严格比较，再逐项限制成员差集只能落入原预算；非边界的 1100 项矩阵继续严格比较主键、状态和输出，不因此放宽。
