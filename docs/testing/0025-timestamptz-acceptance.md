# 0025：TIMESTAMPTZ 与 timezone 实施验收

- 状态：设计评审通过；第 1 步（类型与存储）、第 2 步（过滤）、第 3 步（时区与属性 RPC）各自评审通过并提交到本地；本记录随第 4 步（差分、参照用例、文档与验收）提交。**未推送、未发布**。
- 日期：2026-11-03
- 合同与设计：[0025](../decisions/0025-timestamptz-and-timezone.md)，继承 0004、0006、0014（过滤语法）、0015（nullable 与默认值）、0023（集合与数据库属性）。
- 仓库基线：分组检索合入后的 `4bfd87e`；本地提交依次为 `d8bdfd5`（第 1 步）、`229bb9d`（第 2 步）、`abb1f15`（第 3 步）。
- 参照：milvus-lite v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`，保持只读；SDK 固定 2.6.17 / 3.0.2。参照端在 macOS 上运行，时区库是系统 tzdata。
- 开发产物沿用 `0.3.0rc3` 版本字符串，只以 wheel 摘要识别；不是新的发行候选。

## 结论

标量类型 TIMESTAMPTZ（UTC 微秒、以 `Z` 结尾的 ISO 输出、nullable 与默认值、`INVERTED` 定义）、过滤中的 `ISO` / `INTERVAL` 字面量与运算、请求 / 集合 / 数据库三级 `timezone`（`fold=0` 换算）、`time_fields`、集合与数据库的 `timezone` 属性已实施。行为以真实 3.x 为真值：探针 655 个场景在最终构建上重跑，与 3.x 的差异只剩已登记的 43 项（逐项归因见下）；差分用例逐项比较完整结果（含错误码与文案）。时区库为内置 tzdata（jiff 0.2.38），不依赖运行平台。存储格式沿用 3.x 的 `timestamp[us, tz=UTC]` 与 `schema.json` 的 `timestamptz`，不新增私有格式；新增依赖只有 jiff。

## 实施范围

| 步骤 | 内容 | 提交 |
| --- | --- | --- |
| 设计 | 调查（参照代码、探针对跑真实 3.x）、合同、测试矩阵；评审通过 | 随第 1 步 |
| 1 | `Timestamptz` 类型、`schema.json` / Arrow / WAL / Parquet、gRPC 编解码与输出、默认值回显、`ARRAY<TIMESTAMPTZ>` 拒绝、整数范围校验；互通用例 | `d8bdfd5` |
| 2 | 过滤：ISO / INTERVAL 求值、类型检查、`in`、加减与回绕、三条求值路线的差异；差分表 237 个表达式 | `229bb9d` |
| 3 | 时区：jiff 与 `fold=0` 换算、`TimezoneSource` 与锁边界、三级时区与两个校验点、`time_fields`、集合与数据库 `timezone` 属性、type 21 的逐行解析；时区 fixture | `abb1f15` |
| 4 | 探针转差分、复制参照用例、差异登记、README / AGENTS、本记录、固定统计、第 3 步评审遗留项的处理 | 本批 |

第 4 步同批处理的产品代码改动（均有测试钉住，均为第 3 步评审遗留）：数据库属性文件里 `timezone` 校验不过的加载不缓存，文件修正后下次调用重新读取（3.x 加载失败不缓存）——Describe / Alter 与所有读取数据库时区的操作（naive 写入、`time_fields`、naive 默认值）共用同一个带校验的加载器，空串属性与 3.x 一样报 6（第 4 步首轮评审指出读取路径曾绕过校验，已修复）；`Collection::new_in` 的校验与归一化共用一次数据库时区读取，避免并发 `AlterDatabase` 时看到改前改后两个值；type 21 的无偏移字符串在配置的时区本身不可用时向上传 3.x 的错误，不再被当成「越界」换成占位而报出误导性的 `date value out of range`（差分用例先失败后通过）。

## 登记的差异

正式登记于 [0025「与参照的差异」](../decisions/0025-timestamptz-and-timezone.md#与参照的差异)，0006 差异表新增一行汇总：原始 gRPC 越界整数微秒（含使用越界整数默认值的写入）整批拒绝；内置 tzdata 与时区名写法（大小写不敏感，`posixrules` / `localtime` / `zone.tab` 等平台文件按找不到）；`ARRAY<TIMESTAMPTZ>` UNIMPLEMENTED；不涉及时间值的算术仍 UNIMPLEMENTED；手改的 `schema.json` 字符串默认值按范围外 schema 处理；手改的数据库属性文件里 `timezone` 不可用时，只有需要时区的操作和 Describe / Alter 报错。0023 取舍 1 与 0006 的属性文件语义行已同步。

## 构建身份

| 产物 | SHA256 |
| --- | --- |
| macOS arm64 开发 wheel（统计、2.6 线与互通复验所用） | `c2d417ec3b715f4b47d2f2d9fe9341d2802cf9bb4da7e45f342341fab72f0f8c` |

wheel 由 `abb1f15` 之上带第 4 步产品代码改动的工作区构建（`crates/core/src/db.rs`、`collection.rs`，`crates/grpc/src/insert.rs`，上节三项），随第 4 步提交。未发布的开发产物，属本地留存。

## 测试

| 套件 | 结果 |
| --- | --- |
| `cargo test --workspace` | 475 通过（第 3 步为 473，新增 2：创建集合时校验与归一化只读一次数据库时区；数据库时区读取路径对未知名字 / 空串报错且不缓存；数据库属性测试补「文件修正后重新读取」） |
| `cargo test -p milvus-lite-core --features fault-injection` | 621 通过（第 3 步为 619） |
| Python 端到端 `tests/python`（pymilvus 3.0.2） | 547 通过（525 + 22：复制自参照实现的 16 个 TIMESTAMPTZ 用例与此前因数据库 `timezone` 属性阻断的 6 个数据库用例） |
| 同一套件，pymilvus 2.6.17（CPython 3.10，同一 wheel，仓库外运行） | 547 通过 |
| 差分 `tests/differential`（3.0.2 线，全量） | 457 通过（最后一次改动用例之后顺序单独运行 991 秒，无其他负载）。此前一次在同机并行跑 clippy、cargo 与 wheel 构建时，`test_cli_stop_policy_flushes_or_recovers_the_same_logical_data` 等待子进程退出超过 30 秒而失败，单独重跑通过；评审修复后的全量重跑无失败 |
| 差分，pymilvus 2.6.17 线 | 受影响的 5 个文件（`test_timezone.py`、`test_timestamptz.py`、`test_timestamp_filters.py`、`test_databases.py`、`test_collection_properties.py`）27 通过；全量未在这条线上跑 |
| 互通 `tests/storage_interop`（Arrow 15 / 25） | 255 通过（评审修复后重跑，425 秒；本批无新增） |
| `pytest tests/release tests/ci tests/sharding tests/packaging` | 871 通过 |
| 分片计划核对（`tests/sharding/shard.py plan --expect 457 --expect-group 118`） | 457 / 118，与工作流中的预期一致 |
| 时区 fixture `generate_timezone_fixtures.py --verify`（CPython 3.12.14） | 评审修复后重跑，退出码 0（脚本成功时不输出） |
| `cargo fmt --all --check`；clippy（默认与 fault-injection，`-D warnings`）；core / grpc 的 Windows msvc 目标 `cargo check`（含 core 的 fault-injection） | 评审修复后重跑，干净（整个 workspace 的 Windows 检查会被 py crate 的 `build.rs` 按设计拒绝，未做） |

预期计数随新增用例同步：Python 525→547，差分 447→457（会话组 118 不变），互通 255 不变；涉及 `ci.yml`、`compare.yml`、`scorecard.py`、候选自检夹具、分片 README、打包 README、AGENTS 与 0020。

变异验证（评审修复后逐项做，改前备份、改后恢复并核对）：去掉数据库属性校验失败时的缓存清除，`invalid_stored_zone_fails_every_zone_read_and_is_not_cached` 与 `unknown_timezone_in_the_file_fails_describe_and_alter_like_3x` 失败；让 `database_timezone_of` 绕过校验加载器（评审指出的缺陷），前一个核心测试失败；去掉 `new_in` 的读取记忆（读取变两次），`creation_reads_the_database_zone_once_for_validation_and_normalization` 失败。type 21 的路径错误修复由差分用例在修复前失败、修复后通过钉住（第 4 步首次实现时的观察）。

### 探针对跑

第 3 步及此前的 7 份探针脚本在最终构建上重跑，与保存的 3.x 原始输出逐场景比较：

| 探针 | 场景 | 与 3.x 的差异 | 归因 |
| --- | --- | --- | --- |
| MilvusClient 全面探针（取值形态、过滤、请求时区、夏令时、属性、`time_fields`、杂项） | 269 | 4 | `ARRAY<TIMESTAMPTZ>`（UNIMPLEMENTED）；`zone.tab`、`tzdata.zi`、`posixrules` 三个平台文件名（3.x 在 macOS 上分别报非 TZif 文件或接受，本实现按找不到 6） |
| 原始 gRPC（数据槽、越界整数、区间溢出、响应形态） | 90 | 29 | 5 个越界整数写入本实现拒绝（3.x 写入成功）；其余 24 个是 3.x 读取被毒化的集合而报 1，本实现读取同一批请求成功。无毒化的对照集合上的场景全同 |
| 表达式语义 | 42 | 0 | |
| 请求时区与 `time_fields` 的校验时机、错误先后 | 39 | 0 | |
| type 21 字符串与越界默认值 | 34 | 4 | 使用越界整数默认值的写入本实现拒绝、3.x 写入成功，及其后的读取 |
| 三条求值路线（Arrow / Hybrid / Python） | 147 | 6 | 不涉及时间值的算术（`n + 1` 一类）本实现 UNIMPLEMENTED |
| type 21 无偏移字符串、Delete / Upsert、数据库时区即时生效、naive 默认值 | 34 | 0 | |

探针脚本与 3.x 的原始输出由负责人本地留存，不入库；入库的是由它们整理出的差分用例。

### 差分用例的覆盖

| 文件 | 内容 |
| --- | --- |
| `test_timestamptz.py` | 43 种写入取值形态、输出格式、nullable / 默认值 / Upsert / Delete / 描述、`schema.json` 字节相同、保留字、原始 gRPC 整数微秒与越界默认值（已登记差异单独断言）、type 21 字符串（含集合时区、混合批次、解码错误先后、时间列在向量列之前与之后两种字段顺序、`error_code`；type 21 由引擎事后解析，坏向量列总是先报，type 26 在 adapter 按列顺序解析，列顺序决定先后，两者都由断言区分） |
| `test_timestamp_filters.py` + `timestamp_filter_cases.py` | 239 个过滤表达式（含 ARRAY 在 Hybrid 路线上的回退）的结果类别由 3.x 给出，两边逐项比较；count / search / delete / 动态字段在各请求上的表现 |
| `test_timezone.py` | 请求时区矩阵（18 个平台无关的取值）与平台相关名字（仅本实现一侧）；夏令时 / 历史 / 极端时区 31 个样本的写入与过滤字面量；集合与数据库时区属性（创建、描述、修改、删除、默认值、对已有集合的即时影响）；Delete / Upsert / type 21 与数据库时区；校验时机与错误先后（37 项）；`time_fields` 输出（含 Query iterator）；原始 gRPC 数据槽与响应形态；索引、主键、分区键、分组字段；手改属性文件里不合法的 `timezone`（路径形态、未知名字、空串）：两边各自的表现、数据错误（不合法或越界的时间字符串）先于属性错误、文件修正后不缓存 |
| `test_databases.py`、`test_collection_properties.py` | 数据库与集合 `timezone` 属性的校验、保存、删除，属性文件里未知名字的 Describe / Alter |

3.x 一侧随平台变化的时区名（名字大小写变体、平台文件、`Factory`、旧别名）不进对比；比较只取 `zone.tab` 里的区域、`Etc/` 与 `UTC`，避免 Linux 发行版把旧别名拆进 tzdata-legacy 带来的差异。

## 固定统计（本批重跑）

方法同 0024：参照 760 条、上游 90 条、49 项探针，被测为本批 wheel，参照端沿用 0014 收口的同一批输入。

- 参照用例：681 / 760（0024 为 659，**新增 22**，丢失 0）。新增的是 16 个 TIMESTAMPTZ 用例与 6 个数据库 `timezone` 属性用例；TIMESTAMPTZ 与数据库 `timezone` 不再是任何用例的首个阻断。其余 79 条未通过的首个阻断：text analyzer 39、rerank 20、HybridSearch 7、GEOMETRY 7、算术过滤 4、多向量 1、跳过 1。
- 上游子集：89 / 90，不变（剩 1 条多向量字段）。
- 探针：39 / 49，不变（剩 `multi_vector_hybrid`、`sparse_vector`、`bm25_full_text`、`text_match`）。
- 统计只计首个阻断，数字是诊断样本，不是收益承诺。

## 残余风险与未覆盖

- **平台**：全部在 macOS arm64 本机执行。Linux 两个架构、Intel Mac 与 CI 环境未跑；预期计数（差分 457、Python 547）须在下次 Compare 与 CI 运行时确认。
- **Linux 上的 3.x 与时区库**：参照端的 tzdata 随平台与版本变化。差分只比较平台稳定的名字，但夏令时样本的预期值在 macOS 上得出；Linux 参照端的 tzdata 版本若在这些样本上有规则差异，Compare 会失败而不是静默通过。时区换算的另一道对照是对 CPython 3.12 zoneinfo 生成的常驻 fixture（38 个区域，988 个本地时间与 418 个瞬间），该 fixture 的 `--verify` 本批已重跑；第 3 步另做过 597 个区域在范围两端的一次性核对，这一项本批没有重跑。
- **全量差分**：评审修复后在 3.0.2 线单独跑过一次全量（457 通过）；2.6.17 线只跑了受影响的文件，全量未在这条线上跑。
- **未做**：wheel 与 sdist 的安装验收、发行物检查；`ARRAY<TIMESTAMPTZ>`、HybridSearch 的 `time_fields`、Hybrid / rerank 内的分组不在本切片。
- **README 的表述**：README 对应已发布的 `0.3.0rc3`，本批新增一条「开发线，尚未随预发布交付」的说明，并写明与 3.x 的已知差异与降级行为。

## 证据类别

仓库内可重跑：上述测试命令、差分用例与场景表（`tests/differential/test_timezone.py`、`test_timestamptz.py`、`test_timestamp_filters.py`、`timestamp_filter_cases.py`）、时区 fixture 及其 `--verify`、核心与翻译层测试、`scorecard.py` 与工作流中的预期计数。本地留存：探针脚本与 3.x 原始输出（655 场景）、统计的原始输出与环境、开发 wheel、2.6.17 线的运行环境、各步骤评审时冻结的 diff。以上结果均未推送，第三方在推送之前无法核对本记录所指的提交。
