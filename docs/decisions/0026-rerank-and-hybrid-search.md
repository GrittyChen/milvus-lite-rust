# 0026：HybridSearch 与 rerank（RRF / Weighted / boost / decay）

状态：设计评审通过；第 1 步（Search 的 boost 与 `function_score` 解码）与第 2 步（Search 的 L2：rrf / weighted / decay、`GroupByOp`、rerank 后的 `round_decimal`）已实施并评审通过；第 3 步（HybridSearch：`rank_params`、顶层与子请求的 `function_score`、分区、分组、时区与 `time_fields`）已实施并评审通过；第 4 步（差分、参照用例、文档与验收）已实施并评审通过；四步均已合入 main（`da8689f`），开发与 CI 验收见[验收记录](../testing/0026-rerank-hybrid-acceptance.md)。对照版本为 3.x v3.2.1（`43d1257`）。

13 个 `tie *` 的三平台验收已在合入前的 CI 完成（Compare 的 diff-3.0.2 在 Linux x86_64、Linux arm64、macOS 各一次，见[验收记录](../testing/0026-rerank-hybrid-acceptance.md#ci-验收合入前)）。变异验证已在第 4 步完成；本切片不声称性能提升，不做性能基准。

## 背景与目标

缺口统计（0025 收口后，参照用例 681 / 760）里，rerank 与 HybridSearch 仍被 `UNIMPLEMENTED` 拒绝。这一类是 pymilvus 用户最常用的检索形态之一（多路召回融合、按字段衰减、按条件加权），而且是 0024（分组）与 0025（时区）明确留下的遗留项：Hybrid 与 rerank 内的分组、Hybrid 路径的 `time_fields`。

目标：让 `hybrid_search`、`search(ranker=...)` 与 `hybrid_search(ranker=Function)` 在本实现上与 3.x 行为一致，范围见下。

## 范围（负责人已冻结）

### 三个协议表面

当前实现把 `function_score` 和 `function_chains` 合并成一个 `UNIMPLEMENTED`，这是边界模糊的根源。三个表面在 3.x 里是三条不同的路径：

| 协议表面 | 3.x 的实际路径 | 本切片 |
| --- | --- | --- |
| `HybridSearchRequest.rank_params`（pymilvus 的 `RRFRanker` / `WeightedRanker`） | `parse_rank_params` → `build_hybrid_rerank_chain` → `MergeOp` | **纳入** |
| `SearchRequest.function_score` / `HybridSearchRequest.function_score`（pymilvus 的 `Function` / `FunctionScore`） | `decode_hybrid_function_score`：boost 为 L0，另允许最多一个 L2（rrf / weighted / decay / model） | **纳入 boost + rrf / weighted / decay；model 除外** |
| `SearchRequest.function_chains`（公共 L2 算子语言） | `prepare_search_function_chain` | **不纳入**，保持 `UNIMPLEMENTED` |
| `HybridSearchRequest.function_chains` | 3.x 自己报 `SchemaValidationError("function_chains is not supported for hybrid search yet")` | **按 3.x 拒绝**（code 6，同文案），不是 `UNIMPLEMENTED` |

两个容易搞错的点：

- Search 的 `function_score` 不只有 boost。Search 与 Hybrid 共用 `decode_hybrid_function_score`，所以 Search 也支持一个 L2 `rrf` / `weighted` / `decay`。`decode_boost_function_score` 是 servicer 未使用的旧路径，不能当行为真值。
- `adapter/grpc/reranker.py` 里的 Weighted（min-max 归一化）是死代码。servicer 实际走 `function/builder.py` → `function/ops/merge_op.py`。

### 不纳入

- 公共 `function_chains`（9 条参照用例），另做 L2 算子切片。
- model rerank（外部 provider）：Search 与 HybridSearch 顶层的合法 model 通过全部校验后返回带明确说明的 `UNIMPLEMENTED`，登记差异；HybridSearch 子请求里的 model 与其他子请求 L2 一样只解码、被忽略，与 3.x 一样成功（第 10 节）。
- `random_score`：候选行真正用到时（该函数的过滤命中）同样返回 `UNIMPLEMENTED` 并登记（3.x 用 sha256 派生随机数，纳入要新增 sha2 依赖）；没有候选行时与 3.x 一样成功（第 10 节）。
- 多向量字段（schema 仍拒绝，另立切片）、BM25 / sparse / text analyzer（全文检索切片）。因此建表用了 BM25 的 Hybrid 用例仍会被 text analyzer 挡住，**不能用“Hybrid 全绿”作验收口径**。

### 目标用例计数

阻断参照用例共 28 条：

| 分类 | 数量 | 本切片 |
| --- | --- | --- |
| Search boost（6 个正常 + 3 个参数化非法） | 9 | 是 |
| Search L2 decay | 1 | 是 |
| Search 动态字段 `["*"]` + rerank | 1 | 是 |
| HybridSearch | 7 | 是 |
| 公共 `function_chains` | 9 | 否 |
| 多向量字段 | 1 | 否 |

本切片的首个阻断目标是 **18 条**，逐条 ID（取自 0025 收口后固定统计的 junit 结果，验收按 ID 记账）：

| 来源 | 用例 |
| --- | --- |
| `test_boost_ranker`（9） | `test_search_with_boost_ranker_filter`、`..._preserves_vector_output`、`..._filter_field_not_output`、`..._function_score_modes`、`test_search_boost_ranker_with_search_filter_and_limit`、`test_search_boost_ranker_score_affected`、`test_search_boost_ranker_invalid_params[ranker0-weight / ranker1-weight / ranker2-@]` |
| `test_dynamic_field_coverage`（1） | `test_dynamic_star_survives_grpc_ranker` |
| `test_hybrid_search`（Search decay 1） | `test_search_top_level_decay_uses_hidden_input_field` |
| Hybrid（7） | `test_hybrid_search::test_hybrid_top_level_decay_uses_hidden_input_field`、`::test_hybrid_timestamptz_timezone_and_time_fields`；`test_boost_ranker::test_hybrid_top_level_boost_ranker_filter_field_not_output`；`test_dynamic_field_coverage::test_dynamic_star_survives_hybrid_search_group_by`；compatibility 的 `TestHybridSearch::test_hybrid_search_basic`、`TestHybridSearchWeighted::test_weighted_ranker`、`TestHybridSearchWithFilter::test_hybrid_search_with_scalar_filter` |

解除后可能暴露二级阻断（HNSW 定义、动态字段、`time_fields` 等），验收以实际跑出的首个阻断为准，不预先承诺全绿。7 条 Hybrid 用例都只用一个 dense 字段：4 条单路（boost、timestamptz、decay、filter），3 条双路（dynamic group_by、weighted、basic）。

## 3.x 真值

以下均由 3.x（`43d1257`）实测与读码得出。探针 `probe_rr.py` 共 2822 个场景（Hybrid RRF / Weighted / 分组 / function_score、Search boost / L2 rerank、候选窗口、`limit <= 0` 分路径、组键语义、`rank_params` 与 decay 的参数类型、TIMESTAMPTZ 与 timezone、错误先后、非动态集合、分区与路由级参数、拒绝时机），原始输出与校验和留存于仓库外；转为差分用例后可重跑。其中范围内、转为通过用例的场景与登记差异、不纳入的场景见“测试矩阵”的分类。

### 1. SDK 可达面与原始请求

pymilvus 3.0.2 的可达面比协议窄，影响差分用例怎么写：

- `hybrid_search(ranker=)` 只收 `BaseRanker`（`RRFRanker` / `WeightedRanker`，写入 `rank_params`）或单个 `Function`（写入 `function_score`，不带 `function_score.params`）。**`FunctionScore`（多个函数、`boost_mode` / `function_mode`）只能走原始 gRPC**。
- `search(ranker=)` 收 `Function` 或 `FunctionScore`，可带 `boost_mode` / `function_mode`。
- Hybrid 的 `timezone` / `time_fields`：SDK 的 kwargs 进入每个子请求的 `search_params`，**`rank_params` 里没有**；`AnnSearchRequest.param` 里写的 `timezone` 会嵌进 `params`，不生效。路由级、`rank_params` 级的位置只能用原始请求探测。
- `round_decimal`：Search 的 SDK 参数写入 `search_params` 的顶层键 `round_decimal`，**服务端**对响应 `scores` 取整（探针：`round_decimal=2` 时原始 `scores` 为 `1.77`）；写在用户 `search_params` 字典里的同名键被嵌进 `params`，不生效，且 SDK 仍发 `round_decimal=-1`。Hybrid 的 `round_decimal` 在 `rank_params` 里，服务端忽略，**pymilvus 在客户端**对返回的 `distance` 取整；比较响应一律用原始 `scores`。

### 2. 解码与分流

`function_score` 的解码（`decode_hybrid_function_score`）：

- 逐个函数读 `params`：`reranker` 取小写。`boost` 要求 `input_field_names` 为空（否则 code 6 `Boost Ranker function '<name>' requires empty input_field_names`）。
- boost 的参数校验（code 6）：`params.reranker` 必须是 `boost`、`weight` 必填且可转 float、`filter` 必须是字符串（空串视为没有）、`random_score` 必须是对象。`weight` 接受数字字符串（`"2.5"` 可用，`"abc"` 报错，`"nan"` / `"inf"` 被 `float()` 接受）。
- 非 boost：最多一个，第二个报 code 6 `Hybrid FunctionScore supports at most one non-boost rerank function`。`reranker` 不在 `rrf / weighted / decay / model` 且没有 `provider` 时报 code 6 `Unsupported Hybrid FunctionScore reranker '<值>'`（空串也是这条，因此只写 `strategy` 参数不行）。`reranker` 大小写不敏感（`RRF` 可用），函数 `type` 被忽略（BM25 类型照样当 rerank 用）。
- `function_score.params` 的 `boost_mode` / `function_mode`：小写后必须是 `multiply` 或 `sum`，否则 code 6 `FunctionScore params.boost_mode must be 'Multiply' or 'Sum'`（`function_mode` 同）。
- L2 参数规整：`rrf` 的 `k` 转 float（`"abc"` → code 1 `could not convert string to float: 'abc'`）；`weighted` 的 `weights` 可为 JSON 数组字符串，非数组 → code 6 `Weighted reranker params.weights must be an array`，`norm_score` 缺省 False，字符串 `true` / `false`（大小写不敏感）与布尔以外 → code 6 `Weighted reranker params.norm_score must be bool`；`decay` 的参数**不规整**，原样保留（见下 decay 的 `norm_score`）。
- 参数值解码：JSON 能解析按 JSON，否则保留字符串（`"False"` 是字符串，`"false"` 与 `"false "` 是布尔）。
- 解码阶段内部的先后：先校验 `function_score.params` 的两个 mode，再按函数顺序逐个解码（探针 `fs mode bad + bad fn` 报 mode 错误）。**解码阶段也可能报 code 1**：`k` 与 `weights` 元素的 `float()` 转换失败（`could not convert string to float: 'abc'`），它们先于路由错误（探针 `fs k abc + bad anns h`、`fs weight elem bad`）。

分流：

- Search：boost 作为 L0（引擎内），L2 在引擎外。`function_score` 与 `function_chains` 同时出现 → code 6 `function_score and function_chains cannot be used together`。
- Hybrid：`function_chains` 非空 → code 6，在解码一切之前。boost 并入每个子路由（合并规则见第 8 节）。

### 3. Hybrid 的 `rank_params`

解析（`parse_rank_params`）：`strategy` 缺省 `rrf`，`params` 缺省 `{}`，`limit` 缺省 10（非整数 → code 1 Python `int()` 文案），`offset` 缺省 0。`round_decimal`、`strict_group_size`、`rank_group_scorer` **被忽略**（探针：三者都不影响结果）。`group_by_field` 非空字符串才生效；`group_size` 经 `int()`。

- `strategy` 只认小写 `rrf` / `weighted`，其余（含 `RRF`、`max`）→ code 1 `Unsupported hybrid rerank strategy: '<值>'`。该检查发生在各路检索之后（先于它的是路由内的错误，如 anns_field、过滤表达式、未加载）。
- `limit` ≤ 0：`` `limit` value <n> is illegal `` 是 **pymilvus 客户端**的校验，服务端不报错，服务端语义按路径不同，见“`limit <= 0` 与负 `offset`”。`limit` 缺省时为 10；`limit` / `offset` / `group_size` 走 Python `int()`：`"3"`、`" 3 "`、`2.5`（→ 2）、`true`（→ 1）、`1e2`（→ 100）可用，`null` 与 `[]` 报 code 1 `int() argument must be a string, a bytes-like object or a real number, not '<类型>'`，`"3.5"`、`abc` 报 code 1 `invalid literal for int() with base 10: '<值>'`。`group_by_field` 不是非空字符串时视为没有分组；`group_size` 为 `null` 视为缺省。
- `strategy` 的非字符串值也进同一条 code 1 文案，值按 Python repr（`5`、`None`、`True`、`[]`）；`params` 不是对象时 code 1 `'<类型>' object has no attribute 'get'`（`str`、`int`、`list`、`NoneType`）。
- **`rank_params` 路径基本没有参数校验**，与 `function_score` 路径不同：
  - RRF：`k` 直接参与 `1/(k + rank + 1)`，不校验范围。`k=0` 可用；负数只有在某一名次上分母恰为 0 才报 code 1 `float division by zero`（`k=-1` 第一名即触发，`k=-2` 第二名触发，`k=-5` 在只有 2 个候选时不触发），`k=-1.5` 正常计算出负分；布尔按 0 / 1。类型错误（在第一个候选上才报，所有路由都没有候选时成功，探针 `empty rp rrf k=str`）：字符串 `can only concatenate str (not "int") to str`，列表 `can only concatenate list (not "int") to list`，`null` `unsupported operand type(s) for +: 'NoneType' and 'int'`，对象 `unsupported operand type(s) for +: 'dict' and 'int'`。
  - Weighted：`weights` 先取 Python `len()`：数字与 `null` 报 code 1 `object of type '<类型>' has no len()`；字符串和对象按字符数 / 键数计（`"[0.5,0.5]"` 报 `weighted merge requires 2 weights, got 9`）；个数不等于路由数报 `weighted merge requires <路由数> weights, got <实际数>`。元素不校验范围（负数、大于 1 都可用，布尔按 0 / 1），类型错误在第一次相乘时才报（没有候选时成功，而 `len()` 的错误没有候选也报，探针 `empty rp wtd *`）：字符串 / 列表 `can't multiply sequence by non-int of type 'float'`，`null` `unsupported operand type(s) for *: 'NoneType' and 'float'`。`norm_score` 取 **Python 真值**：缺省、`false`、`0`、`0.0`、`""`、`[]`、`{}`、`null` 为否，其余（包括 `"false"` 字符串、`[0]`）为是。pymilvus 的 `WeightedRanker` 默认 `norm_score=True`。
  - 路由数不一致：各路 `nq` 不同 → code 1 `MergeOp requires all inputs to have the same number of chunks: input 0 has <a>, input 1 has <b>`；`requests` 为空 → code 1 `MergeOp requires at least one input`。

### 4. 合并与打分

各路得分取引擎检索输出的**公共分数**（`Collection.search` 末尾已把内部距离转成公共分数；BM25 再取负），Hybrid 合并与 Search L2 的 rrf / weighted / decay 全部在公共分数上计算（`hit_score_for_chain`）。合并按 `$id` 去重，**entity 取第一次出现的那一路**，合并结果按“首次出现顺序”排列。之后两条路径的排序规则不同：

- **无分组（`SortOp`）**：按得分排序（方向见 Weighted 表，其余降序），NaN 排在最后，**同分按 `$id` 升序**（整数按数值，VARCHAR 按 Unicode 码点，探针 `tie h wtd`：`Z, a, k1, k10, k2, k9, é, 中`），不是首次出现顺序。
- **有分组（`GroupByOp`，第 7 节）**：没有 `$id` 平局规则，组内与组间都是 Python 稳定排序，同分保持首次出现顺序（探针 `tie h group order`）。

**RRF**：`score = Σ 1/(k + rank + 1)`，`rank` 为在各路候选列表里的 0 起位置。探针 `h rrf default cosine`：两路第一名 63 与 30 同分，结果是 `30, 63`（`$id` 升序，尽管 63 先出现）；同样的数据加上分组后，63 所在的组排前（首次出现）。

**Weighted**（`MergeOp`，**不是** min-max）：

| `norm_score` | 度量 | 每路得分 | 排序 |
| --- | --- | --- | --- |
| 真 | COSINE | `(1 + s) / 2` | 降序 |
| 真 | IP / 其他正相关 | `0.5 + atan(s) / π` | 降序 |
| 真 | BM25 | `2 · atan(s) / π` | 降序 |
| 真 | L2 及其他 | `1 − 2 · atan(d) / π` | 降序 |
| 假，全是正相关度量（IP、COSINE、BM25、…） | — | 原始分数 | 降序 |
| 假，全是 L2 类 | — | 原始距离 | **升序** |
| 假，混合度量 | 正相关不变，L2 类 `1 − 2 · atan(d) / π` | 转换后 | 降序 |

最终得分 `Σ weights[r] · 每路得分`，缺席的路不计入。路由的度量来自该路 `param.metric_type`（缺省取其向量字段索引的度量），所以同一 collection 的两路可以用不同度量；已探针混合 COSINE+L2、IP+L2、COSINE+IP。单路时 Weighted 也走同一公式。

**decay**（`function_score` 的 L2）：

- 先用 `score_mode`（`max` / `sum` / `avg`，缺省 `max`，其他值 code 1 `unsupported score_mode: only supports [max, sum, avg]`）合并各路，每路得分先按 `norm_score` 归一化，不归一化时按 `force_descending` 的方向转换（L2 类 `1 − 2·atan(d)/π`，正相关不变）。因此**单路 Search 的 L2 度量 + decay 也会把距离转成上表的方向分数**。
- 衰减因子（`DecayReranker.compute_factor`）：`d = max(0, |v − origin| − offset)`，`d == 0` 为 1；`gauss`：`exp(d² · ln(decay) / scale²)`；`exp`：`exp(ln(decay) · d / scale)`；`linear`：`max(0, 1 − (1 − decay)/scale · d)`。字段值为空 → 因子 0；最终得分 = 合并得分 × 因子；相乘前先检查这一行的分数与因子都是有限数（合并得分为 `None` 时直接得 0），所以因子为 NaN 时报的是下面的 `num_combine row values must be finite numeric non-bool values`。
- **`norm_score` 的 Python 真值陷阱**：decay 的 `norm_score` 不经规整，pymilvus 把布尔写成字符串 `"True"` / `"False"`，`"False"` 是非空字符串，因此真值为真。探针：`norm_score=False` 与 `True` 结果相同。
- 参数校验（code 1，顺序即报错顺序）：输入字段数必须为 1（`decay reranker requires exactly 1 input field`）、`score_mode`、输入字段存在（`input field <f> not found in collection schema`，动态字段同样不算）、字段类型必须是 INT8 / 16 / 32 / 64、FLOAT、DOUBLE（`decay input field <f> must be numeric, got <类型>`，BOOL、VARCHAR、JSON、向量、TIMESTAMPTZ 均拒绝）；然后是 `function` / `origin` / `scale` 缺失（code 1，消息是 `'<key>'` 的 `KeyError`）、`function` 必须是 `gauss` / `exp` / `linear`（区分大小写）、`scale > 0`、`offset >= 0`、`0 < decay < 1`（文案见 `rerank/decay.py`，类型不对时是 Python 的 `TypeError` / `ValueError` 文案）。
- 参数的 JSON 类型（探针 `decay param *`，文案逐字复现，code 均为 1）：`origin` 经 `float()`（`"5"`、`true` 可用；`null` / `[1]` 报 `float() argument must be a string or a real number, not '<类型>'`；`"x"` 报 `could not convert string to float: 'x'`）；`scale`、`offset`、`decay` 先比较后转换（字符串、`null`、列表报 `'<=' / '<' not supported between instances of ...`；`true` 按 1，所以 `decay=true` 报 `decay must be 0 < decay < 1, got True`）；`scale=1e400` 是无穷大，因子恒为 1；`function` 非字符串报 `decay: invalid function <repr>, ...`。
- `score_mode` 校验时取小写、合并时用原值：`"MAX"` / `"Sum"` 通过校验，在合并第一个候选时报 code 1 `Unknown merge strategy: MAX`（没有候选时成功）；`5`、`null` 报 `unsupported score_mode`。
- 输入字段是隐藏字段：用户 `output_fields` 里没有也要取，响应前剔除。整数主键能通过类型校验，但候选行里**没有主键列**，值恒为空、因子恒为 0，得分全部为 0，结果按 `$id` 升序（探针 `s l2 decay field id`、`pk rr_pki s l2 decay on pk`）；VARCHAR 主键报类型错误。
- 数值边界（探针 `num *`）：字段值 NaN 时 `d = max(0, NaN) = 0`，因子为 1；`scale` 极小使 `scale²` 为 0 时，gauss 对 `d != 0` 的行报 code 1 `float division by zero`（`exp` / `linear` 不报）。合并得分与因子相乘前逐行检查：合并得分为 `None` 时这一行直接得 0；否则分数与因子都必须是有限数，任一非有限报 code 1 `num_combine row values must be finite numeric non-bool values`（探针：boost 的 `inf` 进入窗口后接 decay 报错；字段值为空时因子是 0，不触发），NaN 行若被 boost 排序截断在窗口外就不会触发（探针 `num boost nan + decay` 成功）。

**weighted / rrf 的 `function_score` 路径校验**（code 1，先于计算）：`rrf` 的 `k` 必须在 `(0, 16384)`（`The rank params k should be in range (0, 16384)`）；`weighted` 的 `weights` 必填且非空（`weighted reranker requires weights parameter`）、个数等于路由数（`the length of weights param mismatch with ann search requests`）、每个在 `[0, 1]`（`rank param weight should be in range [0, 1]`）。这与 `rank_params` 路径的无校验不同，**必须分开实现**。

### 5. boost（L0）

距离变换作用在引擎的**内部距离**（越小越好；输出前才转成公共分数：COSINE `1 − d`、IP `−d`、L2 `d²`）：

- 函数值：每个函数 `weight`（`filter` 不匹配则跳过）；`function_mode` 合并：`multiply` 连乘、`sum` 求和；没有匹配的函数则不变。
- `boost_mode`：`multiply`——COSINE：`1 − (1 − d) · v`，其他：`d · v`；`sum`——COSINE / IP / BM25：`d − v`，其他：`d + v`。
- **L2 的 `weight` 乘在内部距离（开方后）上**，公共分数是它的平方；IP 的内部距离是负内积。
- 之后用 `SortOp` 按内部距离升序排序：NaN 最后，**同距离按 `$id` 升序**（探针 `s boost w=0 g==1 l2`、`tie s boost`），再进入分组 / offset / limit。`weight` 可以是 `inf` / `nan`，按 IEEE 计算（探针 `num boost *`）。

`filter` 的求值是 **3.x Python 后端的逐行求值**（`compile_filter` + `_eval_row`），不走搜索过滤那套按整条表达式静态选路线的规则；行里有全部 schema 字段（含主键），**没有动态字段**。空串 filter 等于无 filter（对所有命中生效）。

- 动态字段开启的集合：`dyn > 5`、`$meta['x'] > 1` 在任何 `output_fields` 下都从不命中，也不报错；未知字段名同样不命中、不报错；JSON 字段的 `j['k'] == 1` 可用。
- **动态字段关闭的集合**（探针 `nodyn boost filter ...`）：未知字段按 `compile_filter` 报 code 6 `unknown field '<名>' at column 1 ... available fields: [...]`（与搜索自身过滤的未知字段同文案）；`$meta[...]` 报 code 6 `$meta access requires schema enable_dynamic_field=True`。
- 语法 / 类型错误（code 6）在应用 boost 时报出：先于它的是 collection 不存在（100）、未加载（101）、`anns_field`、搜索自身过滤表达式的错误，**即使没有命中也会编译**。
- 3.x 的 boost 过滤能用算术（`age + 1 > 5`）与 `text_match`；本实现的过滤范围（0006 / 0014）当前对二者返回 `UNIMPLEMENTED`，boost 过滤沿用这一范围，**登记为差异**（见“登记的差异”），不在本切片扩大过滤范围。

### 6. 候选窗口（过取）

引擎 `Collection.search` 取 `effective = top_k + offset`；有分组则 `max(effective · group_size · 3, effective · 10)`；有范围检索则 `max(·, effective · 5)`；**有 boost 则 `max(·, effective · 10)`**，boost 作用在全部候选上，再分组 / 切 offset / 取前 `top_k`。

- **Search + L2（rrf / weighted / decay）**：`search_top_k = limit + offset`；有分组再 `max(·, limit · group_size · 3)`；再 `max(·, limit · 10)`；引擎的 offset 置 0、分组关掉；rerank 链再做分组 / 排序 / `Limit(limit, offset)`。探针：`limit=5, offset=0` 时 decay 只在距离前 50 名里重排（第 49 名可见，第 50 名不可见）；`offset=3` 时窗口按 `max(8, 50)` 仍是 50。
- **Search + boost**：`top_k` 不变，引擎再乘 10。探针：`limit=1, offset=30` 的窗口是 310。
- **Search + boost + L2**：上面 L2 的 `search_top_k` 作为引擎的 `top_k`（offset 为 0），引擎再因 boost 乘 10，boost 之后取 `hits[:search_top_k]` 交给 L2 链。补充探针 `s2 win boost+decay lim1 off30`：`search_top_k = max(31, 10) = 31`，引擎窗口 310，boost 把 15 个标记行提到前面，返回前 31 个，链偏移 30 后得到 id 16。
- **Hybrid**：每路 `sub_top_k` = 路由自己的 `limit`；有分组则 `max(sub_top_k, limit · group_size · 3)`；有 boost 则再 `max(·, limit · 10)`；引擎再对 boost 乘 10。没有分组也没有 boost 时，窗口就是路由 `limit`——**最终 `limit` 比路由 `limit` 大并不会放大候选**（探针：`routelimit5` 的 decay 只重排 5 个）。
- **路由 `limit`（`topk`）<= 0**：窗口仍按上面的公式。没有分组也没有 boost 时 `sub_top_k <= 0`，该路没有候选，其余路照常（探针 `h sub request limit 0 raw` 返回 5 条，就是第二路的前 5 名；单路时为空）；有分组或 boost 时窗口被 `limit · group_size · 3` / `limit · 10` 放大，该路照常有候选（探针 `lim h single route limit 0 group`、`lim h single route limit 0 boost`）。

### 7. 分组

**链式候选行**（Search + L2 与 Hybrid 的 rerank 链，也是第 4 节各公式的输入）：每个命中是一个扁平的名字到值的行，构造顺序决定覆盖关系（探针 `vc *`、`cf *`、`cfm *`、`sf *`、`st *`、`nv *`）：

1. `$id` = 主键，`$score` = 公共分数（f64）。
2. 再用 entity **覆盖**。entity 是投影：`output_fields` 为 `None` 或 `[]` 时是引擎默认（全部非主键 schema 字段 + 全部动态键），否则是“用户 `output_fields` ∪ 内部注入字段”（分组字段、decay 输入字段），含 schema 字段与动态键，**不含主键**。同名的非主键字段或动态键只要在投影内就盖掉虚拟值，合并按盖掉后的值去重与计分（探针 `cf dyn h rrf output ['$id']`：10 行被 `$id` 并成 3 个命中）；不在投影内就不覆盖（`sf $id field s decay ['age']`）。
3. 合并把 `$score` 写成合并得分；decay 链再写 `_decay_score`（因子）并覆盖 `$score`。
4. 分组：`GroupByOp` 用**分组前的行**取 `row.get(field)` 作为组键，分组后把 `$group_score` 写进每个入选的行；响应的组值取**分组后的行**。
5. 输出：从 entity 剥掉虚拟列后再编码。

各名字的存在性（前提都写明：“同名字段”指投影内的非主键 schema 字段或动态键）：

| 名字 | 组键（分组前的行） | 同名字段的组值（分组后的行） | 同名字段在 `fields_data` 的输出 |
| --- | --- | --- | --- |
| `$id` | 同名字段值，否则主键；始终存在 | 字段值 | L2 链剥除，空值占位；响应的主键列是盖掉后的值 |
| `$score` | 最终得分（同名字段值只作为路由输入分，再被合并覆盖）；始终存在 | 最终得分 | L2 链剥除，空值占位；boost 链不剥，输出字段值 |
| `_decay_score` | decay 链：因子；rrf / weighted：同名字段值，没有则空值组 | decay 链是因子，否则字段值 | decay 链输出因子，否则字段值；不剥 |
| `$group_score` | 同名字段值，没有则空值组；分组前没有虚拟值 | **组得分**（分组后写入的） | L2 链剥除，空值占位；boost 链不剥，输出字段值 |
| `$distance` | L2 链没有虚拟值：同名字段值，没有则空值组。经过 boost（Hybrid 路由 boost、Search + boost + L2）时 L0 已剥除：空值组 | 字段值（经过 boost 时是空值占位） | L2 链输出字段值；boost 链剥除，空值占位 |
| `$meta` | 不存在（动态键已展开），空值组；同时是保留字段名（code 6） | — | — |

这些名字不是 schema 字段时，响应不写 `group_by_field_value`（探针 `vc *`、`cfm *`）。例：`sf _decay_score field h rrf group gs1` 组值 `[100, 101, 102]`；`sf $distance field s decay group gs1` 组值 `[100, 101, 102]`；`sf $group_score field s decay group gs1` 按字段分 3 组，组值是组得分 `[1, 0, 0]`（INT64 字段 `int()` 截断）；`cfm $group_score s decay ['id'] gs1` 按动态键分 3 组，响应没有组值。

**boost 链（L0）**同样是扁平行：`$id` + `$distance`，再用 entity 覆盖；**`$distance` 同名字段参与距离变换**（探针 `sf $distance field s boost None` 得分 -99）；`$id` 同名字段让响应的 id 变成字段值，boost 不去重，可能出现重复 id（`sf $id field s boost None`）。排序后从 entity 剥掉 `$id` 与 `$distance`。

**Search + boost 的引擎分组遇到这些同名字段**，结果按下面的先后决定，**上面的链式存在性表不适用**（探针 `sf * s boost group *`、`st * s boost group *`，补充探针 `probe_s4.py`）：

1. **0024 的字段类型校验先行**，与名字无关：FLOAT、DOUBLE、JSON、ARRAY、TIMESTAMPTZ、向量报 code 6 `group_by_field '<名>' has type <类型> which is not supported for group_by`（`st $id fieldD s boost group *`、`st $distance fieldD s boost group *`）。
2. **校验通过（BOOL、整数、VARCHAR）的非主键 `$id` / `$distance`，先过 boost 与响应的转换**：
   - `$distance` 是 VARCHAR 时参与距离变换：命中过滤的行做 `float()`，非数字报 code 1 `could not convert string to float: '<值>'`（`st $distance fieldV s boost group *`）；数字字符串全部命中时成功，全不命中报 code 1 `unsupported operand type(s) for -: 'float' and 'str'`，部分命中时排序比较报 code 1 `'>' not supported between instances of 'float' and 'str'`。
   - `$id` 是 VARCHAR 时，响应编码主键列做 `int()`：整数字符串成功（`"2"`），其他报 code 1 `invalid literal for int() with base 10: '<值>'`（`st $id fieldV s boost group *`）。
3. **转换成功后**，这两个字段已被 L0 剥除、取不到值，全部落入一个空值组：组值是该类型的占位加 `valid_data=false`，响应的主键列是 `$id` 的值（`sf $id field s boost group gs1`：一个命中 `102`，组值 `[0]` / `[false]`；这是 0024 允许类型下非主键 INT64 的情形）。这是扁平模型的后果，不是 0024 的缺陷，按 3.x 复现。
4. **主键同名**（`$id`、`$distance`、`$score`、`$group_score`、`_decay_score` 中任一）：按 0024 的主键缺陷处理，3.x 是单个空值组，本实现不复制，归“沿用已有差异：0024”（`st $id pkV s boost group *`），不属于这里的扁平模型。
5. **非主键 `$score`、`$group_score`、`_decay_score`**：boost 链不读也不剥，按字段值分组，组值是字段值，不是最终得分或组得分（`st $score fieldV s boost group gs1`：`['s2', 's0', 's1']`；`sf $group_score field s boost group gs1`：`[102, 100, 101]`）。

**主键不进 entity**：既不覆盖虚拟值，也不被剥除，响应里始终按命中的 id 在 schema 名下编码，名字是 `$id`、`$score`、`_decay_score`、`$group_score`、`$distance` 时也一样（探针 `sf * pk * None`：`fields_data` 都是主键值）。主键同名时 `row.get` 取到的是虚拟列，所以组键与组值单列：

| 主键名 | 组键 | 组值 |
| --- | --- | --- |
| `$id` | 主键（每行一组） | 主键，有效 |
| `$score` | 最终得分 | 得分按字段类型转换，有效 |
| `_decay_score` | decay 链：因子；rrf / weighted：空值组 | decay 链：因子转换，有效；rrf / weighted：占位，`valid_data=false` |
| `$group_score` | 空值组（分组前没有） | 组得分转换，有效（`sf $group_score pk s decay group gs1`：`[1]`，`valid_data` 为空） |
| `$distance` 与其他名字 | 空值组 | 占位，`valid_data=false` |

值写进响应时按字段类型转换（`_build_field_data`）：整数字段 `int()` 截断，DOUBLE 原样，VARCHAR 是 Python `str()`；INT8 / INT16 **不按字段宽度收窄**（`int()` 直接写进 protobuf 的 int32 槽：INT8 字段的组得分 25600 仍是 25600，超出 int32 报 `Value out of range`），JSON 是 `json.dumps`（非有限值写成 `NaN` / `Infinity` / `-Infinity`）（`st $score pkV s rrf group gs1` 的组值是 `'0.01639344262295082'`）。

同名冲突**按这个扁平模型复现 3.x**，不登记差异。非数字覆盖值也一样（探针 `nv *`、`st *`，以 3.x 原始结果为准，code 都是 1），按路径：

- `$id`：合并路径用它做键，dict / list 报 `unhashable type: '<类型>'`；boost 路径不合并，dict / list 在响应编码时报 `int() argument must be ...`；字符串在所有路径报 `invalid literal for int()`，`null` 报 `int() argument ... not 'NoneType'`；布尔按 1，合并路径并成一个命中。
- `$score`：只有用到它的路径才报。decay 报 `num_combine row values must be finite numeric non-bool values`（布尔也报，`null` 得 0；超出 int64 的整数按 Python 整数参与计算，只有转不成有限 float64 时才报这一条）；weighted 不归一化时字符串 / 列表报 `can't multiply sequence by non-int of type 'float'`、dict 报 `unsupported operand type(s) for *: 'float' and 'dict'`，归一化时报 `float()` 的错误，`null` 得 0，布尔按 1；rrf 与 boost 不读，成功。
- `$distance`：只有 boost 读，非数字报 `float()` 的错误（数字字符串的三种情况见上面引擎分组第 2 条，无分组时相同），布尔按 1；L2 链不读，成功。
- `_decay_score`：decay 链被因子覆盖，其余路径不读，全部成功。

boost 路径的其余覆盖值（第 1 步补充探针 `probe_s1x.py`，855 个场景，单独记账，转差分时并入“通过差分”；覆盖 COSINE / IP / L2 × 字段类型 × 过滤命中情况）：

- **TIMESTAMPTZ 同名字段是 `datetime`**，不是整数微秒：`float()` 报 `... not 'datetime.datetime'`，`int()` 同样；VARCHAR 主键下 `$id` 的 `str()` 是 `2025-01-01 00:00:00+00:00`（有微秒时带六位小数）。JSON 同名字段是存储文本，ARRAY 是列表。
- `$distance` 没被任何函数命中时不做 `float()`，转公共分数时按度量报：COSINE `unsupported operand type(s) for -: 'float' and '<类型>'`，IP `bad operand type for unary -: '<类型>'`，L2 字符串 / 列表 `can't multiply sequence by non-int of type '<类型>'`，其余 `unsupported operand type(s) for *: '<类型>' and '<类型>'`。
- VARCHAR 主键下 `$id` 的值为 `None`：`ids` 是字符串 `None`，**主键列是空值占位加 `valid_data=false`**，两者不同。整数主键在 `int(None)` 已报错，到不了这里。
- 过滤里读主键时，读到的是盖掉后的**原生值**（3.x `eval_row[pk] = row.get(pk, row.get("$id"))`），语义检查仍按主键的 schema 类型（`id == 's1'` 对整数主键照报 code 6）。NaN 与无穷不是空值，`datetime` 与字符串比较不成立。实现把主键操作数改绑到值的来源（同名列或动态键 `$id`），不经 JSON 之类的中间表示；此时其余动态字段（包括名字叫 `pk` 的键、`$meta['$id']`）仍不可见。
- 向量字段本身叫 `$id`：VARCHAR 主键下 `ids` 是向量的 `str()`（`[1.0, 0.05000000074505806]`），`fields_data` 里的向量列被剥除，是零向量加 `valid_data=false`。整数主键在 `int(list)` 报错；向量字段叫 `$distance` 时按上面的算术报错。
- `str()` 是精确的 Python 文本：ARRAY 内的 NaN 是 `[nan, 0.5]`，超出 int64 的动态值（只能经原始请求写入）是完整的十进制数字；整数主键下超出 int64 报 code 1 `Value out of range: <值>`。

Search 的分组有两条路径，字段校验不同：

- **Search + boost（无 L2）**：分组在引擎里做，沿用 0024（字段必须存在且是 INT8 / 16 / 32 / 64、BOOL、VARCHAR，否则 code 6；`strict_group_size` 生效），组值是字段值或 0024 的主键结果，**链式候选行的存在性表不适用**；同名字段的先后见上。探针 `s boost group field <名>`：未知字段、向量、DOUBLE、FLOAT、JSON、动态字段都报 code 6。
- **Search + L2 与 Hybrid**：引擎里的分组关闭，分组在 rerank 链的 `GroupByOp` 里做，**不经过字段校验**，也**忽略 `strict_group_size` 与 `rank_group_scorer`**：
  - 组键是 `row.get(字段)` 的 **Python 值**，用 Python 的相等与哈希（`GroupKey` 合同，探针 `gk *` / `gk1 *`）：
    - schema 标量：INT / BOOL / VARCHAR 按值，空值是一个组；FLOAT / DOUBLE 按数值，`0.0` 与 `-0.0` 同组，**每个 NaN 各自成组**。
    - TIMESTAMPTZ：按时刻（UTC 微秒）相等，`2025-01-01T00:00:00Z` 与 `2025-01-01T08:00:00+08:00` 同组，相差 1 微秒不同组，空值是一个组，与请求或集合的 `timezone`、`time_fields` 无关（探针 `gt * ts *`）。
    - schema JSON：按**存储的 JSON 文本**（不解析）。探针：`{"a":1,"b":2}` 与 `{"a": 1,  "b": 2}` 同组（写入时已规整），`{"b":2,"a":1}`（键序不同）、`{"a":1.0,"b":2}`（数字写法不同）各自成组。只覆盖可写入的 JSON：对象与数组（JSON 标量经 pymilvus 写入 3.x 会失败，不可达）。
    - 动态字段：按解码后的 Python 值。`True`、`1`、`1.0` 同组，`0`、`False`、`-0.0`、`0.0` 同组，整数与浮点**精确比较**（`2^53 + 1` 与 `2^53` 不同组，`2^53` 与 `9007199254740992.0` 同组），字符串按值且与数字不同组（`"1"` 独立），键缺失是空值组。NaN / inf 经 SDK 序列化后已是 `null`，落入空值组；原始请求写入的动态 NaN 沿用现有写入校验，不在本切片扩展。
    - 不可哈希：动态 dict → code 1 `unhashable type: 'dict'`；动态 list、向量、ARRAY 的非空 list → code 1 `unhashable type: 'list'`。ARRAY 的 `null` 是 `Null` 组，**全空时可以正常分组**（探针 `arr * all null filter`、`arr * window null`）。**只有候选行中的值参与**：不可哈希的值落在窗口外或被过滤掉时成功（探针 `arr s window null` 窗口 10 内全空成功，`arr s window mixed` 窗口 20 报错），没有候选时成功，且晚于 L2 的参数校验（探针 `gk unhashable + bad L2 s` 报 decay 错误）。
    - 未知字段、主键（候选行无主键列，任何主键类型；与虚拟列同名的主键见上表）：全部落入一个空值组。普通字段叫 `id`、`distance` 时没有特殊处理（探针 `pk field *`：链式分组下空值是一个组；引擎分组的同名缺陷见差异表）。
  - 排序：组内按得分稳定排序取前 `group_size` 个（`hits[:n]`，`n` 为 0 时已换成 1；负数是 Python 切片，留下除最后 |n| 个之外的全部），组得分取 `max`，组按组得分排序（得分方向见下），取 `[offset : offset + limit]` 个组，展平。同分按首次出现顺序（稳定排序）。
  - 响应的 `group_by_field_value`：**没有类型白名单**。分组字段在 schema 里且至少有一个命中时就写（3.x 直接走 `_build_field_data`）；动态字段、未知字段、没有命中时不写。按类型（探针 `gt *`）：

    | 类型 | `type` | 数据 | 空值占位 |
    | --- | --- | --- | --- |
    | BOOL | 1 | `bool_data` | `false` |
    | INT8 / INT16 / INT32 | 2 / 3 / 4 | `int_data` | `0` |
    | INT64 | 5 | `long_data` | `0` |
    | FLOAT / DOUBLE | 10 / 11 | `float_data` / `double_data` | `0.0` |
    | VARCHAR | 21 | `string_data` | `''` |
    | JSON | 23 | `json_data`（`json.dumps` 文本） | `null` |
    | TIMESTAMPTZ | 26 | `string_data`，UTC `Z` 文本，**不随 `timezone` / `time_fields` 变化** | `''` |
    | ARRAY | 22 | `array_data`，保留 schema 的 `element_type`（只有全空时才能到这里） | `[]` |

    `valid_data` 只在这一页组值里**确实有空值**时才写（有空值才写，全部有值时为空列表，即使字段可空）。每个命中写自己的值：同组内 `-0.0` 与 `0.0` 保留各自的符号（探针 `gk s l2 d`）。候选行无主键列时，主键组值全部是空值，所以是占位值加全 `false`（探针 `gt h id`、`pk rr_pk h group pk`；与虚拟列同名的主键见上表）。向量到不了这里（分组时已报 `unhashable`）。
  - 方向：Weighted 不归一化且全是 L2 类度量时升序，组排序随之升序。

### 7a. `limit <= 0` 与负 `offset`

三条路径语义不同，**不能共用一句结论**（探针 `lim *`，都绕过 SDK 的客户端校验；响应 `top_k` 都是请求值原样）：

| 路径 | 无分组 | 有分组 |
| --- | --- | --- |
| Hybrid（顶层 `limit`） | `limit <= 0` 不加 `Limit`，返回全部合并后的候选（20 条），`offset` 不生效；`limit > 0` 用 `Limit` | 组切片 `groups[offset : offset + limit]`（Python 切片）：`limit=0` 无组；`limit=-1, offset=0` 留下除最后一组外的全部；`limit=-3, offset=1` 取 `[1:-2]`；`limit=-1, offset=1` 为空 |
| Search + L2 | 窗口 `max(limit + offset, limit · 10)`：`<= 0` 时没有候选、结果为空（`limit=0/-1, offset=0`）；`limit <= 0` 而 `limit + offset > 0` 时有候选且不加 `Limit`，返回窗口内全部（`limit=0, offset=5` 返回 5 条，`limit=-1` 返回 4 条，`limit=-3` 返回 2 条） | 同样的窗口再并上 `limit · group_size · 3`，然后 `groups[offset : offset + limit]`；探针中 `limit <= 0` 全部为空 |
| Search + boost（引擎路径；普通 Search 同） | `effective = limit + offset`，`<= 0` 时没有候选；`offset` **仅 `> 0` 时切片** `hits[offset : offset + limit]`；负 `offset` 只缩小 `effective`，不切片（探针 `eng boost offset -1` 返回 4 条，`-3` 窗口更小，`-100` 为空）；`limit <= 0` 为空 | 引擎分组（0024）：负 `group_size` 为空，0 换成 1，`offset` 同样仅 `> 0` 时切片，负 `offset` 只缩小 `effective`（探针 `eng * group offset -1` 有结果，`-3` 为空），`limit <= 0` 为空 |

并列跨过候选窗口边界时，3.x 由 `argpartition` 任取（60 行同分、窗口 20，取到 `31, 33, 34, ...`），本实现取引擎全序的前 20 个，boost 之后的结果因此可能不同。这与普通 Search 的截断相同，属于 [0004](0004-m1-scope.md#决策) 已允许的差别；差分用例不让并列跨窗口。

引擎路径有 boost、`offset` 不为正时还要 `hits[:top_k]`，**作用在分组展平之后**：`limit=2, group_size=3` 只返回 2 条（探针 `group trunc`），而普通 Search 同样的参数返回 6 条。

**链**（Search + L2、Hybrid）用 `hits[:n]`、`scored_groups[offset : offset + limit]` 与 `LimitOp` 的 Python 切片；**引擎**（Search + boost、普通 Search）不是切片语义，见表。`Limit` 的确切形式（`limit > 0` 时）：`start = min(offset, len)`，`end = min(start + limit, len)`，`chunk[start:end]`，负 `offset` 按 Python 切片——`offset=-1, limit=4` 在候选多于 3 条时为 `chunk[-1:3]`，即空（探针 `s l2 offset -1`、`h rrf offset neg`）。分组时负 `offset` 同样按切片（`s l2 group offset -3` 取到命中，`-1` 为空）。

### 8. 响应与输出

- `top_k` 字段为请求的 `limit`；`topks` 为每个查询的命中数。
- Search 的 `group_size` / `offset` / `round_decimal` 的非整数取值沿用现有 `search_params` 解析（[search.rs](../../crates/grpc/src/search.rs) 用 `pyint`），rerank 路径不另写规则，探针 `sp *` 是回归。
- Search 的 rerank 路径：`round_decimal >= 0` 时，**rerank 之后**对最终公共分数做 Python `round`（再存为 f32）；Hybrid 服务端忽略 `round_decimal`。
- 输出字段：boost 把全部非主键 schema 字段与请求字段一起取出再剔除；L2 的隐藏输入字段与分组字段同理。`output_fields` 里的不存在字段不报错。
- Hybrid 的分区：每路用该路 `partition_names`，没有才用顶层 `partition_names`（探针：顶层写了不存在的分区、路由写了 `_default` 时成功；两层都写则以路由为准）。
- Hybrid 的子请求不做范围检索：3.x 调用 `col.search` 时不传 `radius` / `range_filter`，子请求里的这两个参数（`params` 对象里或顶层）只在解析时经过 `float()` 校验，不过滤候选、也不放大窗口（补充探针 `s3 route0 range_filter=0.9`、`s3 route radius boost`）。子请求的 `offset`、`round_decimal`、`group_by_field` / `group_size` / `strict_group_size` 同样只解析、不生效（非整数照样报 code 1）；子请求的 `function_chains` 被忽略（3.x 只在 servicer.Search 里检查它）。
- Hybrid 的子请求自带 `function_score`：**只做解码期校验**，只取其中的 boost 并入该路，L2 不构建、不校验（探针 `route fs *`）。解码期会报错的：未知 `reranker`（6）、两个 L2（6）、`norm_score` 与 `weights` 形态错（6）、`k` / 权重元素转换失败（1）、model 缺 `queries`（6）、boost 参数错（6）。解码期不报错、被静默忽略的：`k=0`、`weights=[]`、权重个数或范围不对、decay 的任何参数、合法 model。子请求的 boost 过滤语法错误照样在应用时报 6；子请求的 `random_score` 按 boost 使用。子请求的 `output_fields` 被忽略。顶层与路由的 boost 合并，函数按“路由在前、顶层在后”，`boost_mode` / `function_mode` 在顶层有 boost 时取顶层的（没写就是缺省 `multiply`，不继承路由的），否则取路由的。
- 顶层 `function_score` 有 L2 时，**忽略 `rank_params` 的 `strategy` / `params`**（探针：`strategy="foo"` 不报错）；只有 boost 没有 L2 时仍按 `rank_params`（缺省 RRF）合并。`limit` / `offset` / 分组仍取自 `rank_params`。
- 隐藏字段与响应：`group_by_field_value` 来自 rerank 链的行（不依赖用户 `output_fields`）。
- decay 链把因子写进行的 `_decay_score`：同名的非主键 schema 字段输出因子（按字段类型转换）；没有同名字段时，投影包含全部动态字段（`output_fields` 为空、`*`、`$meta`）或显式请求了 `_decay_score` 时，它出现在 `$meta` 里，**排在原有 key 之后**（3.x 读回的 `$meta` 按 key 排序，链新写入的 key 追加在末尾；原本就有这个 key 时保持原位）。本实现在取列时按 3.x 读回的顺序排好顶层 key，输出保留这个顺序（补充探针 `s2 decay output empty list`、旧探针 `cf dyn s decay *`）。写进 `$meta` 的因子按 `json.dumps` 输出，非有限值是 `NaN` / `Infinity` / `-Infinity`（字段值为 +inf、`scale=1e308` 时因子为 NaN）；因子由链单独带到输出，`$meta` 里的同名 key 只占位置，因为 JSON 数字表示不了 NaN。
- Hybrid 的时区：输出用的 `timezone` / `time_fields` 取 `rank_params` 里的，没有则取第一个带它的路由；路由过滤与 boost 过滤用的时区是 `路由的 || rank_params 的`；两个校验点都在（未知时区 code 6 `unknown timezone '<名>'`；`time_fields` 非法 code 6）。**decay 的输入字段不能是 TIMESTAMPTZ**（上面的类型校验），公共 `function_chains` 里的 TIMESTAMPTZ decay 不在本切片。

### 9. 错误先后

Hybrid：`function_chains` 非空（6）→ collection 不存在（100）→ `rank_params` 的 `limit` / `offset` / `group_size` 的 Python 转换（1）→ `function_score` 解码（6）→ 逐路依次完整执行：子请求自身解析（如缺 `topk`，6）→ 未加载（101）→ `anns_field`（6）/ 过滤表达式（6）/ 向量维度（1）→ 检索 → 链构建：`strategy`（1）与各 L2 校验（1）。已探针的样例：`function_chains` 先于不存在的 collection；`h unloaded + bad strategy`、`h unloaded + bad anns` 都报 101；`bad strategy + bad filter` 报过滤错误（路由先于链构建）；`function_score` 解码错误先于路由错误；第一路的错误先于第二路。

Search：collection 不存在（100）→ 解码 `function_score`（boost 参数与 L2 的 `reranker`，6；`function_score` 与 `function_chains` 同时出现，6）→ 未加载（101）→ 引擎校验：`anns_field`（6）、搜索自身过滤表达式（6）→ boost 过滤编译（6，没有命中也编译）→ L2 校验（1）。样例：`s unloaded bad boost` 报 6 而 `s unloaded boost` 报 101；`bad boost + bad anns` 报 boost 解码错误；`bad L2 + bad anns` 报 `anns_field`；`bad L2 + bad filter` 报过滤错误；`unknown reranker + bad filter` 报 `reranker` 错误。

### 10. model 与 `random_score`

3.x 中合法的 provider 只有 `cohere`；`reranker` 不是 `model` 但带 `provider` 也走 model。逐场景的 3.x 结果与本实现处理：

| 场景（探针名） | 3.x | 本实现 |
| --- | --- | --- |
| `h fs model no provider`、`s l2 model bad`、`route fs model no queries`、`fs provider only no queries` | code 6 `Model reranker requires params.queries` | **相同** |
| `h fs model bad queries` | code 6 `Model reranker params.queries must be a non-empty array` | **相同** |
| `fs model 2 inputs` | code 1 `model reranker requires exactly 1 input field`（检索之后） | **相同** |
| `fs model no field` | code 1 `input field nosuch not found in collection schema` | **相同** |
| `s model bad field` | code 1 `model input field age must be VARCHAR, got INT32` | **相同** |
| `h fs model provider unknown`、`fs model api_key param` | code 1 `Unknown rerank provider: 'nosuch'. Supported: 'cohere'` | **相同** |
| `s model valid + bad anns`、`h model valid + bad route` | code 6（路由错误在先） | **相同** |
| `s model valid + unloaded` | code 101 | **相同** |
| `route fs model valid` | 成功，子请求的 L2 被忽略 | **相同**（不报 `UNIMPLEMENTED`） |
| `h model valid`、`s model valid`、`s l2 model`、`fs provider only` | 无 key 时 code 1 `Cohere API key is required...`；有 key 时调用外部服务 | **登记差异**：通过全部校验后返回 `UNIMPLEMENTED` |
| `s boost random_score bad` | code 6 `... params.random_score must be an object` | **相同** |
| `s random_score + bad anns`、`+ bad filter`、`+ unloaded` | code 6 / 6 / 101 | **相同** |
| `fs random_score no hits` | 成功、空结果（没有候选行，不会用到） | **相同** |
| `s boost random_score`、`s boost random_score noseed`、`s random_score valid`、`route fs boost random` | 成功，按 sha256 派生 | **登记差异**：只有候选行真正用到时（该函数的过滤命中）才返回 `UNIMPLEMENTED` |

## 取舍（评审已回复的结论）

1. **boost 过滤读不到动态字段**：照 3.x，登记限制只写“动态字段在 boost 过滤中不可见”；动态字段关闭时未知字段报 code 6（见 5）。
2. **分组字段不校验：方案 A（负责人已确认）**。Search + L2 与 Hybrid 一律按 `GroupByOp`（组键按 `GroupKey` 合同：未知字段单空组、不可哈希值报 `unhashable`、DOUBLE / JSON / 动态可分组、`strict_group_size` 忽略）；Search + boost 的引擎分组保持 0024 校验。不登记分组校验差异。
3. **`rank_params` 无校验照 3.x**：异常类型清单写在第 3 节并逐字复现，Rust 侧不 panic、不溢出，除法分母为 0 显式产生 `float division by zero`，不同 Python 文案不合并成一个 code 1。清单以外的类型组合（如 `weights` 元素是嵌套对象）在差异表登记。
4. **Hybrid 忽略 `round_decimal` / `strict_group_size` / `rank_group_scorer`**；`strict_group_size` 在 Search + L2 也被忽略；`round_decimal` 只在 Hybrid 忽略，Search 照常。README 写“Hybrid 忽略”，不写“rerank 忽略”。
5. **负 offset / 负 `group_size` / `limit <= 0` 分路径复现**：链（Search + L2、Hybrid）用 `hits[:n]`、`scored_groups[offset : offset + limit]`、`LimitOp` 的 Python 切片，不复用 0024 的 `group_order`；引擎（Search + boost，普通 Search 同）负 `group_size` 为空、0 换成 1、`offset` 仅 `> 0` 时切片、负 `offset` 只缩小 `effective`、`limit <= 0` 为空。各路径都覆盖；链的负 `group_size` 有单测。

## 实现设计

### 分数空间的两空间契约

3.x 的顺序是：boost 在引擎内作用于**内部距离**，随后 `Collection.search` 把内部距离转成**公共分数**，Hybrid 合并与 Search L2 只消费公共分数。本实现当前 `Hits.scores` 已经是公共分数（`flat` 返回 L2 的平方距离、COSINE 的相似度、IP 的点积），因此：

- 内部距离（L2 = 开方距离，COSINE = `max(1 − sim, 0)`，IP = `−dot`）由引擎的 float32 分数经 `Metric::internal` 还原，仅在存在 boost 时消费；普通 Search 的分数与排序不变。还原结果与引擎打分时的内部距离**逐位相同**（单测按步长抽样约 100 万个 float32 位模式核对），所以不在扫描路径上多带一列；以后引入无法由对外分数精确还原的度量时，要重新评审这一点。它与 3.x 的差别只来自 float32 的求和顺序（numpy 与本实现不同），[0004](0004-m1-scope.md#决策) 已登记分数与 `round_decimal` 的允许误差，普通 Search 同样如此（dim 8 实测：L2 普通检索 `round_decimal=6` 为 `377.948028` 对 `377.948120`），导出一列也不会改变这个差别。
- boost 只消费 internal（f64 里算），完成后一次性转成 public f64（COSINE `1 − d`、IP `−d`、L2 `d²`，与 `Metric::public_f64` 的方向相反，另写一个 `Metric::public_from_internal`，不复用它）。
- Hybrid 合并、Search L2 的 rrf / weighted / decay 只消费 public f64；不从 public 反推 internal。
- 响应层对 rerank 路径的最终 public 分数只做 `round_decimal`（Python `round`）与 f32 存储，**不再经过 `round_score` 里的 `Metric::public_f64`**（那个函数接收的是内部 f32 并按度量转换，对已是公共分数的输入会二次转换）。普通 Search 的现有路径保持不变。

### core：`rerank` 模块

- `rerank::boost`：`BoostSpec { functions, boost_mode, function_mode }`。过滤表达式只编译一次；**求值入口写死**：`ParsedFilter::parse_with` → `Tree::compile`（对所有形状都构造 `Tree`，包括普通搜索会走 legacy 快路径的简单表达式）→ `python::evaluate_tree`，每个候选一行 `DynamicRow`（`columns` 是候选的 schema 字段列，含主键；`meta = None`，即没有动态字段）。在 `filter` 模块新增一个 `pub(crate)` 入口封装这一步，**不改变普通搜索过滤的静态选路与现有快路径**。实现第 1 步先写最小测试证明该入口：schema 字段比较、JSON 路径、`like`、`in`、null、时间戳字面量，并断言动态字段开启 / 关闭时未知字段分别“不命中”与 code 6、`age + 1 > 5` 与 `text_match` 返回 `UNIMPLEMENTED`。
- `rerank::chain`（第 2 步实现，一个模块）：合并（RRF；Weighted 含三类度量的归一化与方向转换；decay 用的 `max` / `sum` / `avg`）、decay 的三条曲线与因子、`SortOp` / `LimitOp` / `GroupByOp`（稳定排序、Python 切片语义）；`GroupByOp` 是独立实现，不复用 0024 的 `group_order`。decay 的 `sum` / `avg` 照 CPython 3.12 `sum()` 的三段（i64 快路径、浮点的 Neumaier 补偿且整数不补偿、溢出后的通用路径）求和，宽整数是精确的 Python 整数，平均是整数真除法的正确舍入；rrf 的 MergeOp 在 3.x 里构造时不传度量，所以总是降序、不归一化；weighted 的某一路分数被同名字段盖成 `None` 时按缺席处理（3.x 的 `is not None`）。组键写成 `GroupKey` 枚举，按第 7 节的合同比较，不用 `serde_json::Value` 或 `f64` 的位比较：`Null`；`Num`（BOOL、整数、FLOAT、DOUBLE 与动态数字共用一个变体，比较沿用 `filter::python` 里已有的 Python 数值规则，包括整数与浮点的精确比较和宽整数；`-0.0 == 0.0`；NaN 不等于任何值，含自身，所以各自成组）；`Str`；`JsonText`（schema JSON 的存储文本，只与同类相等）；TIMESTAMPTZ 列是 `ColumnData::Int64` 的 UTC 微秒，走 `Num`（同列内比较，不会与其他类型混合）；不可哈希的值构造时直接返回 `unhashable type` 错误。组的查找用线性探测的小表或按 `GroupKey` 派生的规范键加 NaN 特判，不能依赖 `f64` 位比较的 `HashMap`。
- 候选窗口：Search + boost、Search + L2、Hybrid 三组公式各写成独立函数并单测（含溢出、负值、0）。**不复用 0024 的 `group_window`**：rerank 的分组窗口是 `limit · group_size · 3`（不含 offset），与它的 `(limit + offset)` 形式不同。
- 输入输出分层：rerank 入口显式携带“用户 `output_fields`”与“内部注入字段”（boost 过滤用的全部非主键 schema 字段、decay 输入字段、分组字段），候选行按内部集合取列，响应只按用户集合编码；分组字段值作为独立列从 rerank 链带到响应（`Hits.group_values` 现在只覆盖引擎分组，这里为链式分组补一条来源，与引擎分组共用响应编码）。组值编码复用 [output.rs](../../crates/grpc/src/output.rs) 的 `group_by_field_data` / `field_data`（已覆盖全部类型与 TIMESTAMPTZ 的 UTC 文本）；需要补的是：主键组值是全空列（字段本身不可空），`valid_data` 按“这一页有空值才写”，动态字段与未知字段不写。
- Hybrid 入口（第 3 步实现）：gRPC 层逐路依次执行“子请求解析 → 取这一路分区的读快照 → load 状态 → `ReadSnapshot::rank_route`（窗口来自 `window::hybrid_route`，boost 是路由与顶层合并后的）”，保持第 9 节的错误先后；各路的读快照可以不同（分区选择集决定可见性，见 0017），候选行记住来自哪一路，取列时从各自的快照读。全部路由完成后 `hybrid_merge` 构造合并链（顶层 `function_score` 有 L2 时用 `Chain::build`，否则用 `Chain::rank_params`，后者保留 `k` / `weights` / `norm_score` 的原始值），检查各路查询数，执行链并取列；Search + L2 与它共用同一个链执行与取列函数。没有子请求时不检查 load 状态，合并时报 `MergeOp requires at least one input`。
- **并发**：沿用 0005——`Compute::for_each` 只能从计算池之外调用，所以 Hybrid 的各路在 RPC 阻塞线程上**顺序**执行，每路内部按现有方式并行；合并、boost、排序、分组在 RPC 线程上单线程完成；**禁止在一次 `for_each` 的叶子任务里再提交下一层**。测试覆盖多路 × 多 nq × 并发请求，debug 下不触发 `current_thread_index` 断言。不新增锁。

### grpc

- 新增 `HybridSearch` 到 `IMPLEMENTED_METHODS`；`rank_params` 与 `function_score` 的解码放在独立的解码模块，保持 3.x 的错误先后（见“错误先后”）；`SearchRequest.function_score` 接入现有 `search::parse`，把原来合并的 `note("rerank and function chains")` 拆开：`function_chains` 保持 `UNIMPLEMENTED`，`function_score` 按本设计处理，`model` / `random_score` 按第 10 节的逐场景表处理。
- Search 的 `round_decimal` 仍读 `search_params` 的顶层键（现有行为）。
- 响应编码沿用 `output` 的现有函数（含 0024 的分组字段值、0025 的时区与 `time_fields`）。

## 登记的差异

只有下面几类。第 10 节里写“相同”的 model / `random_score` 场景不是差异，错误码与文案逐字一致。

- Search 与 HybridSearch 顶层的合法 model（`cohere`）通过全部校验后：`UNIMPLEMENTED`，3.x 是 code 1（缺 key）或调用外部服务。HybridSearch 子请求里的 model 被忽略，两边都成功，不是差异。
- 候选行真正用到的 `random_score`：`UNIMPLEMENTED`，3.x 成功。
- 公共 `function_chains`（Search）：`UNIMPLEMENTED`。
- boost 过滤中的算术表达式与 `text_match`：`UNIMPLEMENTED`（3.x 可用；等待过滤范围与全文检索切片）。
- 路由查询向量维度或度量非法：沿用 [0004](0004-m1-scope.md#决策) 的严格校验文案（已登记），3.x 是 numpy 原始报错。
- 引擎分组（普通 Search 与 Search + boost）的两个 3.x 缺陷：沿用 [0024](0024-group-by-search.md) 已登记的“不复制”。主键名不是 `id` 时按主键分组变成单个空值组；可空字段叫 `distance` / `id` 时取错值（探针还看到 `id` 字段配 VARCHAR 主键时 3.x 报 `invalid literal for int()`）。**链式分组按 3.x**：主键（除与虚拟列同名）不在候选行，是单个空值组，不是差异；只有非主键字段名为 `id` / `distance` 的引擎错值在链式路径不存在。
- `GroupByOp` 的排序键含 NaN 时（`weight` 为 `nan` 的行进入分组且落在窗口内）：3.x 的结果依赖 Timsort 的比较序列，不逐位复现，本实现把 NaN 排在该序列的末尾。无分组路径的 NaN 按 `SortOp` 复现，不在此列。
- 参数里的 JSON 非标准字面量（Python `json.loads` 接受的 `NaN`、`Infinity`）与超出 f64 的整数参数：本实现按字符串或溢出处理，不逐一复现。`rank_params` 的 `limit` / `offset` / `group_size` 超出 int64 时沿用 0004 的饱和规则：3.x 的 `limit` 在响应编码 `top_k` 时报 `Value out of range`，带分组或 boost 时各路窗口由 numpy 报 `Maximum allowed dimension exceeded`，`group_size` 同样由 numpy 报错，本实现按饱和值执行并成功；`offset` 两边都返回空结果（补充探针 `s3 rank * huge`）。
- 子请求的 `params` 不是严格 JSON（如 `{'nprobe': 10}`）：与 Search 相同，沿用 0004 / 0016 已登记的拒绝（UNIMPLEMENTED）；3.x 把它当字符串忽略（补充探针 `s3 route* params={'nprobe': 10}`）。
- 第 3 节清单之外的极少见参数类型组合的 Python 异常文案（只登记没有逐一复现的组合，附场景）。`rank_params` 的 weighted 里，字符串或列表权重遇到被同名字段盖成整数的分数时，3.x 先做序列重复再在累加时报 `unsupported operand type(s) for +=`，本实现直接给出同一条累加错误，不构造重复后的序列（重复次数极大时 3.x 另报 OverflowError / MemoryError）。
- 链算出的值（得分、组得分、decay 因子）写进 TIMESTAMPTZ、ARRAY 或向量类型的同名字段时（字段名是 `$score`、`$group_score`、`_decay_score`，或主键叫这些名字并按它分组）：本实现返回 UNIMPLEMENTED。3.x 按各自的转换得到依赖实现细节的结果（ARRAY 写成空数组、向量报 `len()` 的类型错误等），没有逐一复现。

## 测试矩阵

第 1 步已转入仓库的差分在 [test_rerank_boost.py](../../tests/differential/test_rerank_boost.py)，第 2 步的在 [test_rerank_l2.py](../../tests/differential/test_rerank_l2.py)（Search + L2 的三种函数、窗口、参数的原始 JSON 类型、错误先后、扁平模型与 2 个 Search + decay 平局场景），第 4 步把设计阶段探针的 2822 个场景与 `probe_s4.py` 的 164 个场景整体转入 [rerank_cases.py](../../tests/differential/rerank_cases.py) 与 [test_rerank_scenarios.py](../../tests/differential/test_rerank_scenarios.py)，按下表的类别两边分别断言；第 3 步的在 [test_hybrid_search.py](../../tests/differential/test_hybrid_search.py)（6 个 Hybrid 平局场景；rank_params 与 function_score 两条合并路径、混合度量、各路窗口、子请求的 boost 与 L2、rank_params 原始值、错误先后；宽整数 `$score` 的多路 `$id` 碰撞；时区与输出；NaN decay 因子写进动态 `$meta`，Search + L2 的同一场景在 test_rerank_l2.py）。第 3 步的对照集合是旧探针全部 2822 个场景加补充探针 `probe_s3x.py` 的 245 个（宽整数 `$score` 与 `$id` 碰撞下 decay 的 max / sum / avg、路由 search_params 里的其他键、nq=2 的多路、别名与数据库、partition key、过滤模板、跨路的错误先后、空的 requests、多路与空集合、rank_params 原始值、输出），实施后的差异只剩登记差异（过滤范围 6、model 4、`random_score` 4、0004 的维度文案 1、0024 的主键分组 41、超出 int64 的 rank_params 整数 4、子请求的非严格 JSON `params` 2）。第 2 步的对照集合是旧探针里 907 个 Search + L2 场景加补充探针 `probe_s2x.py` 的 366 个（rrf `k` / weighted `weights` 与 `norm_score` 的原始类型、Search 版 decay 参数类型与 `score_mode`、rrf / weighted 的切片与分组、boost + L2 窗口、超出 SDK 校验的 `round_decimal`、错误先后、没有命中），实施后除 3 个 model 登记差异外与 3.x 相同。探针共 2822 个场景，分类由归档里的 `classify.py` 机器生成，**不把全部场景写成通过**：

| 类别 | 数量 | 内容 | 断言 |
| --- | --- | --- | --- |
| 通过差分 | 2749 | 其余全部，包括错误场景（错误码与文案是断言的一部分）；第 10 节写“相同”的 model / `random_score` 场景在此 | 比较响应里的原始 `ids` / `scores`（不用 SDK 的 `Hit`：字段叫 `id` 时它的 `id` 键取到字段值；`distance` 又被客户端取整）：分数容差 1e-5、顺序逐项一致、`group_by_field_value` 的类型 / 数据 / `valid_data`、错误码与文案；`cf *`、`cfm *`、`sf *`、`st *`、`nv *`、`vc * star` 还严格比较 `fields_data` 每列的名字 / 类型 / 数据 / `valid_data`（同名字段的空值占位与主键值要能区分）。**13 个 `tie *` 场景全部严格断言 ids / scores / 顺序**，不使用 0024 的组级不变量（它只适用分组、不比分数、不容忍截断成员差）：引擎并列全序是“源内名次 → 原始 Parquet 行”，本机普通 Search 实测与 3.x 一致，已有跨平台精确并列差分作证；CI 的 Linux x86_64、Linux arm64、macOS 各跑一次作为验收。组值逐命中比较（同组内 `-0.0` / `0.0` 保留符号，比较时 `-0.0` 不等于 `0.0`）；`group_by_field_value` 里的 NaN 比较时全部视为相等（组键计数仍按每个 NaN 各自成组） |
| 登记差异：model | 4 | `h model valid`、`s model valid`、`s l2 model`、`fs provider only` | 3.x 一侧断言 code 1 文案（测试环境不设 key），本实现一侧断言 `UNIMPLEMENTED` 与说明 |
| 登记差异：`random_score` | 4 | `s boost random_score`、`s boost random_score noseed`、`s random_score valid`、`route fs boost random` | 3.x 一侧成功，本实现一侧断言 `UNIMPLEMENTED` 与说明 |
| 登记差异：过滤范围 | 6 | `s boost filter arithmetic`、`s boost filter text_match`、`dyn boost filter age + 1 > 5`、`dyn boost filter text_match(...)`、`nodyn boost filter age + 1 > 5`、`nodyn boost filter text_match(...)` | 3.x 一侧成功，本实现一侧断言 `UNIMPLEMENTED` |
| 沿用已有差异：0004 | 1 | `h bad dim` | 两边都是 code 1，文案按 0004 |
| 沿用已有差异：0024 | 42 | `pk rr_pk*` / `pk rr_pki*` 的 `s plain group pk`、`s boost group pk`，`pk field distance s {plain,boost} gs*`、`pk field id s {plain,boost} gs*`，`sf * pk s {plain,boost} group *`，`st * pkV s boost group *` | 3.x 一侧断言其缺陷结果，本实现一侧断言 0024 的正确结果 |
| 探针准备 | 13 | `sf create *`（12）、`cf dynamic insert` | 建表与写入的接受结果，用来固定冲突场景的前提，不是 rerank 断言 |
| SDK 客户端校验 | 3 | `h rrf limit 0`、`h rrf limit -1`、`s boost limit 0` | 请求没到服务端，两边相同；服务端语义由 `lim *` 覆盖 |

不纳入的公共 `function_chains`、多向量字段、BM25 / sparse 没有探针场景，也不写用例。基线（no-op，防止以后误判为遗漏）：`misc h consistency kw`、`misc h guarantee_timestamp` 这两个参数在 Hybrid 上不起作用；`h fs empty functions` 的空 `function_score` 等于没有；`s boost function_score params only` 固定 mode 大小写不敏感。

- **同名冲突**：`cf *`、`cfm *`（动态键，后者多值）、`sf *`（INT64 字段与主键）、`st *`（DOUBLE / VARCHAR 字段、VARCHAR 主键）、`nv *`（非数字覆盖值）与 `vc *`（虚拟列分组）按扁平模型严格断言 ids / scores / 组值 / `fields_data`。路径覆盖 Search 的 decay、rrf、weighted、boost（含引擎分组）、普通，Hybrid 的 rank_params、顶层 L2、路由 boost；`output_fields` 覆盖 `None`、`[]`、`*`、显式、隐藏。引擎分组里主键的结果归“沿用已有差异：0024”。`sf create *` 固定 3.x 接受这些名字（只有 `$meta` 是保留名，code 6）；本实现已知同样接受，第 1 步重建后复跑。引擎分组同名字段的类型矩阵与数字字符串另有补充探针 `probe_s4.py`（164 个场景，单独记账，不计入上表；转差分时并入“通过差分”）。部分命中时排序比较的文案，操作数顺序取决于 Timsort 的比较序列，差分只断言 code 1 与 `not supported between instances of` 这一段。
- **窗口专项**：`rr_win`（400 行，按距离名次放置标记行）固定 Search + boost、Search + L2、Hybrid 的候选窗口，覆盖 offset、分组、boost、路由 limit、`limit <= 0`；`h sub request limit 0 raw`、`lim h route0 limit *`、`lim h single route limit *`、`h group limit raw -1`、`lim h group limit *` 按命中数与 `top_k` 逐格断言。
- **Python 参照用例**：上面 18 条 ID 逐条记账（INVENTORY 记录状态与首个阻断）；`test_hybrid_search.py` 里建表用了 BM25 的用例保持被 text analyzer 阻断，写明 ID，不计入通过；`test_function_chain_l2` 全部是公共 chain，不纳入。
- **单元测试**：`SortOp` 平局（`$id` 升序，VARCHAR 按码点）与 NaN 排最后；非有限值进入 decay 的报错与 `scale²` 为 0；RRF / Weighted / decay 的公式与方向（三度量、混合度量）、两空间转换、Python 真值（`norm_score`、`k` 的各类 JSON 类型）、`GroupByOp` 切片（含负 `group_size`、`offset`、`limit`）、`GroupKey`（`True / 1 / 1.0`、`2^53` 精确比较、`-0.0`、NaN 各自成组、JSON 文本、不可哈希）、boost 变换表、三组候选窗口公式、`filter` 的 Python 逐行入口。
- **原始 gRPC 差分**：`FunctionScore`（多函数、`boost_mode` / `function_mode`）、路由级与 `rank_params` 级时区、子请求自带 `function_score`、`function_score` 与 `rank_params` 并存、Search 的 `round_decimal`（`search_params` 顶层键）与 Hybrid 忽略。
- **变异验证**：Weighted 改成 min-max；boost 在公共分数上算（或 L2 乘在平方距离上）；窗口少乘 10；`norm_score` 取严格布尔；Search + L2 分组恢复字段校验；`GroupByOp` 的负 `group_size` 当空组；`round_decimal` 二次经过 `public_f64`；`GroupKey` 改成位比较（`-0.0` 分组、NaN 同组）；`SortOp` 平局改成首次出现；组值用类型白名单（FLOAT / DOUBLE / JSON / TIMESTAMPTZ 丢组值）；子请求 L2 也做链校验；Search + L2 的 `limit <= 0` 返回全部候选。

## 实施步骤

1. core：两空间还原、boost 与三组候选窗口、`filter` 的 Python 逐行入口，Search 的 `function_score`（boost）与解码；单元测试。
2. core：L2 合并（rrf / weighted / decay）、`GroupByOp` 与 Search 的 L2 路径、`round_decimal` 的 rerank 后取整；单元测试。
3. grpc / core：HybridSearch（`rank_params`、`function_score`、分区、分组、时区与 `time_fields`）。
4. 差分、参照用例、文档与验收记录；计数同步。

每步按“冻结 diff → 评审 → 授权 → 提交”的节奏，设计通过前不动产品代码。
