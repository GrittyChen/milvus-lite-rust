# 0015 切片 A：首轮负责人评审后的修复

- 日期：2026-10-16
- 状态：两项阻断及同类残留已由负责人复审通过；新发现的 JSON 写入性能回归另见[专项修复](0015-json-write-regression.md)
- 合同：[0015](../decisions/0015-nullable-defaults-and-array.md)；首轮产物与历史证据见 [A 验收](0015-scalar-nullable-acceptance.md)
- 参照：milvus-lite v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`

## 核实与最小修复

**阻断 1，已确认。** 独立原始 gRPC 探针中，typed JSON / `$meta` 的坏值与缺主键同时出现时，A6 先报 JSON，真实 3.x 先报 `primary key 'id' missing or None`；向量长度错误的对照保持一致。首轮的语法错误校验放在 gRPC 解码层，改变了 0015 冻结的错误优先级。

移除 gRPC 层的额外 JSON 解析，使用 InsertBatch 已保留的原始列：正常选中的列沿既有引擎校验路径处理；被同名后列覆盖、或带 nullable mask 的 JSON 列保留借用，在逐行校验轮到该 schema 字段时检查 JSON 语法。Insert 与 Upsert 共用该检查，Upsert 合并出的单行与原始批次行号分别传递。空值探测只识别真正的 JSON null，畸形文本作为非空输入交由后续类型/JSON 校验拒绝；因此 JSON 槽位被用于主键时，向量检查及标量类型错误也恢复原优先级。`$meta` 恢复既有按行记录错误、在该行其余业务检查后报告的路径。协议形态/长度/valid_data 仍先检查；0004 的坏 JSON 明确报错差异、默认值及存储格式保持原合同。

这样利用既有批次数据，避免为错误增加新协议字段、输入列变体或全列解析缓存；移除的是 gRPC 的重复解析，引擎原有校验和转换流程仍保留。

新增/扩展的 44 项原始 gRPC 用例覆盖 Insert/Upsert、typed JSON/动态 JSON、缺主键、向量长度/维度、mask 长度、第二行坏值与第一行业务错误、被覆盖的标量/向量字段、合法 JSON 的后列覆盖、主键/向量错误槽位、默认值/auto_id 与掩码组合、失败后已有行保持。最终 wheel 的 44 项均通过；与 3.x 应一致的 30 项另在真实参照上通过，不拿已登记的坏 JSON 差异作相等断言。

**变更方反证闭环。** 最初 32 项在 A6 上 16 failed / 16 passed，在第一版修复 R1 上全部通过；补充的 12 项在 R1 上又检出 9 项失败，其中两项为 Upsert 把被掩码覆盖的坏 JSON 换成默认值/auto_id 后接受请求。该证据说明只保存被同名列覆盖的值不够，因此最终也保存带掩码的原始 JSON。R2 的 44 项中另有 2 项暴露同一路径的历史诊断问题：`py_type_name` 把畸形 JSON 当成 NoneType，真实 3.x 解码后保留为 str；已按参照修正这一个分支。原失败、R1/R2 的构建和日志全部保留，未放宽断言或用重跑覆盖失败。

**阻断 2，已确认。** 按原说明新建只装 PyArrow 15.0.0 / NumPy 1.26.4 的环境，新增 9 项互通用例得到 6 passed / 3 failed，三项 reference15 写入路径均报找不到 milvus_lite。第一次尝试的旧参照解释器来自另一 checkout，被既有来源检查拒绝；该准备失败日志保留，随后按文档新建正确参照环境再复现，不将准备错误归因于产品。

[环境说明](../../tests/storage_interop/README.md) 和 AGENTS 命令补齐 Arrow 15/25 均安装固定参照引擎的要求；Arrow 15 固定 PyArrow 15.0.0 / NumPy 1.26.4。互通 fixture 对三条解释器预先导入 `milvus_lite.db.MilvusLite`、核对包版本及 checkout 来源；缺失引擎时明确指出对应环境变量和准备文档。保留未装引擎的最小环境作反证，已确认它在用例执行前给出指定诊断。断言及互通业务步骤未放宽。

## 同类扫描与影响矩阵

扫描模式为 `insert::decode|upsert_with|insert_with`、`InputValues::Json|parse_json|parse_dynamic_json|from_slice`、`validate_row|decode_dynamic|check_column_len|next_seq|append`，范围是 grpc Insert/Upsert → core Db/Collection → WAL/内存发布，以及两个 Arrow 子进程写入入口与共享解释器 fixture。

| 不变量/状态 | 检查与证据 |
| --- | --- |
| 协议形态优先 | 坏 JSON 叠加向量/掩码长度错误，按原解码检查报告 |
| 逐行/字段优先 | 主键、向量检查先于同一行 JSON；第一行业务错误先于第二行坏 JSON；typed 与动态分别覆盖 |
| 覆盖语义 | 合法同名列仍由后列决定；坏 JSON 不因后列、nullable mask、默认值或 auto_id 而被隐藏；原始行号不被 Upsert 单行化重置 |
| 失败前写入边界 | 列形态、JSON/默认值/业务类型、Arrow 规范化及范围转换都在当前请求第一次 WAL 追加、next_seq 发布之前；仍沿用既有维护/恢复副作用边界 |
| 已有数据与成功路径 | 失败批次保留原有行且不发布新行；两条 SDK 全套及存储互通重跑 |
| 环境失败 | 缺参照引擎的 Arrow 15 明确提前失败；按更新文档从零建立的两套参照环境用于完整互通 |

此前自查只对坏 JSON 搭配有效主键/向量验证掩码与覆盖，没有把新增检查与前序业务错误组合，因而漏掉优先级回归；此前的 Arrow 15 验收环境已装参照引擎，没有验证文档中最小环境。此次增加上述反例与从零建环境的复验。

## 构建与重验

产品相对 A6 仅修改 `core/collection.rs` 与 `grpc/insert.rs`。R1/R2 的文件清单、wheel 和失败日志均单独保留，不能借用它们的通过数作为最终构建验收。最终身份如下：

| 输入/产物 | SHA256 |
| --- | --- |
| 产品文件清单 | `4eaaf36cb553b394bd28acef12894c71d7da5a7b4ffe8678e159389e129ed9e6` |
| macOS arm64 开发 wheel | `ac66de0f930cd4aa7514a0ea9ebca5dbbb247f393a7580c8b53f3e78ff3be90a` |
| 原生库 | `b2166dbfbccc77b419981a237943810c4c4c446c388172dcb9d6e8db98947291` |
| sdist | `34a167a90401789303f98240a9b42406fb07e3a7acd2021bb2ffd2cd2c70725b` |

wheel/sdist 的归档内容、元数据、arm64/minimum macOS 11.0 及不含 FI 入口的静态检查通过。仍保留原版本元数据，不替换正式 rc3 发行物；这不是 0013 最终候选的完整平台验收。

| 最终构建检查 | 结果 |
| --- | --- |
| fmt / workspace all-targets Clippy | 通过，Clippy `-D warnings` |
| workspace / core fault-injection | 301 / 387 passed，无 ignored |
| 原始 gRPC 定向 / 真实 3.x 对照 | 44 / 30 passed |
| pymilvus 2.6.17 / 3.0.2 Python 全套 | 各 388 passed，无 skip |
| 两条 SDK 完整存储互通 | 各 169 passed；Arrow 15/25 环境均按更新文档从零新建，未注入参照源码路径 |
| pymilvus 2.6.17 完整差分 | 232 passed |
| pymilvus 3.0.2 完整差分并发复验 | 三次各 232 passed；独立服务/端口/目录，均无 skip |
| 统计工具自检与清单核对 | 9 项自检通过；旧/新 760/139 条集合相同且无重复 |

八份最终 Python 套件 JUnit 逐项核对用例数，failure/error/skip 均为 0；两条 SDK 实际加载的原生库哈希与上表一致。产品清单的 75 项文件在构建、测试和最后核验间未变。

仓库外证据索引 `review-evidence-sha256.json` 收录 104 项文件，包括原失败、R1/R2/最终产物、原始日志、JUnit、环境身份和并发复验脚本；索引 SHA256 为 `b0e85ff0fc6404ed3d5b4305dc9e1ac1f5d03a72dd6d69f877599918a31bbf9c`。

## 差分偶发失败调查

负责人第一次差分失败未保留明细，第二次通过；现有信息无法判断失败用例、阶段或原因，不将它当作确定的产品缺陷，也不以随后一次通过撤销该异常。

本轮对最终 wheel 启动三次独立完整差分并发运行，服务、端口、数据目录各自独立，保留日志、JUnit、负载采样；增加仓库外诊断插件，在首次失败时立即保存用例身份、完整异常及当时数据/服务日志副本，不改断言或自动重试。另跑 pymilvus 2.6 差分全套。R1 已有三次并发各 232 passed 的历史诊断，最终产物仍独立重验，三次也均为 232 passed。

最终并发复验约 610 秒，每 15 秒采样，共 41 份负载记录，1 分钟系统负载范围 13.68–38.57，本机为 10 个逻辑 CPU；同时运行的 SDK/互通套件结束后，三次差分继续执行完毕。未出现失败或重试，因此没有生成首次失败快照。**结论限于此次负载与次数内未复现；负责人原异常仍因缺少明细无法归因，不能宣布偶发问题不存在或已修复。**

## 文档与证据边界

0006 第 14 项补充链接，明确 nullable 标量/默认值的拒绝条件已由 0015 修订；统计 README 补充旧 JUnit JSON 与新 pytest 节点格式及四个哈希的对应，独立核对 760/139 项转换后集合相同且无重复。

本记录冻结时不推进 ARRAY，也未提交、切版本或发布；后续各切片均已评审收口，按切片作本地提交，不推送。未重跑 Linux/Intel Mac 或最终候选平台矩阵；旧版回退、固定统计和性能如仍引用 A6，均标明历史身份，不当作本次新 wheel 的独立重跑证据。
