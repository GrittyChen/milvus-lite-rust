# 非 key 重复主键 Upsert 合并修复

- 状态：实施验收及独立评审通过；独立于已评审通过并本地提交的 0017 B。
- 基线：`d2729f8`；参照 v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`。
- 不切版本、不推送、不发布；本修复独立评审通过后单独本地提交，不与 B 合并提交。

## 合同修订与最小改动

参照 `Collection.upsert` 在调用 `insert(merged)` 前，对每个原始行单独 `_get_raw(pk)`；因此所有行读取同一请求前的旧值。原非 key 实现的 `pending` 会把前一行候选作为后一行旧值，和这条语义不符。0006 曾主动登记此差异，本轮按已采纳的后续工作方向撤销，而非把旧版本描述为从未登记的缺陷。0007 同步明确请求前状态。

初始 `{id:1,x:0,y:0}`，partial Upsert `[{id:1,x:1},{id:1,y:2}]`，结果由旧版本的 `x=1,y=2` 改为参照的 `x=0,y=2`。新主键时，前行新增的动态字段同样不得流入后行。普通 Upsert 也使用此规则；SDK 是否补字段仍按真实请求列集合解释。

仅移除非 key 合并过程的候选 `pending` 及不再需要的回存转换；原始行号、JSON 检查、默认值/null/ARRAY 转换顺序、全批校验后 WAL、序号分配、写许可和快照保持既有路径。key 专用入口不改，存储编码不变。

## 验证设计与反证

- 修复前 N5：新增真实差分分别在新 PK 普通 Upsert、已有 PK partial Upsert 上稳定失败。前者 Rust 保留 `x=1`，参照无 `x`；后者 Rust 为 `x=1`，参照 `x=0`。两份原始失败日志单独保留。
- 新增差分覆盖两类 PK、active/segment/reopen/显式分区、普通/partial、存在/新 PK，逐项比较两端结果及 Flush/重开；替换旧差分中有意断言两端不同的条目。
- 引擎测试覆盖两类 PK、存在/新 PK、活跃/已提交旧行，完整主键响应、动态合并、向量保留、旧快照及 Flush/重开。
- 回归门槛：fmt/Clippy、workspace/FI、两条 SDK Python 与差分；JSON 错误优先级、nullable/ARRAY、分区和并发部分 Upsert 由对应套件覆盖。此处不改共享格式，互通按相关范围验证。

## 结果与证据边界

本轮 macOS arm64、CPython 3.12.14；两条 SDK 分别为 3.0.2 / 2.6.17，均安装同一个新 wheel。开发版本字段仍为 `0.2.0rc1`，不能作为构建身份：

| 输入 / 产物 | SHA256 |
| --- | --- |
| 127 项 crate / 构建输入清单 | `24d5186f1067735424769ac53ffd0289dcdf7b6ef39ffa125771396e208e5006` |
| 本轮 macOS wheel | `6ca974ec3c54cba0c50e03c542e0544d2f6cca740ef51942cfd830bad622bf4c` |
| 两条 SDK 实际原生库 | `2b3fddf9b341d895c3fea627d4ba8ad16b31d6f07673d4e9007448b5b392a8ce` |

| 本轮实跑 | 结果 |
| --- | --- |
| Rust workspace / core FI | 357 / 455 passed |
| Python 全套 / SDK 3.0.2、2.6.17 | 各 473 passed |
| 完整差分 / SDK 3.0.2、2.6.17 | 各 408 passed（含新增 32 项） |
| Arrow 15/25 双向互通 | 211 passed |
| fmt、默认及 FI workspace/all-targets Clippy、diff 检查 | 通过 |

Rust 按每个顶层 harness 的最终结果计数，不重复计入故障子进程输出。为控制磁盘，验证设置 `CARGO_INCREMENTAL=0`、`CARGO_PROFILE_DEV_DEBUG=0`、`CARGO_PROFILE_TEST_DEBUG=0`；关闭增量缓存与调试符号，不改变测试选择或断言。Python、差分和互通各自在独立进程执行；本轮没有运行性能计时，不以并发测试耗时作性能结论。

不把 B 的测试数量写成本修复后的结果。Linux / Intel Mac、固定统计、回退和正式性能本轮没有新增实测；不改格式及旧版本能力门槛，最终发布矩阵仍按 0013。

## 调用链与影响检查

检索 `upsert_partition_from_db|upsert_partition_with_pk_type|partial_update|insert_key_batch`，核对 gRPC Upsert → Db mutate 写许可/状态锁 → 非 key 行合并 → ARRAY 全批转换 → 原 insert/WAL 路径；默认与显式分区共享该入口，key 提前分派至 B 的独立实现。检索 `pending.get|pending.insert|previous.is_some` 确认候选状态仅从此路径移除，没有保留第二个累计出口；其它读取源重映射中的 `previous` 不属于合并候选，未修改。

每行仍先校验原始 JSON 证据及业务字段，再规范化标量；全部行业务校验后才进行 ARRAY 元素转换，最终统一调用原写入路径。移除的 `values` 回存仅供旧 `pending` 使用，不影响同一行的其它字段。现有并发部分 Upsert、原行号错误优先级、失败无写入、旧快照、WAL/重开、nullable/ARRAY 和分区套件分别承担回归证据；不把单个动态字段探针视作覆盖全部这些合同。

## 交付

本轮 8 个文件，产品只修改 `collection.rs`，其余为回归与合同/验收记录；B 已单独本地提交 `d2729f8`。独立评审通过后仅更新收口状态并单独本地提交，不推送、不打 tag、不发布。原始失败、最终日志、构建指纹和完整源码快照保存在仓库外，和 N5 原证据分开。

## 独立评审收口

2026-10-19 独立评审通过当前全部 8 个变更文件，未发现阻断或需要本轮处理的新增问题。独立实跑 macOS Rust/FI 357/455，两条 SDK Python 各 473，SDK 3.0 完整差分 408，SDK 2.6 非 key/key 专项 68，Arrow 15/25 全套互通 211；fmt、默认/FI Clippy、diff 检查通过。修复前 N5 构建运行新增 active 的 8 组组合，均被字段差异断言拦住。

127 项产品输入、366 项源码快照、wheel/native 及 27 项实施归档证据身份匹配。SDK 2.6 完整 408 项为核验归档；Linux/Intel Mac、最终发布矩阵、固定统计、回退和性能无本轮新增实测。独立评审日志另存仓库外，不追写原实施证据包；评审记录 SHA256 为 `10c3ed0e533c47b06cbfe6c0aa1d76cbac5c983cdfd1c724406b397582fd0471`。本次通过属于本修复的实施收口，后续整体跨平台验收和发布仍分别执行。
