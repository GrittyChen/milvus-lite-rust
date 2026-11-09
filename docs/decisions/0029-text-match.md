# 0029：standard 分词与 text_match

状态：已实施。参照版本 `1320d26`。

## 范围

- `text_match(field, 'query')` 过滤，覆盖 Query、Search、count、Delete、boost、HybridSearch 子请求、分组、分区与 iterator。
- 建表属性 `enable_analyzer`、`enable_match`、`analyzer_params`：接受、持久化、回显，与 3.x 数据目录互通。
- standard 分词（不带停用词）。

不包含：停用词与 FNV term hash（只有 BM25 用得到，属于全文检索切片）；BM25 function（继续拒绝，带 function 的 3.x 集合仍然打不开）；Jieba；与算术混用的 `text_match`（算术过滤切片之前继续返回 `UNIMPLEMENTED`）。

## 3.x 的行为

`text_match` 的语义在 [python_backend.py](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/search/filter/eval/python_backend.py#L273)：

- 字段和查询都先 `str.lower()`，再按 `\w+`（`re.UNICODE`）切词，多个词是 OR；查询切不出词时不匹配。
- 不要求 `enable_match` 或 `enable_analyzer`，分词器固定 standard，完全不读 `analyzer_params`。
- 只对 Python `str` 分词。NULL、整数、浮点、布尔、ARRAY、TIMESTAMPTZ 都返回 False，不是未知，所以 `not text_match(t, 'x')` 选中 NULL 行。
- JSON 字段是字符串列，按存储的 JSON 文本整体分词，键名也算词。
- `_rewrite_dynamic_field_refs` 跳过 `TextMatchOp`，schema 外的名字在动态集合里也报 `unknown field`。向量字段和保留字段报 code 6，`text_match($meta, ...)` 是语法错误。
- 语义分析只置 `has_meta_access`，整条表达式走 hybrid：先走列式，碰到 `TextMatchOp` 抛 `TypeError` 再回退逐行。排在它前面的列式操作数可能先报错，例如 `n == 99999999999999999999 and text_match(t, 'hello')` 报 code 1，操作数交换后返回空。

建表（[_decode_field](https://github.com/milvus-io/milvus-lite/blob/1320d26/milvus_lite/adapter/grpc/translators/schema.py#L203)）：

- 两个开关只认大小写不敏感的 `true`，任意字段类型都接受并回显。pymilvus 只保留向量、VARCHAR、ARRAY 上的这些属性，其余类型只能由原始 gRPC 构造。
- `analyzer_params` 是 `json.loads` 的结果：`JSONDecodeError` / `ValueError`（含超过 4300 位的整数）当作没有设置；解析成功且为真值时保存，不要求是对象。回显用 `json.dumps`（`ensure_ascii=True`，保留键序）；`schema.json` 用 `sort_keys=True`，所以新进程重开后回显变成排序后的键序。

## 设计

**分词**：[analyzer.rs](../../crates/core/src/analyzer.rs) 按码位处理，流式小写、切词，查询词在编译表达式时分词一次，命中第一个查询词就结束。大小写、`\w` 和词尾 sigma 用 [generate_filter_unicode.py](../../tests/differential/generate_filter_unicode.py) 从 CPython 3.12 生成的固定表：Rust 标准库的 `to_lowercase` 在 1.98.1 上与 CPython 3.12 有 55 个码位不一致，而且随工具链的 Unicode 版本变化。

**过滤**：新增 `Kind::TextMatch { field, query }`，字段保留为子节点，因为 boost 里 `$id` 改绑主键后要按改绑的字符串分词。路线与 3.x 相同：提升到 Hybrid，列式求值碰到该节点返回可回退错误，再走已有的逐行路线。列式路线按节点排除该字段的物化，同一列被别的节点引用时仍然保留。

**`analyzer_params`**：判定在 `filter::classify_analyzer_params`，分两步，解码用 Python 语义，取值用 serde_json：

| 输入 | 处理 |
| --- | --- |
| Python 解析失败 | 当作没有设置 |
| 解析成功但为假值（`0`、`-0`、`0.0`、`1e-999`、`""`、`[]`、`{}`、`null`、`false`） | 当作没有设置 |
| 真值，能表示成严格 JSON，容器深度不超过 124 层 | 保存并回显 |
| 真值，但无法表示（见下文差异） | `UNIMPLEMENTED` |

124 层是 `schema.json` 里 serde_json 能读回的上限：字段比值多包三层，单独解析的上限是 127 层。

没有持久化格式变更，三个键都是 3.x 已有的。此前的版本打开带这些属性的集合会明确报错。

## 登记的差异

| 情形 | 3.x | 本实现 | 理由 |
| --- | --- | --- | --- |
| Unicode 版本 | 随运行的 Python 版本变化 | 固定 CPython 3.12 / Unicode 15.0，与过滤表达式的词法分类一致（[0006](0006-m2-scope.md)） | 分词要在各构建上逐位相同 |
| `analyzer_params` 含 `NaN`、`Infinity`、`1e999` 这类溢出数，或深度 125 到约 1000 层，或键名是 `$serde_json::private::Number` | 建表成功 | 建表返回 `UNIMPLEMENTED` | 这些值无法写成本实现重开时能读回的严格 JSON，私有键会被 serde_json 静默改成数字 |
| `analyzer_params` 含孤立代理，或深度超过约 1000 层（取决于调用栈） | 报 code 1；前者还会留下集合目录 | 建表返回 `UNIMPLEMENTED` | 两边都拒绝，错误码不同 |
| 重复键把不可表示的值覆盖掉，如 `{"a":"\udc00","a":{}}` | 建表成功 | 建表返回 `UNIMPLEMENTED` | serde_json 在解析中途失败；方向是拒绝，不会静默出错 |
| 3.x 写出的 `schema.json` 含 `NaN`、`Infinity`（含 `1e999` 落成的）、超过 124 层的 `analyzer_params`，或上述私有键与其他键并存、私有键的值不是数字文本 | 能打开 | 打开该集合报 code 1 `storage error`，drop 可用，同库其他集合不受影响 | 同上 |
| 3.x 写出的 `schema.json` 里，某个对象只含上述私有键，值是数字文本（如 `"1"`、`"1e5"`） | 原样回显 | 重开后该对象被当作数字回显（如 `1`、`100000.0`） | 读文件时无法区分；这个键极少见，`text_match` 也不读 `analyzer_params` |
| JSON 字段的顶层字符串（如 `{"j": '"a\\nb"'}`） | 存解码后的无引号文本 | 存带引号的 JSON 文本，见 [0004](0004-m1-scope.md) | 差异产生在写入时。`==`、`!=`、`in`、`like`、比较和 `text_match` 这些按存储文本的整字段操作都受影响；3.x 读本实现写的目录，结果与本实现相同，所以不在分词前解码 |
| 读到越界 TIMESTAMPTZ 的毒化目录时走 `text_match` | 回退路线 `to_pylist()` 全表，报 code 1 | 按列求值，不读无关的时间列，不报错 | 见 [0025](0025-timestamptz-and-timezone.md) |

## 验收

- [analyzer_standard.rs](../../crates/core/tests/analyzer_standard.rs) 对照真实 StandardAnalyzer 导出的语料（`tests/differential/generate_analyzer_fixtures.py`），覆盖全部有小写映射的码位；[fuzz_analyzer.py](../../tests/differential/fuzz_analyzer.py) 手动运行，种子 29、31 各 20 万条零差异。
- [test_text_match.py](../../tests/differential/test_text_match.py) 67 条差分：行为矩阵（内存与落盘）、错误先后、各读取路径、多 segment 与分区、`$id` 改绑、属性取值矩阵与全部字段类型、登记差异。
- [互通 test_text_match.py](../../tests/storage_interop/test_text_match.py) 14 条：Rust 与 Arrow 15/25 的 3.x 互相打开 WAL 和 segment，新进程重开后的键序，不可表示的 `analyzer_params` 打开失败时可 drop。
- 迁入参照 `TestSearchTextMatch` 4 条。依赖 BM25 schema 的其余 `text_match` 用例留到全文检索切片。

没有性能声明。`text_match` 与 3.x 一样逐行全扫，不建倒排索引。

## 下一个候选需要写明

支持 `text_match` 和 `enable_analyzer` / `enable_match` / `analyzer_params`；上表的差异；带这些属性的集合不能再由此前的版本打开。
