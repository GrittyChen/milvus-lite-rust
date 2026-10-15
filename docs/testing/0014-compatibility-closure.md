# 0014 索引与过滤表达式收口验收

- 状态：统计与逐项对照已完成并评审通过；发布另行授权
- 日期：2026-10-15
- 范围：[0014](../decisions/0014-index-definitions-and-filter-expressions.md) 的切片 A、B 完成后，按原方法重跑三组缺口统计

## 结论

固定样本中，参照用例新增通过 138 条，上游子集新增通过 37 条，调用探针新增成功 14 项；三组原来成功的样本均没有退回失败。参照用例仍有 384 条明确的 UNIMPLEMENTED 和 1 条原有跳过，上游仍有 15 条范围外能力阻断。没有发现这两组用例因结果不同而失败；这句话不适用于只检查调用成功与否的探针，也不证明样本外的全部行为一致。

本记录提供切片完成后的效果证据。引擎正确性、两条 SDK 与平台证据另见[切片 A 验收](index-definitions-acceptance.md)、[切片 B 验收](filter-tree-acceptance.md)；rc3 仍须按 [0013](../decisions/0013-first-release-platform-and-packaging.md) 从冻结提交重新构建并执行候选验收。

## 输入身份与环境

| 输入 | 固定身份 |
| --- | --- |
| 当前源码 | `72a8738391df0a13612b634a038a57d9ef132d8e`；包含 A、B 及已独立提交的存量 metric load、测试隔离与显式解锁修复 |
| 当前源码 git archive SHA256 | `eeba6ab47cb8c77ce315572aeb21c66d9731ef1e75d60a1006b0416c425e4fc4` |
| 当前本地统计 wheel SHA256 | `3dc0d8904a8129fbaf99d32e2edb67d9ccc670102c2fa214fb9ed3a643270873` |
| 当前原生库 SHA256 | `ea53c786106af7d34e1533a376004138d8aff5a675dd9c2e8aef7e5c66c0a02d`，与 B 最终验收相同 |
| rc2 基线 | 已发布的 `fe56005`；原生库 `7251892f5d08ef064997c2a97a516d0754002bd39f176fe186815d332056d865`，发行物见[rc2 候选记录](first-release-acceptance.md#010rc2-候选验收) |
| 参照 | milvus-lite 3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7` |
| 上游 Milvus 用例 | `dcda428d54d11e6084156e2342c513f08c9927bf`，工作区干净 |
| 主机与客户端 | macOS arm64，CPython 3.12.14，pymilvus 3.0.2，NumPy 2.5.4 |
| 测试与参照依赖 | pytest 9.1.1、pytest-timeout 2.4.0；参照含 PyArrow 25.0.1、faiss-cpu 1.15.1 |

项目版本尚为 `0.1.0rc2`，本地统计 wheel 用提交与哈希区分，未作为发行物发布。两个 Rust 测试环境按旧环境的冻结包版本独立重建，仅替换本包；旧 rc2 环境与结果保留。上游旧环境的 pytest 9.1.1 与 pytest-asyncio 0.24.0 的声明约束不兼容，重建时用 `--no-deps` 原样复制已有组合，未通过升级插件改变统计环境。

## 方法与口径

1. **参照 adapter / compatibility。** 原统计中 3.x 通过 899 条，排除依赖其内部模块的 139 条，固定其余 760 条。rc2 当时按两个完整目录执行，本次按这 760 个用例标识执行，不再运行排除项；执行集合改变可能影响模块共享 fixture 的状态。本次参照 760 条全部通过，作为该子集可独立执行的对照，不据此断言共享状态影响为零。两份新结果的用例集合均与清单完全相同，无重复或缺失。参照副本的 293 个相关源码、测试与兼容垫片文件，与 `43d1257` 逐字节一致。
2. **Rust 测试适配。** 沿用原统计的外部 ServerManager 垫片；测试本体不改，adapter 的清理 fixture 仍只清理本实现支持的默认 database。faiss 可用性探测沿用原统计，`GAP_ANN_AS_FLAT` 关闭，不把 ANN 请求改写成 FLAT。测试断言、timeout 180 秒及失败结果原样保留。
3. **上游子集。** 沿用参照 tools 列出的子集，以及原统计对 4 个已改名类的映射；参数化后固定 90 条。参照与当前各跑一次，两份标识集合均与旧 90 条相同。保留原 pytest 兼容垫片、OMP 线程数 1、timeout 300 秒；关闭 rerun 插件，没有新增重试。
4. **调用探针。** 原 49 项脚本不改，随机种子 7；每项独立建库，只检查调用是否成功，不比对返回内容。参照本次仍有同样的 43 项成功，另 6 项的成败状态与旧结果相同。三个来源分别统计，不将重叠场景相加为独立能力数。

rc2 列来自原统计保存的结果，本次没有重跑 rc2。比较按用例身份逐项进行；首个阻断仍会遮蔽后续问题，新增通过数也不能拆成 A、B 的独立因果贡献。

| 来源 | 固定样本数 | 本次 3.x 通过 / 成功 | rc2 旧结果 | 当前结果 | 新增通过 / 成功 |
| --- | ---: | ---: | ---: | ---: | ---: |
| 参照 adapter / compatibility | 760 | 760 | 237 | 375 | 138 |
| 上游 Milvus python_client 子集 | 90 | 90 | 38 | 75 | 37 |
| MilvusClient 调用探针 | 49 | 43 | 11 | 25 | 14 |

## 参照用例：首个阻断的变化

当前 760 条的原始 pytest 结果为 375 passed、339 failed、45 errors、1 skipped。339 个调用阶段失败与 45 个 fixture 阶段错误均由明确的 UNIMPLEMENTED 引起，不改写成通过。唯一跳过仍是原用例 `test_delete_with_filters_nullable_vector_field`：捕获 nullable 字段的 UNIMPLEMENTED 后自行 skip；本轮没有新增跳过。

按 rc2 首个阻断，新增通过的 138 条分为：向量索引定义 99 条、过滤表达式 39 条。原来 110 条向量索引定义阻断中，另 11 条继续前进后遇到其他能力：CreatePartition 3、HybridSearch 3、query iterator 2、range search 2、grouping search 1。

| 首个阻断类别 | rc2 | 当前 | 解释 |
| --- | ---: | ---: | --- |
| schema 类型 | 189 | 189 | nullable 120、ARRAY 42、TIMESTAMPTZ 16、GEOMETRY 7、默认值 3、多向量字段 1；另有上述 nullable 跳过 1 条 |
| 显式向量索引类型 | 110 | 0 | 99 条通过，11 条进入后续阻断 |
| 全文、rerank 与 hybrid | 62 | 65 | 新暴露 HybridSearch 3 条 |
| partition 与 partition key | 55 | 58 | 新暴露 CreatePartition 3 条 |
| 过滤表达式 | 39 | 0 | 原 39 条全部通过，不代表范围外算术等构造已支持 |
| database | 37 | 37 | 范围未变 |
| iterator、range search 与 group_by | 17 | 22 | 新暴露 5 条 |
| rename、alias、truncate 与 alter | 13 | 13 | 范围未变 |
| 明确 UNIMPLEMENTED 合计 | 522 | 384 | 加上各自原有跳过 1 条，缺口分别为 523 / 385 |

当前缺口的首个阻断均在 0014 以外，没有将新暴露的范围外能力写成已实现。schema 仍为最大的阻断类别，后续切片排序继续参考这个结论，同时保留只计首个阻断的局限。

## 上游子集与探针

上游 90 条当前为 75 passed、15 failed。新增通过 37 条在旧结果中分别被 DropIndex 21 条、CreateIndex 12 条及显式向量索引类型 4 条挡住。剩余 15 条按失败 API 归为 partition 8、alias 5、rename 1、多向量 schema 1；检查了底层日志中的 UNIMPLEMENTED。上游包装器把这些 RPC 失败转成“预期成功却返回 False”的断言，不将这类断言误记为返回结果不一致。

49 项探针中，新增成功的是：

- 过滤 9 项：or、not、not in、LIKE、JSON 路径、嵌套 JSON 路径、json_contains、is null、带 or 的 Search。
- 索引 5 项：HNSW 建表、IVF_FLAT 建表、插入后创建 HNSW、标量 INVERTED、DropIndex。

其余 18 项“3.x 成功、当前失败”的探针，分别停在 ARRAY schema 3 项、集合元数据能力 6 项、iterator / range / group 4 项、多向量 / 稀疏 / 全文 4 项、nullable 1 项。比如 ARRAY 上的 contains 在建 schema 时即失败，不能据此断言 JSON 数组上的 contains 未实现。

## 证据保留与边界

原统计脚本、测试副本、用例清单、环境冻结清单、源归档、本地 wheel、JUnit、日志及逐项前后对照 JSON 保存在仓库外；原始输出与数据目录不入库。关键输入的 SHA256：

| 证据 | SHA256 |
| --- | --- |
| 760 条用例身份清单 | `372f0443afb5d242dc39cc5a4baad82f9bdc7f9a0ac483aaa4ff6f507ef3a902` |
| 139 条内部依赖排除清单 | `aa834087b15f16990ab6fd68f6625c2ebdebface98b9c3a567b84b7efc0ef371` |
| 上游选择清单 | `217cef46dc41296f90ff71514f419467ce36f5c888a58fa8b3feee5b81397206` |
| 49 项探针脚本 | `ecf7af124b2a1a98f5ada1f7cc4f87852b0af14672d848dae1df9abb76002bba` |

原参照 `TestLocalDBMode` 的两项用例在关闭客户端连接后直接删除数据目录，没有先停止 ServerManager 服务。Rust 在进程退出停服时报告 WAL 路径缺失，旧 rc2 日志也有同样的错误。本次保留原测试方法与该诊断；这批通过数不作为正常停服、真正重开或持久化验收证据。

本次只重跑上述 macOS arm64 / pymilvus 3.0.2 统计，没有重跑全量 Rust、另一条 SDK、完整差分 / 互通、S1 性能或 Linux / Intel Mac 的候选矩阵。相关已有证据仍以各切片记录为准；真实重启、断电及候选阶段的未验证项仍按 0013 与既有边界处理。版本切换、推送、tag 与发布尚需分别授权。
