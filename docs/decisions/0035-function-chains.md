# 0035：公共 function chain

状态：设计第一稿 2026-11-15 评审通过（0 阻断，5 项建议已并入本稿）；第 1 步（解码与校验）已实施并评审通过（2026-11-15）；第 2 步（执行与结果）已实施并评审通过（2026-11-15）；第 3 步（证据与文档）已实施并评审通过（2026-11-16）。动手前的调查（三轮真实 3.x 探针、2.6 线原始请求验证）经负责人复核，决策 A–F 已确认。参照版本 `1320d26`（pymilvus 3.0.2 / 2.6.18、CPython 3.12）。

## 范围

`SearchRequest.function_chains`（pymilvus 3.0 的 `client.search(function_chains=FunctionChain(...))`）：阶段为 L2 rerank 的一条链，算子 `map` / `sort` / `limit`，表达式 `num_combine` / `decay` / `round_decimal`，以及 `rerank_model` 的校验。[0026](0026-rerank-and-hybrid-search.md) 把它划在范围外，Search 收到它一直返回 `UNIMPLEMENTED`。

不包含：

- 3.x 也不支持的能力：其他阶段、其他算子与表达式（`filter`、`merge`、`xgboost` 等）、多条链、HybridSearch 上的 `function_chains`（3.x 报 code 6，本实现已相同）。
- `rerank_model` 的执行（决策 B）。

## 3.x 的行为

来源：参照的 [function_chain.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/adapter/grpc/function_chain.py)、[translators/function_chain.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/adapter/grpc/translators/function_chain.py)、[validator.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/function/validator.py)、[compiler.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/function/compiler.py) 与 `ops/`、`expr/` 下的实现，以及在真实 3.x 上的探针（约 330 个场景，本地留存，见“证据”）。下面只写合同要点，逐条文案以参照代码和第 3 步的组件真值为准。

### 1. 执行位置

引擎按**普通 Search** 执行：`limit`、`offset`、分组、范围检索、过滤、分区都照常，不放大候选窗口。链作用在引擎返回的命中行上，每个查询各自一个 chunk、互不影响。链执行完之后才做 search 级的 `round_decimal`，再编码响应。

### 2. 行与名字

每个命中是一行：`$id` 与主键名都是主键值，`$score` 是公共分数（BM25 见第 6 条），其余是引擎返回的 entity。

- 可读的名字：`$score`、`$id`、主键名、先前 `map` 的输出，以及类型为 BOOL、INT8–INT64、FLOAT、DOUBLE、VARCHAR、TIMESTAMPTZ 的 schema 字段。其余 `$` 开头的名字、动态字段、JSON、ARRAY、GEOMETRY 与各种向量字段都不能读（code 6，三种不同文案）。
- 可写的名字：`$score`，或不以 `$` 开头且不与 schema 字段同名的名字。输出可以与动态字段同名，只在链内可见。
- 每个名字有一个种类（数值、文本、布尔、时间戳），由字段类型或表达式的输出决定；表达式的输出都是数值。主键的种类随主键类型。

### 3. 解码与校验

全部是 code 6，按下面的先后：

1. `function_score` 与 `function_chains` 同时出现。
2. `order_by_fields` 冲突（第 7 条）。
3. 链的条数不是 1。
4. 逐个算子解码：算子名、表达式名、输入名、输出名为空；参数或字面量的 oneof 未设置；对象参数的空键。**所有算子解码完才开始校验**，后面算子的解码错误先于前面算子的校验错误。未知的阶段枚举值在解码的最后报。
5. 阶段不是 L2 rerank；没有算子。
6. 主键名是 `$id` 或 `$score`（3.x 允许建这样的 collection，本实现也允许）。
7. 逐个算子校验，文案带 `function chain op[i]` 前缀，表达式内的错误再带 `expression <名字>`：
   - `map`：必须有表达式、不得声明 `inputs`、恰好一个输出；参数解析与种类检查；输出名检查。
   - `sort`：不得有表达式与输出；`column`（非空字符串）、`desc`（布尔，缺省真）、`tie_break_col`（可选的非空字符串）；`inputs` 必须等于 `(column,)` 或 `(column, tie_break_col)`；输入名可读。
   - `limit`：不得有表达式、输入、输出；`limit` 是正整数，`offset` 是非负整数（缺省 0），布尔不算整数。
   - 其他算子名：`unsupported function chain operator`。算子名与表达式名大小写敏感。
   - 算子的多余参数，以及 `num_combine` / `decay` / `round_decimal` 的多余参数被忽略；`rerank_model` 例外（见下）。

表达式的校验：

- `num_combine`：至少两个数值参数（列或有限的数值字面量）；`mode` 是 `sum`（缺省）/ `multiply` / `max` / `min` / `avg` / `weighted` 之一；`weighted` 要求与参数等长的有限数值 `weights`，其他模式不得带 `weights`。
- `decay`：恰好一个数值或时间戳参数；`function` 是 `gauss` / `exp` / `linear`；`origin`、`scale` 必填，`offset` 缺省 0、`decay` 缺省 0.5，都必须是有限数值（布尔不算），且 `scale > 0`、`offset >= 0`、`0 < decay < 1`。
- `round_decimal`：恰好一个数值参数；`decimal` 是 0–6 的整数。
- `rerank_model`：恰好一个文本参数；参数里不得有 `base_url`、`api_key`、`token`、`secret`（键名不分大小写）；`queries` 是非空的字符串序列且每个元素非空，个数等于 nq；`provider` 只有 `cohere`（不分大小写）；`model_name` 必须是字符串；其余参数名报 `unsupported rerank_model parameter`（按 `repr` 排序取第一个）。

### 4. 执行

- `map`：按 chunk 读入各参数列（字面量重复成列），算出一列写回。
- `num_combine`：
  - 任一输入为 `None` 时该行得 `0.0`。
  - 输入不是有限数值时报 code 1 `num_combine row values must be finite numeric non-bool values`。
  - `sum` 是 Python 的 `sum`：整数精确（可超过 int64），遇到浮点后按浮点逐个相加。
  - `multiply` 从 `1.0` 起乘，结果总是浮点；`max` / `min` 是 Python 的比较，返回第一个最大 / 最小的元素本身；`avg` 是 `math.fsum(v / n for v in values)`；`weighted` 是 `sum(v * float(w))`。
  - 结果不是有限数（含整数大到无法转成浮点）时报 code 1 `num_combine produced a non-finite result`。整数因此不会超过 2^1024。
- `decay`：`None` 得 `0.0`；TIMESTAMPTZ 取 epoch 秒：3.x 的行里是带时区的 `datetime`，`timestamp()` 的结果等于存储的 UTC 微秒值（整数）除以 10^6 的正确舍入结果，本实现直接从微秒值这样算；其余 `float(value)` 后套 [0026](0026-rerank-and-hybrid-search.md) 已实现的 decay 公式。
- `round_decimal`：`None` 保持 `None`，其余是 Python 的 `round(value, decimal)`（整数仍是整数）。
- `sort`：`None` 与浮点 NaN 不论升降序都排在最后；主列相等时按 `tie_break_col` 升序（它的 `None` / NaN 也在最后），没有 `tie_break_col` 时保持原顺序；排序稳定。
- `limit`：先跳过 `offset` 行，再取 `limit` 行。
- `rerank_model`：见决策 B。

### 5. 结果

- 分数取链结束时的 `$score`，经 Python 的 `float()` 后按 float32 返回：超出 float32 范围得 `inf`，整数分数同样处理。
- `$score` 是 `None` 时（例如 `round_decimal` 作用在可空字段上）在响应编码处报 code 1，先后是：search 级 `round_decimal` 的 `round()`（`type NoneType doesn't define __round__ method`）→ 请求时区的第二次校验 → `float()`（`float() argument must be a string or a real number, not 'NoneType'`）。只有链结束时还在结果里的行参与，被 `limit` 截掉的不报错。
- 分组值随行走；`topks` 是每个查询链结束后的行数。
- 投影见下节。

### 6. BM25

`$score` 取 `-distance`，链执行后**不再取回负号**：不带链时 BM25 的分数是负数，带任何链（哪怕只有 `limit`）都变成正数。稀疏 IP 与 dense 的三种度量不变号。这是 3.x 的明确行为（参照有单元测试），照做。

### 7. `order_by_fields`

3.x 的 Search 只用这个 search_params 键判冲突：值先按 JSON 解析（解析失败保留原文），结果为真且带链时报 code 6 `order_by and function rerank cannot be used together`。`0`、`[]`、`null`、`""` 不冲突，`" "` 冲突。不带链时这个键被忽略。

### 8. 错误先后

collection 不存在（100）→ 请求自身的解析错误 → `anns_field` 与度量的解析（6）→ 第 3 节的全部检查（6）→ `rerank_model` 建 provider（决策 B）→ 未加载（101）→ 过滤等引擎内的检查 → 链的运行期错误（1）。输出字段不在先后里：3.x 对它只做投影，不认识的名字只是不返回，不报错。

实测样例：未加载的 collection 上非法链报 6、合法链报 101；`anns_field` 错误先于链的错误；过滤语法错误晚于链的错误。

Search iterator 发的也是 `SearchRequest`，3.x 对带链的请求不区分是否带 iterator 参数；本切片同样不特殊处理，矩阵里用原始请求覆盖一格。

## 投影

实测 3.x（探针 `probe2` / `probe3`，20 余格）：**除 `output_fields` 含 `*` 或 `$meta` 外，带链的投影与同一请求不带链时逐格相同。** 包括：

- 缺省（不传 `output_fields`）：全部非主键字段与 `$meta`，链读过的字段照常返回。
- 显式字段：只返回点名的；链读了没点名的字段，它不返回。
- 显式动态字段：`$meta` 里只有点名的键。
- 临时列：不返回。点名一个与动态字段同名的临时列，返回的是原动态字段值。
- 主键、重复字段名、JSON / ARRAY / 向量 / TIMESTAMPTZ 字段、分组、`time_fields`、无动态字段的 collection 上点名不存在的字段：都与不带链相同。
- 例外（第 2 步实测，探针 `probe5` / `probe6`）：名为 `$id`、`$score` 的名字被去掉。schema 里叫这两个名字的非主键字段返回空值占位，动态字段里的这两个键不返回；不带链时照常返回。来源是 3.x `_project_entity` 跳过 `$id` / `$score`，与 [0026](0026-rerank-and-hybrid-search.md) 的 L2 链相同。本实现照做，不登记差异。

含 `*` 或 `$meta` 时 3.x 有缺陷：除主键与另外显式点名的字段外，schema 字段全部返回类型默认值（`0`、`""`、JSON `null`、空数组、零向量），并把 `valid_data` 全标为假（SDK 读成 `None`，非 nullable 字段也一样），`$meta` 只剩显式点名的动态键（没有则为 `{}`）；只有 `limit` 的链也这样。

本实现的合同（决策 A）：**带链时的投影与同一请求不带链时完全相同**（上面名为 `$id` / `$score` 的例外照 3.x），链只改变行的顺序、行数与分数。实现上不另写投影逻辑，响应沿用普通 Search 的输出路径；链需要的字段只进入内部取列（见“设计”第 3 条），不进入响应。

## 设计

1. **分层**与 3.x 相同：
   - `crates/grpc` 新增 `function_chain` 模块，把 proto 解码成与协议无关的链表示（第 3 节第 4 步的错误在这里报），并解析 `order_by_fields`。
   - `crates/core` 在 `rerank` 下新增公共链模块：按 schema 校验链表示（第 3 节第 5–7 步）、得出需要取值的字段、执行。值与比较复用 `rerank::pyval`，decay 公式与 Python `round` 复用 0026 的实现，超过 int64 的整数用 [0032](0032-arithmetic-filters.md) 已引入的 num-bigint。不新增依赖。
2. **入口**：[service.rs](../../crates/grpc/src/service.rs) 的 Search 在度量解析之后、读快照（未加载检查）之前完成链的解码、校验与 `rerank_model` 的处理，保持第 8 节的先后。现有的 `check_function_chains` 并入这一步。
3. **取列与取值**（[collection.rs](../../crates/core/src/collection.rs) 的 `search_chained_in`）：
   - 校验产出链读到的 schema 字段清单（按首次出现的顺序，不含 `$score`、`$id`、主键名与临时列），对应 3.x 的 `required_schema_fields`。
   - 引擎按请求本身检索（投影就是请求的 `output_fields`），得到每个查询排好的命中（行位置与分数，还没有取列）。链的行只含 `$score`、`$id`、清单里的字段与临时列，清单里的字段直接按命中的行位置从存储列读取。3.x 的做法是把清单并进内部的 `output_fields`，再在响应里按请求的字段投影回去；两者可观察的结果相同，这里不必扩大投影。
   - 链结束后，按结果行重排命中的行位置，回写分数，分组值按新的行位置取；然后走普通 Search 的取列与输出路径。没被点名的链输入与临时列因此都不返回。
   - `$score` 是 float32 内部分数按 3.x 换算出的 float64 对外分数（与 0026 相同）；BM25 取负。
4. **资源**：链在 RPC 所在的阻塞线程上执行，不进计算池；行数不超过引擎返回的命中数，整数不超过 2^1024，不需要另设上限。

## 决策

| | 事项 | 结论 |
| --- | --- | --- |
| A | 带链时 `output_fields` 含 `*` 或 `$meta` 的 3.x 投影缺陷 | 不照抄。投影与不带链时相同，登记差异 |
| B | 合法的 `rerank_model` | 校验类错误与 3.x 逐字一致；通过全部校验后，在未加载检查之前返回带说明的 `UNIMPLEMENTED`，登记差异。不创建 provider、不读 `COHERE_API_KEY`、不发外部请求（与 0026 对 model rerank 的做法一致） |
| C | pymilvus 2.6 线 | 2.6 的 SDK 与 proto 都没有 function chain。测试用手写的线格式编码构造链，经“未知字段保留 + 普通 stub”发送；两条线用例数相同、零跳过。见“测试机制” |
| D | 参照用例 | 28 个函数改后复制（断言不动，只换链的构造与发送），3 条不复制并写明覆盖方式，1 条已复制。见“参照用例” |
| E | BM25 分数变号、`order_by_fields` 的判定、主键名为 `$id` / `$score` 的拒绝 | 照 3.x 复刻 |
| F | 实施分步 | 三步，各自带测试、评审后单独提交 |
| G | 按名为 `$score` / `$id` 的字段分组时，3.x 用分组值盖掉链里的 `$score` / `$id`（第 2 步评审补测时发现，2026-11-15 负责人确认） | 不照抄。分组值只进分组输出，分数与 id 不受影响，登记差异 |

## 实施步骤

1. **解码与校验**：proto 解码、链表示、按 schema 的校验、`order_by_fields`、`rerank_model` 的校验与 `UNIMPLEMENTED`、错误先后。
   - 这一步结束时还没有执行：**通过全部校验的链仍返回 `UNIMPLEMENTED`**（沿用现有文案），不得静默成功；第 2 步放开。返回的位置与 `rerank_model` 的相同：全部校验之后、未加载检查之前。现有的 `UNIMPLEMENTED` 在未加载检查之后，这一步要前移，否则“未加载 + 合法链”会报 101。这是只存在于两次提交之间的中间状态，不随任何版本发布。
   - 测试：解码与校验每条文案的单元测试；经原始请求的错误先后（未加载、`anns_field`、过滤、collection 不存在）；`function_score` 冲突、`order_by_fields` 各取值、多条链；主键名 `$score` / `$id`；合法链与合法 `rerank_model` 返回 `UNIMPLEMENTED` 的断言（前者第 2 步改为与 3.x 对照）。
2. **执行与结果**：四个表达式中的三个、三个算子、分数回写、BM25、分组、`round_decimal`、投影。
   - 测试：各表达式与算子的单元测试（含 `None`、NaN、超过 int64 的整数、`fsum`、非有限结果）；经原始请求的端到端用例覆盖投影表的每一格、三种 dense 度量、BM25、稀疏 IP、多查询、分组、`offset`、范围检索、`time_fields`。
3. **证据与文档**：组件真值、端到端差分、参照用例、SDK 冒烟、已有用例与清单的更新、README、AGENTS、验收记录。
   - 计数同步：`tests/python` 至少增加 31 条（改后复制 30 条加 SDK 冒烟 1 条，另加前两步的端到端用例），差分收集数与会话组数随新文件变化；确切数字以第 3 步收集到的为准，不在这里预先写死。
   - `tests/python` 的同步点：[ci.yml](../../.github/workflows/ci.yml) 两处、[scorecard.py](../../tests/release/scorecard.py)、[test_candidate.py](../../tests/release/test_candidate.py)、[打包验收 README](../../tests/packaging/README.md)、AGENTS 命令表、INVENTORY。
   - 差分的同步点：[compare.yml](../../.github/workflows/compare.yml) 两处、`scorecard.py`、`test_candidate.py`、[分片 README](../../tests/sharding/README.md)。

每一步提交时本步改动的行为都有测试；第 3 步补的是与 3.x 的系统性对照。

## 与 3.x 的差异

- **`*` / `$meta` 投影**（决策 A）：带链且 `output_fields` 含 `*` 或 `$meta` 时，3.x 返回类型默认值（`valid_data` 全为假）与残缺的 `$meta`，本实现返回真实值。其余投影相同。
- **合法的 `rerank_model`**（决策 B）：3.x 没有 key 时报 code 1 `Cohere API key is required...`，有 key 时调用外部服务；本实现返回 `UNIMPLEMENTED`。两边都发生在未加载检查之前。
- **按名为 `$score` / `$id` 的字段分组**（决策 G）：3.x `execute_search_function_chain` 在建行时先写 `$id` / `$score`，再执行 `row[group_by_field] = hit["_group_by_value"]`，分组字段恰好叫这两个名字时，分组值盖掉了虚拟列。实测（探针 `probe8`）：
  - 按 `$score` 分组：链的入口分数是分组值，响应分数随之变成分组值；分组值是字符串时报 code 1 `could not convert string to float`，链读 `$score` 时报 `num_combine row values must be finite numeric non-bool values`。
  - 按 `$id` 分组：链结束后按 `$id` 找回原命中，分组值不是本查询的主键时报 code 1 `function chain runtime output chunk 0 row 0 has unknown $id value ...`；恰好等于别的行的主键时，响应的 id 换成那个主键（可出现重复 id），字段值仍是原行的。
  - 本实现不盖：分组值照常进分组输出，分数与 id 与不按这个字段分组时相同；名为这两个名字的字段在 entity 里仍按“投影”的例外返回空值占位。不带链时两边相同。

## 测试机制

2.6 线的事实：pymilvus 2.6.18 的 `SearchRequest` 没有 `function_chains`（3.0.2 是 24 号字段），`schema_pb2` 里也没有任何 FunctionChain 消息，客户端没有 `FunctionChain`。

- **构造**：测试辅助里的手写线格式编码（不依赖 protobuf 生成类），两条线用同一段代码得到同一份字节。支持 bool、int（含负数）、float、str、bytes、数组、对象与未设置的 oneof（`None`）。
- **发送**：把 24 号字段的字节并入 `SearchRequest`（`MergeFromString`），经普通 stub 发送；2.6 的消息保留未知字段，字节原样带出，响应就是普通的 `SearchResults`。[0026](0026-rerank-and-hybrid-search.md) 对 HybridSearch 的 15 号字段已用同样的写法。已验证：2.6.18 客户端对真实 3.x 服务，带链请求返回正确的重排结果与两种错误文案。
- **编码的正确性**：新的真值脚本在参照环境（pymilvus 3.0）运行，对**测试里用到的每一种链形态**把编码结果解析回 `schema_pb2.FunctionChain`，与 SDK 构造的 `to_proto()` 比较相等；SDK 构造不出的形态（未设置的 oneof、空名字、未知算子等）按字段逐项核对。覆盖至少：三个算子、四个表达式、`rerank_model` 的 `queries` 与各参数、加权数组、负整数、空数组与空对象、`$id` / `$score`、字符串主键的链、未设置值。脚本带 `--verify`，不占 pytest 用例数。
- **SDK 冒烟**：保留一条经 `client.search(function_chains=...)` 的端到端用例。为了不违反零跳过与两条线同数，它是同一个用例函数：3.0 线同时用 SDK 与手写编码各发一次，断言两次结果相同且等于期望；2.6 线只发手写编码的那一次，断言同一期望。

## 参照用例

两份参照文件的链全部用 pymilvus 3.0 的 `FunctionChain` / `col` / `fn` 构造（标着“原始请求”的用例也是 `.to_proto()` 后放进请求），没有一条能在 2.6 线原样运行。因此本切片对“测试体原样复制”的规则做一处范围明确的偏离，称为**改后复制**：

- 可观察行为的断言不变：ids、分数、顺序、字段、错误码与文案保持参照原样。
- 链的构造与请求的发送换成上节的机制：[chainwire.py](../../tests/python/chainwire.py) 提供与 pymilvus 3.0 `FunctionChain` / `FunctionChainStage` / `col` / `fn` 同名同参的写法，用例体里构造链的代码因此不变；`client.search(function_chains=chain)` 改为在 `with sending(client, chain):` 里发出同一个 search（SDK 照常解析响应），原始请求的 `function_chains.append(chain.to_proto())` 改为 `attach(request, chain)`。
- 依赖参照内部对象或调用顺序的断言（monkeypatch），替换为等价的可观察断言；只有下表点名的四条有这种替换，替换点逐条写明。
- 不因 2.6 的写法放宽断言，也不把 2.6 特有的行为写成通过。
- [INVENTORY.md](../../tests/python/adapter/INVENTORY.md) 逐条标“改后复制”，写明链的构造与发送方式；有断言替换的四条另注明替换点。

[tests/adapter/test_function_chain.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/tests/adapter/test_function_chain.py)（22 个函数，展开 24 条）：

| 用例 | 处理 |
| --- | --- |
| `reranks_by_hidden_field_and_serializes_score`、`preserves_explicit_dynamic_output_field`、`preserves_default_dynamic_output_fields`、`preserves_explicit_dynamic_field_on_temp_collision`、`preserves_default_dynamic_field_on_temp_collision`、`limit_changes_each_query_chunk`、`keeps_query_chunks_independent`、`without_sort_preserves_ann_order`、`explicit_id_tie_break`、`rewrites_metric_scores`（3 条）、`preserves_group_by_response_metadata`、`rejects_function_score_function_chain_conflict`、`rejects_unknown_rerank_provider_as_illegal_argument`、`rejects_request_rerank_endpoint_as_illegal_argument`、`raw_hybrid_search_rejects_function_chains` | 改后复制 |
| `uses_primary_field_name_as_id_alias` | 改后复制；去掉对参照 `Collection.search` 入参的观察（本实现没有对应对象），保留结果、分数与 `output_fields == ["title"]` 的断言 |
| `rejects_order_by_function_chain_conflict_before_ann`、`rejects_unsupported_rerank_param_before_ann` | 改后复制；参照用 monkeypatch 让引擎检索直接失败来证明“先于检索”，改为在未加载的 collection 上发同一请求（报 6 而不是 101） |
| `rejects_primary_name_conflicting_with_system_column` | 改后复制；参照用 monkeypatch 替换 schema，改为经公开接口建主键名为 `$score` 的 collection（3.x 与本实现都允许） |
| `keeps_requested_candidate_budget` | 不复制：断言的是参照引擎的入参。“不放大候选窗口”由差分用例覆盖（`limit` 小于总行数时链只在最近的那几行里重排） |
| `parse_search_request_accepts_legacy_shape_without_new_messages` | 不复制：直接调用参照内部函数（现状不变） |
| `search_without_function_chain_preserves_legacy_behavior` | 已复制 |

[tests/compatibility/test_function_chain_l2.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/tests/compatibility/test_function_chain_l2.py)（10 条）：

| 用例 | 处理 |
| --- | --- |
| `sdk_reranks_by_scalar_field`、`sdk_uses_hidden_input_field`、`sdk_temp_column_not_returned`、`sdk_limit_op`、`rejects_write_readonly_system_column`、`rejects_reserved_temp_output`、`rejects_read_internal_system_input`、`rejects_read_unknown_system_input`、`rejects_with_order_by` | 改后复制 |
| `rejects_with_function_score` | 不复制：断言的是 pymilvus 3.0 客户端的 `ParamError`，请求不发出；服务端的冲突由上表的 `rejects_function_score_function_chain_conflict` 覆盖 |

合计改后复制 28 个函数（30 条）、不复制 3、已复制 1。compatibility 文件整体复制到 [tests/python/compatibility/](../../tests/python/compatibility/test_function_chain_l2.py)，起服务的 fixture 换成本包的 server_manager，并去掉上游在 pymilvus 没有 Function Chain API 时的整模块跳过。

## 测试矩阵

- **组件真值**：新生成器在参照环境直接调用 3.x 的解码、校验与执行函数，产物入库，`--verify` 重算比较。
  - 解码与校验：第 3 节每一类错误的文案，含先后（后面算子的解码错误先于前面算子的校验错误）。
  - 执行：`num_combine` 六种模式的数值（固定种子的随机输入加边界：`None`、NaN / 无穷、超过 int64 的整数、整数与浮点混合、接近溢出、`fsum` 的抵消情形）；`decay` 的三种函数与 TIMESTAMPTZ；`round_decimal`；`sort` 的各种列种类、`None` / NaN、平局与稳定性；`limit`。
  - 线格式编码的等价核对（见“测试机制”）。
- **端到端差分**（两条 SDK 线，原始请求）：
  - 投影表的每一格（缺省、显式字段、链读了没点名的字段、动态字段、临时列及其同名碰撞、主键、分组、`time_fields`），断言没点名的链输入不出现在响应里；含 `*` / `$meta` 的格子单列，3.x 一侧断言默认值与残缺的 `$meta`，本实现一侧断言与不带链相同。
  - 度量：L2、IP、COSINE、BM25（变号，含只有 `limit` 的链）、稀疏 IP；稀疏字段作输入的拒绝。
  - 与检索参数的组合：过滤、分区、`offset`、分组（含 `group_size`）、范围检索、search 级 `round_decimal`（含整数分数）、`timezone` / `time_fields`、多查询、空结果。
  - 错误先后：collection 不存在、`anns_field`、度量、未加载、过滤、输出字段，各自与非法链、合法链组合。
  - `order_by_fields` 的各取值，带链与不带链。
  - 分数：超出 float32、整数分数、`None` 分数（含同时带 search 级 `round_decimal`）。
  - `rerank_model`：每条校验文案与 3.x 对照；合法请求单列为差异（3.x code 1，本实现 `UNIMPLEMENTED`），含未加载的 collection。
  - HybridSearch 带 `function_chains` 的拒绝；`function_score` 冲突。
  - 带 iterator 参数的 `SearchRequest` 加链：用原始请求，search_params 带 pymilvus `search_iterator` 发出的那组键（两个 iterator 版本各一条），与 3.x 对照。SDK 的 `search_iterator` 没有 `function_chains` 这个命名参数，是否经 `**kwargs` 透传在第 2 步核实后决定要不要另加 SDK 用例。第 2 步放开执行时先加原始请求这一格。
  - MemTable 与 flush 后各一遍（链的取值经过两种数据源）。
- **参照用例**：上节的 28 个函数，加一条 SDK 冒烟。
- **已有用例的更新**（都属于“测试断言的是切片前的范围”，提交说明写明）：断言 Search 的 `function_chains` 返回 `UNIMPLEMENTED` 的用例；0026 文档与 README 中“公共 `function_chains` 不纳入”的表述。

## 第 1 步验收

本机（macOS arm64），参照 `1320d26`，pymilvus 3.0.2 / 2.6.18。

改动：[public.rs](../../crates/core/src/rerank/public.rs)（链表示与按 schema 的校验）、[function_chain.rs](../../crates/grpc/src/function_chain.rs)（proto 解码、冲突判定与入口）；[search.rs](../../crates/grpc/src/search.rs) 摘出链的请求部分并解析 `order_by_fields`，[service.rs](../../crates/grpc/src/service.rs) 在度量解析之后、读快照之前调用。原先在未加载检查之后才报的 `UNIMPLEMENTED` 前移到这里。

| 项目 | 结果 |
| --- | --- |
| `cargo fmt --check`、clippy（默认与 `fault-injection`） | 通过 |
| `cargo test --workspace` | 681 通过（新增 11：校验 8、解码 2、JSON 真值 1） |
| tests/python（3.0.2 / 2.6.18） | 各 665 通过，无跳过（新增 [test_function_chains.py](../../tests/python/test_function_chains.py) 4 条） |
| 差分中涉及 Search 入口与 rerank 的三个文件（两条线） | 各 151 通过 |
| 调查探针改为打本实现后与 3.x 的输出逐条比对（本地留存） | 3.x 的 118 条错误，码与文案全部相同；3.x 成功的 158 条带链请求在本实现为 `UNIMPLEMENTED`；其余 13 条不同都是本步的中间状态（见下） |

中间状态（第 2 步放开）：通过全部校验的链返回 `UNIMPLEMENTED`，因此 3.x 在运行期才报的错误（`num_combine produced a non-finite result` 等 8 条）、“未加载 + 合法链”的 101、“过滤语法错误 + 合法链”的 6 在本步都是 `UNIMPLEMENTED`。合法的 `rerank_model`（3 条）是决策 B 的登记差异，不随第 2 步改变。

计数：`tests/python` 661 → 665，已同步 `ci.yml` 两处、`scorecard.py`、`test_candidate.py`、打包验收 README 与 AGENTS 命令表。完整差分与互通本步没有重跑：改动只在 Search 入口对带链请求的处理，不带链的请求路径只多了一次空判断。

评审后的修改（评审通过，建议项）：
- 补 `order_by_fields` 的 `NaN` 与 `1e999` 两个取值。后者查出一个缺陷：[schema.rs](../../crates/grpc/src/schema.rs) 的 `truthy`（Python 对 `json.loads` 结果的真值判断）把超出 f64 范围的数字当成假，3.x 里它是 `inf`、为真。已在这个共享函数里修正并加单测；它的其他调用点（`strict_group_size`、`function_score` 的 `provider`、建表属性）同样按 Python 的真值取值，一并更正。
- 补 `function_score`、`order_by_fields` 与两条链同时出现的先后断言。
- 改后重跑：fmt、clippy、gRPC 单测 39、tests/python 在 3.0.2 上 665、在 2.6.18 上 [test_function_chains.py](../../tests/python/test_function_chains.py) 与 [test_grpc_translation.py](../../tests/python/test_grpc_translation.py) 共 118。

实现中确定的一处细节：proto 的 map 没有顺序，一个参数表里有多个未设置的值时 3.x 报哪一个取决于它的迭代顺序；本实现按键排序后报第一个。只有一个非法值时两边相同。

## 第 2 步验收

本机（macOS arm64），参照 `1320d26`，pymilvus 3.0.2 / 2.6.18。

改动：
- [public.rs](../../crates/core/src/rerank/public.rs)：增加链的编译（`Plan`）与执行。三个表达式、三个算子都有；一个算子对全部查询做完才进入下一个，所以运行期错误的先后与 3.x 相同。`rerank_model` 不编译（决策 B，入口处已先返回）。`num_combine` 的 `sum` 与 `avg` 复用 0026 的 Python `sum` 与整数真除法，`fsum` 照 CPython 的 `math.fsum` 实现，decay 复用 0026 的曲线。整数 `scale` 的平方按精确乘积再转浮点，与 3.x 用参数原值相乘一致。
- [collection.rs](../../crates/core/src/collection.rs) 的 `search_chained_in` 见“设计”第 3 条。
- [search.rs](../../crates/grpc/src/search.rs)：带链的请求走这条路径；`$score` 为 `None` 时按第 5 节的先后报错。`py_round` 移到 `rerank::pyval`，供链与响应编码共用。
- [service.rs](../../crates/grpc/src/service.rs)：校验通过的链交给检索，不再返回 `UNIMPLEMENTED`。

| 项目 | 结果 |
| --- | --- |
| `cargo fmt --check`、clippy（默认与 `fault-injection`） | 通过 |
| `cargo test --workspace` | 689 通过（新增 8：`num_combine` 六种模式、`fsum`、decay 三种函数与 TIMESTAMPTZ、`round_decimal`、`sort` 的各种列与缺失值、链的执行顺序、运行期错误的先后、`rerank_model` 不编译；期望值取自在参照环境直接调用 3.x 表达式与算子的结果） |
| tests/python（3.0.2） | 671 通过，无跳过（评审后补一条差异用例，为 672） |
| tests/python 中链相关的两个文件（2.6.18） | [test_function_chains.py](../../tests/python/test_function_chains.py) 与 [test_grpc_translation.py](../../tests/python/test_grpc_translation.py) 共 124 通过 |
| 差分中涉及 Search 入口与 rerank 的三个文件（3.0.2） | 151 通过 |
| [test_function_chains.py](../../tests/python/test_function_chains.py) 直接打真实 3.x | 11 条中 8 条通过；失败的三条恰是登记差异的用例（`*` / `$meta` 投影、合法 `rerank_model`、按名为 `$score` / `$id` 的字段分组）。期望值因此是 3.x 的真值，不是手算的 |
| 调查探针打本实现与 3.x 逐条比对（本地留存，新增 `probe5` / `probe6` 覆盖 `None` 分数、整数与超范围分数、各模式数值、排序列种类、iterator 参数、检索参数组合、名为 `$id` / `$score` 的字段与动态键） | 387 条中 371 条相同；16 条不同都属于两项登记差异（`*` / `$meta` 投影 13 条、合法 `rerank_model` 3 条）；差异 G 是评审补测时发现的，不在这 387 条内，由 `probe8` 与第 3 步的差分覆盖 |

[test_function_chains.py](../../tests/python/test_function_chains.py) 由 4 条变为 11 条。第 1 步断言合法链返回 `UNIMPLEMENTED` 的用例换成执行结果的用例，只保留合法 `rerank_model` 的断言（决策 B）。新增的用例覆盖：投影表各格（MemTable 与 flush 后各一遍）、`*` / `$meta` 的差异、名为 `$id` / `$score` 的字段与动态键、三种 dense 度量与 BM25（含只有 `limit` 的链变号）、稀疏 IP 与稀疏字段作输入的拒绝、各表达式与可空输入、排序的各种列、`offset`、过滤、空结果、范围检索、多查询、分组（含 `group_size`）、search 级 `round_decimal`（含整数分数）、`time_fields`、两个 iterator 版本的参数、运行期错误与 `None` 分数的先后，以及决策 G 的分组。计数 `tests/python` 665 → 672，已同步 `ci.yml` 两处、`scorecard.py`、`test_candidate.py`、打包验收 README 与 AGENTS 命令表；发行与打包脚本自检 687 通过。

SDK 的 `search_iterator`：签名里没有 `function_chains`，经 `**kwargs` 透传给每一页的检索。实测 3.x 与本实现都只返回第一页的重排结果，之后迭代结束，两边相同。迭代器带链的组合由原始请求那一格覆盖，不另加 SDK 用例（2.6 线也没有对应的 SDK 写法）。

评审后的修改（评审通过，0 阻断，建议项）：
- 补“按名为 `$score` / `$id` 的字段分组”一格，补测时查出 3.x 的缺陷，负责人确认不照抄，登记为决策 G，用例单列。
- 补“`$score` 为 `None` + 非法 `time_fields` 分量”一格。3.x 先对全部命中做 `float()`，再编码字段（`time_fields` 的分量在这里校验），所以先报 `float()` 的错；带 search 级 `round_decimal` 时仍是 `round()` 最先。3.x 实跑与本实现相同，`time_fields` 本身的错误在没有 `None` 分数时照常报出。
- 改后重跑：本文件在本实现上 11 条通过、在 3.x 上 8 条通过（失败的是三条差异用例）；在 2.6.18 上本文件通过；发行与打包脚本自检通过。
- `search_iterator` 带 `function_chains` 的 SDK 写法在第 3 步的 SDK 冒烟里补一条可重跑的断言。

没有重跑：2.6 线 tests/python 全量、完整差分与互通。改动只在带链请求的路径上；不带链的请求只多了一次空判断，以及 `py_round` 换了位置（实现不变，单测随之移动）。

## 第 3 步验收

本机（macOS arm64），参照 `1320d26`，pymilvus 3.0.2 / 2.6.18。本步没有改产品行为：产品源码里只在 `#[cfg(test)]` 中增加两个读取真值的测试。

改动：
- **组件真值**：[generate_function_chain_fixtures.py](../../tests/differential/generate_function_chain_fixtures.py) 在参照环境直接调用 3.x 的 `function_chain_to_repr`、`validate_function_chain`、`compile_function_chain` 与链的执行，写两个文件：
  - [function_chain_validation.json](../../crates/grpc/tests/fixtures/function_chain_validation.json)，280 条：第 3 节每一类解码与校验错误（含后面算子的解码错误先于前面算子的校验错误、各种阶段值、四种 schema 包括主键名为 `$score` / `$id` 与 VARCHAR 主键），以及通过校验的链读到的字段与是否用了 `rerank_model`。[function_chain.rs](../../crates/grpc/src/function_chain.rs) 的单测把每条链的线格式字节解码后校验。
  - [function_chain_execution.json](../../crates/core/tests/fixtures/function_chain_execution.json)，688 条：`num_combine` 六种模式在随机输入与边界值上的结果；从 int64 一路累加到 float 上限附近的整数，以及越过上限时的报错；`fsum` 的抵消；decay 三种函数配八组参数（含整数 `scale` 的精确平方、下溢为 0 的 `sigma²`）与七种输入列（含 TIMESTAMPTZ 与主键）；`round_decimal` 0–6 位；`sort` 在各种列种类、空值、NaN、平局、平局列与字符串主键上的顺序；`limit`；运行期错误的先后；120 条固定种子的随机链。[public.rs](../../crates/core/src/rerank/public.rs) 的单测执行同样的链与输入行，逐行比较 `$id`、`$score` 与临时列的 Python `repr`。
  - 编码的核对在生成时完成、不写进文件：953 条链另用 SDK `to_proto()` 构造，与 chainwire 的字节解析结果相同；SDK 构造不出的（未设置的值、空名字、空键）逐字段还原核对；复制用例与 SDK 冒烟里的 24 种 SDK 写法同时交给 chainwire 的同名写法与 `pymilvus.function_chain`，结果相同。
- **端到端差分**：[test_function_chains.py](../../tests/differential/test_function_chains.py) 4 条，共 654 次比较（481 次有结果行，其余是 code 1 / 6 / 100 / 101 的错误）。覆盖“测试矩阵”的各格：投影表 × 6 种链（MemTable 与 flush 后各一遍，同时断言与不带链时字段相同）、16 个表达式、12 种排序、17 组检索参数（过滤、分区、`offset`、范围检索、多查询、分组含 `group_size` / `strict_group_size`、search 级 `round_decimal`、`timezone` / `time_fields`、两个 iterator 版本）；三种 dense 度量、BM25 与稀疏 IP（含只有 `limit` 的链、`round_decimal`、稀疏字段作输入的拒绝）；26 条非法链在已加载与未加载的 collection 上；冲突与 `order_by_fields` 的 15 个取值；14 种错误情形各配非法链、合法链、运行期出错的链与 `None` 分数的链；运行期错误与 `None` 分数的各种组合；HybridSearch 的拒绝。三项登记差异单列一条，两边各自断言（A 断言 3.x 的默认值与 `valid_data`、B 要求环境里没有 `COHERE_API_KEY` 以免 3.x 调用外部服务、G 断言 3.x 的覆盖与报错文案）。
- **参照用例**：按“参照用例”一节改后复制 adapter 19 个函数（21 条）与 compatibility 9 条，[INVENTORY.md](../../tests/python/adapter/INVENTORY.md) 逐条标注。[chainwire.py](../../tests/python/chainwire.py) 增加与 SDK 同名的写法、`sending` 与 `attach` 的字段号参数。
- **SDK 冒烟**：[test_function_chains.py](../../tests/python/test_function_chains.py) 的 `test_sdk_and_wire_chains_give_the_same_results`。3.0 线用 SDK 的 `search(function_chains=...)` 与 `search_iterator(..., function_chains=...)` 各发一次，再用手写编码各发一次；2.6 线只发手写编码；期望相同。实测 3.x 与本实现、两条 SDK 线四种组合的结果都一样：带链检索按链重排；带链的 `search_iterator` 只返回第一页的重排结果就结束（手写编码在 2.6 线上同样如此）。
- **文档**：README 增加“公共 function chain（当前源码新增，`0.5.0rc1` 不含）”一条，HybridSearch 一条里 Search 的 `function_chains` 改为“`0.5.0rc1` 中为 UNIMPLEMENTED，当前源码已支持”；[0026](0026-rerank-and-hybrid-search.md) 的“不纳入”两处注明之后由本切片实现；AGENTS 的索引、端到端测试与组件真值命令。差异 A 的描述补上 3.x 把 `valid_data` 全标为假（差分实测）。

| 项目 | 结果 |
| --- | --- |
| `cargo fmt --check`、clippy（默认与 `fault-injection`） | 通过 |
| `cargo test --workspace` | 691 通过（新增 2：校验真值、执行真值） |
| 组件真值 `--verify` | 通过；把真值文件改一个字后两个单测与 `--verify` 都报错 |
| tests/python（3.0.2） | 703 通过，无跳过 |
| tests/python 中本步新增与改动的三个文件（2.6.18） | 43 通过 |
| 改后复制的 31 条与 [test_function_chains.py](../../tests/python/test_function_chains.py) 直接打真实 3.x | 31 条全部通过；test_function_chains.py 12 条中 9 条通过，失败的仍是三条差异用例 |
| 差分新文件（3.0.2 / 2.6.18） | 各 4 通过 |
| 完整差分（3.0.2，4 片） | 860 通过，无跳过 |
| 发行、打包、平台、CI 与分片脚本自检 | 886 通过 |
| 分片计划核对 | tests/python 703 / 0，差分 860 / 404 |

计数：`tests/python` 672 → 703（改后复制 30、SDK 冒烟 1），已同步 [ci.yml](../../.github/workflows/ci.yml) 两处、[scorecard.py](../../tests/release/scorecard.py)、[test_candidate.py](../../tests/release/test_candidate.py)、[打包验收 README](../../tests/packaging/README.md) 与 AGENTS 命令表；差分 856 → 860，会话组 404 不变（新文件用自己的 `pair`），已同步 [compare.yml](../../.github/workflows/compare.yml) 两处、`scorecard.py`、`test_candidate.py` 与[分片 README](../../tests/sharding/README.md)。

README 全文不能出现 `$` 与花括号（[发行物检查](../../tests/packaging/check_artifacts.py)防止拼接安装命令），新的一条因此用“分数”“主键”“整个动态字段”代替 `$score`、`$id`、`$meta`。

合入前 CI 的修正：执行真值在两个 Linux 的 rust job 上有一处 decay（gauss）的值差 1 ULP（`0.9999999885767878` 对真值的 `0.999999988576788`），macOS 上逐位相同。原因是 decay 的曲线经过 `exp` / `ln`，3.x 的 `math.exp` 与本实现的 `f64::exp` 都调用平台 libm，真值在 macOS 上生成，glibc 在少数输入上最后一位不同；这是平台数学库的差异，3.x 换平台同样会变，端到端差分的 1e-5 容差本来就容纳它。修正只在测试：链里含 decay 的用例，浮点值允许 4 个 ULP，id、整数、`None` 与错误文案仍逐位比较，其余用例不变。变异核对：把该值换成 Linux 的结果通过，decay 用例里改出明显不同的值失败，非 decay 用例只改 1 ULP 也失败。

没有重跑：2.6 线 tests/python 全量与完整差分、存储互通。产品行为自第 2 步提交后没有变化，这几项沿用此前的证据；本步改的是测试、真值文件、计数与文档，2.6 线跑了全部新增与改动的用例。

## 证据

动手前的调查留存在本地（探针脚本、原始输出、调查记录与 2.6 线验证），属于[证据分类](../testing/README.md)里的本地留存；合同中引用的 3.x 行为已由仓库内可重跑的组件真值（`--verify`）与差分用例覆盖（第 3 步验收）。
