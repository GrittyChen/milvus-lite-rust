# JSON 数字私有键修复

- 状态：实施与开发验收完成，独立代码评审及增量复审通过。
- 基线：`15ebbe8`；参照 `1320d26`。
- 持久化格式不变，`pyjson` 输出、`arbitrary_precision` 与名字校验规则不变。

## 问题

serde_json 开启 `arbitrary_precision`（宽整数原样保存，见 [0006](../decisions/0006-m2-scope.md) 第 26 项）后，解析器把每个数字编码成只含 `$serde_json::private::Number`（下称 T）一个键的 map 交给 visitor；它自带的 `Value` 反序列化只看对象的首键是不是 T，是就按数字读。3.x 用 `json.loads`，T 只是普通键。凡是把文本解析成 `Value` 的地方都受影响：写入路径、WAL 与 Parquet 的读回、`schema.json` / manifest 中的 `Value` 字段与整文件解析、别名与数据库属性文件、gRPC 的请求参数。T 也是合法的集合名、分区名、字段名、别名与数据库名（两边都只排除 `/`、`\`、`\0`、`.`、`..`），所以名字也能触发。

修复前在 `15ebbe8` 上实测（同一脚本分别跑 3.x 与本实现，再交叉打开对方的目录）：

| 类别 | 例子 | 3.x | 修复前 |
| --- | --- | --- | --- |
| 静默改值 | JSON 字段或动态字段的嵌套值 `{T:"1"}`、`{"a":{T:"1"}}`、`[{T:"1.5e3"}]` | 原样 | 读回 `1`、`{"a":1}`、`[1500.0]`，过滤结果随之改变 |
| 误拒写入 | `{T:"abc"}`、`{T:1}`、`{T:"1","b":2}`、动态字段顶层键名 T | 接受 | code 6 |
| 集合卡死 | 动态字段的值 `{"b":2,T:"1"}` | 正常 | 写入通过；`$meta` 按 `sort_keys` 写出后 T 排到首位，此后 flush 永久失败、停服报 `storage shutdown failed`，重开时 WAL 恢复失败，集合无法加载 |
| manifest | 分区名 T；在名为 T 的字段上建索引；索引参数 `{"M":16,T:"1"}` | 正常 | 建成功，manifest 的整文件解析失败，重开后集合打不开 |
| schema 改写 | 属性只含 T 的集合 rename；属性是 T 加其他键的集合 rename 或 alter | 正常 | 前者 `schema.json` 被写成 `"properties": 1`，重开后集合打不开；后者改写报错。字段级的 `default_value`、`analyzer_params` 只含 T 时同样会在改写中变成数字 |
| 别名 | 别名 T 指向非数字集合名 | 正常 | 重开后 `aliases.json` 解析失败，该数据库几乎所有集合入口报 code 1 |
| 别名 / 数据库属性 | 别名 T 指向集合 `1`；数据库只有属性 `{T:"1"}` | 正常 | 整个文件被读成数字，读成空表；下一次写入用空表重写文件 |
| 索引参数 | `{T:"1"}`；`{"x":{T:"2"}}`；`{T:"1","M":16}` | 原样 | 分别被丢成 `{}`、改成 `{"x":2}`、建索引返回 `UNIMPLEMENTED` |
| 请求参数 | 检索参数 `radius: {T:"5"}` | `float(dict)` 报错 | 当作 5 执行范围检索 |
| `analyzer_params` | 任何位置含 T | 建表成功 | 建表返回 `UNIMPLEMENTED`（0029 曾登记为差异） |
| snapshot 元数据 | 3.x 的 snapshot 元数据以分区名、字段名为键，T 排在首位 | 正常 | truncate 读取元数据的 `id` 时整文件解析失败 |
| 3.x 目录 | 3.x 写出含上述值或名字的数据目录 | — | 对应集合或数据库打不开 |

集合属性的类型化读取不受影响（属性的键是 map 键，不经过首键判断），出错的是 rename 与 alter 的整文件改写。snapshot 元数据的类型化读取与三种 intent 文件不受影响（结构没有 `Value` 字段，名字只出现在 map 键或字符串数组里），truncate 对 snapshot 元数据的整文件读取受影响。序列化一侧不需要改：serde_json 只在 `serialize_struct` 的名字为 T 时特殊处理，那是 `Number` 自己的序列化。

## 修复

[json_text.rs](../../crates/core/src/json_text.rs) 是文本解析的唯一入口：

- 自己的 `Value` visitor 除 `visit_map` 外逐项同 serde_json 自带的实现。首键是 T、键不经 `visit_string`（经 `visit_str` / `visit_borrowed_str`）送来、值经 `visit_string` 送来，三者同时成立才是数字：serde_json 的数字键固定由 `visit_borrowed_str` 送出，数字文本固定由 `visit_string` 送出；用户写的值在文本里不会经 `visit_string`，在 `from_value` 里键经 `visit_string`，在 `&Value` 里值经 `visit_borrowed_str`，三种来源都能区分。
- 类型化结构的 5 个 `Value` 字段（schema 的 `default_value`、`analyzer_params`、function `params`，manifest 索引的 `build_params`、`search_params`）与 schema 的 `properties` 用 `deserialize_with` 接入，字段类型不变。
- 所有文本解析点改走它：JSON 字段、`$meta` 与 ARRAY 的写入，WAL / Parquet 的 JSON 与 `$meta` 读回，manifest 的整文件解析，schema 的 rename / alter 改写，别名与数据库属性文件，intent 与 snapshot 文件，`analyzer_params`，gRPC 的检索、查询、索引、`function_score` 与 hybrid 参数。
- [clippy.toml](../../clippy.toml) 禁止直接调用 `serde_json::from_slice` / `from_str` / `from_reader` / `from_value` 与 `Deserializer::from_*`，只有 `json_text` 例外。clippy 同时禁止 `Deserializer::new`，但拦不住三类情况：类型化结构里新增的裸 `Value` 字段、`.parse::<Value>()` / `Value::from_str`、`StreamDeserializer`。第一类由模块说明约束，并由每个持久化结构含 T 的单测兜底；`json_text` 的类型化入口在 debug 构建中拒绝顶层的裸 `Value`、`Map<String, Value>`、`Vec<Value>`、`Option<Value>`，嵌套的容器（如 `HashMap<String, Value>`）不在检查内。这些作为接受的残余风险，当前没有调用点。

0029 同步注销与 T 相关的登记：建表时含 T 的 `analyzer_params` 与 3.x 一样接受，3.x 写出含 T 的 `schema.json` 能原样读回；`NaN`、`Infinity`、溢出数与深度的登记保留。

## 已有数据

分界是改值发生在写入前还是写入后：写入的仍是对象、只是读回时被误认的，修复后能读回；写入前已被改成数字或丢弃的，无法恢复。

| 修复后能读回 | 无法恢复 |
| --- | --- |
| 因 `$meta` 卡死的集合（WAL 与 segment 里仍是对象） | 已被改成数字的 JSON 字段值与动态字段嵌套值（落盘的是改写后的文本） |
| 因分区名、建了索引的字段名或 `{"M":16,T:"1"}` 这类索引参数打不开的集合 | 建索引时被丢成 `{}` 或嵌套值被改成数字的索引参数 |
| 指向非数字集合名的别名 T | rename 后写成 `"properties": 1` 的集合：修复后打开仍报 code 1 storage error，可以 drop，同库其他集合不受影响；被修复前的 rename / alter 改写过、字段级对象已变成数字的 `schema.json`（包括 3.x 写出的目录） |
| 只有属性 T、尚未被再次修改的数据库属性文件 | 读成空表之后又被写入过的别名文件与数据库属性文件（T 已不在文件里） |
| 3.x 写出、未被修复前版本改写过的含 T 的目录 | |

修复版写出的含 T 的数据，3.x 能读；本实现修复前的版本仍然读不了，属于预期。

## 验证

- Rust 单测：`json_text` 与 serde_json 自带实现逐项比对（各种数字写法、宽整数、`1e999`、转义、重复键、深度 127 / 128、出错文案）；T 的各种形态（只含 T、T 后还有键、T 不在首位、转义写法的键、嵌套、数组内、T 套 T、`null` 与数组值）分别经 `from_slice`、`from_str`、`from_reader`、`from_value`、`&Value` 解析；两个 `deserialize_with` 函数；schema、manifest、别名、数据库属性文件、`$meta` 写出与读回、`analyzer_params` 经 `schema.json` 往返。
- 差分 [test_json_private_key.py](../../tests/differential/test_json_private_key.py)：JSON 字段与动态字段的上述形态（WAL 与 flush 两种状态，重开前后的读回与过滤）、分区名与字段名 T、带 T 属性的集合 rename 与 alter、别名 T（含重开后的其他集合入口）、数据库属性 T（含重开后修改再重开）、`analyzer_params` 与索引参数经原始 RPC 回显、检索参数。[test_text_match.py](../../tests/differential/test_text_match.py) 移出两条 T 用例。
- 互通 [test_json_private_key.py](../../tests/storage_interop/test_json_private_key.py)：本实现、3.x Arrow 15、3.x Arrow 25 各自写出同一组含 T 的值与名字（WAL 与 flush），三方读出一致；换另一方追加写入后三方仍一致。
- 反证（由负责人本地留存）：新增差分 6 条与互通 6 条在修复前的构建（`15ebbe8`）上全部失败，失败点分别是误拒写入、manifest 打不开、别名让其他集合入口报错、范围检索把对象当作 5。另在仓库外副本上逐项撤掉修复，16 项均有对应单测失败：`json_text` 的键与值判别（4 项）、schema 的 3 个字段与改写、manifest 的整文件解析与索引参数、别名、数据库属性、`$meta` 读回、JSON 写入、`analyzer_params`、truncate 读 snapshot 元数据。

### 结果

macOS arm64、CPython 3.12；两条 SDK 线为 pymilvus 3.0.2（仓库 `.venv`）与 2.6.17（安装同一份 wheel 的独立环境）；参照为 `1320d26`，Arrow 25 与 Arrow 15 各一套。命令见 [AGENTS.md](../../AGENTS.md)「常用命令」，差分与互通按 [分片工具](../../tests/sharding/README.md) 以 4 个分片运行。原始日志由负责人本地留存。

| 套件 | 结果 |
| --- | --- |
| `cargo fmt --check`、默认与 `--all-features` 的 workspace / all-targets Clippy | 通过 |
| Rust workspace / core 故障注入 | 575 / 717 passed |
| 端到端，SDK 3.0.2 / 2.6.17 | 各 575 passed |
| 差分，SDK 3.0.2 / 2.6.17 | 各 773 passed（group 387；新增 6 条，移出 2 条） |
| 互通（Arrow 15 / 25 双向） | 286 passed（新增 6 条） |
| 工具自检（packaging、platform、ci、sharding、release、benchmarks） | 956 passed |
| Unicode 表与 standard 分词真值 `--verify` | 通过 |

JSON 写入回归按 [专项 README](../../benchmarks/json-write/README.md) 的口径：修复前（`15ebbe8`）与修复后各一个 release wheel，独立环境，pymilvus 3.0.2，每批 5000 行、五批，顺序三轮并轮换先后；验证全部通过。中位数的轮间范围互相重叠，没有测出回归：

| 模式 | 修复前 | 修复后 |
| --- | --- | --- |
| raw | 94.6–95.8 ms | 93.6–95.5 ms |
| sdk | 164.8–168.8 ms | 166.1–167.9 ms |

schema 与 manifest 每次打开只解析一次、文件很小；`$meta` 读回与写入用同一个 visitor，因此不另做重开基准。本结果只说明没有回归，不作为性能提升的证据。
