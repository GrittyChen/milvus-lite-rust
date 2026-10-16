# 0015 B：ARRAY 切片验收

- 状态：负责人切片 B 评审通过并收口（2026-10-16）；不替代 0013 最终候选验收
- 日期：2026-10-16
- 合同：[0015](../decisions/0015-nullable-defaults-and-array.md)
- 参照：milvus-lite v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`
- 前置：切片 A 已由负责人全范围复审通过，JSON 写入修复一并收口。本记录只验收其后的 B 增量
- 发布线：0.2.0 开发线；本轮不切换包版本、提交、打 tag 或发布

## 实现与产物身份

B 接入 BOOL、INT8/16/32/64、FLOAT、DOUBLE、VARCHAR 元素的 ARRAY。存储列使用 offsets 和有类型子列，父列与子列 validity 分开；已有列的全有效快路径保持。沿用 3.x 的 schema.json、Arrow IPC WAL、Parquet 与 schema_format_version=2，没有新字段或私有空值格式，没有新增依赖、线程或锁。

| 身份 | SHA256 |
| --- | --- |
| 已通过评审的 A 原生库 | `d1d8cdfa2b89f9ec30902d91a1679260ff4cff3d7777a254b74b93ff966e23e4` |
| B3 原生库 | `283da7afb42a0c9be412f1c3eaba049186e05fd15ed93a09b337d47f7507e7b8` |
| B3 macOS arm64 abi3 wheel | `96101e996440349d276d2c2cea866e33eded86329c95447c56be5949576522b1` |
| B3 sdist | `a35ae27dbac51d5edbaacea0abb15f72a5502332ab95b44e66306f69196b4ff9` |
| B3 的 76 项源码指纹清单 | `3a90a72353c50ad3f24631c01a4a8e3c8a09060adb8f7a1e111e2927154a881f` |

产物版本暂沿用 `0.1.0rc3` 元数据以遵守本轮不切版的约定，但内容是未发布的开发构建；不能把它当作已发布 rc3。原始记录、wheel、venv、数据目录和变异副本均在仓库外，目录名为 `mlr-0015-array`；固定统计在同级原调查工作区的 `array-b3-final*` 运行目录。B1/B2 是验收中间构建，不能与本表身份混用。仓库开发环境已安装同一 B3 wheel，实测原生库哈希一致，并通过 8 项 ARRAY 安装后冒烟。

## 已冻结的语义

- [ARRAY 组件真值](../../crates/core/tests/fixtures/array_v321.json) 包含 336 项 schema/default/业务输入、152 项 Arrow 元素转换和 768 项三路线过滤；由 [生成器](../../tests/differential/generate_array_fixtures.py) 对真实 Arrow 15/25 核验。业务矩阵包括缺失、None、空数组、正常/错误元素、元素 null、容量超限及空/非空列表默认值。
- 全部记录的主键、向量、外层数组/容量等业务检查完成后才做元素转换。Insert 和 Upsert 的原始 proto 同时覆盖后行主键错误与前行元素越界；失败批次不写入任何行。元素有效位长度在 Rust 输入边界也检查。
- 原始 proto 按行 ScalarField 的实际槽位解码；ArrayArray.element_type 不覆盖 schema。父 valid_data 接受紧凑/完整布局。参照会忽略 ARRAY 建表参数 max_length，VARCHAR 元素不做服务端长度校验；Describe 对数组默认值不编码 ValueField。
- 空父列、空数组、元素 null 独立保存。含元素 null 的存量数组可由引擎读取并参与过滤；实际投影该数组时，gRPC 按参照报 code 1，错误原文依元素类型不同。不投影该列的 count/过滤可正常执行，不把子 null 转成零值。
- MemTable/WAL 的 Arrow 子字段名为 item，segment 的 PyArrow 诊断为 element；静态比较/in 的错误原文保留这一区别。hybrid 的失败仍回退整个表达式。NaN 元素的成员判断保留同一对象身份，直接相等比较仍为假。
- 新建 ARRAY INVERTED 被拒绝；人工存量定义在只有 WAL 时可 Load，存在非空 segment 且需要构建时按参照报 code 1。其它标量的 INVERTED 定义与数组过滤组合继续扫描，不宣称新增真实倒排执行。

## 验证矩阵

下列均指 B3 原生库；完整日志与 JUnit 在仓库外保存，不以中间构建代替最终构建。

| 检查 | 结果与边界 |
| --- | --- |
| fmt / Clippy workspace all-targets `-D warnings` | 通过 |
| Rust workspace / core fault-injection | 310 / 396 passed |
| Python 全套，pymilvus 3.0.2 / 2.6.17 | 各 420 passed，分别使用 CPython 3.12.14 / 3.10.21 |
| 与真实 3.x 差分，两条 SDK | 全套各 276 passed；收集后增加的原始响应与索引组合 10 项分别补跑通过，共 286 个不同用例，无跳过 |
| 存储互通，两条 SDK | 各 184 passed；包括新增 ARRAY 15 组 |
| ARRAY 真值 | Arrow 15.0.0 / 25.0.1 各核验通过；336 + 152 + 768 项，fixture SHA256 `a55ea841915a5b060e3c42969f64e9098de929001cf4daed27a91f8aea9e8e38` |
| 受控故障 | 带父/元素 null 的 WAL 正常进程退出、残批/空间不足；Flush 的 manifest replace 前后、删 WAL 后；compaction replace 前后，共七种状态通过。逐项检查主键、父 null、空列表、元素 null、默认值与删除结果 |
| rc1/rc2/rc3 回退 | B3 实际写出的 WAL / segment / compacted × 三旧版，共 9 组通过；旧能力可读，新属性明确拒绝，受保护 collection 的文件名与内容不变，拒绝后健康集合仍可读 |
| 反证 | 两个隔离源码副本分别把父 null 变空数组、忽略写盘时的元素有效位，均由 ARRAY 业务状态/Arrow 断言拒绝（exit 101）；独立 CARGO_TARGET_DIR，不改变正式实现 |
| 工具自检 | benchmarks / packaging / platform 共 230 passed；固定统计工具另 9 项自检通过 |
| wheel / sdist | 正式无 FI 入口；arm64 / abi3 / macOS 11.0 标签及二进制检查通过。sdist 的 75 项在包产品源码与工作区指纹一致，ARRAY fixture 和 Rust 测试齐全；开发用 rust-toolchain.toml 按 0013 故意不随 sdist 发布 |
| 固定统计 | 见下文；完整样本允许已登记范围外失败，不能当作全绿默认套件 |
| 性能 | 36 次旧能力专项 + 24 次 JSON 四方轮换 + 9 次 ARRAY 密度 + 12 次 S1 交错复测均完成，见下文 |

[引擎生命周期](../../crates/core/tests/array.rs) 还覆盖 Arrow slice 的 offsets 归零、八种父/子有效位、默认值、真实合并、保留旧内存快照、Upsert/Delete；[互通矩阵](../../tests/storage_interop/test_arrays.py) 由真实 Arrow 15/25 读文件、3.x 引擎交替写入，并恢复被 Rust 删除/合并期间钉住的真实 snapshot。原始 proto 的输出比较包括 Query 空结果/全 null/空数组和 nq=2 Search 的数组 FieldData 序列化字节，不只比较 SDK 归一后的值。

## 固定统计收口

固定输入、外部源码和样本集合保持 [统计工具](../../tests/compatibility_audit/README.md) 的 commit 与校验和；没有客户端干预，没有更换样本、跳过失败或改写断言。

| 样本 | rc3 | A | B3 |
| --- | --- | --- | --- |
| adapter / compatibility，760 条 | 375 passed | 489 passed | 531 passed |
| 上游 Milvus 固定 90 条 | 75 passed | 75 passed | 75 passed |
| 固定调用探针，49 项 | 25 成功 | 26 成功 | 29 成功 |

rc3 / A 是各自已归档记录；本次重跑的是 B3 三组。逐条身份核对，A → B3 新增 42 个通过，全部原先因 `ARRAY fields` 明确拒绝，原先通过的用例没有退回。B3 的其余 224 failure、4 error 都归类为已登记的 UNIMPLEMENTED，另 1 skipped 是原样本中的 nullable 向量跳过；不加入默认套件，也不把样本比例当作全功能兼容率。

剩余样本首个阻断中，partition key 38、CreatePartition 23、GetPartitionStatistics 1；query iterator 8、range search 8；alias / rename 各 3。它们仍是首阻断统计，不能承诺补一项后全数转绿。B 评审收口后已完成 [partition 真值/收益调查](0016-partition-investigation.md)，负责人确认先 Query iterator、再 range search，分区延后冻结设计，见 [0016](../decisions/0016-query-iteration-and-range-search.md)；BM25 继续独立里程碑，真实 ANN 仍由数据规模驱动。

## 性能回归

软件测试和构建均退出后，在同机 macOS 27.0.1 / arm64、10 逻辑 CPU、CPython 3.12.14 / pymilvus 3.0.2 / NumPy 2.5.4 / psutil 7.2.2 下，顺序运行既有 S1、批量检索、恢复专项。复用 A 的 10 万/百万行、128 维、全存活且已 Flush 的物理目录与独立真值，rc3 / B3 三轮交换先后顺序，共 36 次。各轮均核对结果与原生库哈希；没有与测试、编译或其它基准并发计时。桌面负载未完全隔离，不作加速声明。

以下为各轮 p50 的中位数 [最小值, 最大值]；时延 ms，ready s，RSS MiB。

| 行数 | 指标 | rc3 | B3 |
| --- | --- | --- | --- |
| 10 万 | S1 count | 0.25 [0.25, 0.38] | 0.25 [0.24, 0.27] |
| 10 万 | S1 Query limit100 | 0.40 [0.36, 0.49] | 0.35 [0.35, 0.38] |
| 10 万 | 整数过滤 Search | 0.56 [0.56, 0.57] | 0.57 [0.54, 0.68] |
| 10 万 | 字符串过滤 Search | 0.59 [0.54, 0.63] | 0.59 [0.55, 0.62] |
| 10 万 | nq=1 | 0.95 [0.90, 0.97] | 0.90 [0.90, 0.92] |
| 10 万 | nq=100 | 27.42 [27.09, 27.68] | 26.73 [26.55, 27.42] |
| 10 万 | 恢复 ready | 0.145 [0.142, 0.146] | 0.145 [0.132, 0.146] |
| 10 万 | 加载后 RSS | 116.8 [115.6, 120.6] | 119.3 [113.4, 119.4] |
| 10 万 | 检索后 RSS | 117.1 [115.9, 120.9] | 119.8 [113.8, 119.9] |
| 百万 | S1 count | 0.72 [0.70, 0.74] | 0.70 [0.62, 0.77] |
| 百万 | S1 Query limit100 | 1.68 [1.59, 1.88] | 1.63 [1.51, 1.76] |
| 百万 | 整数过滤 Search | 3.12 [3.11, 4.02] | 3.06 [2.96, 3.24] |
| 百万 | 字符串过滤 Search | 2.50 [2.33, 2.76] | 2.67 [2.18, 2.69] |
| 百万 | nq=1 | 4.95 [4.91, 5.44] | 4.94 [4.93, 5.13] |
| 百万 | nq=100 | 210.77 [200.01, 214.97] | 200.85 [197.35, 208.76] |
| 百万 | 恢复 ready | 1.118 [1.118, 1.123] | 0.972 [0.957, 0.998] |
| 百万 | 加载后 RSS | 827.7 [827.1, 834.5] | 823.8 [823.0, 824.2] |
| 百万 | 检索后 RSS | 828.3 [827.7, 835.0] | 824.5 [823.7, 824.9] |

首组三轮的百万行字符串过滤中位数上升 6.8%，逐轮方向不一致且范围重叠，保留原数据后补六轮交错复测：rc3 为 2.25 [2.17, 2.70] ms，B3 为 2.17 [2.08, 2.29] ms。该偏高没有在补测中复现；当前限定负载未发现稳定退化，不推导任意场景都无回归。

### JSON 写入

使用仓内 [JSON 写入工具](../../benchmarks/json-write/README.md)，每批 5000 行、每行约 2.7 KB，raw 请求预序列化与 SDK 端到端各计时；三轮轮换 rc3 / 已通过评审的 A / B3 / 真实 3.x，每项每轮 5 个样本，共 24 次。逐批核对写入行与内容、payload / 请求 SHA、解释器 / SDK 版本和原生库身份。下表为每轮最小值的中位数 [轮间范围]，单位 ms。

| 构建 | raw（预序列化） | SDK 端到端 |
| --- | --- | --- |
| rc3 | 206.06 [199.66, 214.88] | 320.14 [311.27, 320.42] |
| 已通过评审的 A | 199.30 [198.12, 209.12] | 313.38 [311.03, 327.62] |
| B3 | 203.47 [198.01, 208.06] | 313.17 [308.37, 317.25] |
| 3.x | 139.77 [132.79, 141.65] | 267.64 [259.26, 269.14] |

本轮未复现 JSON 写入退化，A 的性能修复保持；rc3 / B3 与 3.x 的既有写入差距仍存在，沿用 A 的独立跟进条目，不在 B 增量优化，也不以此表宣称跨实现加速。

### ARRAY 新场景

额外测 10 万行、128 维、INT64 ARRAY 的 0% / 1% / 50% 父空值，非 null 行含空列表和两个普通元素，每种三轮交换密度顺序，共 9 次。谓词期望计数、普通过滤的查询行集与检索距离均由输入独立计算并逐次核对。以下为 p50 中位数 [轮间范围]，单位 ms；新能力没有 rc3 功能基线。

| 父空值密度 | is null count | array_length == 0 count | array_contains(3) count |
| --- | --- | --- | --- |
| 0% | 3.432 [3.412, 4.501] | 30.312 [30.142, 33.060] | 36.837 [36.398, 37.502] |
| 1% | 3.908 [3.906, 4.173] | 29.572 [29.033, 29.663] | 37.941 [36.572, 39.666] |
| 50% | 3.348 [3.339, 3.392] | 29.670 [29.416, 29.682] | 28.024 [26.079, 28.940] |

此密度进程加载后 RSS 中位数分别为 245.95 / 246.52 / 244.25 MiB，包含 NumPy 真值和客户端缓冲，不能与前表独立恢复进程的 RSS 混比。该场景不包含元素 null 的 gRPC 投影、不包含其它元素类型的吞吐结论。准备脚本的首轮语法错误发生在建库和计时前，已修正后使用独立输出目录完整重跑，失败日志保留；产品源码和断言没有因此改变。

旧能力记录在外部 `perf/paired-array-b3`，JSON 在 `performance`，ARRAY 在 `perf/array-density-final`，补测在 `s1-recheck`。各报告保存工具/数据/原生库指纹与轮次，外部 `evidence-sha256.json` 汇总关键证据。ARRAY 密度诊断脚本沿用 A 的输入/真值计算方式，只作限定场景证据，未加入默认测试或发行包。

## 调用链与同类扫描

- 使用 `rg` 全域搜索 `ColumnData::`、`InputValues::`、`DataType::`、`element_type` 和 `max_capacity`，保存消费方清单。按 A 的源码清单比对 B 的 18 个产品文件增量；未用只读评审中的工作区总 diff 混淆 A/B。
- 核对 schema → 原始列解码/掩码 → 全部逐行业务检查 → 元素转换 → WAL → MemTable → frozen/segment → recovery/compaction/snapshot；Input/Column 的取行、拼接、合并、投影与 Upsert 反向取值均覆盖 ARRAY。主键和向量仍不由 ARRAY/Nullable 包裹。
- 核对 Arrow/Python/hybrid 三路线、静态 kernel 失败与整树回退、空值/空列表/元素 null/NaN；输出统一经 fallible fields_data，Query 和 Search 均传播投影错误，不返回部分字段。新建索引、人工存量定义、需要构建/无需构建与失败后的加载状态沿已有入口处理。
- 确定性失败边界在 WAL/序号提交前：父/子有效位长度、外层类型和容量、元素转换/越界、JSON 检查、过滤绑定/求值错误。原始 proto 失败写入无新增行，失败 Delete 的持久化文件不变；已有跨源失败/序号保护套件与新增 ARRAY 反例一起通过。

## 验收中关闭的问题

1. 简单 ARRAY 比较误入旧 S1 绑定；ARRAY 字段改走现有完整布尔树，保留标量快路径。
2. 数组静态 Arrow 错误及 Flush 前后子字段名不完整；按真实参照返回原文，内存诊断信息不写入新格式。
3. 存量 ARRAY INVERTED 的 build 错误曾错误映射成 schema code 6；真实目录对照后改为 code 1，WAL/segment 分别固定。
4. Rust 输入曾只检查父掩码，未检查直接提供的子列掩码长度；现于转换前统一校验并有反例测试。
5. 本轮 README 新增的仓内相对链接被发行长描述门禁拦住；已移除该链接，保留未发布开发线说明，打包检查器及其严格断言不变。

## 负责人独立评审

负责人以 A 最终 sdist（SHA256 前缀 `fb71fb47`）及源码清单逐项核对基线，确认 B 的 18 个产品文件增量和开发环境 B3 原生库身份。评审只读，结论为“通过”，没有阻断、建议或待确认项。

独立重跑：fmt / Clippy、Rust 310 / FI 396、pymilvus 3.0.2 的 Python 420、完整差分 286、存储互通 184；ARRAY fixture 在 Arrow 15.0.0 / 25.0.1 均通过。额外的 79 项端到端探针有 77 项一致，另 2 项为已登记的数组元素算术表达式；Rust Parquet 的读取与 `element` 子字段诊断亦与参照自产目录一致。

负责人另补 ARRAY 写入计时，未发现写入问题；这是其独立限定探针，原始脚本和日志在仓库外，本轮不把该结果扩展为包含 2.5.1 的正式性能提升声明。两条 SDK 中的 2.6、回退、固定统计、正式性能轮换、变异反证、Linux / Intel Mac 未由负责人独立重跑；前五项采信本记录，平台边界仍保留。

## 范围与后续

nullable 向量、JSON/嵌套/向量元素、新标量类型、真实 INVERTED/ANN、AddCollectionField/AlterCollectionSchema 仍按 0015 延后。没有扩大 rc3 与 3.x 原有 JSON 写入差距的处理范围。

Linux、Intel Mac、最低系统和最终候选平台矩阵未在本轮重跑。切片本机验收不能替代 0013 重新构建发行物后的全部候选门槛。A/B 已收口，下一步先做 partition 的参照真值、收益和存储设计调查，再冻结新的实施合同。
