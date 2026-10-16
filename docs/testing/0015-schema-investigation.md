# 0015 schema 调查与参照真值

- 日期：2026-10-16
- 状态：统计工具与设计调查完成；[0015](../decisions/0015-nullable-defaults-and-array.md) 已确认，切片 A 实施中。本记录仅描述实施前的调查证据
- 参照：milvus-lite v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`
- 范围：macOS arm64，CPython 3.12.14；本机实跑，未重跑 Linux 或最终候选矩阵

## 输入与方法

[离线工具](../../tests/compatibility_audit/README.md)与用例身份清单进入仓库；参照测试、上游源码、环境、JUnit、事件和数据目录均留在仓库外。原始统计、各干预模式、组件与生命周期分别保留，不覆盖旧 0014 或 rc3 发行证据。

| 输入 | 身份 |
| --- | --- |
| 本仓基线 | `7d53c28`，本轮仅增加调查工具与设计文档，未修改产品 |
| 正式 rc3 wheel | macOS arm64；SHA256 `a84b6e4b3cd48531263ba531e387f738956b1b7045c025a0e05e8cc8f16e73a9` |
| rc3 原生库 | `53b74ce741952c5357835bc7c5c66eb317db4e077b77821d78fa3f0b3e3c84d4` |
| 上游 Milvus | `dcda428d54d11e6084156e2342c513f08c9927bf` |
| 760 条 nodeid 清单 | SHA256 `55481d4fc03ff0f4f53097ff61b51d56590dc8ef31201bd315f8213d6dd266f2`；由原用例身份转换成 pytest nodeid，身份集合不变 |
| 124 条 schema 调查清单 | SHA256 `c0e61983d6491f9ddc228e2a3f79a63390611d75177fd70b2d4724f578296cdc` |
| 原 49 项探针 | SHA256 `ecf7af124b2a1a98f5ada1f7cc4f87852b0af14672d848dae1df9abb76002bba`，逐字节沿用 |
| pytest / timeout | 9.1.1 / 2.4.0 |
| SDK / grpcio | 3.0.2 / 1.84.0；协议专项另用 2.6.17 |
| Arrow 25 环境 | PyArrow 25.0.1 / NumPy 2.5.4；含 faiss-cpu 1.15.1 |
| Arrow 15 环境 | PyArrow 15.0.0 / NumPy 1.26.4；组件与引擎探针 |

上游两端沿用原环境的依赖冻结与参照 tools/pytest_compat，关闭 rerun。其 pytest 9.1.1 与 pytest-asyncio 0.24.0 存在已知声明约束不兼容，按原组合运行，没有升级插件改变样本。完整环境与命令保存在每次 run.json，不把各环境的 Arrow 版本混为存储互通证据。

## 原始三组统计复现

| 固定样本 | 参照 | 已发布 rc3 | 与 0014 收口 |
| --- | --- | --- | --- |
| adapter / compatibility 760 条 | 760 passed | 375 passed、339 failed、45 errors、1 skipped | 一致 |
| 上游子集 90 条 | 90 passed | 75 passed、15 failed | 一致 |
| 49 项调用探针 | 43 成功、6 失败 | 25 成功、24 失败 | 一致 |

各组没有缺失、重复或新增用例；参照源码/测试由固定 git archive 展开，Rust 的服务和清理 fixture 适配沿用原口径。139 条内部依赖排除项单独保留，未作为失败后临时剔除。原始 nullable 向量跳过仍保留，未将它计入通过；384 条明确 UNIMPLEMENTED 不包括该跳过。调用探针仅检查成功与否，不证明结果一致或停服持久化。

## nullable 使用分类

120 条 nullable 失败的建表声明均涉及现有标量；按每条用例的 nullable 类型集合分类，互不重复：

| nullable 类型集合 | 用例数 |
| --- | ---: |
| BOOL、FLOAT、VARCHAR | 46 |
| FLOAT、VARCHAR | 41 |
| JSON | 21 |
| VARCHAR | 7 |
| INT64、VARCHAR | 2 |
| INT64、FLOAT、VARCHAR | 2 |
| VARCHAR、JSON | 1 |

另有 1 条向量 nullable 用例原本跳过，与这 120 条分开。原始 Rust 在建表处被阻断，不能据此观察后续输入；在参照的 scalar-nullable 干预运行中，116 条观察到 Insert/Upsert 调用，其中 54 条在对应 nullable 字段上显式传 None，29 条省略这些字段，二者并集 79 条。这是客户端调用输入记录，不代表写入已成功，也不统计 JSON 内部的 null 为列空值。

## 受控反事实：124 条

不修改测试本体、断言或记录值；只在客户端构造 schema 时移除对应属性，两端采用相同干预。下表五类互斥，总数每行均为 124；“参照不再通过”不算后续能力缺口，配对通过不算正式兼容性收益。

| 模式 | 无对应干预 | 参照不再通过 | 两端通过，仅诊断 | 残余 UNIMPLEMENTED | 其他待检查失败 |
| --- | ---: | ---: | ---: | ---: | ---: |
| 只移除已有标量 nullable | 4 | 63 | 55 | 2 | 0 |
| 只移除 default_value | 121 | 2 | 1 | 0 | 0 |
| 移除已有标量 nullable 与 default_value | 1 | 65 | 56 | 2 | 0 |
| 移除全部 nullable | 3 | 64 | 55 | 2 | 0 |

配对后残余的 2 条均为 CreatePartition。未干预样本包括仍被默认值挡住的用例；defaults 模式也不能越过原 nullable 阻断。去掉全部 nullable 没有比只去掉标量 nullable 增加配对通过的样本，不能因此推导 nullable 向量没有价值。

结论仅支持当前切片排序：先处理标量 nullable / 默认值。未通过用例中的新阻断与干预引起的输入/断言失效要分别记录；正式净收益只能在实际实现后重跑同一批原始样本得到。

## 组件与协议真值

| 专项 | 本轮实跑 | 边界 |
| --- | --- | --- |
| schema / 记录验证 / Arrow batch | Arrow 15/25 各 696 项，结果逐项相同 | 9 个现有标量类型与 8 个 ARRAY 元素类型，nullable、默认值、缺失/None、零值与错误输入；异常也是结果 |
| 空值过滤 | Arrow 15/25 各 510 项，结果逐项相同 | typed/JSON/动态混合与 ARRAY，保存实际 backend、掩码或错误阶段/类型/原文，覆盖三路线 |
| 原始 gRPC | 两条 SDK 环境各 32 个 INT64 布局组合 | nullable/default 组合×紧凑/完整/全 null/全有效/无 mask/缺列/错误长度；记录原始状态与字段 |
| SDK Insert | 2.6.17 / 3.0.2 各 9 项 | 无默认值、默认 5、默认 0 × 缺失/None/0，捕获实际请求与返回 |
| SDK Upsert | 两条 SDK 各 18 项 | 普通/partial × 上述配置与输入，先写入 x=7 作为对照 |

协议专项的 Insert、Upsert 与 raw gRPC 三组观察在两条 SDK 间一致。以 nullable INT64 为例，missing/None 的 Insert 均发送 valid_data=false；服务填默认值或保留 null，不能将最终查询值归因于客户端已预填。partial Upsert 的缺失保留旧值，显式 None 清为空或填默认值，0 不被默认值覆盖。

Arrow 掩码例子见 0015。ARRAY 空父列、空数组、含 null 元素不同；参照对 array_length(None)==0 的组件结果为 True。这些行为必须按整表达式路线保留，不能统一替换成 SQL 三值逻辑。schema/Arrow 组件与协议专项分开：组件可表达的默认值不等于 SDK/ValueField 可声明的默认值。

## nullable 向量四状态

在两个 Arrow 环境分别由真实引擎正常写入 id=1 的 None、id=2 的 [0,0]、id=3 的 [1,0]，FLAT / L2；子进程使用 os._exit 退出，保留未 Flush WAL。

| 状态 | id=1 的 Get/Query | Search [0,0] | 存储观察 |
| --- | --- | --- | --- |
| 首次 MemTable | [0,0] | 与真实零向量一起命中，距离 0 | 已在构造 batch 时填零 |
| WAL 重放 | [0,0] | 同上 | 原 None 未恢复为空 |
| Flush 后 segment | [0,0] | 同上 | vector_null_mask=None |
| 实际 compaction 后 | [0,0] | 同上，新增的 None 行也参与检索 | 原输入文件已被替换，合并后 mask 仍为 None |

两个 Arrow 版本在这些观察上相同。compaction 通过正常追加小 segment 达到真实触发条件，断言旧文件被替换，没有把一次空维护当作 compaction。没有直接构造 Arrow null，也没有实跑完整 nullable 向量的 gRPC/SDK 矩阵；其实施按 0015 延后。本轮观察证明正常入口的 None 立即变零，不证明参照所有来源都具有同一行为。

## 已发行旧包的回退预检

由参照分别生成 Arrow 15/25 × 遗留 WAL/已 Flush segment 四套目录，各含 legacy、nullable、default、ARRAY 四个 collection。已发布 rc1、rc2、rc3 的 macOS arm64 wheel 各读独立副本，共 12 次运行：

- legacy 的 Load/Query 成功，读出 id=1/x=7；拒绝新属性后再次读取仍成功。
- nullable/default/ARRAY 的实际 Load 均返回 gRPC UNIMPLEMENTED，原文包含 schema 的 unsupported attributes，36 次拒绝全部符合预期；不把任意连接异常算成成功拒绝。
- 正常停服后，受保护的三个 collection 的文件名与内容哈希不变。legacy 的 WAL 被正常 Flush 属允许行为，因此不要求整个数据库的文件集合不变。

原生库 SHA256：rc1 `ec6c960578de77914df50548332d9077f2d3512e223a51480e7a9563af2d37ce`、rc2 `7251892f5d08ef064997c2a97a516d0754002bd39f176fe186815d332056d865`、rc3 见输入表。此处写入方是参照，**新 Rust 实施后的写出→回退仍为必过门槛**。

## 证据摘要与未验证范围

关键输出 SHA256；这些是本轮证据身份，不要求再次运行生成相同的随机文件名或路径元数据：

| 输出 | SHA256 |
| --- | --- |
| values / Arrow 15 | `1b83bd07442d5949704e5eedc397ed0181fd47716348ea31796dae03d2e58ad2` |
| values / Arrow 25 | `08a8656cbfb16f33d415ea9494a1abb2bda99bc6abcbf533cd719ee1b0d3bca8` |
| filters / Arrow 15 | `8e945ebc262e331683b93fb7a981eac50d3b8da2636b0deb540992ac8e50942a` |
| filters / Arrow 25 | `fb6c6e5dc3e7bfc41a849e07aa90c307b49e5e55490283f7cc5f050ecca10216` |
| vectors / Arrow 15 | `9dc06f8660e6a444959621ba02b4bf125c1ef70e129410810b90b96eb0660dde` |
| vectors / Arrow 25 | `77e61d96a48b18d9423a3f0b9b148b096f615dfd6a2888954a94179a2758e54d` |
| raw / SDK / Upsert，2.6.17 | `661b1ab556ebe3af9d07ffa5ffdcef7c0e817e8215f62e5525e4230c6a05c658` |
| raw / SDK / Upsert，3.0.2 | `393c8a2a1dbc7674a59b626992b66f4f38cb7374afb0848706fab81ff48512df` |

最初协议探针的结果序列化不支持 protobuf repeated 容器，修正工具后在新目录重跑；回退工具也修正了虚拟环境解释器符号链接使用和错误码序列化。这些属于调查工具准备问题，失败输出保留，不记作产品失败或通过证据。

工具反例检查显式运行，不进入默认测试。产品代码没有改变，未执行新增能力的 Rust/FI、完整差分/双向互通、三专项性能或 Linux/Intel Mac 候选验收；它们是各实现切片的后续必过门槛。raw/SDK 专项当前以 INT64 为代表，不由此宣称其他类型、非有限默认值、所有错误优先级或任意历史故障目录已验证。0015 的完整验收矩阵需要在实施时继续补齐。
