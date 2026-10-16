# 0016 切片 B：Range search 验收

- 日期：2026-10-16；状态：独立评审通过并收口（2026-10-16）
- 合同：[0016 B](../decisions/0016-query-iteration-and-range-search.md#b冻结合同)，含实现负责人决定、独立评审认可的阈值误差边界
- 前置：[Query iterator A](0016-query-iterator-acceptance.md) 已评审收口；分区继续独立延后
- 参照：只读 milvus-lite v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`
- 平台：macOS 27.0.1 / arm64；CPython 3.12.14（SDK 3.0.2）/ 3.10.21（SDK 2.6.17）

## 增量与身份

以 A 的 76 项产品源文件清单及 sdist 为基线，产品仅修改四个文件：

- [collection.rs](../../crates/core/src/collection.rs)：复用 TopK 全局合并后的候选集合，按 float64 公共距离原地保留范围内条目，再截断、分页与物化；普通路径不做范围换算。
- [lib.rs](../../crates/core/src/lib.rs)：导出协议无关的 SearchRange；已有 SearchRequest 的字段与调用方兼容。
- [search.rs](../../crates/grpc/src/search.rs)：解码并传递范围值，保留参数、load、范围外功能的拒绝顺序；负 topk 配正 offset 时保持真实候选窗是否为空。
- [pyint.rs](../../crates/grpc/src/pyint.rs)：范围参数的 JSON Number 复用既有 proto_float，区分指数溢出与巨大整数转 float 溢出；外部显式调用 pyint::float 的只有范围参数；仅修改它的 Number 分支，proto_float 自身和 schema 默认值路径未改，没有扩大其它请求参数的转换范围。

不新增锁、存储格式、module、依赖、线程或跨 RPC 状态；不修改距离计算内核、索引度量选择、Query / Delete 分派及写入协议。范围比较在全局近邻窗口之后，不能为了多返回结果而在扫描阶段提前过滤。

| 产物 | SHA256 |
| --- | --- |
| 产品源文件清单 | `822fddb65588d762da6273d0a6fd002c1a670378f4e48c927cd38600aefc1aa2` |
| 开发 wheel | `22e32f5cf71eae4caad21997d1a431e359c90562d635925d55a9cccaf833a47e` |
| 原生库 | `87ccdfcfc3c7fd4f4809adf7de2c4fc7f5a5e0e3ea9256208ac665a0f9aa9877` |
| sdist | `7d5154bd7f92c4c50f0cffb64698a8b8ced0e55b052babb991bd6bdda3e6dcaf` |

B2 测试 wheel 与表中收尾 wheel 的原生库同哈希，后者更新 README 元数据。两条 SDK、开发环境及性能环境均核对该原生哈希。保留原包版本元数据，不覆盖公开 rc3，不是发布候选。

## 真值和可区分的边界

[固定请求矩阵](../../tests/compatibility_audit/range_cases.py) 及 [参照真值](../../tests/compatibility_audit/fixtures/range_v321.json) 覆盖三种度量 × 五种生命周期，共 15 组 / 1100 项。两条 SDK 的结果逐项相同，均通过 --verify；同时冻结输入矩阵 SHA256。正常样本必须成功，预定错误样本必须失败，不能只比较双方相等。

| 边界 | 参照事实与验收判据 |
| --- | --- |
| 有限窗口 | L2 topk=1、range_filter=36 返回空；topk=2 返回 6、7；offset=1 返回 7。若提前按范围筛选再取近邻，会错误返回第 6 条，测试可以区分 |
| float64 比较 | 同一行公开距离约 1.2500000367；radius=1.25000002 排除，1.25000005 保留。若用响应 float32 的 1.25 比较会失败；round_decimal=0 仍不能提前舍入 |
| 开闭边界与特殊值 | radius / range_filter 单独、组合、相等、反向、零、负值、布尔、null、NaN / infinity 字符串、JSON 指数溢出与整数溢出 |
| 解码与优先级 | 内层非 null 优先、顶层回退、重复键；坏 radius 先于 range_filter / offset，坏解码先于 released；成功解码的 released 请求为 101 |
| 空候选窗 | 负 topk / 正 offset、零 topk 及动态条件均返回空；核对零窗分支不进入数据求值，仍保留表达式编译检查 |
| 输出与过滤 | 两条查询向量的 topks、主键、分数及序列化字段；typed、动态、混合过滤，nullable / ARRAY 输出；空集合和 VARCHAR 主键另测 |
| 生命周期 | 活跃、segment + MemTable、segment、关闭重开、实际 compaction；合并态逐端断言 manifest 只有一个数据文件 |
| 并行与旧快照 | 三线程、129 条查询、多个写入/Flush 来源；旧快照在删除和 Flush 后仍保留原行，新快照不返回已删除行 |

V1 另覆盖三种度量、INT64 / 非 ASCII VARCHAR、空集合、并列分数去重、批量 1/7、带/不带 limit，以及首批之后插入、删除、Flush。中途写入用例按 Get 和最终集合核验删除效果，不依赖两条 SDK 不同的 delete 返回包装。每个 iterator 都有明确结束与 close。

精度合同由实现负责人决定，本轮独立评审认为合理：随机 128 维输入复现 19 / 20 条成员差异，但距离差只约 1.82e-5，在 0004 原预算内。固定 [边界输入](../../tests/compatibility_audit/fixtures/range_boundary_v321.json) 的独立差分先检查每端严格比较，再要求成员差集只能落在原阈值误差带内；带外不放宽，服务端不加 epsilon。非边界矩阵不因此放宽主键、状态、错误或字段断言。

## 构建与测试

| 实际检查 | 结果 |
| --- | --- |
| fmt / workspace all-targets Clippy `-D warnings` / diff --check | 通过，无告警 |
| Rust workspace / core fault-injection | 313 / 399 passed，无 ignored |
| Python 全套，SDK 3.0.2 / 2.6.17 | 各 420 passed |
| 差分全套，SDK 3.0.2 / 2.6.17 | 各 323 passed；之后补充的精度边界和中途写入各 1 项在两条线分别通过，共覆盖当前 325 项 |
| 新增范围差分 | 初版 15 组通过；扩展为空集合/并列/VARCHAR 后 27 组在两条线分别通过；最后两项独立补验，不声称最后一次单命令收集了 325 项 |
| Arrow 15/25 双向存储互通 | 184 passed |
| 基准 / 打包 / 平台脚本自检 | 230 passed；不等于实际平台验收 |
| 统计工具反例自检 | 11 passed |
| 参照 fixture | 两条 SDK 各 1100 项 --verify 通过 |
| A 原生库反证 | 新增代表用例因 range search UNIMPLEMENTED 失败，确认不是双方空结果造成的假通过 |

环境和测试草稿中的失败独立保留：2.5.1 缺 pkg_resources，按既有 baseline/setup.sh 的 setuptools<81 补齐后重跑；V1 batch=1 大距离间隙的提前终止经源码和两条真实参照证实，改为显式冻结限制，不降低正常批量的完整行数断言。旧的“1e999 所在 params 必须拒绝”断言被真实参照与新增矩阵否定：arbitrary_precision 能解析其对象，未使用的参数可忽略；范围数值本身必须正确区分 infinity 与整数溢出，故修复转换并补验。

## 固定统计

以新原生库重跑三组未干预样本，逐项身份配对：

| 固定样本 | Query A | Range B2 |
| --- | --- | --- |
| 参照原始 760 条 | 539 passed | 547 passed |
| 上游 90 条 | 75 passed | 75 passed |
| 调用探针 49 项 | 30 成功 | 32 成功 |

新增 8 条全部为此前 range search 首阻断：6 条范围请求、2 条 search_iterator；原先通过的项目无退回。其余 208 failure / 4 error 都是已登记范围外 UNIMPLEMENTED；1 skipped 仍为原样本的 nullable 向量。调用探针增加 range_search 与 search_iterator；不能把通过比例当成全功能兼容率。

## 性能

口径与复现见 [专项 README](../../benchmarks/range-search/README.md)。软件测试和编译全部退出后，Query A / B2 / 3.x / 2.5.1 轮换三轮，共 12 次顺序运行。版本均为 CPython 3.12.14 / pymilvus 3.0.2，OpenMP / BLAS 为 1，Rust 计算池保留宿主默认 10 线程；不宣称同 CPU 核数的执行器效率。

数据为 10 万行、128 维、FLAT / L2，仅第一维 id/65536 递增，其余为零。它提供精确可验的排序和范围结果，不代表真实向量分布。范围 [0,1000) 包含全部点，便于两代官方实现与 Rust 使用同一结果集。每项 raw RPC 2 次预热 / 9 次计时；V1 每批 1000、总计 5000，测 3 次。每次核对写入、主键、topks、分数与完整分页，不与软件套件或其它基准并发。保留桌面负载噪声和离群值。

下表单位 ms，为三轮 p50 的中位数 [最小值, 最大值]：

| 项目 | Query A | B2 | 3.x | 2.5.1 |
| --- | --- | --- | --- | --- |
| 普通 nq=1 | 0.817 [0.753, 0.882] | 0.881 [0.719, 0.896] | 37.221 [36.898, 37.822] | 1.383 [1.359, 1.518] |
| 普通 nq=32 | 8.437 [7.587, 8.914] | 7.768 [7.603, 8.330] | 240.983 [240.929, 241.748] | 7.401 [7.088, 9.473] |
| 范围 nq=1 | 未支持 | 0.914 [0.828, 0.961] | 34.811 [34.718, 34.876] | 2.057 [1.957, 2.075] |
| 范围 nq=32 | 未支持 | 7.953 [7.607, 7.974] | 248.352 [247.665, 250.731] | 16.374 [15.911, 20.291] |
| V1 5000 条 | 未支持 | 112.176 [110.052, 132.636] | 3744.724 [3742.907, 3757.147] | 35.677 [34.745, 35.934] |

普通路径未见稳定退化。V1 仍慢于 2.5.1，不以范围 RPC 的数字宣称全面超越；本片保留 3.x 的有限候选窗、严格比较与现有 SDK 算法，不为了性能改合同。结果不用于外推百万行、ANN 召回或任意环形范围的性能。

## 自查与证据边界

按 [评审规则](../../.agents/skills/code-review-rules/SKILL.md) 自查，搜索 `SearchRange|search_filtered_with_range|search_filtered_in_blocks|TopK|public_f64|pyint::float`，覆盖参数解析与 fallback → service 读快照 → 三路线过滤 / 已发布有效位 → 并行叶子 TopK → 全局合并 → 范围换算和比较 → 截断/offset → gather → output / round_score → SDK V2 探测及 V1 续读。

普通 Search、Query iterator、Get、Delete 的原有入口一并核对；range=None 不做公共距离转换，范围比较没有移进叶子扫描。f64 边界条件、NaN 的正向比较、零窗不求值、错误优先级和不返回部分投影均有组件或端到端证据。写入、恢复、索引与落盘代码未修改，以既有故障注入和互通作为回归；不生成没有新增格式的 rc1–rc3 回退种子。

Linux / Intel Mac 最终候选矩阵未执行；本机 arm64 结果不能补齐它们。分区、group_by、rerank、稀疏向量、真实 ANN、服务端 V2 / MVCC 继续范围外；V1 还受有限候选窗与 SDK 探测上限约束，不能承诺任意批量/数据下完整遍历。

源清单、两代官方环境、wheel/sdist、所有失败和成功日志、1100 项捕获及 --verify、阈值原始探针、反证和四方性能保存在仓库外 mlr-0016-range；三组统计运行身份 range-b2 / range-b2-upstream / range-b2-probe。变更未提交，未切版本、打 tag 或发布；最终候选仍按 0013 重建验收。


## 独立评审收口

独立评审结论为通过，无阻断、建议或待确认项。基线为 0016 A 的 sdist（`b988db8e…`），确认产品仅上述四文件变化，开发与两条 SDK 环境的原生库均为 `87ccdfcf…`。

独立执行 fmt / Clippy、Rust 313 / 故障注入 399、Python 3.0.2 全套 420、一次运行收集齐的 3.0.2 差分全套 325、从零准备环境的互通 184，以及 2.6.17 范围差分 29，均通过。两条 SDK 各 423 项端到端探针中，402 项逐字节一致；其余 21 项仅分数第 4 位小数相差 1，属于 0004 已登记的分数误差，经探针舍入后显现，普通检索也存在。全部主键集合、顺序、条数相同，31 项报错也一致；随机探针未触发阈值成员误差带。

阈值误差带作为实现负责人的设计决定接受评审，不以评审者事前确认为依据。独立评审认为预算和测试均合理：先验证每端自身的严格比较，再限制成员差集只能落在 0004 原预算内，带外不得放宽；服务端不加 epsilon。

2.6 全套、1100 项 --verify、固定统计、四方性能、A 构建反证和 Linux / Intel Mac 未由独立评审者重新执行，采信前述验收记录。以上未提交状态与构建哈希描述冻结验收时点；收口后仅作本地提交归档，不推送、不切版本、不打 tag、不发布。
