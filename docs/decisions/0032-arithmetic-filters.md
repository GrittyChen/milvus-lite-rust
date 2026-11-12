# 0032：过滤表达式中的算术

状态：设计经六轮独立复核与负责人两轮评审，2026-11-12 通过；第 1 步已实施并复核通过；第 2 步已实施并复核通过；第 3 步待实施。参照版本 `1320d26`。负责人于 2026-11-12 确认：大整数用 `num-bigint`；字符串 / 列表结果设 64 MiB 上限（按实际分配，每码位 4 字节），拼接与重复共用，先算大小再分配；大整数设 2^20 位上限；输入的解析和转换同样计入预算（表达式字面量已由解析器限制在 4300 位内，不另计）；3.x 的 OverflowError 情形照报原文，之后才是本实现的资源错误；已有比较的 null 错位检查一并修正；CPU 与时间不设上限，登记为残余风险；参照的内部测试只作真值来源。

## 范围

- 过滤表达式中的 `+`、`-`、`*`、`/` 与一元负号，用于 Query、Search、HybridSearch、iterator 与 Delete 的过滤，以及 rerank 的 boost 过滤和稀疏检索中 MemTable 的逐行过滤。三条求值路线（Arrow、hybrid、Python）分别复刻 3.x。
- 已支持的「时间戳 ± 区间」保持不变（[0025](0025-timestamptz-and-timezone.md)）。
- 取代 [0014](0014-index-definitions-and-filter-expressions.md)「已确认的取舍」第 3 条（算术继续明确拒绝）。

不包含：`%`、`**`（3.x 解析阶段即拒绝，本实现的解析器已一致）；GEOMETRY（第 7 步的下一片，几何谓词继续 UNIMPLEMENTED）。解析器不改：`a + 1 in [...]`、`(a + 1) is null` 在 3.x 中都是解析错误，本实现已一致。

## 3.x 的行为

实测用真实 3.x 的 `parse_expr` / `compile_expr` / `evaluate`，Arrow 15.0.0 与 25.0.1 的结果与文案完全相同；错误码另经 pymilvus 3.0.2 的 Query 对 MemTable 与 flush 后的数据各测一次，两者相同。探针在仓库外，定稿后转成组件真值与差分。

**语义检查**（[semantic.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/search/filter/semantic.py#L437)）：操作数须为 int、float 或动态值，否则报 code 6 `arithmetic '+' requires numeric operands, left side is ...`。ARRAY 字段的语义类型是动态值，因此 `arr + 1` 能通过检查，到求值时才失败（见下）。结果类型：任一侧动态则动态；`/` 或含 float 则 float；否则 int。一元负号对字面量直接折叠（`parser.py` 550–562 行），其余写成 `0 - x`。本实现已复刻这一层。

**路线**沿用 [0014](0014-index-definitions-and-filter-expressions.md) 的静态选择：含路径或 JSON / 数组函数的走 Python，否则含动态键或 `text_match` 的走 hybrid，否则走 Arrow。算术不改变路线选择。rerank 的 boost 过滤（`RowFilter`）与稀疏检索中 MemTable 的逐行过滤（`RecordFilter`）在 3.x 中总是逐行走 Python 路线，所以同一个 `f * f` 在 segment 上按 float32、在这两处按 double 计算；这是 3.x 自身的行为，照样复刻。

所有求值期错误经 gRPC 均为 code 1，文案为异常原文（MemoryError 的文案为空）。

### Arrow 路线（[arrow_backend.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/search/filter/eval/arrow_backend.py#L177)）

字段为列，整数字面量是 int64 标量（超出 int64 时报 `OverflowError: Python int too large to convert to C long`），浮点字面量是 double 标量。先求左操作数、再求右操作数，结果再交给比较；标量与标量运算得到标量，再与列广播。

**类型提升**（`pc.add` / `subtract` / `multiply` / `divide` 实测）：

| 左 \ 右 | int8 | int16 | int32 | int64 / 整数字面量 | float32 | double / 浮点字面量 |
| --- | --- | --- | --- | --- | --- | --- |
| int8 | int8 | int16 | int32 | int64 | float32 | double |
| int16 | int16 | int16 | int32 | int64 | float32 | double |
| int32 | int32 | int32 | int32 | int64 | float32 | double |
| int64 | int64 | int64 | int64 | int64 | float32 | double |
| float32 | float32 | float32 | float32 | float32 | float32 | double |
| double | double | double | double | double | double | double |

- 整数运算按结果类型**回绕**：`int8 + int8` 在 int8 内回绕（`127 + 127 = -2`），与字面量运算则在 int64 内回绕（`9223372036854775807 + 1 < 0`）。
- 整数转浮点是**带检查的隐式转换**：转 float32 要求 `|v| ≤ 2^24`，转 double 要求 `|v| ≤ 2^53`，否则报 ArrowInvalid `Integer value 16777217 not in range: -16777216 to 16777216`，报第一个越界的值。整数字面量与 float32 列运算时同样按 float32 检查。**检查范围是这个整数操作数自己的有效位**，与另一侧是否为 null 无关：`n = [2^53+1, null]`、`f = [null, 1.0]` 时 `n + f` 照样报错；null 槽里存的越界值不检查。
- 浮点按结果类型的 IEEE 运算：float32 与 float32 的乘积是 float32（`f * f == 1.21` 为假），溢出为 `inf`。
- **除法**：先把左、再把右的整数**列**转成 double（带检查，范围 ±2^53，按各自的有效位），整数**标量**用 Python `float()` 转换（正确舍入、不检查）；之后按上表做浮点除法，`float32 / float32` 仍是 float32。除以零得 `inf` / `-inf` / `nan`，不报错。
- **一元负号**是 `0 - x`：`0` 是 int64 标量，所以 `-small` 在 int64 中计算（`-(-128) = 128`），`-f` 对 `+0.0` 得 `+0.0` 而不是 `-0.0`，列值 `-2^63` 取负回绕为 `-2^63`。字面量先折叠：`-(-9223372036854775808)` 折叠成 `9223372036854775808`，求值时报 OverflowError。
- null 传播：任一操作数为 null，结果为 null，比较后为「未知」。
- **内核缺失**：列表（ARRAY 字段）参与算术时报 ArrowNotImplementedError `Function 'add' has no kernel matching input types (list<item: int64>, int64)`。函数名为 `add` / `subtract` / `multiply` / `divide`，类型是转换之后的类型：`a / 2` 报 `(list<item: int64>, double)`，`i8 / a` 报 `(double, list<item: int64>)`，`-a` 报 `subtract` 的 `(int64, list<item: int64>)`。
- **空类型**（全为 null 的列，hybrid 中缺失或全 null 的动态键）：与有类型的一侧运算时取那一侧的类型（int8 / int16 / int32 / int64 保持同宽，float32 / double 保持不变；`/` 时整数侧先转成 double），结果是该类型的全 null 列；`null` 与 `null` 得 null 类型；与 bool / string / 列表运算报内核缺失，文案为 `(bool, bool)`、`(string, string)`、`(list<item: T>, list<item: T>)`。因此 `(x + 1) == 'a'` 在全 null 的 x 上报 `Function 'equal' has no kernel matching input types (int64, string)`（hybrid 中触发回退），而 `x == 'a'` 不会。可观察的后果：全 null 的 x 上，`x + 1 == 'a' and age == 9223372036854775808` 回退后由 Python 路线短路，不报错；`x == 'a' and age == 9223372036854775808` 与 `x + 1 == 2 and age == 9223372036854775808` 都报 OverflowError。

### Python 路线（[python_backend.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/search/filter/eval/python_backend.py#L203)）

逐行对 Python 值调用 `operator.add / sub / mul / truediv`；先求左右两侧，任一侧为 `None` 得 `None`；`TypeError` 与 `ZeroDivisionError` 被吞成 `None`（「未知」）；其余异常向上传播。

| 情形 | 结果 |
| --- | --- |
| int 与 int（含 bool） | 精确大整数；`True + 1 == 2` |
| int / int | 正确舍入到最近偶数的 float；结果超出 double 时报 `OverflowError: integer division result too large for a float`；除数为 0 → 未知 |
| int 与 float | int 先正确舍入成 float，超出 double 报 `OverflowError: int too large to convert to float`（不吞，且先于判零）；之后 IEEE 运算，溢出为 `inf`；`/ 0.0` → 未知 |
| str + str、list + list | 拼接 |
| str * int、int * str、list * int、int * list（bool 当 int） | 重复，见下 |
| 其余组合（str + int、list * float、str * str、dict 参与等） | TypeError → 未知 |

**重复的判定顺序**（实测，字符串与列表相同，除第 3 步外）：

1. 次数必须能放进 `Py_ssize_t`（64 位下为 `[-2^63, 2^63-1]`），否则报 `OverflowError: cannot fit 'int' into an index-sized integer`；基值为空时也报。
2. 次数 ≤ 0 或基值为空：结果为空，不再检查。
3. 长度乘积超过 `2^63-1`：字符串报 `OverflowError: repeated string is too long`（长度按码位计）；列表报 `MemoryError`（文案为空）。
4. 否则分配；机器内存不足时报 `MemoryError`。

### hybrid 路线（[hybrid_backend.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/search/filter/eval/hybrid_backend.py#L71)）

把引用的动态键物化为合成列后走 Arrow 路线，合成列的类型推断沿用 0014 已实现的规则。int64 合成列上同样回绕（`x = 2^62` 时 `x * x` 为 0），double 合成列上 `1 / x` 对 0 得 `inf`（Python 路线得未知）。ArrowInvalid（含整数转浮点检查失败）、ArrowTypeError、ArrowNotImplementedError（bool / string / 列表 / 空类型的内核缺失）以及 TypeError、NotImplementedError、KeyError 属于可回退的失败：丢弃本数据源（一个 segment 或 MemTable）的局部结果，整条表达式在该数据源上从头走 Python 路线；不是逐行或逐个条件回退。OverflowError 不在回退类别中：字面量超出 int64、`$meta` 含超出 int64 的整数时直接报错，与已实现的比较一致。

### 输入中的大整数从哪里来

pymilvus 在客户端拒绝超过 64 位的 JSON 整数，所以经 SDK 写入的数据里没有大整数；它们只能来自表达式中的字面量（Python 路线保留宽整数）、原始 RPC 写入的 JSON / 动态字段，以及 3.x Python API 写出的数据目录。3.x 的 `int()` 与 `json.loads` 拒绝超过 4300 位的十进制整数；本实现的表达式解析器与 JSON 路径解码沿用这一限制，但 Python 路线读取动态字段 `$meta` 时直接借用 serde 的任意精度数字（[python.rs](../../crates/core/src/filter/python.rs) 的 `operand`），这条路径上没有同样的检查。因此本设计不依赖 4300 位：任何十进制整数文本在解析成 `BigInt` 之前都按位宽估算检查（见「资源上限」）。

## 设计

**闸门**：

- [eval.rs](../../crates/core/src/filter/eval.rs) 的 `general_arithmetic` 被 `Predicate` 与 [filter.rs](../../crates/core/src/filter.rs) 的 `RowFilter::compile` 两处调用。它同时承担「含 `Kind::Unsupported`（几何谓词）就先于语义检查返回 UNIMPLEMENTED」（[0006](0006-m2-scope.md) 第 1 项）。改为只扫描 `Kind::Unsupported`，保留这一错误先后；`unsupported()` 的文案改为只提几何谓词（`geometry predicates are not supported`）；未开放路线的算术另用 `arithmetic is only supported on schema fields in ordinary filters`，第 2 步完全放开后删除。
- [tree.rs](../../crates/core/src/filter/tree.rs) 中 `Kind::Arith` / `Kind::Neg` 在类型检查通过后返回 UNIMPLEMENTED 的分支，改为得出 int / float / dynamic。
- 闸门分两步放开。第 1 步只放行静态路线为纯 Arrow 的表达式（不含动态键、路径、JSON / 数组函数与 `text_match`），hybrid 与 Python 路线的算术仍是 UNIMPLEMENTED，这样纯 Arrow 的组件真值与端到端用例在第 1 步就能跑，又不会让 hybrid 回退到尚未实现的 Python 算术时静默得到「未知」。第 2 步末三条路线都实现后完全放开。`RowFilter`（boost 过滤）与 `RecordFilter`（稀疏检索 MemTable）不论静态路线总是逐行走 Python，所以第 1 步它们各自保持「含算术即 UNIMPLEMENTED」：`RowFilter::compile` 与 `RecordFilter::compile` 在树编译（含语义检查）之后检查树中是否含数值算术，`RecordFilter` 不再依赖调用方先经 `Predicate::from_parsed`。闸门因此排在语义检查之后：语义非法的算术（如 boost 过滤中的 `flag + 1 > 0`）先报与 3.x 相同的语义错误，语义合法的才 UNIMPLEMENTED，比原先「先于语义检查」更接近 3.x；同时 Python 路线 `Kind::Arith` 的 `_ => Value::Null` 兜底在第 1 步就改为 UNIMPLEMENTED，未实现的组合显式失败。两者都在第 2 步放开。
- 含算术的比较不满足 `legacy_shape` 的「字段对字面量」，自然落到布尔树；纯比较的 S1 路径不变。Delete、iterator、count(*) 与分区都经 `Predicate::from_parsed`，不需另外接入；`RowFilter`、`RecordFilter` 各自接入 Python 路线的算术。

**一元负号**：在树绑定阶段把 `Kind::Neg(x)` 统一改写为 `Arith("-", Lit(Int 0), x)`，节点位置取负号的位置。改写在 `Tree::compile` 克隆的 AST 上做，发生在类型检查之后、构造 operands 与求值之前（已有的 `Kind::Neg` 类型检查分支及其文案保留），不改解析器（解析 fixture 依赖现有的 `Kind::Neg`）；新增的字面量节点排在父节点之后，三条路线的遍历都基于栈、不依赖节点次序。这样 [columns.rs](../../crates/core/src/filter/columns.rs)、[python.rs](../../crates/core/src/filter/python.rs) 的求值与子节点枚举只需处理二元算术；时间戳的取负仍由已有的类型检查拒绝。

**Arrow 列内核**（[columns.rs](../../crates/core/src/filter/columns.rs)）：

- `Column` 按上表得到结果类型：整数结果保留原宽度（`ColumnData::Int8` … `Int64`），浮点结果为 `Float` / `Double`，空类型按上文取另一侧的类型、两侧都是空类型时保持 `Shape::Null`；标量与列沿用现有的 `scalar` 标记，比较继续使用已有的按类型比较与安全转换。
- 整数运算用 `wrapping_*`；整数转浮点按目标类型检查 `|v| ≤ 2^24` / `2^53`，检查范围为该操作数自己的有效位 ∩ 活行，先左后右，按活行顺序报第一个越界值，失败归入可回退的内核失败；`/` 先把整数列按 double 检查转换，整数标量按正确舍入转换、不检查。派生列（算术结果）用自己的有效位。
- 列表与不支持的类型按 3.x 的文案报内核缺失（沿用 `compare` 中已有的 `column_type` 命名，类型取转换后的类型），归入可回退的内核失败。
- 只对活行求值与检查（0014 已登记的有效位差异照旧适用，见下文差异表）。
- 不引入 `BigInt`。

**Python 值**（[python.rs](../../crates/core/src/filter/python.rs)）：

- 新依赖：`num-bigint` 0.5.1 成为 `milvus-lite-core` 的**直接运行时依赖**（默认 feature 只有 `std`）。它已经通过 arrow-buffer / parquet 60.0 出现在 `Cargo.lock` 中，不新增 crate、版本、构建脚本或 C 代码，但 core 的依赖列表多一项；纯 Rust，MIT / Apache-2.0。只在 Python 路线使用，Arrow 与 hybrid 快路径不使用。
- `Value` 增加共享所有权的算术结果：整数 `Arc<BigInt>`（比较直接按数值，不转十进制文本），字符串 `Arc<Vec<u32>>`，列表 `Arc<Vec<SharedValue>>`。用 `Arc<Vec<T>>` 而不是 `Arc<[T]>`：先对 `Vec` 做 `try_reserve_exact` 再 `Arc::new`，数据不再搬动；`Arc<[T]>` 从 `Vec` 转换时会重新分配，且分配失败时 abort。`SharedValue` 是单独的类型，只含 clone 为 O(1) 的形式（`Null`、bool、float、`i64` 与 `Arc<BigInt>` 整数、借用的 JSON 节点、`Decoded`、保留「借用列 + 下标」以维持 NaN 身份的 `ArrayFloat`、`Arc<Vec<u32>>`、`Arc<Vec<SharedValue>>`；不含 `Cow::Owned`），列表元素只能是它，漏转时编译报错，元素按固定常量计费由类型保证。ARRAY 与 JSON 列表中的 null 元素转成 `SharedValue::Null`；ARRAY<TIMESTAMPTZ> 目前不存在，放开时再加时间戳形式。输入中的整数仍是现有的十进制文本，参与算术时才解析；表达式中的整数字面量在编译时解析一次。每个节点的值只被父节点读一次，`node_value` 读取时取走，不复制。比较（含大整数与 float、bool 与大整数、列表的相等与有序比较及 NaN 元素的身份语义）、`in`、LIKE、`text_match` 等已有逻辑要能接收这些结果。
- [python.rs](../../crates/core/src/filter/python.rs) 中 `Kind::Arith` 现有的 `_ => Value::Null` 兜底改为按上表逐项列出的组合，只有表中的 TypeError / ZeroDivisionError 情形得到「未知」，不留默认分支。
- 整数真除法：用 `div_rem` 取商和余数，按 CPython `long_true_divide` 的做法得到正确舍入（最近偶数）的 double，处理符号、次正规数与下溢；不先转 double 再除。int → float 同样正确舍入，超出报 `int too large to convert to float`。
- 重复按上文四步的顺序：先检查次数的类型与范围，再处理空结果，再检查长度乘积，最后查资源上限并分配。

**资源上限**（3.x 没有，是登记的差异）：

- `MAX_FILTER_VALUE_BYTES = 64 MiB`：单个字符串或列表结果按**本实现实际分配的字节数**计。字符串在 Python 路线中按码位存成 `u32`，每个码位 4 字节；列表每个元素按固定常量 `FILTER_LIST_ELEMENT_BYTES = 64` 计，代码中静态断言共享形式的元素 `size_of` 不超过它，元素内容共享、不重复计算。拼接与重复使用同一上限。
- `MAX_FILTER_INT_BITS = 2^20`（约 31.5 万位十进制）：大整数的位宽上限，以**估算值**为准：乘法为两侧 `bits()` 之和，加减为较大者加一；估算超过上限即拒绝（真实位宽可能小 1，边界测试按估算值写）。按位宽而不按 64 MiB，是因为大整数乘法的耗时随位宽超线性增长，64 MiB 的乘法每行就要数十秒。
- **输入、字面量与转换同样计入**，都在分配之前检查：
  - 行内输入的十进制整数文本（含 `$meta` 值）在解析成 `BigInt` 之前，按 `ceil(位数 × log2 10)` 估算位宽，超过 `MAX_FILTER_INT_BITS` 直接报资源错误，不先解析再检查。表达式中的整数字面量已由解析器限制在 4300 位以内（超出是解析错误 `invalid integer literal`，与 3.x 相同），约 1.43 万比特，不需要预检；它在编译期解析一次、各行共享，不按行计费。
  - 读取输入字符串时得到的码位数组（读 VARCHAR / JSON 列时由已有的取值逻辑生成，读 JSON 字符串时临时转换）是输入的表示，生成时不经预算：单个值的大小受输入本身约束。作为拼接或重复的操作数时，它只被读取、随即释放，不计费；成为结果列表的元素、被结果保留时才计费：已拥有的直接移入 `Arc`、不拷贝，此时它已经分配，计费只保证累计账目正确，不能阻止这次分配；借用的输入先计费、再按预算拷贝。两者都按码位数 × 4 字节计。
  - 输入列表 / ARRAY 转成共享形式：按元素数 × `FILTER_LIST_ELEMENT_BYTES` 计费，元素中的字符串按上一条处理。
  - 解析出的 `BigInt` 按 `bits / 8` 计费。
  - 这些转换与算术结果一样受单值上限约束，并计入每行累计。
- **每行累计**：Python 路线逐行求值，同一行所有新分配（算术结果，以及上一条的解析与转换；大整数按 `bits / 8`）累计不超过 64 MiB，下一行清零。行与行串行，节点值共享不复制，所以一次过滤求值中因算术新增的内存峰值不超过这一份预算（不含输入数据本身已占用的内存，也不含比较、LIKE 与读取 JSON 字符串时与输入或结果同量级的瞬时工作区）。
- 超限报新增的 `EngineError::FilterResourceExhausted`，gRPC 为 code 1（与 3.x 的 MemoryError 同码），文案固定为 `filter value exceeds the 67108864-byte limit` / `filter integer exceeds the 1048576-bit limit`。这个错误不被 TypeError → 未知吞掉，不是 UNIMPLEMENTED，在 hybrid 中不回退；[status.rs](../../crates/grpc/src/status.rs) 的穷尽匹配同步加上 code 1。用于 Delete 时整个请求失败，行集与序号不变。
- 先算大小再分配：长度与字节数用 `checked_mul` / `u128` 计算，`Vec` 的分配用 `try_reserve_exact`（按请求的确切容量计费，不让分配器多给的容量漏计），失败返回同一个资源错误（上限通过 crate 内的测试参数注入，使 `try_reserve_exact` 失败与边界路径可以在小尺寸上测试；正式构建中上限是常量）；不对不可信的大小直接 `with_capacity`、`repeat` 或 `vec!`。能保证不 abort 的只有这些 `Vec` 数据分配：`Arc::new` 的控制块分配（固定几十字节）与 `num-bigint` 内部的分配（位宽已先行受限）在内存耗尽时仍会 abort，登记为残余风险。
- 顺序：3.x 报 OverflowError 的情形（第 1 步，及字符串的第 3 步）照报原文；之后才检查本实现的上限。3.x 在第 3 步对列表报 MemoryError、在第 4 步可能成功或 MemoryError 的情形，本实现超限时报资源错误。
- 上限只约束内存，不约束 CPU 与时间：在大结果上做 LIKE、或在百万行上逐行构造接近上限的值，或 `$meta` 中 4300 位到预检上限之间的整数每行都要做一次平方复杂度的十进制解析（3.x 中不会有这样的数据），一个请求仍可能长时间占用工作线程且不可取消。3.x 同样如此，且本服务是进程内嵌入、单一使用者，受影响的是使用者自己的请求与进程；LIKE 对每个 token 分配长度加一的状态数组是既有实现。作为接受的残余风险登记，不加每次求值的总量预算（正常的逐行拼接在大数据集上也会累计出很大的总量，总量预算会误伤）。

**错误先后**：Arrow 与 hybrid 先求左操作数、再求右操作数，and / or 的所有操作数都求值（0014）；Python 路线按表达式顺序，and 遇 False、or 遇 True 短路，未访问的节点不产生错误；hybrid 中字面量的 OverflowError 在回退之前就报出，回退后 Python 路线中的 OverflowError 照样报出。

## 与 3.x 的差异

| 差异 | 3.x | 本实现 | 理由 |
| --- | --- | --- | --- |
| 过滤中间值的大小上限 | 无上限；字符串 / 列表重复或拼接按需分配，内存不足或列表长度乘积越界时报 code 1 空文案的 MemoryError；大整数无界增长 | 单值 64 MiB（按实际分配字节）、整数 2^20 位（按估算）、每行累计 64 MiB，超限报 code 1 固定文案；作为算术操作数的输入转成共享形式时也计入（如 ARRAY<VARCHAR> 的 `arr + arr`） | 3.x 的结果取决于机器内存，无法逐位对齐；不设上限时一个请求就能让服务 OOM 或长时间占满 CPU。正常数据（VARCHAR、ARRAY、JSON 的取值）及其转换远低于上限 |
| hybrid 动态列的类型推断：数字与其他类型混排（既有差异，算术让它改变数值） | pyarrow 按元素顺序推断并报错：`[2**64, 's', ...]` 先遇到超出 int64 的整数，物化时报 `OverflowError: Python int too large to convert to C long`，而 `['s', 2**64]` 报 ArrowTypeError、整源回退 Python；float 之后出现的 bool（`[1.5, 0.1, False, 0.1]`、`[0.0, 2, 1.5, True]`）推断为 double，bool 当作 0.0 / 1.0：`4611686018427387904 / x` 在 False 行得 inf、命中，`x * 1180591620717411303424` 与 `x + 1 == 2 and age == 9223372036854775808` 因字面量超出 int64 报 OverflowError | 数字与 bool、字符串、列表等混排时一律整源回退 Python 路线：上述 `[2**64, 's', ...]` 不报错、按 Python 求值（`'s' * 2**70` 报 `cannot fit 'int' into an index-sized integer`）；False 行除零得未知、不命中；乘以大字面量得到精确结果；`and` 按 Python 短路，不报字面量溢出 | 推断与报错先后改的是所有 hybrid 过滤共用的 [dynamic.rs](../../crates/core/src/filter/dynamic.rs) `infer`，不在算术切片中修改；复刻 pyarrow 的推断与报错顺序另开切片处理，届时注销本条。影响用例：[filter_arith.json](../../crates/core/tests/fixtures/filter_arith.json) 中这四组动态值，3.x 一侧留在语料，本实现一侧固定在 [tree_tests.rs](../../crates/core/src/filter/tree_tests.rs) 的 `REGISTERED_HYBRID_INFERENCE` |
| 算术下的有效位差异（沿用 0006 / 0014 已登记的「只看活行」） | 已删除或被覆盖的旧版本行参与 hybrid 的类型推断与 Arrow 的转换检查 | 只看活行 | 原差异写的是「可能选择不同 backend / 报不报错」。有了算术后它还会改变**数值**：例如 3.x 因某个死行是字符串而整源回退到 Python（精确大整数），本实现只看活行、走 Arrow（int64 回绕），`x * x` 的结果不同。在 0006 的登记中补写这一后果 |

接受的残余风险（见上文「资源上限」，验收时复述）：CPU 与时间不设上限，内存上限不等于 CPU 上限；`Arc` 控制块与 `num-bigint` 内部分配在内存耗尽时 abort。

## 已并入第 1 步修正的既有问题

复核发现：已有的列比较（[columns.rs](../../crates/core/src/filter/columns.rs) 的 `compare`）只在两侧都有效时才做整数转浮点的检查，而 3.x 按整数操作数自己的有效位检查，`n == f`（`n = [2^53+1, null]`、`f = [null, 1.0]`）在 3.x 中报 ArrowInvalid，本实现返回结果。这不是本切片引入的，也没有登记。负责人决定在本切片修正、不登记为差异：只改 `compare` 中转换检查的位置：比较只转换整数那一侧，按该操作数自己的有效位 ∩ 活行检查，文案与 3.x 相同；补对应的组件真值（两侧 null 错位、null 槽中的越界值不报）。「先左后右」只涉及两侧都要转换的 `/`。本切片的算术内核按同一规则实现。

## 实施步骤

每一步做完由子 agent 复核，直到没有问题。

1. **Arrow 列内核与纯 Arrow 开闸**：修正上文「已并入第 1 步修正的既有问题」中 `compare` 的检查位置；类型提升、回绕、带检查的转换、除法、负号改写、标量广播、列表与空类型的内核缺失、错误分类；纯 Arrow 路线对外放开，hybrid、Python 以及 boost 过滤与稀疏检索 MemTable 中的算术仍 UNIMPLEMENTED（第 1 步验收含这两处仍返回 UNIMPLEMENTED 的用例）。
2. **Python 值并开闸**：`num-bigint` 依赖、共享的结果值、整数与浮点运算、正确舍入的真除法、字符串与列表的拼接和重复、资源上限与每行累计预算；`RowFilter` / `RecordFilter` 接入；完全放开闸门，只保留几何谓词的前置扫描。
3. **真值、差分、文档**：
   - **组件真值**：新建同结构的算术生成器（不扩充现有 `FIELDS`，以免改动全部既有 fixture），Arrow 15 / 25 各核验一次。覆盖：
     - 类型提升全表、回绕、±2^24 / ±2^53 边界、除零与 `nan` / `inf`、`-0.0`、字面量折叠与列值取负；
     - 可空整数与浮点列（null 槽里放越界值、两侧 null 错位）；
     - 标量与标量、嵌套中间类型（`(small + small) + age`、派生列再进入 `/`）；
     - 含 null 元素的 ARRAY 与 JSON 列表的重复、拼接与相等；
     - 输入的位宽预检（`$meta` 中超长整数）与借用字符串、列表转换的计费；
     - ARRAY 字段与空类型的内核缺失文案，空类型取另一侧类型的各组合，以及上文三条差分对子；
     - 一元负号的类型错误文案与列号（`-flag`、`-s`、`-ts`、`-vector`）；
     - Python 路线结果与既有比较的互操作：`x + y == x + y`、`x * 2 == y + y`、大整数与 float、bool 与大整数、字符串结果的 LIKE、列表结果的相等与有序比较（含来自同一 JSON `NaN` 的元素）；
     - 负数与 bool、大整数真除法的舍入（正负、恰在中点、次正规、下溢、上溢）；
     - 字符串与列表的拼接与重复（四步顺序的每个分支）；
     - 错误先后（左右顺序、and / or 全求值与 Python 短路、可回退与不可回退、回退前后的 OverflowError）；
     - hybrid 的整源回退，`text_match` 与算术同在一个表达式。
   - **模糊对照**：仿 [fuzz_temporal.py](../../tests/differential/fuzz_temporal.py) 增加算术的直接对照，覆盖 Arrow 的值 × 有效位 × 标量 / 列组合，以及 Python 路线随机的 int / float / str / list / bool / None 与四个运算符。
   - **资源上限**：本实现单侧的边界测试（恰在上限与上限加一，按估算值；每行累计；大元素的 ARRAY&lt;VARCHAR&gt; 乘大次数，验证按预期拒绝且共享转换不随次数放大；经注入的小上限测 `try_reserve_exact` 失败路径；资源错误使 Delete 失败、行集与序号不变）。3.x 一侧只固定确定的点（OverflowError 类，以及 `'a' * 2**62`、`[1] * 2**62` 这类必然分配失败的情形），不依赖取决于机器内存的结果；差分里两边分别断言。
   - **端到端差分**：Query / Search / Delete / HybridSearch / iterator 的算术过滤，覆盖 MemTable、flush 与重开（同一表达式在 MemTable 与 segment 上走不同路线时的结果）、死行改变 hybrid 推断导致的数值差异（按登记两边分别断言）、稀疏检索 MemTable 与 segment，两条 SDK 线。
   - **参照测试**：[tests/adapter/test_milvus_compat_v2.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/tests/adapter/test_milvus_compat_v2.py) 的 `test_add_in_filter`、`test_divide_in_filter`、`test_arithmetic_in_search_filter` 经 pymilvus 走 gRPC，按 adapter 规则复制并替换起服务的 fixture。[tests/search/filter/test_e2e.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/tests/search/filter/test_e2e.py)、`test_semantic.py` 与 `tests/engine/test_collection_filter.py` 直接导入 3.x 的 `parse_expr`、`compile_expr`、各 backend 与 `Collection`，不能复制运行，只作真值来源：把其中的表达式、数据与期望（含错误）转成组件真值或差分用例。[INVENTORY](../../tests/python/adapter/INVENTORY.md) 只盘点 `tests/adapter`：把 `test_milvus_compat_v2.py` 的整文件条目拆出这三条标为已复制，并更新状态表的条数；内部测试不进 INVENTORY，作为真值来源的清单列在本切片的验收记录中。
   - **rerank**：[test_rerank_scenarios.py](../../tests/differential/test_rerank_scenarios.py) 中 `FILTER_RANGE` 的三条（`s boost filter arithmetic`、`dyn` / `nodyn boost filter age + 1 > 5`）改为两边对照。
   - **随开闸与文案改动同步的断言**：[test_grpc_translation.py](../../tests/python/test_grpc_translation.py) 中以 `id + 1` 等算术断言 UNIMPLEMENTED 的各处（749、999、1085、1091、1254 行附近），[timestamptz.rs](../../crates/core/tests/timestamptz.rs) 445–457 行对 `id + 1 > 2`、`-id > 0` 的 `Unsupported` 断言，[test_filter_s1.py](../../tests/differential/test_filter_s1.py) 343–344 行对 `age + 1 > 0` 的断言，[test_timestamp_filters.py](../../tests/differential/test_timestamp_filters.py) 的 `GENERAL_ARITHMETIC` 集合及其「先于语义检查」的断言，[filter.rs](../../crates/core/src/filter.rs) 的 `out_of_scope_constructs_stay_unimplemented`。
   - **文档**：0006（588、594、603 行附近的登记）、0014（267 行与已确认取舍第 3 条标注被本文取代）、0025（112、182 行的登记差异）、0026（373 行）、INVENTORY 中关于算术的条目、AGENTS 的技术栈（`num-bigint` 直接依赖）、知识索引与架构地图、README 的支持范围。
   - 不对算术过滤作性能声明；跑 S1 过滤专项确认纯比较路径没有退化。

## 验收

实施后补充。
