# 0032：过滤表达式中的算术实施验收

- 状态：设计经负责人两轮评审通过；第 1 步（Arrow 列内核与纯 Arrow 开闸，`57cac67`）、第 2 步（Python 值、资源上限并完全开闸，`59a612d`）各自评审通过；第 3 步（真值、模糊对照、差分、参照用例与文档）已实施并复核通过，本记录随第 3 步提交。**未发布**。
- 日期：2026-11-13
- 合同与设计：[0032](../decisions/0032-arithmetic-filters.md)，继承 0006、0014（三条求值路线与有效位边界）、0025（时间戳 ± 区间）、0026（boost 过滤）、0030（稀疏检索的 MemTable 过滤）。
- 仓库基线：第 2 步的 `59a612d`。
- 参照：milvus-lite `1320d26`（v3.2.2 之后），保持只读；组件真值与模糊对照在 Arrow 15.0.0 / NumPy 1.26.4 与 Arrow 25.0.1 / NumPy 2.5.x 两个参照环境各跑一次，差分的参照环境为 Arrow 25。SDK 为 pymilvus 3.0.2 与 2.6.x。
- 开发产物沿用 `0.5.0rc1` 版本字符串，不是新的发行候选。

## 结论

过滤表达式中的 `+`、`-`、`*`、`/` 与一元负号已在三条求值路线上复刻 3.x，用于 Query、count、iterator、Search、稀疏检索、HybridSearch、Delete 与 boost 过滤。行为以真实 3.x 为真值：组件真值逐条一致，随机表达式的直接对照没有不一致，经 SDK 的端到端差分两条线全部通过。与 3.x 的差异只有 [0032「与 3.x 的差异」](../decisions/0032-arithmetic-filters.md#与-3x-的差异) 登记的三项（资源上限、hybrid 推断、只看活行），0006 的差异总表同步新增资源上限一行、在有效位一项补写算术的后果；0025 的④与 0026 的④（boost 过滤中的算术）随之注销。模糊对照另发现一处与算术无关的既有文案差异（ARRAY 上的 LIKE），按负责人决定在本切片修正。

## 实施范围

| 步骤 | 内容 |
| --- | --- |
| 1 | `columns` 的数值内核：pyarrow 的类型提升、按结果宽度回绕、整数转浮点的带检查转换（按操作数自己的有效位）、除法前的 double 转换、一元负号改写为 `0 - x`、ARRAY 与空类型的内核缺失；已有比较的 null 错位检查一并修正；纯 Arrow 路线开闸 |
| 2 | `python_num`：精确整数、正确舍入的 int → float 与真除法、资源预算；`python`：共享的结果值、按值类别穷尽分派的算术、字符串与列表的拼接和重复；`RowFilter` / `RecordFilter` 接入；`FilterResourceExhausted`（code 1）；完全开闸，只保留几何谓词的前置扫描 |
| 3 | 组件真值扩充、直接模糊对照与探针、资源上限的单侧回归、端到端差分、参照 adapter 用例 4 条、计数同步、文档；ARRAY 上 LIKE 的文案修正 |

## 仓库内可重跑的验证

| 套件 | 结果 | 命令与判据 |
| --- | --- | --- |
| Rust 单测与引擎用例 | 655 条通过 | `cargo test --workspace`，0 ignored |
| fault-injection | 792 条通过 | `cargo test -p milvus-lite-core --features fault-injection` |
| fmt 与 clippy（默认与 fault-injection） | 无告警 | 见 AGENTS.md「常用命令」 |
| 算术组件真值 | 1798 条，Arrow 15 / 25 `--verify` 一致 | `generate_filter_arith_fixtures.py --verify`；Rust 一侧为 `tree_tests::arithmetic_matches_reference_corpus` |
| 算术模糊对照 | 种子 32（约 5 万条）与种子 33（约 20 万条），Arrow 15 / 25 各 0 不一致 | `fuzz_arith.py`，见下 |
| 端到端（pymilvus 3.0 / 2.6） | 各 652 条通过 | `tests/python`，其中新迁入的参照用例 4 条 |
| 差分（pymilvus 3.0 / 2.6） | 各 832 条通过，会话组 404 | `tests/differential`；算术场景 40 条，见下 |
| 工具自检 | 956 条通过 | `tests/packaging tests/platform tests/ci tests/sharding tests/release tests/benchmarks` |

以上均在第 3 步的最终代码上串行运行（macOS arm64；pymilvus 3.0.2 与 2.6.18，2.6 线在仓库外目录以构建出的 wheel 运行），原始日志由负责人本地留存。

**组件真值**（[generate_filter_arith_fixtures.py](../../tests/differential/generate_filter_arith_fixtures.py) → [filter_arith.json](../../crates/core/tests/fixtures/filter_arith.json)）用真实 3.x 的 `parse_expr` / `compile_expr` / `evaluate`，按 0032「实施步骤」第 3 条的覆盖清单逐项生成：

- 类型提升全表、回绕、±2^24 / ±2^53 边界、除零与 `nan` / `inf`、`-0.0`、字面量折叠与列值取负；
- null 槽：可空列在 null 槽里存放越界值（Arrow 一侧以 mask 构造，fixture 的 `slots` 记录），以及 null 槽经加减后仍越界的派生列；两侧 null 错位；
- 标量与标量、嵌套的中间类型与派生列再进入 `/`；
- 含 null 元素的 JSON 列表与 ARRAY 的重复、拼接、相等与有序比较；
- 空类型（全 null 的动态键）与 int8 / int16 / float / double 及 `/` 的组合；ARRAY 的内核缺失；
- 一元负号的类型错误（`-flag`、`-s`、`-vector`；`-ts` 由 [timestamp_filter_cases.py](../../tests/differential/timestamp_filter_cases.py) 覆盖）；
- Python 路线结果与比较、`like` 的互操作，bool 与大整数，JSON `NaN` 的身份；
- 整数真除法的舍入：恰在中点向偶数、次正规数、下溢到 ±0、最大有限值、上溢（恰在最大有限值与 2^1024 的中点），两侧不超过 2^53 的快路径与一般路径；
- 字符串与列表重复的四步顺序的每个分支，以及 3.x 必然分配失败的确定点（`'é' * 2**62`、`[None] * 2**62`）；
- 错误先后（左右顺序、and / or 全求值与 Python 短路、可回退与不可回退、回退前后的 OverflowError）、hybrid 的整源回退、`text_match` 与算术同在一个表达式；
- 参照内部测试中的算术表达式逐条转写（见「参照用例」）。

3.x 的 MemoryError（code 1、空文案）对应本实现的资源错误；`REGISTERED_HYBRID_INFERENCE` 中的 40 条登记差异，测试同时断言本实现的结果与 3.x 不同，其余逐条比较掩码、错误文案与阶段。

**模糊对照**（[fuzz_arith.py](../../tests/differential/fuzz_arith.py)，Rust 一侧为 [filter_arith_probe](../../crates/core/examples/filter_arith_probe.rs)）：每张随机表含 INT8 / INT16 / INT32 / INT64 / FLOAT / DOUBLE（随机 null）、BOOL、VARCHAR、ARRAY、JSON 列与 `$meta`，动态键按「全整数 / 全浮点 / 数字 / 非数字 / 混排 / 全缺失」六种取值；每张表生成 25 条覆盖 Arrow、hybrid、Python 三条路线的表达式（深度至多 3 的四则运算与负号、比较、and / or / not、Python 路线的 `like`、少量非法操作数与超出 int64 的字面量）。本实现在内存集合上执行 Query，命中主键与错误文案须与 3.x 相同。两类结果取决于本实现之外的条件，生成时排除：3.x 中长度超过 10^6、又不至于让分配大小越界的字符串 / 列表结果（成败取决于机器内存，约 0.3%，计入 `discarded`）；0032 登记的 hybrid 推断差异（混排取值不含超出 int64 的整数，含 float 时不含 bool），后者由组件真值两边分别断言。报告中的覆盖计数要求三条路线、命中与不命中、各类错误与 MemoryError 都出现。

**资源上限的单侧回归**：第 2 步的边界单测之外，[filter_tree.rs](../../crates/core/tests/filter_tree.rs) 的 `filter_resource_errors_fail_the_request_without_writing_a_delete` 在已 flush 的 segment 有命中、MemTable 才触及上限时，断言 count、Query 与 Delete 都报资源错误，文件、行集与序号（之后写入的 `current_seq`）不变；`python::tests::list_null_elements_become_shared_null` 断言 JSON 列表的 null 元素（serde 节点与平面解码节点）归一为 `Shared::Null`。

**端到端差分**（[test_filter_arith.py](../../tests/differential/test_filter_arith.py)）：数据先写入并 flush 一批（segment），再写入一批留在 MemTable，所以同一个表达式在两个数据源上可能走不同的路线。

- 32 个表达式（Arrow、hybrid、Python 各类）在「segment + MemTable」「全部 flush」「重开」三个阶段，比较 Query、count、分页、query iterator、稠密 Search、稀疏检索、HybridSearch，最后比较 Delete 与剩余行。
- 6 个错误表达式：Arrow 转换失败（MemTable 才有越界值，segment 先求值且有命中）、字面量超出 int64、int 转 float 溢出、重复次数放不进 `Py_ssize_t`、资源上限；各消费者都失败，Delete 不落盘，flush 后行集不变。资源上限一条两边分别断言（本实现 code 1 固定文案，3.x code 1 空文案）。稀疏检索在 MemTable 上逐行走 Python 路线，Arrow 转换失败在那里不出现，两边都成功，这是 3.x 自身的行为。
- 按数据源分路线：hybrid 的 `x * x == 0` 在全整数的 segment 上回绕为 0、在含字符串的 MemTable 上精确计算；稀疏检索的 `f * f` 在 MemTable 上按 double、flush 后按 float32，两边一致之外断言这两个现象确实出现。
- 死行改变 hybrid 推断：已删除的字符串行使 3.x 整源回退 Python，本实现只看活行、走 Arrow，`x * x == 0` 的结果不同；删除前两边一致，删除后与重开后两边分别断言。

rerank 中三条 boost 过滤的算术场景（[test_rerank_scenarios.py](../../tests/differential/test_rerank_scenarios.py)）在第 2 步已由断言 UNIMPLEMENTED 改为两边对照。

## 参照用例

- adapter：从 `1320d26` 的 `tests/adapter/test_milvus_compat_v2.py` 复制 `TestArithmeticFilters` 的 4 条（0032 列出 3 条，同类的 `test_multiply_in_filter` 一并复制）到 [test_milvus_compat_v2.py](../../tests/python/adapter/test_milvus_compat_v2.py)，用例本体与 helper 与上游逐行一致，起服务的 fixture 按本目录 conftest 替换；逐条状态见 [INVENTORY.md](../../tests/python/adapter/INVENTORY.md)。
- 内部测试只作真值来源（直接导入 3.x 的 `parse_expr`、`compile_expr`、各 backend 与 `Collection`，不能复制运行），其中的算术表达式逐条转写进组件真值（score → `d`，title / category → `s`，active → `flag`，`$meta["score"]` → `$meta["x"]`）：
  - [tests/search/filter/test_e2e.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/tests/search/filter/test_e2e.py)：表达式表中 F2a 的算术与组合项（140–171 行）、`test_meta_arithmetic` 与 `_META_DIFF_EXPRS` 中的两条 `$meta` 算术；
  - [tests/search/filter/test_semantic.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/tests/search/filter/test_semantic.py)：F2a 的算术类型检查（261–297 行，含字符串、bool 操作数的拒绝与一元负号）与 `test_meta_in_arithmetic`；
  - [tests/engine/test_collection_filter.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/tests/engine/test_collection_filter.py)：F2a 的 Search / Query 算术、`-age < -25`、与 LIKE 的组合、字符串字段算术的拒绝（402–497 行）与 `test_query_meta_arithmetic`。

## 变异验证

第 1、2 步的变异见各自的提交说明。第 3 步：

| 变异 | 抓住它的用例 |
| --- | --- |
| 撤回 ARRAY 上 LIKE 的文案修正 | `arithmetic_matches_reference_corpus`（新增的 4 条） |

## 与 3.x 的差异与残余风险

差异只有 0032 登记的三项，复述如下：

- **资源上限**：单值与每行累计 64 MiB（按本实现实际分配的字节）、整数 2^20 位（按估算）；3.x 没有上限，内存不足时报 MemoryError。
- **hybrid 动态列推断**：数字与其他类型混排时本实现整源回退 Python，3.x 按元素顺序推断与报错。复刻推断另开切片，排在本切片之后、GEOMETRY 之前，届时注销 0006 / 0032 的登记与 `REGISTERED_HYBRID_INFERENCE`。
- **只看活行**：沿用 0006 / 0014 的登记，算术使它还会改变数值。

接受的残余风险：

- 资源上限只约束内存，不约束 CPU 与时间：在大结果上做 LIKE、在百万行上逐行构造接近上限的值，或 `$meta` 中 4300 位到预检上限之间的整数每行都做一次平方复杂度的十进制解析，一个请求仍可能长时间占用工作线程且不可取消（3.x 同样如此）。
- `Arc` 控制块（固定几十字节）与 `num-bigint` 内部的分配在内存耗尽时 abort；能保证不 abort 的只有经 `try_reserve_exact` 的数据分配。

## 性能

不对算术过滤作性能声明。纯比较路径的回归按 [S1 专项](../../benchmarks/s1/README.md) 的口径检查，只比较本实现改动前后（改动前为第 1 步之前的 `5e26904`，改动后为第 3 步的最终代码；同机、同一个库、Python 3.12 / pymilvus 3.0.2 / NumPy 2.5.3），两规模各三轮、轮换顺序，表中为三轮 p50 的中位数（最小–最大），单位 ms：

| 行数 | 项目 | 改动前 | 改动后 |
| --- | --- | --- | --- |
| 10 万 | 计数 `cat >= 0` | 0.20（0.20–0.21） | 0.22（0.19–0.22） |
| 10 万 | Query limit 100 | 0.26（0.26–0.26） | 0.26（0.25–0.26） |
| 10 万 | 过滤 Search（整数） | 0.40（0.40–0.41） | 0.40（0.40–0.41） |
| 10 万 | 过滤 Search（字符串） | 0.39（0.39–0.40） | 0.39（0.39–0.39） |
| 100 万 | 计数 `cat >= 0` | 0.38（0.38–0.40） | 0.37（0.36–0.38） |
| 100 万 | Query limit 100 | 0.84（0.82–0.85） | 0.83（0.83–0.85） |
| 100 万 | 过滤 Search（整数） | 1.64（1.62–1.68） | 1.63（1.60–1.67） |
| 100 万 | 过滤 Search（字符串） | 1.36（1.33–1.37） | 1.36（1.35–1.39） |

两版本的范围相互重叠，没有看到退化。这只是回归检查，不与 2.5.1 / 3.x 比较，也不构成性能结论；原始输出由负责人本地留存。
