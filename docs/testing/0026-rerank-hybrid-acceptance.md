# 0026：HybridSearch 与 rerank 实施验收

- 状态：设计评审通过；第 1 步（Search 的 boost）、第 2 步（Search 的 L2 rerank）、第 3 步（HybridSearch）各自评审通过并提交到本地；本记录随第 4 步（差分、参照用例、文档与验收）提交。**未推送、未发布**。
- 日期：2026-11-06
- 合同与设计：[0026](../decisions/0026-rerank-and-hybrid-search.md)，继承 0004、0005、0006、0017（分区可见性）、0024（引擎分组）、0025（时区）。
- 仓库基线：0025 合入后的 `b9a64ee`；本地提交依次为 `c0be6ec`（设计）、`813b511`（第 1 步）、`6a34aa9`（0025 的时区名小修）、`18215dd`（第 2 步）、`18ffcaa`（第 3 步）。
- 参照：milvus-lite v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`，保持只读；SDK 固定 2.6.17 / 3.0.2。
- 开发产物沿用 `0.3.0rc3` 版本字符串，只以 wheel 摘要识别；不是新的发行候选。

## 结论

`hybrid_search` 的 `RRFRanker` / `WeightedRanker`（`rank_params`），以及 Search 与 HybridSearch 的 `function_score`（任意个 boost 加最多一个 rrf / weighted / decay）已实施。行为以真实 3.x 为真值：设计阶段探针的 2822 个场景与引擎分组同名字段的 164 个场景整体转入仓库差分，逐场景比较整条结果（含错误码与文案），登记差异与沿用已有差异的场景两边分别断言；第 1–3 步的补充探针（1466 个场景）在最终构建上重跑，差异只剩已登记的项与设计已放宽的排序报错操作数顺序。不改存储格式、不新增依赖、不加锁。

## 实施范围

| 步骤 | 内容 | 提交 |
| --- | --- | --- |
| 设计 | 调查（参照代码、2822 个场景的 3.x 真值）、范围冻结、两空间分数契约、扁平候选行模型、`GroupByOp` 组键、候选窗口、错误先后、测试矩阵；评审通过 | `c0be6ec` |
| 1 | core：两空间还原、boost、三组候选窗口、`filter` 的 Python 逐行入口；gRPC：`function_score` 解码、Search 的 boost | `813b511` |
| 2 | core：rrf / weighted / decay 的合并、`GroupByOp`、Python 切片、rerank 后的 `round_decimal`；Search 的 L2 路径 | `18215dd` |
| 3 | core / gRPC：HybridSearch（`rank_params`、顶层与子请求的 `function_score`、分区、分组、时区与 `time_fields`），decay `sum` / `avg` 的 CPython `sum()` 语义，NaN decay 因子写进动态 `$meta` | `18ffcaa` |
| 4 | 探针场景整体转入差分、参照用例逐 ID 记账、0006 差异登记、README / AGENTS、本记录、固定统计、变异验证、计数同步 | 本批 |

本批没有产品源码改动（只增加测试与文档）。

## 登记的差异

全部登记于 [0026「登记的差异」](../decisions/0026-rerank-and-hybrid-search.md#登记的差异)，本批在 0006 差异表新增一行汇总：Search 与 HybridSearch 顶层通过全部校验的 model rerank、候选真正用到的 `random_score`、Search 的公共 `function_chains`、boost 过滤里的算术与 `text_match` 为 UNIMPLEMENTED（HybridSearch 子请求里的 model 被忽略、没有候选行的 `random_score` 返回空结果，与 3.x 相同，由差分的 `route fs model valid`、`fs random_score no hits` 两个场景比较）；`rank_params` 的整数参数超出 int64 按 0004 的饱和值执行；子请求的非严格 JSON `params` 沿用 Search 的拒绝；链算出的值写进同名 TIMESTAMPTZ / ARRAY / 向量字段为 UNIMPLEMENTED；`GroupByOp` 的排序键含 NaN 时不逐位复现；JSON 非标准字面量与少见参数类型组合不逐一复现。引擎分组的 0024 缺陷与路由向量维度的 0004 文案沿用已有登记。本批没有新增差异。

## 构建身份

| 产物 | SHA256 |
| --- | --- |
| macOS arm64 开发 wheel（统计与 2.6 线复验所用） | `3ae5f9818b6cc2a467c6703bd6cdf6517069a56b197ea67955d1eed570b875b2` |
| 其中的 `milvus_lite._native` | `9d8aba8037ba908d03846151c7e95967577b9fb87de4215f9cec489d81c3f3d2` |

wheel 由 `18ffcaa` 之上的工作区构建，产品源码与 `18ffcaa` 相同；README 在构建之后修改，只影响 wheel 的元数据。未发布的开发产物，属本地留存。

## 测试

| 套件 | 结果 |
| --- | --- |
| 差分 `tests/differential`（pymilvus 3.0.2，全量，参照环境未装 tzdata） | 472 通过（1297 秒，最后一次改动用例之后顺序运行，无其他负载）；第 3 步为 470，新增 `test_rerank_scenarios.py` 的 2 个 |
| 差分，pymilvus 2.6.17 线（同一产品源码的 wheel，仓库外运行） | rerank 相关的 4 个文件 15 通过（`test_rerank_scenarios.py` 首次运行发现 SDK 客户端校验的分类数不同，见上文，修正预期后重跑 2 通过）；全量未在这条线上跑 |
| Python 端到端 `tests/python`（pymilvus 3.0.2） | 564 通过（计数不变；`test_grpc_translation.py::test_search_l2_rerank_functions` 增加 `round_decimal` 的精确断言，2.6.17 线上该用例也通过） |
| `pytest tests/release tests/ci tests/sharding tests/packaging` | 871 通过 |
| 分片计划核对（`tests/sharding/shard.py plan --expect 472 --expect-group 118`） | 472 / 118，与工作流中的预期一致 |
| `cargo test --workspace`、fault-injection、fmt、clippy | 本批没有 Rust 改动，未重跑；第 3 步提交时为 workspace 532、FI 672，fmt / clippy 干净。变异验证的每一项都在原样恢复源码后重建扩展 |

预期计数随新增用例同步：差分 470→472（会话组 118 不变），Python 564 不变；涉及 `compare.yml`、`scorecard.py`、候选自检夹具 `test_candidate.py`、分片 README 与 0020。

## 变异验证

0026 测试矩阵列出的 12 项，`SortOp` 平局拆成链与 boost 两处，共 13 项。每项改源码 → core 与 gRPC 的单测 → 重建扩展 → `test_rerank_scenarios.py`（没有失败时再跑三个分步差分文件），之后恢复源码并核对 `crates/` 无改动：

| 变异 | 单测失败 | 场景差分失败 |
| --- | --- | --- |
| Weighted 改成 min-max 归一化 | — | 102 / 2822 |
| boost 的 L2 乘在平方距离上 | `transform_table_operates_on_internal_distance` 等 4 个 | 18 |
| Search + L2 的窗口少乘 10 | `search_l2_keeps_limit_and_offset_apart` | 135 |
| decay 的 `norm_score` 取严格布尔 | — | 12 |
| Search + L2 的分组恢复字段类型校验 | — | 251 |
| `GroupByOp` 的负 `group_size` 当空组 | `group_slices_and_scores_like_python` | 4 |
| rerank 的 `round_decimal` 二次经过 `public_f64` | — | 0（见下） |
| `GroupKey` 改成浮点位比较（`-0.0` 单独成组、NaN 同组） | `keys_follow_python_hash_equality` | 6 |
| 链的 `SortOp` 平局改成首次出现 | `rrf_scores_follow_rank_and_ties_break_on_id` | 107 |
| boost 的 `SortOp` 平局改成首次出现 | `sort_puts_missing_last_and_breaks_ties_by_id` | 20 |
| 组值按类型白名单（FLOAT / DOUBLE / JSON / TIMESTAMPTZ 丢组值） | — | 107 |
| 子请求的 L2 也做链的构造期校验 | — | 3 |
| Search + L2 的 `limit <= 0` 返回全部候选 | `search_l2_keeps_limit_and_offset_apart` | 11 |

`round_decimal` 的二次换算只多一次 float32 舍入，在差分 1e-5 的分数容差下不可见，三个分步差分文件也全部通过。本批为此在 `test_grpc_translation.py::test_search_l2_rerank_functions` 补了精确断言：`round_decimal=9`（SDK 只允许到 6，改写发出的请求）时 rrf 的分数必须逐位等于 3.x 实测的 `float32(round(1/61, 9))`；补测后该变异使这个用例失败，恢复后通过。

## 探针对跑

差分转入的是下表前两行；后三行是第 1–3 步的补充探针，相应差分已随各步提交（`test_rerank_boost.py`、`test_rerank_l2.py`、`test_hybrid_search.py`）。五份探针都在最终构建上重跑，与存档的 3.x 输出逐场景比较：

| 探针 | 场景 | 与 3.x 的差异 | 归因 |
| --- | --- | --- | --- |
| 设计阶段全量探针（三个协议表面、合并与打分、窗口、分组、时区、同名冲突、主键形态、平局、数值边界） | 2822 | 56 | model 4、`random_score` 4、过滤范围 6、0004 维度文案 1、0024 引擎分组缺陷 41（该类 42 个场景中有 1 个两边恰好相同） |
| 引擎分组同名字段的类型矩阵与数字字符串 | 164 | 10 | 部分命中时排序报错的操作数顺序（设计已放宽为只比较错误码与固定文案） |
| 第 1 步补充（Search + boost） | 855 | 6 | 同上，排序报错的操作数顺序 |
| 第 2 步补充（Search + L2） | 366 | 0 | |
| 第 3 步补充（HybridSearch） | 245 | 6 | 超出 int64 的 `rank_params` 整数 4、子请求的非严格 JSON `params` 2 |

全量探针与第 3 步补充的差异名单与第 3 步提交时逐项相同。3.x 一侧重跑全量探针与存档输出逐场景相同（探针确定性）。

## 差分用例的覆盖

| 文件 | 内容 |
| --- | --- |
| `rerank_cases.py` + `test_rerank_scenarios.py`（本批） | 全量探针 2822 个场景与引擎分组同名字段 164 个场景原样转入，对两边各跑一遍再逐场景比较整条记录（SDK 命中与原始响应：ids、scores、topks、top_k、`group_by_field_value`、`fields_data`、状态；浮点容差 1e-5，NaN 视为相等，组值区分 `-0.0`）。分类同设计「测试矩阵」，每类的场景数都有断言：通过差分 2749（3.0.2 线；2.6.17 线的 proto 没有 `function_chains`，改写该字段的 4 个场景在客户端失败，归入 SDK 客户端校验）、探针准备 13、SDK 客户端校验 3、model 4（3.x 一侧 code 1 与 Cohere 缺 key 的文案，本实现 UNIMPLEMENTED 与说明）、`random_score` 4 与过滤范围 6（3.x 一侧成功，本实现 UNIMPLEMENTED 与说明）、0004 1（两边 code 1，本实现文案全文）、0024 42（两边的 ids / 组值 / 有效位或错误各自钉住） |
| `test_rerank_boost.py`（第 1 步） | Search + boost 的平局、窗口、offset、引擎分组、`round_decimal`、同名字段与宽整数 |
| `test_rerank_l2.py`（第 2 步） | Search + L2 的三种函数、窗口、参数的原始 JSON 类型、错误先后、扁平模型、链算出的值、NaN decay 因子 |
| `test_hybrid_search.py`（第 3 步） | HybridSearch 的平局、两条合并路径、混合度量、各路窗口、子请求的 boost 与 L2、`rank_params` 原始值、错误先后、宽整数、时区与输出、NaN decay 因子 |

13 个 `tie *` 场景（Search + boost 与普通 Search 5、Search + decay 2、Hybrid 6）在全量探针里，也在各步的差分里，ids / scores / 顺序严格断言。

## 参照用例逐 ID 记账

设计「目标用例计数」的 18 条首个阻断用例，在固定统计环境（pymilvus 3.0.2，本批 wheel）里全部通过：

| 来源 | 用例 | 结果 | 本仓 |
| --- | --- | --- | --- |
| `tests/adapter/test_boost_ranker.py` | `test_search_with_boost_ranker_filter`、`..._preserves_vector_output`、`..._filter_field_not_output`、`..._function_score_modes`、`test_search_boost_ranker_with_search_filter_and_limit`、`test_search_boost_ranker_score_affected`、`test_search_boost_ranker_invalid_params`（3 条） | 9 通过 | 已复制（第 1 步） |
| 同上 | `test_hybrid_top_level_boost_ranker_filter_field_not_output` | 通过 | 已复制（第 3 步） |
| `tests/adapter/test_dynamic_field_coverage.py` | `test_dynamic_star_survives_grpc_ranker` / `test_dynamic_star_survives_hybrid_search_group_by` | 2 通过 | 已复制（第 1 步 / 第 3 步） |
| `tests/adapter/test_hybrid_search.py` | `test_search_top_level_decay_uses_hidden_input_field` / `test_hybrid_top_level_decay_uses_hidden_input_field`、`test_hybrid_timestamptz_timezone_and_time_fields` | 3 通过 | 已复制（第 2 步 / 第 3 步） |
| `tests/compatibility` | `TestHybridSearch::test_hybrid_search_basic`、`TestHybridSearchWeighted::test_weighted_ranker`、`TestHybridSearchWithFilter::test_hybrid_search_with_scalar_filter` | 3 通过 | compatibility 套件不复制进本仓，只在固定统计中运行 |

设计列出的另 10 条阻断用例不在本切片，首个阻断不变：公共 function chain 9（`test_function_chain_l2`）、多向量字段 1（`test_issue_fixes::test_multi_vector_fields`）。`test_hybrid_search.py` 里建表用了 BM25 的用例仍被 text analyzer 阻断，不计入通过；INVENTORY 里它们的状态不变。

## 固定统计（本批重跑）

方法同 0025：参照 760 条、上游 90 条、49 项探针，被测为本批 wheel，参照端沿用 0014 收口的同一批输入。

- 参照用例：699 / 760（0025 为 681，**新增 18**，丢失 0）。新增的正是上表 18 条；rerank 与 HybridSearch 不再是任何用例的首个阻断。其余 61 条未通过的首个阻断：text analyzer 39、function chain 9、GEOMETRY 7、算术过滤 4、多向量 1、跳过 1。
- 上游子集：89 / 90，不变（剩 1 条多向量字段）。
- 探针：39 / 49，不变（剩 `multi_vector_hybrid`、`sparse_vector`、`bm25_full_text`、`text_match`）。
- 统计只计首个阻断，数字是诊断样本，不是收益承诺。

## 评审 r0 之后

- 阻断：README、0006、0026 第 2 节与本记录把 model 与 `random_score` 写成无条件 UNIMPLEMENTED。实际只有 Search 与 HybridSearch 顶层通过全部校验的 model、候选真正用到的 `random_score` 返回 UNIMPLEMENTED；HybridSearch 子请求里的 model 只解码、被忽略，没有候选行的 `random_score` 返回空结果，两边相同（差分场景 `route fs model valid`、`fs random_score no hits`）。已逐处改成带条件的表述，运行时行为不变。
- 建议，已采纳：`test_rerank_scenarios.py` 在启动 3.x 服务前清除 `COHERE_API_KEY`，保证 model 场景不调用付费外部服务；登记差异与 0004 的场景另外断言错误一侧没有命中、记下的原始响应都是错误状态且不带任何结果数据（ids、scores、topks、字段、组值与有效位，复审 r1 补全）；引擎分组补充场景的 ARRAY 组值改为逐个展开比较（当前 ARRAY 分组都在类型校验处报 6，结果不变）；变异汇总补记 m07 的补测重做。
- 修复后 `test_rerank_scenarios.py` 在 3.0.2 与 2.6.17 两条线各 2 通过。

## 残余风险与未覆盖

- **13 个平局场景的三平台 CI 未执行**：本机（macOS arm64）全部严格通过，设计要求的 Linux x86_64、Linux arm64 与 macOS 各一次需要推送临时分支，等负责人授权。在这之前，Linux 上的并列次序没有实测。
- **平台**：其余结果也全部在 macOS arm64 本机得出；预期计数（差分 472）须在下次 Compare 与 CI 运行时确认。
- **2.6.17 线**：只跑了 rerank 相关的差分文件与改动的 translation 用例，差分与 Python 的全量未在这条线上跑。
- **参照环境**：全量差分只在未装 tzdata 的 3.x 参照环境跑了一次（第 3 步已在装与不装两种环境各跑一次；本批新增的场景不涉及随 tzdata 变化的时区名）。
- **差分容差**：分数按 1e-5 比较，小于它的数值差别（如只差一次 float32 舍入）不由差分发现；变异验证暴露了 `round_decimal` 这一处，已补精确断言，其他同量级的差别没有逐项证明。
- **性能**：本切片不声称性能提升，没有做基准。Hybrid 的各路在 RPC 线程上顺序执行（0005），路数多时的耗时没有测量。
- **compatibility 套件**：其中 3 条 HybridSearch 用例只在固定统计里运行，不进入本仓的默认套件。
- **README 的表述**：README 对应已发布的 `0.3.0rc3`，本批新增一条「开发线，尚未随预发布交付」的说明，并写明与 3.x 的已知差异与不支持项。

## 证据类别

仓库内可重跑：上述测试命令、`tests/differential/rerank_cases.py` 与 `test_rerank_scenarios.py`（转入的 2986 个场景与分类断言）、各步的差分文件、`test_grpc_translation.py`、`scorecard.py` 与工作流中的预期计数。本地留存：五份探针脚本与 3.x 原始输出（共 4452 个场景）及最终构建上的重跑结果、第 4 步动手前冻结的对照集合与判定（含变更记录）、变异验证的脚本与每项的 diff 和日志、固定统计的原始输出与环境、开发 wheel、2.6.17 线的运行环境、各步骤评审时冻结的 diff。以上结果均未推送，第三方在推送之前无法核对本记录所指的提交。
