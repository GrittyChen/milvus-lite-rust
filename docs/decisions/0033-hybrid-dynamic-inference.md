# 0033 hybrid 动态列类型推断的复刻

状态：设计第二稿 2026-11-13 评审通过（第一稿未通过：对象字段顺序的写法会对齐到 Arrow 15，嵌套类型的接口未写清）；已实施并复核通过（2026-11-13）。负责人已确认：版本分歧对齐 Arrow 25.0.1；从 `95e6a84` 开新分支，本文随该分支提交、不推送。参照版本 `1320d26`，pyarrow 25.0.1（差分与 CI 的参照环境）与 15.0.0。负责人 2026-11-13 确认本切片排在 [0032](0032-arithmetic-filters.md) 之后、GEOMETRY 之前。

## 要解决的问题

hybrid 路线把表达式引用的每个动态键物化为合成列再走 Arrow（[0009](0009-top-level-dynamic-filter.md)、[0014](0014-index-definitions-and-filter-expressions.md)）。3.x 的物化就是对该键在本数据源各行的取值直接调用 `pa.array(values)`（[hybrid_backend.py](../../references/milvus-lite/milvus_lite/search/filter/eval/hybrid_backend.py) 的 `_augment_table`）：ArrowInvalid、ArrowTypeError 等使整条表达式在该数据源上回退 Python，`OverflowError` 不在回退清单内、向上报错。

本实现的 [dynamic.rs](../../crates/core/src/filter/dynamic.rs) `infer` 用「全部同类」近似这一过程：数字与其他类型混排一律回退。这与 pyarrow 不一致，已作为差异登记在 [0006](0006-m2-scope.md) 与 [0032「与 3.x 的差异」](0032-arithmetic-filters.md#与-3x-的差异)，[tree_tests.rs](../../crates/core/src/filter/tree_tests.rs) 的 `REGISTERED_HYBRID_INFERENCE` 有 40 条两边分别断言。0032 放开算术后，这一差异会改变数值（除零得 inf 还是未知、int64 回绕还是精确整数），不只是改变路线。本切片按 pyarrow 的实际规则复刻推断与转换，注销这些登记。

`infer` 被两处共用：顶层动态过滤（`dynamic::evaluate`）与完整布尔树的列式路线（[columns.rs](../../crates/core/src/filter/columns.rs) `evaluate`）。两处同时改变。

## 3.x 的实测规则

`pa.array` 先**推断类型**，再**逐个元素转换**。探针在两个 Arrow 版本上执行：长度 1–3 的序列穷举 37 种 JSON 形状的取值（含 null、bool、int64 两端与越界、2^53 附近、浮点与 inf、字符串、各种列表和对象），共 52 059 条；另有 20 万条随机的嵌套序列（深度至 3）。按下面的规则写的模型与 Arrow 25 的结果（成功时的类型与转换后的值、回退、`OverflowError`）**全部一致**；把对象字段改为按键名排序后，与 Arrow 15 全部一致。嵌套 200 层也不报错，没有深度上限需要复刻。

**推断**（arrow `python/inference.cc` 的 `TypeInferrer`）：

1. 顶层序列逐个访问：null 只计数；**bool、float、字符串计数后停止访问后续元素**；整数计数后继续；列表计数，并把其元素交给一个共用的子推断器（每个列表各自遇到 bool / float / 字符串后停）；对象计数，并把每个键的值交给该键的子推断器（对象内不停止，所有键值都被访问）。
2. 校验：出现过列表时，其余非 null 元素必须都是列表，否则 `cannot mix list and non-list`（回退）；对象同理；子推断器递归校验。
3. 类型优先级：列表 > 对象 > double > int64 > bool > string > null。所以 `[1, 'a']` 推断为 int64，`['a', 1]` 在 `'a'` 处停止、推断为 string，`[1.5, True]` 为 double，`[True, 1.5]` 为 bool。

**转换**按行顺序、深度优先，**第一个失败的元素决定结果**：

| 推断类型 | 接受 | 回退 | 报错 |
| --- | --- | --- | --- |
| int64 | int64 范围内的整数 | bool（`Expected integer, got bool`）、其他类型 | 超出 int64 的整数：`OverflowError: Python int too large to convert to C long` |
| double | float 原值；bool 为 1.0 / 0.0；绝对值不超过 2^53 的整数 | 超过 2^53 的整数（**超出 int64 时也是回退**，`PyLong is too large to fit int64`）、其他类型 | 无 |
| bool / string | 同类 | 其他类型 | 无 |
| 列表 | 列表，逐元素按元素类型转换 | 非列表 | 元素报错时 |
| 对象 | 对象，按字段顺序逐个取键（缺键为 null） | 非对象 | 字段报错时 |

**字段顺序**分两层，不能混为一谈：

- **每一行对象内部的键序**：3.x 在写入入口就用 `json.dumps(extras, sort_keys=True)` 序列化动态字段（[validation.py](../../references/milvus-lite/milvus_lite/schema/validation.py) 第 527 行，collection.py 第 2227 行的写入路径调用），之后 MemTable、WAL、segment 读到的每一行对象（含嵌套对象、列表里的对象）键都按键名有序，与客户端发来的键序无关。
- **结构类型的字段顺序**（跨行累积）：Arrow 25 按字段**首次出现**的顺序，即依次访问各行（每行内部已排序）时遇到新键就追加在末尾，**不是**全局按键名排序；Arrow 15 才是全局按键名排序。

`sort_keys` 只保证每行内部有序，不保证跨行首次出现的顺序全局有序。真实 3.x 经 pymilvus 端到端核对（MemTable 与 flush 后结果相同）：

| 行（按写入顺序） | Arrow 25 | Arrow 15 |
| --- | --- | --- |
| `{"b": 1}`、`{"a": 1}`、`{"a": "s", "b": 2**63}` | 字段 `b, a`，先转 `b`：OverflowError | 字段 `a, b`，先转 `a`（int64 遇字符串）：回退 |
| 客户端键序 `{"b": 1, "a": 1}`、`{"b": 2**63, "a": "s"}` | 每行排序后为 `a, b`，先转 `a`：回退 | 回退 |
| `{"b": 1}`、`{"a": "s", "b": 2**63}` | `a` 只出现过字符串、推断为 string，转换成功，再转 `b`：OverflowError | 同左 |

第二行说明不能直接用本实现内存中 `Map` 的插入顺序：MemTable 保留客户端发来的键序（`decode_meta` 原样保存），只有写入 segment 时经 `pyjson::dumps` 排序；若按插入顺序，MemTable 上会先转 `b` 而报错，与 Arrow 25 的 3.x 不一致。第三行说明版本分歧还需要排序靠前的字段已被更早的行推断成会失败的类型。

**动态键本身**（表达式引用的 `$meta` 顶层键）是另一回事：3.x 按 `sorted(keys)` 依次物化，第一个回退的键使整条表达式回退，后面的键不再物化；本实现已按同样顺序处理，不变。

### 与现状的对比

| 取值（按行顺序） | 3.x | 本实现现状 |
| --- | --- | --- |
| `[1.5, True]`、`[1.5, 1, False]` | double，bool 为 1.0 / 0.0 | 回退 |
| `[2**64, 'a']`、`[1, 2**64, 'a']` | OverflowError | 回退 |
| `['a', 2**64]`、`[1.5, 2**64]` | 回退 | 回退 |
| `[1, True]`、`[True, 1.5]` | 回退 | 回退 |
| `[[2**64, 'a']]` | OverflowError | 回退 |
| `[[1.5], [True]]`、`[{'k': True}, {'k': 1.5}]` | 成功（复杂列） | 回退 |
| `[{'a': 1}, {'a': 'x', 'b': 2**64}]` | 先转 `a`：回退 | 回退 |
| `[{'b': 1}, {'a': 'x', 'b': 2**64}]` | Arrow 25 先转 `b`：OverflowError；Arrow 15 先转 `a`：回退 | 回退 |

复杂列（列表、对象）上的比较与算术仍由后续 Arrow 内核失败回退，这一点不变；对它们而言新规则只改变「报 OverflowError、回退、成功」三者中走哪一个。

## 设计

`infer` 的对外接口不变：`Ok(Some(shape))`、`Ok(None)` 表示回退、`Err` 为 OverflowError 文案；顶层 shape 仍为 Null / Bool / Int / Float / String / Complex。内部分两段，都在 dynamic.rs 私有：

1. **推断**：推断器节点保存各类计数、一个列表子推断器、一组对象字段子推断器 `Vec<(String, 推断器)>`。按「3.x 的实测规则」访问取值：顶层与每个列表各自遇到 bool / float / 字符串后停止，对象内不停止。访问一个对象时，先把**这一行**的键按键名排序再逐个访问（复刻写入时的 `sort_keys`），未见过的键**追加**到字段列表末尾；字段列表本身不排序，因而是排序后各行的首次出现顺序（Arrow 25）。嵌套对象、列表里的对象同样如此。键名比较用字节序，对 UTF-8 等同于 Python 按码位排序。
2. **定类型**：校验列表 / 对象混排后，把推断器归约成私有的类型树 `Null / Bool / Int / Float / String / List(Box<类型>) / Object(Vec<(String, 类型)>)`，对象字段保持上面的首次出现顺序。
3. **转换检查**：按行顺序对每个取值递归检查类型树：列表逐元素检查元素类型，对象按类型树的字段顺序逐个取键（缺键视为 null，不依赖该行 `Map` 的键序）。第一个失败的元素决定结果：回退为 `Ok(None)`，超出 int64 为 `Err`。只检查，不构造列表或对象列。
4. 返回类型树的顶层对应的 shape：`List` / `Object` 为 Complex。`materialize` 只处理顶层 Int / Float / Bool / String / Null，Complex 继续返回占位列：现有路线上复杂列只用于判空与触发内核缺失。
- `materialize` 的 Float 分支接受 bool（1.0 / 0.0）。转换检查已排除 double 列中超过 2^53 的整数，`number_float` 里「int too large to convert to float」的分支不再可达，删去。
- 「只看活行」（0006 / 0009 登记，死行不参与推断与转换）**不在本切片范围**，保持不变。
- 开销：推断与检查各遍历一次被引用键在活行中的 JSON 节点，与现状同阶；不缓存、不改锁与计算池。

## 与 3.x 的差异

注销：

- 0006 的「0032 hybrid 动态列推断」一行与 0032 差异表的对应行，改为指向本文；`REGISTERED_HYBRID_INFERENCE` 删除，组件真值的 40 条改为与 3.x 直接比较。
- [fuzz_arith.py](../../tests/differential/fuzz_arith.py) 的「混排」取值不再排除超出 int64 的整数与 float 之后的 bool。

新登记一项**参照版本之间**的差异，沿用 0014 对 Arrow 15 / 25 诊断原文差别的处理（负责人 2026-11-13 确认对齐 Arrow 25.0.1）：对象字段的转换顺序。条件：动态键的值是对象；后出现的行引入了按键名排序更靠前的键，使首次出现顺序与全局排序不同；同时一个字段遇到超出 int64 的整数、另一个字段遇到无法转换的值。此时 Arrow 25 按首次出现顺序、Arrow 15 按键名排序决定先转哪个字段，一个报 OverflowError、一个回退（上表第一行，真实 3.x 端到端复现）。本实现对齐差分、CI 与 S1 使用的 Arrow 25.0.1，Arrow 15 的结果是回退。证据：探针 252 059 条中 39 条结果类别（成功 / 回退 / 报错）不同，全部属于这种情形；其余差别只是成功时结构类型的字段显示顺序，对过滤不可见。

组件真值因此**不**沿用「两版必须完全一致」的核验：每条按版本分别保存期望（只有版本分歧条目的 Arrow 15 期望与 Arrow 25 不同），`--verify` 在各自版本下严格比较；Rust 单测按 Arrow 25 的期望核对，并断言 Arrow 15 期望不同的条目恰好是登记的情形，不能在核验里宽松放过其他差别。

## 实施步骤

一步完成，交负责人复审后单独提交：

1. **组件真值**：新生成器 `tests/differential/generate_dynamic_infer_fixtures.py` 直接调用 3.x 的 `_augment_table`，`$meta` 由真实写入函数 `separate_dynamic_fields` 生成（与写入路径相同的 `sort_keys`），记录每条取值序列的结果：成功时的 Arrow 类型与顶层标量转换后的值、回退、报错原文。语料：长度 1–2 的全部组合、长度 3 的抽样、带种子的随机嵌套序列（对象的键序随机，覆盖「客户端键序与排序不同」），以及版本分歧情形与上表三行。Rust 侧的输入用客户端原始键序构造 `Map`（不预先排序），以覆盖 MemTable 的情形。重新生成须用 Arrow 25 并同时调用 Arrow 15 的参照解释器，各版本 `--verify` 分别严格比较（见「与 3.x 的差异」）。
2. **实现**：按「设计」改 `infer` 与 `materialize`。
3. **已有真值**：[filter_arith.json](../../crates/core/tests/fixtures/filter_arith.json) 第 7–10 组去掉登记后与 3.x 直接一致；filter_dynamic / filter_tree / filter_arith 的生成器在两个 Arrow 版本下 `--verify` 不变。
4. **模糊对照**：放开 fuzz_arith 的混排排除，种子 32 / 33 在 Arrow 15 / 25 下零不一致（版本分歧需要多键对象，fuzz 的取值池中只有单键对象 `{"k": 1}`，不会出现）。
5. **端到端差分**：在 [test_filter_arith.py](../../tests/differential/test_filter_arith.py) 补 `[1.5, True]` 上的算术、`[2**63, 'a']` 的报错（pymilvus 可发送 `2**63`，`2**64` 及以上会被客户端拒绝），以及「字段顺序」表的三行（含客户端键序与排序不同的情形），覆盖 MemTable、flush、重开；两条 SDK 线。差分参照是 Arrow 25，第一行两边直接比较。
6. **全量回归**：fmt、clippy（默认与 fault-injection）、cargo test、fault-injection、两条 SDK 线的 tests/python 与差分；用例计数按清单同步。
7. **开销**：S1 专项不含 `$meta`。用一次性脚本对一个含 `$meta` 比较与算术的过滤在改动前后各测三轮，只确认不退化，不作性能声明。
8. **文档**：本文状态与验收、0006 / 0032 注销与新登记、0009 第 21 行补一句链接、AGENTS 知识索引与命令。

## 验收

开发验收（2026-11-13，最终代码，串行执行）：

| 项目 | 结果 |
| --- | --- |
| 组件真值 [filter_dynamic_infer.json](../../crates/core/tests/fixtures/filter_dynamic_infer.json) | 7280 条，Arrow 25.0.1 与 15.0.0 分别 `--verify` 通过；Arrow 15 不同的 4 条全部是登记的对象字段顺序情形。Rust 单测 `dynamic_inference_matches_reference_arrow_conversion` 按 Arrow 25 逐条一致，把字段顺序换成按键名排序后逐条复现 Arrow 15 |
| 变异 | 去掉每行按键名排序、改用 Arrow 15 的字段顺序、double 拒绝 bool，单测分别失败；把 dynamic.rs 换回 `95e6a84` 的实现，放开排除后的模糊对照（种子 32）出现 147 条不一致 |
| 已有真值 | filter_arith（1798）、filter_dynamic（3570）、filter_tree（4737）在两个 Arrow 版本下 `--verify` 不变；filter_arith 第 7–10 组原登记的 40 条改为与 3.x 直接比较并一致 |
| 模糊对照（放开混排排除） | 种子 32（49 865 条）与种子 33（8000 张表，199 453 条），Arrow 15 / 25 均零不一致 |
| 端到端差分 | `test_dynamic_inference_follows_pyarrow_order` 6 条（float 后的 bool、超出 int64 的整数先后、对象字段顺序三种情形），MemTable、flush、重开三个阶段的 Query / count / Search 两边一致，并断言 3.x 自身的结果 |
| fmt / clippy（默认与 fault-injection） | 无告警 |
| cargo test --workspace / fault-injection | 656 / 793 通过 |
| tests/python（pymilvus 3.0.2 / 2.6.18） | 各 652 通过 |
| 差分（pymilvus 3.0.2 / 2.6.18，参照 Arrow 25） | 各 838 通过（会话组 404）；计数已同步到 scorecard、发行测试、compare.yml 与分片 README |
| 开销 | 20 万行、6 个 `$meta` 过滤（int / float / string / 算术 / 嵌套对象 / 组合），改动前（`95e6a84`）后各三轮、顺序轮换，结果逐条相同，耗时没有退化；一次性测量，不作性能声明 |

残余风险：「只看活行」的登记差异不变（死行仍不参与推断）；Arrow 15 的对象字段顺序按登记与 Arrow 25 不同；组件真值覆盖 null、bool、int64 两端与越界、2^53 附近、有限浮点、字符串、三层以内的列表与对象；非有限浮点（`1e999` 一类解析为 inf 的数字）只在探针中与 pyarrow 核对过，因语料用 Python 的 JSON 编码写出、无法表示非有限值，没有进入组件真值。
