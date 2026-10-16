# 0016 切片 A：Query iterator 验收

- 日期：2026-10-16；状态：负责人独立评审通过并收口（2026-10-16）
- 合同：[0016](../decisions/0016-query-iteration-and-range-search.md)；前一片：[0015 B](0015-array-acceptance.md)，已由负责人评审通过
- 参照：只读 milvus-lite v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`
- 平台：macOS 27.0.1 / arm64；CPython 3.12.14（SDK 3.0.2）及 3.10.21（SDK 2.6.17）

## 实现与产物身份

相对 B3 的 76 项产品源文件清单，只有 [collection.rs](../../crates/core/src/collection.rs) 与 [query.rs](../../crates/grpc/src/query.rs) 改动。引擎复用过滤后的 RowRef，仅 iterator 读取按实际主键比较；adapter 仅按键是否存在选择路线、关闭 Get 快路径。普通 Query/Get/Delete、快照所有权、锁、存储格式均沿用原路径。

| 产物 | SHA256 |
| --- | --- |
| 产品源文件清单 | `3948e92ae45a18661c0aef4e3f3e32e46e8ff39a5cd1fe96b2586e2bb643d71d` |
| 开发 wheel | `c0fd468d576049263ae3e397d2f4f2c0dcc1c5d0cfd5eb7de338c4c495e74564` |
| 原生库 | `49ba7dc39d03774bf788c6d5ddc106d3d9eb6f437ac15bd401ae52dca34ed729` |
| sdist | `b988db8e64a826cebbb88b3297725253027a52d0cc471995a1c376c5b8d6081c` |

初次测试 wheel 与表中 wheel 的原生库完全同哈希；后者重新构建时更新了包内 README，业务源码与原生二进制相同。开发环境也重新安装并核对该原生哈希。sdist 中 75 项产品源文件逐字核对；rust-toolchain.toml 按 0013 不进入源码发行。保留原包版本元数据，本地开发 wheel 不替换公开 rc3，不是发布候选。

## 真值、差分与回归

参照历史 `a8d95b4` 明确把 iterator=False 的 offset seek 纳入有序 Query，必须避开 Get。两条 SDK 的 [冻结真值](../../tests/compatibility_audit/fixtures/query_iteration_v321.json) 各有 10 数据集、280 项结果，逐项相同，已分别执行 --verify。该 fixture 的 release 样本无索引，参照 release 不生效；真正建索引后的 released 错误优先级另由新增差分覆盖。

[新增差分](../../tests/differential/test_query_iterator.py) 的 INT64 / VARCHAR × 活跃、混合、segment、重开、实际合并共 10 组，每组比较 150 个 raw Query（完整状态、错误原文、输出字段、主键名及序列化列），另对 SDK 页面逐页比较、独立断言排序和精确行数。表达式包括 typed、动态路线、nullable 与 ARRAY、PK-IN 重复键；包含零/负分页、错误表达式、有索引的 release。实际合并先检查两端 manifest 都只剩一个文件。B3 运行代表性用例会因 iterator UNIMPLEMENTED 明确失败，作为能力缺失反证。

并发测试在 AFTER_PREPARE 暂停后执行删除与 flush，要求写入可以完成，恢复读取仍返回原快照；分别覆盖普通 Get 和 iterator。引擎另测两种主键、混合块、旧快照与当前快照，以及零/负分页和负 offset。普通 Query 的原序与 Get 的重复/次序/忽略分页均保留并参加对照。

| 本轮检查 | 结果 |
| --- | --- |
| fmt / Clippy all-targets `-D warnings` / diff --check | 通过，无告警 |
| Rust workspace / core fault-injection | 311 / 397 passed，无 ignored |
| Python 全套 SDK 3.0.2 / 2.6.17 | 各 420 passed；补充 SDK 页序断言后两条线分别定向通过 |
| 差分全套 SDK 3.0.2 / 2.6.17 | 各 296 passed；raw UTF-8 输入及成功断言完善后，新增 10 组在两条线分别重跑通过 |
| Arrow 15/25 双向存储互通 | 184 passed |
| 基准 / 打包 / 平台脚本自检 | 230 passed；不等于目标平台运行 |
| 统计工具反例自检 | 11 passed |

环境准备的失败不充作产品证据：互通首跑使用相对解释器路径，子进程换目录后找不到解释器；改成绝对路径后全套通过。SDK 2.6 差分首跑误把参照 PYTHONPATH 传给两端，收集即失败；改为仅参照子进程的包装解释器，主进程明确加载 Rust 原生库后再跑。初版新增测试误以为无索引集合 release 生效，真实两端都继续可读；补建索引后检查实际 released 行为，没有为迁就测试修改产品或放宽断言。性能与 raw 差分草稿把非 ASCII 主键编码为 `\u`，已冻结语法不接受该转义；现改用字面 UTF-8，所有 raw 正常/错误情形均显式断言预期状态，重新捕获并核验两条 SDK 的 280 项 fixture，重跑新增差分和全部正式性能。旧日志保留，不混作最终输入证据。

## 固定统计

三组原始样本均以新构建重跑，逐条身份与 B3 配对：

| 样本 | B3 | Query A |
| --- | --- | --- |
| 参照固定 760 条 | 531 passed | 539 passed |
| 上游固定 90 条 | 75 passed | 75 passed |
| 调用探针固定 49 项 | 29 成功 | 30 成功 |

新增 8 条通过全部是 Query iterator；原先通过的用例无退回。Query A 剩余 216 failure / 4 error 均为已登记范围外 UNIMPLEMENTED；1 skipped 是原样本的 nullable 向量。新增调用成功也只有 query_iterator。剩余 range search 的 8 条仍未解锁，包含 2 条 search iterator。比例不作为全功能兼容率。

## 性能与资源

专项脚本、计时口径及三轮轮换见 [README](../../benchmarks/query-iterator/README.md)。软件套件完成后才开始正式性能运行；100 行并发环境中的脚本冒烟仅证明断言能执行，不进入性能表。

同机 CPython 3.12.14 / pymilvus 3.0.2，B3 与新构建三轮轮换，10 万 INT64 / 10 万非 ASCII VARCHAR / 百万 INT64 共 18 次；每轮 9 个计时样本及 2 次预热。原生库和脚本哈希逐一核对一致。以下是三轮 p50 的中位数 [最小值, 最大值]，单位 ms；桌面负载未完全隔离，不作相对官方两代实现的加速声明。

| 数据 | 普通读取 | B3 | Query A |
| --- | --- | --- | --- |
| 10 万 INT64 | Query limit100 | 0.652 [0.613, 0.688] | 0.628 [0.585, 0.738] |
| 10 万 INT64 | Get 100 个主键 | 0.297 [0.282, 0.493] | 0.279 [0.277, 0.351] |
| 10 万 VARCHAR | Query limit100 | 1.072 [0.884, 1.093] | 0.944 [0.923, 0.946] |
| 10 万 VARCHAR | Get 100 个主键 | 0.337 [0.312, 0.359] | 0.316 [0.292, 0.324] |
| 百万 INT64 | Query limit100 | 5.604 [5.454, 5.658] | 5.666 [5.486, 11.719] |
| 百万 INT64 | Get 100 个主键 | 0.288 [0.273, 0.296] | 0.302 [0.298, 0.350] |

普通读取未见稳定退化；百万 Query 有一轮离群值 11.719 ms，保留原值，不用挑选样本消除它。

| 新增有序读取 | 首批 100，ms | 空值过滤后首批 100，ms | SDK 全量分页，ms |
| --- | --- | --- | --- |
| 10 万 INT64 | 8.148 [8.076, 8.836] | 7.388 [7.221, 7.442] | 199.191 [188.341, 209.532] |
| 10 万 VARCHAR | 19.380 [19.244, 20.769] | 12.594 [12.406, 13.072] | 420.427 [370.587, 420.503] |
| 百万 INT64 | 96.793 [96.355, 101.704] | 84.374 [82.832, 84.558] | 13225.036 [13077.041, 13233.070] |

逐次结果均核对，完整分页以 4096 为批量；10 万为 25 个非空页加终止空页，百万为 245 个非空页加终止空页。临时空间随匹配数增长，单次重新排序，不宣称常数内存或跨页复用索引。

资源诊断：新构建百万行完整分页的累计进程 maxrss 为 1667.9 [1663.0, 1669.8] MiB，包含数据准备、SDK 与服务端；B3 的正式专项只读取小批量，不能直接比较两个峰值。为辨别是否由新增路线引入，另将同一物理目录分别复制后重开，以 raw RPC 做相同的 245 个主键范围、相同返回行数和 nullable 输出，B3 用普通 Query，新构建带 iterator。加载后分别为 346.0 / 346.0 MiB，峰值为 1592.6 / 1593.5 MiB，均在第约 75 次请求后持平至结束。它是旧通用读取路径也可重现的高水位；没有据此断言具体分配器原因或新增内存泄漏，也没有将其悄悄优化进本切片。诊断脚本与逐点记录保存在独立 rss-b3 / rss-query 目录。

## 同类扫描与证据边界

搜索 `query_loaded|query_ordered_parsed|query_parsed|primary_keys`，沿 Query 分派 → 过滤三路线 → live bitmap → RowRef → typed PK 比较 → 分页 → gather → protobuf 输出，连同 Delete 的唯一内部 query_loaded 入口逐项检查。过滤后的 chunks 保留索引对应关系，gather 读取原快照相同位置；排序不复制主键或全部行。活跃、混合、重开与实际合并结果一致，主键唯一性来自既有已发布位图，INT64 / VARCHAR 不被 Nullable 包裹。

未执行 Linux、Intel Mac 最终候选矩阵，不能由本机 arm64 结果补齐。没有新增持久化格式或写入协议，故本轮不再生成 rc1–rc3 回退种子，既有回退证据见 0015；故障注入、互通和两条 SDK 是实跑回归。跨 RPC 的一致快照、服务端游标、分区、range search 和 V2 search iterator 不在本片范围。

正式性能脚本 SHA256 为 `0dbe73b3b0b715b2daa56b3291236efdd164cfa2d8fa9deb882b4c4db302fb56`，三轮结果位于 performance-v2；中断的首版非 ASCII 运行保留在 performance。

原始日志、环境冻结、源清单、wheel/sdist、B3 反证及物理目录保存在仓库外 mlr-0016-query；三组统计保存在既有调查工作区，运行身份 query-a-original-v2 / query-a-upstream / query-a-probe。失败环境和测试草稿日志同样保留，不用于替代最终成功记录。冻结验收时变更未提交、未切版本、打 tag 或发布；本片与后续 range search 均已独立评审收口，现按切片作本地提交，不推送。


## 负责人独立评审

负责人确认相对 B3 仅修改 core/collection.rs 与 grpc/query.rs，原生库为 `49ba7dc3…`，按 0016 合同逐条通过，无阻断、建议或待确认项。独立执行 fmt / Clippy、workspace 311、故障注入 397、SDK 3.0.2 Python 420 / 差分 296、从零建立环境的互通 184；SDK 2.6.17 的 iterator 差分 10 项通过。两条 SDK 各 68 项端到端探针全部与 3.x 一致，包含非 ASCII、不同批量、nullable / ARRAY / 动态字段、主键 IN、offset、迭代中插入/删除/Flush 与 released 101。

负责人另对照 10 万行：首批 100 为当前构建 7.6 ms、3.x 72.3 ms，完整翻页为 0.16 s / 2.44 s，说明每页完整排序在该规模未构成问题。此为负责人提供的补充结果，没有 2.5.1 同机基线，不扩展为规则 7 意义的全面加速声明。2.6 全套、固定统计、正式性能/RSS、fixture --verify 及 Linux / Intel Mac 未由负责人独立重跑；分区调查未纳入这次评审。现进入 range search。
