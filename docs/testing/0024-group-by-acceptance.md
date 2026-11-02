# 0024：分组检索实施验收

- 状态：设计评审通过，第 1 步（core 与 gRPC 翻译）评审通过并已提交本地 `a767eba`；本记录随第 2 步（差分、参照用例、文档与计数）提交，**未推送、未发布**。
- 日期：2026-11-02
- 合同与设计：[0024](../decisions/0024-group-by-search.md)，继承 0004、0006、0016（范围检索的候选窗口）。
- 仓库基线：0023 B 合入后的 `c542cf1`。
- 参照：milvus-lite v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`，保持只读；SDK 固定 2.6.17 / 3.0.2。
- 开发产物沿用 `0.3.0rc3` 版本字符串，只以 wheel 摘要识别；不是新的发行候选。

## 结论

Search 的 `group_by_field`、`group_size`、`strict_group_size` 已实施。行为以真实 3.x 的 131 个场景为真值：候选窗口 `max(end × group_size × 3, end × 10)`（范围检索再不小于 `end × 5`）、分组后 offset 在展平命中上切片、`group_size` 的 `int()` 解析与 `0 → 1`、`strict_group_size` 的 JSON 真值、空值独立成组、`group_by_field_value` 的字段名 / 类型 / 值 / 有效位、校验顺序与错误文案。3.x 的两个回退取值缺陷不复制，已登记（见下）。不改存储格式、不新增依赖、不加锁。

## 实施范围

| 步骤 | 内容 | 提交 |
| --- | --- | --- |
| 设计 | 调查（参照代码、131 个场景的 3.x 真值）、合同、测试矩阵；评审通过 | 随第 1 步 |
| 1 | core：`GroupBy`、`ReadSnapshot::search_grouped`、窗口与分组、分组值列；gRPC：参数解析、`group_by_field_value` 编码；引擎与翻译测试 | `a767eba` |
| 2 | 差分（场景表加对跑测试）、复制的参照用例、INVENTORY、0006 差异登记、README、预期计数与本记录 | 本批 |

## 登记的差异

0006 差异表新增一行：主键不叫 `id` 时按主键分组（3.x 退化为单个空值组）、可空分组字段叫 `distance` 或 `id`（3.x 的空值行各自成组），本实现按字段值正常分组；极大 `group_size` / `limit`（`1e10` 量级）3.x 长时间无响应，本实现正常返回。差分的 `BUGS` 场景两边的输出都单独断言（ids、分组值、有效位）。

HybridSearch 与 rerank / function chain 内的分组不在本切片，仍为 UNIMPLEMENTED；翻译测试钉住「分组与 rerank 同时出现时报 rerank 的 UNIMPLEMENTED」。

## 构建身份

| 产物 | SHA256 |
| --- | --- |
| macOS arm64 开发 wheel（统计与 2.6 线复验所用） | `4c0f526b31cf20d896b813932a3ec973bedd2a10f4425a3fa829c56f270ab024` |

wheel 由 `a767eba` 之上的工作区构建，与 `a767eba` 相比本批没有产品源码改动（只增加测试与文档）。未发布的开发产物，属本地留存。

## 测试

| 套件 | 结果 |
| --- | --- |
| `cargo test --workspace` | 445 通过（第 1 步新增 7：窗口 16 组 limit × size 组合、strict、offset 切片、负 / 极大 `group_size`、空值组与有效位、字段校验与错误优先级、主键任意名字、范围先于分组、多查询、删除与 flush） |
| `cargo test -p milvus-lite-core --features fault-injection` | 592 通过 |
| Python 端到端 `tests/python`（pymilvus 3.0.2） | 523 通过（517 + 6：复制自参照实现的 5 个分组用例与 `["*"]` 随分组返回的 1 个用例） |
| 同一套件，pymilvus 2.6.17（CPython 3.10，同一 wheel，仓库外运行） | 523 通过 |
| 差分 `test_group_by.py`（场景表 134 个：131 个探针场景，去掉空查询向量一个（该错误由 pymilvus 客户端抛出，不经服务端），加语法错误过滤与非法度量名各两个错误优先级场景） | 3.0.2 与 2.6.17 两条线各通过；其中 4 个 `BUGS` 场景两边输出分别断言，并列场景比较组级不变量，其余逐项相同（主键、距离在 1e-5 的相对 / 绝对容差内（按位置逐个比较，度量相关的 0004 误差与并列顺序规则由现有检索差分负责）、实体、`topks`、`top_k`、`group_by_field_value`） |
| 差分子集（group_by、range_search、scenarios、search_across_sources、databases、partitions） | 3.0.2 线 80 通过 |
| `pytest tests/release tests/ci tests/sharding tests/packaging` | 871 通过 |
| `cargo fmt --all --check`；clippy（默认与 fault-injection）；Windows msvc 交叉检查 | 干净（Windows 检查在第 1 步已实跑，本批只增加 Python 与文档） |

预期计数随新增用例同步：Python 516→517（第 1 步）→523，差分 438→439（会话组 118 不变），互通 250 不变；涉及 `ci.yml`、`compare.yml`、`scorecard.py`、候选自检夹具、分片 README、打包 README、AGENTS 与 0020。

变异验证（第 1 步，逐项恢复后重跑通过）：窗口系数 `×3`、`×10`、offset 条件、strict 判定、空值键、组数上限、校验顺序、组容量，均被 core 测试捕获；差分在窗口系数改为 `×2` 后失败（本批实测并恢复）。

## 固定统计（本批重跑）

方法同 0023 B：参照 760 条、上游 90 条、49 项探针，被测为本批 wheel，参照端沿用 0014 收口的同一批输入。

- 参照用例：659 / 760（0023 B 为 652，**新增 7**，丢失 0）。新增的是 5 条 `test_group_by.py` 的 gRPC 用例、`dynamic_field_coverage` 的 `["*"]` 分组用例和 `edge_cases` 的分组用例；`grouping search` 不再是任何用例的首个阻断。其余 101 条未通过的首个阻断：text analyzer 39、rerank 20、TIMESTAMPTZ 16、数据库 timezone 属性 7、GEOMETRY 7、HybridSearch 6、算术过滤 4、多向量 1、跳过 1。
- 上游子集：89 / 90，不变（剩 1 条多向量字段）。
- 探针：39 / 49（0023 B 为 38），新增 `group_by_search`；剩 `multi_vector_hybrid`、`sparse_vector`、`bm25_full_text`、`text_match`。
- 统计只计首个阻断，数字是诊断样本，不是收益承诺。

## 残余风险与未覆盖

- **平台**：全部在 macOS arm64 本机执行。Linux 两个架构、Intel Mac 与 CI 环境未跑；预期计数（差分 439、Python 523）须在下次 Compare 与 CI 运行时确认。完整 439 条差分本机没有跑，只跑了上表的子集。
- **并列距离**：并列落在分组边界时两个实现可能取到不同的行；差分对 `ties` 场景只比较组级不变量，其余场景在本机上与 3.x 逐项相同，Linux 上的并列次序没有实测。
- **极大值**：`1e10` 量级只由引擎测试覆盖，不与 3.x 对跑（3.x 在该量级长时间无响应）。
- **HybridSearch / rerank 内的分组**：不在本切片。
- **README 的表述**：README 对应已发布的 `0.3.0rc3`，本批新增一条说明「开发线，尚未随预发布交付」，并写明与 3.x 的已知差异。

## 证据类别

仓库内可重跑：上述测试命令、差分场景表与测试（`tests/differential/group_by_cases.py`、`test_group_by.py`）、`scorecard.py` 与工作流中的预期计数。本地留存：探针脚本与 3.x 原始输出（131 场景）、统计的原始输出与环境、开发 wheel、2.6.17 线的运行环境、各步骤评审时冻结的 diff。以上结果均未推送，第三方在推送之前无法核对本记录所指的提交。
