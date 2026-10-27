# milvus-lite-rust

> 项目上下文及全局规则。
>
> 意识层硬规则写在这里；机制细节链到权威源（本仓 [README.md](README.md) 与后续 `docs/`，以及参照实现 [references/milvus-lite/](references/milvus-lite/) 的代码、测试与设计文档），本文不复制。
>
> 团队工作语言为中文。

## 项目定位

以官方 [milvus-lite](references/milvus-lite/)（纯 Python 重写的 v3 代，下称**参照实现**）为行为规范，用 **Rust** 重写一个更高效的 milvus-lite。目标是**让 `pymilvus` 用户的代码无需修改**：同样的 `MilvusClient("./demo.db")`、同样的 gRPC 语义与错误码，3.x 的存量数据目录也能直接打开（安装方式见 [pymilvus 接入点](#核心业务术语)）；服务端不再受 GIL 与 Python 解释器开销限制，更快、更省内存。

参照实现是**行为真值**，不是代码模板：对外可观察的行为（RPC 语义、结果与排序、距离数值、错误码与错误信息、过滤表达式语义）以它为准；内部结构按 Rust 惯用法重新设计，不逐文件翻译。

重构基线已定：**以 3.x 为基础，不以 2.5.1（C++）为基础**。3.x 的性能损失主要来自 Python 实现，架构内只有一处需要修正的设计；2.5.1 反而有冷启动全量重放、索引不落盘等架构级瓶颈；决策、数据与由此得出的实现约束见 [0001-重构基线选择](docs/decisions/0001-refactor-baseline.md)。

当前状态：仓库有 README / LICENSE（Apache-2.0）、设计决策（[docs/decisions/](docs/decisions/)）与基线基准脚本（[benchmarks/baseline/](benchmarks/baseline/)），以及 M1（[0004](docs/decisions/0004-m1-scope.md)）完成的工程骨架：三个 crate、进程内服务的生命周期、内存引擎、gRPC 翻译、差分测试与基准。M2 的实现与验收见 [0006](docs/decisions/0006-m2-scope.md)、[0007](docs/decisions/0007-m2-storage-engine.md) 和 [验收记录](docs/testing/m2-acceptance.md)；首发平台与发行物见 [0013](docs/decisions/0013-first-release-platform-and-packaging.md)：`0.1.0rc3`、`0.2.0rc1`、`0.3.0rc1` 与 `0.3.0rc2` 已通过 GitHub Release 预发布（[rc3 发布记录](docs/testing/first-release-acceptance.md#010rc3-发布)、[0.2.0rc1 发布记录](docs/testing/0.2.0rc1-acceptance.md#020rc1-发布)、[0.3.0rc1 发布记录](docs/testing/0.3.0rc1-acceptance.md#030rc1-发布)、[0.3.0rc2 发布记录](docs/testing/0.3.0rc2-acceptance.md#030rc2-发布)），未发布到 PyPI，未声明生产可用。新增 module、依赖或命令时同步更新本文的[架构地图](#架构地图跨文件心智)、[技术栈](#技术栈)与[常用命令](#常用命令)；这几节中标注「待定」的内容不得当作事实引用。

## 核心业务术语

| 术语 | 含义 |
| --- | --- |
| **参照实现** | [references/milvus-lite/](references/milvus-lite/)，被 `.gitignore` 忽略、不随本仓提交的本地 checkout。当前对照版本为 v3.2.1（`43d1257`），升级时同步改这里与差分测试的版本守卫（[tests/differential/conftest.py](tests/differential/conftest.py) 的 `REF_VERSION`）；记录对照结论时写明所依据的 commit（`git -C references/milvus-lite log -1`） |
| **pymilvus 接入点** | `pymilvus` 遇到以 `.db` 结尾的 URI 时，先检查父目录存在，再执行 `from milvus_lite.server_manager import server_manager_instance` 并调用 `start_and_get_uri(uri)`（3.x 的签名为 `start_and_get_uri(path, args=None)`）；返回 `None` 即报 “Open local milvus failed”，否则按返回的 `http://127.0.0.1:<port>` 走普通 Milvus gRPC（参照实现 [server_manager.py](references/milvus-lite/milvus_lite/server_manager.py)）。以上在 pymilvus 2.6.15 / 2.6.17 / 3.0.2 中逐行一致；pymilvus 升级时重新核对。**模块路径、实例名、方法签名、返回约定都是对外契约**。pymilvus 的 `milvus-lite` extra 依赖 PyPI 上的 `milvus-lite`（`>=2.4.0`，非 Windows），这个名字归官方所有，因此本项目以 `milvus-lite-rust` 发布：用户安装 `pymilvus` 与 `milvus-lite-rust`，不用 `pymilvus[milvus-lite]`。它与官方 `milvus-lite` 争用同一个 `milvus_lite` 模块与 `milvus-lite` 命令，**不得同时安装**：pip 不会阻止，后装者覆盖同名文件，卸载任一方都会弄坏另一方；pymilvus 导入失败时的提示还会把用户引向官方包。因此绑定层导入时须检测已安装的官方发行包 `milvus-lite` 并明确报错，README 的安装说明须先 `pip uninstall milvus-lite`。官方包后装覆盖本包的情形检测不到，只能靠文档说明 |
| **CLI** | 参照实现提供 `milvus-lite server --data-dir <dir> [--host] [--port] [--max-workers]` 与 `milvus-lite dump`（[cmdline.py](references/milvus-lite/milvus_lite/cmdline.py)）。本实现先提供 `server`，参数与之一致，按对外契约对待（`--max-workers` 控制 RPC 阻塞池，计算池与维护线程独立；见 [0012](docs/decisions/0012-cli-server.md)）；`dump` 后续再做（见[技术栈](#技术栈)） |
| **gRPC adapter** | Milvus 协议 → 引擎 API 的翻译层。**只翻译、不加能力**：引擎不支持的 RPC 返回 `UNIMPLEMENTED` + 明确信息，绝不静默成功（参照实现架构不变量 15） |
| **`_seq`** | 正常写入的操作全序号；覆盖 / 丢弃 / 去重按 `_seq` 判定。3.x 故障目录中的重复 `_seq` 属例外，按 [0007「数据结构」](docs/decisions/0007-m2-storage-engine.md#数据结构) 记录的来源与平局规则处理 |
| **WAL / MemTable / segment / delta / manifest** | LSM 式存储：写入先落 WAL 与 MemTable，flush 生成不可变 segment，删除写 delta（tombstone），compaction 合并并回收 tombstone；manifest 是唯一真值、原子更新；`data_dir/LOCK` 保证单进程独占。机制见参照实现 [wal-design.md](references/milvus-lite/docs/wal-design.md) 与 [modules.md](references/milvus-lite/docs/modules.md) |
| **index（向量 / 标量 / 稀疏）** | 与 segment 1:1 绑定、生命周期严格对齐、不可变；距离归一化在索引内部完成，上层看到的距离与 brute force 一致。见参照实现 [index-design.md](references/milvus-lite/docs/index-design.md) |
| **filter 表达式** | Milvus 风格标量 / JSON / 动态字段 / 数组 / `text_match` / 几何谓词。语法与语义见参照实现 [filter-design.md](references/milvus-lite/docs/filter-design.md) |
| **function chain** | BM25、text embedding、hybrid 合并、rerank（boost / decay / model）等检索函数链。见参照实现 [funcchain-design.md](references/milvus-lite/docs/funcchain-design.md) 与 [fts-design.md](references/milvus-lite/docs/fts-design.md) |
| **差分测试** | 同一输入分别喂给参照实现与本实现，断言对外结果一致（允许的数值误差须显式声明；距离类的默认误差见 [0004「决策」](docs/decisions/0004-m1-scope.md#决策)）。这是本项目「行为对齐」的主要证据，不是可选项 |

## 知识索引（动手前主动查阅，勿凭记忆）

参照实现的设计文档**部分滞后于代码**（例如 modules.md 仍描述已不存在的 `adapter/grpc/proto/` 与 `translators/expr.py`，而 proto 实际直接取自 `pymilvus.grpc_gen`）。文档与代码或测试冲突时，**以代码与测试为准**。

| 场景 | 权威源 |
| --- | --- |
| 对外功能面 / 已知限制 / 支持的类型、索引、度量 | 参照实现 [README.md](references/milvus-lite/README.md)（«Supported Features» / «Known Limitations»） |
| 逐文件架构 / **架构不变量全表** | 参照实现 [docs/modules.md](references/milvus-lite/docs/modules.md)（«Architecture Invariants»） |
| WAL 格式、flush、recovery、崩溃点 | 参照实现 [docs/wal-design.md](references/milvus-lite/docs/wal-design.md) |
| 向量 / 标量 / 稀疏索引 | 参照实现 [docs/index-design.md](references/milvus-lite/docs/index-design.md) |
| 过滤表达式 | 参照实现 [docs/filter-design.md](references/milvus-lite/docs/filter-design.md) |
| gRPC 兼容面与 RPC 映射 | 参照实现 [docs/grpc-adapter-design.md](references/milvus-lite/docs/grpc-adapter-design.md) |
| 全文检索 / BM25 | 参照实现 [docs/fts-design.md](references/milvus-lite/docs/fts-design.md) |
| function chain / rerank | 参照实现 [docs/funcchain-design.md](references/milvus-lite/docs/funcchain-design.md) |
| search / query iterator | 参照实现 [docs/search-iterator-design.md](references/milvus-lite/docs/search-iterator-design.md) |
| 近期兼容性改动的设计与计划 | 参照实现 [docs/superpowers/](references/milvus-lite/docs/superpowers/) |
| 参照实现的演进顺序（可借鉴的垂直切片划分） | 参照实现 [docs/roadmap.md](references/milvus-lite/docs/roadmap.md) |
| 某行为的真值 / 边界用例 | 参照实现 [tests/](references/milvus-lite/tests/)。`adapter/` 与 `compatibility/` 经 pymilvus 走 gRPC，是复用价值最高的一批，但它们用 `milvus_lite.adapter.grpc.server.start_server_in_thread` 在进程内起服务、部分用例还直接引用参照实现内部模块，**复用时须替换起服务的 fixture，不能直接指向本实现** |
| 上游 Milvus client 用例子集 | 参照实现 [tools/run_upstream_milvus_client_supported.sh](references/milvus-lite/tools/run_upstream_milvus_client_supported.sh)（依赖 Milvus 源码中的 `tests/python_client`，路径由 `MILVUS_CLIENT_TEST_DIR` 指定） |
| 为什么以 3.x 而非 2.5.1 为基础 / 两代架构对比 / 3.x 的耗时归属 / Rust 实现的性能约束 | [docs/decisions/0001-refactor-baseline.md](docs/decisions/0001-refactor-baseline.md) |
| 数据目录与 3.x 互通的范围 / 共享文件的约束 / 索引文件共存规则 / 跨实现验证矩阵 | [docs/decisions/0002-data-dir-compat.md](docs/decisions/0002-data-dir-compat.md) |
| 进程内服务的生命周期：同路径复用 / 进程退出时的停止与持久化 / fork 后的处理 / 与 3.x 在这些点上的差异 | [docs/decisions/0003-in-process-server-lifecycle.md](docs/decisions/0003-in-process-server-lifecycle.md) |
| 第一个里程碑（M1）做什么、不做什么 / 阶段性对外差异 / 验收标准 | [docs/decisions/0004-m1-scope.md](docs/decisions/0004-m1-scope.md) |
| 检索的线程模型：RPC 与计算池的分工、计算任务的约束、锁的边界、停服与 fork 下的计算池 | [docs/decisions/0005-search-thread-model.md](docs/decisions/0005-search-thread-model.md) |
| 第二个里程碑（M2）做什么、不做什么 / 3.x 存储层的实测事实与缺陷 / 与 3.x 的差异 / 持久化保证 / 验收 | [docs/decisions/0006-m2-scope.md](docs/decisions/0006-m2-scope.md) |
| M2 存储引擎的结构 / 取锁顺序 / 快照与有效位图 / WAL、flush、drop 与关闭的提交和失败协议 | [docs/decisions/0007-m2-storage-engine.md](docs/decisions/0007-m2-storage-engine.md) |
| 首发 compaction、正常关闭排空、GC 与独立已提交删除状态 | [0010](docs/decisions/0010-compaction-and-tombstone-gc.md)、[0011](docs/decisions/0011-committed-delete-state.md) |
| CLI server 参数、工作者、信号与停服错误（已实现，冻结范围复审通过） | [0012](docs/decisions/0012-cli-server.md) |
| 首发平台、PID 1、发行物验收与候选阶段决策（rc1 版本、补跑范围、RC 只发 GitHub Release、公开地址；rc3 已发布；2026-10-27 依 0021 修订，候选执行见 0021，Intel Mac 改为标准层） | [0013](docs/decisions/0013-first-release-platform-and-packaging.md) |
| rc2 之后的索引定义、DropIndex 与过滤表达式切片；删除意图和实施验收矩阵 | [0014](docs/decisions/0014-index-definitions-and-filter-expressions.md) |
| 标量 nullable、默认值与 ARRAY；向量 nullable 延后、真值矩阵与前后兼容（A、B 均评审通过并收口；新功能进入 0.2.0 开发线） | [0015](docs/decisions/0015-nullable-defaults-and-array.md)、[调查记录](docs/testing/0015-schema-investigation.md)、[A 验收](docs/testing/0015-scalar-nullable-acceptance.md)、[修复记录](docs/testing/0015-scalar-nullable-review-fixes.md)、[JSON 写入专项](docs/testing/0015-json-write-regression.md)、[B 验收](docs/testing/0015-array-acceptance.md) |
| Query iterator / range search（A、B 均已独立评审收口）、分区调查结论及冻结边界 | [0016](docs/decisions/0016-query-iteration-and-range-search.md) |
| 显式分区 A 实施与修复复审通过，已随 0.3.0rc1 交付：目录安全、按选择集可见性、Drop 保留全局删除；partition key 由下一行 B 合同与验收接续 | [0017](docs/decisions/0017-partitions.md)、[原始 RPC / Linux 边界与转存原型](docs/testing/0017-partition-design-investigation.md)、[可见性组件与资源诊断](docs/testing/0017-visibility-component.md)、[A 实施验收](docs/testing/0017-partition-acceptance.md)、[实施评审修复](docs/testing/0017-partition-review-fixes.md) |
| partition key B：设计、实施验收及独立产品评审通过，提交 d2729f8 已随 0.3.0rc1 交付；三项差异与证据单列 | [B 调查](docs/testing/0017-partition-key-investigation.md)、[B 合同](docs/decisions/0017-partitions.md#b-合同设计已通过)、[B 验收](docs/testing/0017-partition-key-acceptance.md)、[B 性能](docs/testing/0017-partition-key-performance.md) |
| 非 key 同批重复主键 Upsert：撤销累计合并差异，按请求前状态独立合并；实施及独立评审通过、单独提交，已随 0.3.0rc1 交付 | [修复记录](docs/testing/0017-non-key-upsert-fix.md) |
| 0017 整体跨平台开发验收（独立评审通过；FI Clippy 仅开启 core，最终候选须覆盖完整 FI 编译面；与最终发布矩阵分开） | [跨平台收口](docs/testing/0017-cross-platform-acceptance.md) |
| 0.3.0rc1 已不可变预发布（冻结 / tag 为 27aabf2；候选与发布后记录均独立复审通过，公开安装检查通过；完整三层 FI 与两 Linux 原生矩阵） | [候选及发布验收](docs/testing/0.3.0rc1-acceptance.md)、[范围与执行清单](docs/releases/next-candidate-plan.md)、[发行说明](docs/releases/0.3.0rc1.md) |
| 候选测试的并行编排与确定性分片：实测耗时依据、不损失测试效果的定义、P0 编排并行、P1 分片工具、门槛与试点（方案与分片工具均评审通过；三平台试点完成并处理其发现的参照端口复用与参照崩溃索引文件问题；负责人确认下一候选采用 P0 + P1） | [0019](docs/decisions/0019-test-sharding.md)、[试点验收](docs/testing/0019-pilot-acceptance.md) |
| 0015／0016 冻结提交的跨平台开发验收、平台证据边界与 SDK 临时端口复用的缓存隔离 | [跨平台验收](docs/testing/0015-0016-cross-platform-acceptance.md) |
| 0.2.0rc1 已发布：冻结范围、固定发行说明、候选复审及公开下载证据 | [0013](docs/decisions/0013-first-release-platform-and-packaging.md#首发候选矩阵)、[发行范围与说明](docs/releases/0.2.0rc1.md)、[候选验收](docs/testing/0.2.0rc1-acceptance.md) |
| 0018 JSON 写入优化：调查、合同、诊断工具修复及产品实施均已独立评审通过，J01–J08 已随 0.3.0rc2 交付；峰值资源问题继续单独跟进 | [合同](docs/decisions/0018-json-write-normalization.md)、[调查与证据](docs/testing/0018-json-write-investigation.md)、[实施验收](docs/testing/0018-json-write-acceptance.md)、[诊断工具](benchmarks/json-write/diagnostics/README.md) |
| 0.3.0rc2 已不可变预发布（冻结 / tag 为 df6fd30；仅纳入已审 0018 优化；候选独立复审通过，公开安装与发布门槛通过，发布后记录独立复审通过） | [候选及发布验收](docs/testing/0.3.0rc2-acceptance.md)、[候选方案](docs/releases/0.3.0rc2-candidate-plan.md)、[发行说明](docs/releases/0.3.0rc2.md)、[准备核验](docs/testing/0.3.0rc2-preparation.md) |
| 持续集成：快速层与对照层的范围、零跳过与汇总判定、必须通过检查与推送方式、不纳入 CI 的候选内容（方案评审通过；工作流已实施、试点完成，main 必须通过 ci-pass 与 compare-pass；试点中一次停服路径段错误根因未查明；valgrind 与停服窗口定向压力未发现问题，对当前产品源码与依赖谱系显式做发布级风险接受，任何再现即撤回，范围与停止条件见停服段错误调查） | [0020](docs/decisions/0020-continuous-integration.md)、[试点记录](docs/testing/0020-ci-pilot.md)、[工作流与固定依赖](.github/ci/README.md)、[停服段错误调查](docs/testing/shutdown-segfault-investigation.md) |
| 在 CI 中构建与验收发行物：前置继承、冻结身份、正式附件上的差分与互通、候选结果、版本消费与最后一道多主体来源证明、演练与平台调整（方案评审通过；工具与发行工作流已实施，第一次演练、不签发的失败注入、rc901 签发即消费验证与 rc903 七主体签发及公开下载核验（含拒绝测试）完成，容器 job 诊断与 macOS 多份崩溃报告的等待已修复；停服段错误为当前谱系显式风险接受；真实候选的 sign 一旦开始，禁止重跑该运行的任何 attempt，受控测试版本注入按 0021 第 20 条执行；2026-10-27 已最终采纳，并入 0022 签发前保全门闩，候选按 0021 执行，0013 本地流程只作诊断；README 与发行说明的支持矩阵随首个 CI 候选的版本切换修改） | [0021](docs/decisions/0021-release-build-in-ci.md)、[试点记录](docs/testing/0021-release-pilot.md) |
| 签发前的候选保全门闩：环境审批、本地保全与机器核对的保全回执、目录布局与崩溃恢复、失败语义与试点计划（实施与试点完成：门闩拒绝、错误回执与先保存后签发均在正式环境实测，测试版本 rc904 已签发、未发布；2026-10-27 已采纳并入 0021 第 3、13、19、20 条；两个环境长期保留；`test/rc904` 已删除） | [0022](docs/decisions/0022-pre-signing-preservation-gate.md)、[试点记录](docs/testing/0022-gate-pilot.md) |
| 验收与调查记录引用的证据分哪几类、谁能复核（仓库内可重跑 / 公开产物 / 本地留存），以及验收与调查记录的写法约定 | [证据的存放与复核范围](docs/testing/README.md) |
| 性能基线怎么测 / 口径 / 注意事项 | [benchmarks/baseline/README.md](benchmarks/baseline/README.md) |
| 代码审查 / 修复复审（冻结合同、完整调用链、同类全域清扫、分级与证据门槛、二三阶影响矩阵） | [code-review-rules](.agents/skills/code-review-rules/SKILL.md)（Claude Code 经 `.claude/skills` 软链读取同一份） |

## 架构地图（跨文件心智）

参照实现的分层（自外向内）：

```text
pymilvus ──server_manager──▶ adapter/grpc ──▶ db（多 collection 生命周期、LOCK）──▶ engine/Collection
                                                                                  ├─ storage/  WAL · MemTable · segment · delta · manifest · snapshot
                                                                                  ├─ search/   bitmap 管线 · filter 编译与求值 · dense/sparse 执行器
                                                                                  ├─ index/    向量索引 · 标量倒排 · 稀疏倒排
                                                                                  ├─ analyzer/ 分词 · BM25 term hash
                                                                                  ├─ function/ BM25 · embedding · hybrid · rerank
                                                                                  └─ schema/   类型系统 · 校验 · 持久化
```

本实现沿用这套**职责边界**，按 cargo workspace 拆成三个 crate（crate 内的 module 随实现补在这里）：

| crate | 对应参照实现 | 职责 |
| --- | --- | --- |
| `crates/core` | [db.py](references/milvus-lite/milvus_lite/db.py) 及其下各层 | 引擎：多 collection 生命周期、LOCK、存储、检索、索引、schema。不依赖 protobuf 与 Python。module：`lock_order`（五层锁与查表叶子锁的 debug 顺序检查）、`maintenance`（每 Db 单线程、按句柄计数的后台维护与正常关闭排空；见 0010）、`db`（数据库与 collection 生命周期、写入许可、显式分区 Create/Drop 的独占许可、按名字懒打开与失败判定缓存、关闭闸门）、`partition_key`（固定 16 桶的 MD5 路由与逻辑注册）、`collection::partition_key`（请求前合并、全桶预构造与自动 ID 碰撞保护）、`collection`（不可变列式读快照、稳定块引用与源映射、小块合并、主键读取准备、写入校验、get / count / 检索、load 状态）、`compute`（服务的计算池，rayon-core 只在这里调用）、`schema`、`data`（列式数据、仅实际空值时物化的列有效位与私有的按类型主键表 `PkMap`；`data::array` 保存 ARRAY offsets、有类型子列、父/元素有效位和 Arrow 转换口径）、`index`（索引定义、度量与统一的 3.x 侧车文件名解析）、`projection`（output_fields 展开）、`filter`（`parser` / `temporal` / `unicode` 负责完整语法与诊断，`eval` 保留 S1 绑定与列式快路径，`tree` 负责扩展节点绑定与静态路线，`columns` 负责布尔树的列式求值，`dynamic` 负责整源动态列绑定，`python` / `python_json` 实现共享的 Python 值规则与平面 JSON 解码，主键 in 保留快路径；范围见 0006 / 0009 / 0014）、`flat`（暴力检索的距离）、`data_dir`（LOCK 与数据库 staging 残留清理）、`storage::wal`（3.x Arrow IPC stream WAL 的读写与已校验前缀恢复）、`storage::manifest` / `schema_file`（3.x JSON 格式的解析与编码）、`storage::parquet_file` / `arrow_data`（3.x Parquet 与列式值的转换）、`storage::memtable` / `visibility`（位次、tombstone、跨数据源 `_seq`、有效位图及读快照；`visibility::partitions` 用真实 PkMap、本地位图及稀疏遮挡支持先选分区再判版本）、`storage::partition_paths`（安全名称、物理目录身份及仅内存的分区 ID）、`storage::partition_drop`（保留全局 delta 的同次 manifest 提交准备）、`storage::fs_commit` / `flush`（schema、manifest 与冻结代的提交和 WAL 清理）、`storage::snapshot`（读取 3.x snapshot 元数据、钉住集合与各自的 manifest 副本）、`storage::index_files`（索引目录链、侧车匹配与钉住定义预检）、`storage::index_intent`（私有删除意图、跨重开封锁与安全清理）、`storage::index_platform`（Unix 删除权限、可信启动来源与缓存边界）、`storage::recovery`（加载已提交 segment、delta 与 WAL 并建位图）、`storage::compaction`（单分区已提交文件的有界选择、物理行映射、临时输出与全分区阈值的 delta GC）、`storage::runtime`（按逻辑批次冻结及一次提交的多分区输出、WAL 代、同步、独立已提交删除表与故障状态）、`error`（与 3.x 异常一一对应的错误及 Python repr 格式化）、`pyjson`（按 Python `json.dumps` 的格式输出 JSON） |
| `crates/grpc` | [adapter/grpc/](references/milvus-lite/milvus_lite/adapter/grpc/) | proto 代码生成（protox）、协议翻译与错误映射；作为服务宿主，负责服务实例（runtime、计算池、引擎、端口）的启动与停止，计算池交给引擎。module：`service`（各 RPC 的实现）、`schema` / `insert` / `search` / `query` / `index` / `output`（请求解码与响应编码）、`status`（错误映射）、`pyint`（按 Python `int()` / `float()` 解析参数；默认值描述单独拒绝 protobuf 整数越界并复用固定 Unicode 数字分类）、`guard`（未实现的 RPC 与超限请求）、`server`（服务宿主）。RPC 的处理体在 tokio 的阻塞线程池中执行，检索在其中经计算池并行（[0005](docs/decisions/0005-search-thread-model.md)） |
| `crates/py` | [server_manager.py](references/milvus-lite/milvus_lite/server_manager.py) | PyO3 扩展，加上 Python 包 `milvus_lite`：`server_manager.py` 维护路径表，导入时检测官方发行包。`cmdline.py` / `__main__.py` 解析独立 CLI、信号与关闭；合同见 [0012](docs/decisions/0012-cli-server.md)。根 [pyproject.toml](pyproject.toml) 是唯一发行配置；[build.rs](crates/py/build.rs) 校验目标平台与解释器，门槛见 [0013](docs/decisions/0013-first-release-platform-and-packaging.md) |

服务实例的所有权、同路径复用、进程退出与 fork 的处理见 [0003](docs/decisions/0003-in-process-server-lifecycle.md)。跨文件的隐含约定：

- **已实现 RPC 的唯一清单**在 [crates/grpc/build.rs](crates/grpc/build.rs) 的 `IMPLEMENTED_METHODS`：只为清单中的方法生成 trait 方法（编译器保证都有实现），其余 RPC 与其他 service 由 [guard.rs](crates/grpc/src/guard.rs) 统一返回 `UNIMPLEMENTED`。新增 RPC 只改这份清单。
- **引擎的每次使用都经过关闭闸门**：[service.rs](crates/grpc/src/service.rs) 中每个使用引擎的 RPC 处理体先调用 `Db::enter`，[Server](crates/grpc/src/server.rs) 停止时 `Db::close` 等所有在途任务结束才释放 LOCK（0003「进程退出」）。绕过它直接使用引擎，停止时 LOCK 可能先于引擎任务释放。
- **持久化写入只经 Db 入口**：insert、Flush、CreateIndex / DropIndex、CreatePartition / DropPartition 与 create/drop 从 [db.rs](crates/core/src/db.rs) 取得写入许可；持久化 collection 的直接 `Collection::insert` / `create_index` 会拒绝调用。完整的锁序、读快照与按名字并行打开见 [0007](docs/decisions/0007-m2-storage-engine.md#取锁顺序)。
- **检索的计算任务不拿锁、不阻塞**：[compute.rs](crates/core/src/compute.rs) 的 `Compute::for_each` 只能从计算池之外的线程（RPC 所在的阻塞线程）调用，叶子任务只写自己独占的数据；rayon-core 其余提交任务的接口由 [clippy.toml](clippy.toml) 禁止。违反时会死锁或优先级反转（[0005](docs/decisions/0005-search-thread-model.md)）。
- **fork 安全由两层共同保证**：[server_manager.py](crates/py/python/milvus_lite/server_manager.py) 按进程号跳过继承来的服务；Rust 的 [Server](crates/grpc/src/server.rs) 在非创建者进程中拒绝 `stop`、析构时直接泄漏。

已确定的边界约束：

- **Python 绑定层只做胶水**：提供 [pymilvus 接入点](#核心业务术语)要求的模块与入口，启动 Rust gRPC 服务并返回 URI；不在 Python 侧实现任何引擎逻辑。
- **gRPC 层只翻译**：除服务宿主职责（runtime、线程池、端口、启停）外，只做协议解码 / 编码与错误映射，业务语义全部在引擎层。
- **引擎层与协议无关**：不引用 protobuf 生成类型，便于独立测试与基准。

## 技术栈

方向已定，具体依赖的版本在首次引入时补在这里，并说明理由；确切版本以 [Cargo.toml](Cargo.toml) 与 [Cargo.lock](Cargo.lock) 为准。

- **依赖边界：不引入 C++、CMake 或系统库依赖**（不绑定 FAISS 等），允许经 `cc` crate 编译、不需要额外安装任何东西的纯 C 代码，优先选纯 Rust。官方 3.x 删除了整套 C++ 构建（见 [0001](docs/decisions/0001-refactor-baseline.md)），本项目同样避免这类构建与分发负担：wheel 与 sdist 必须能在各目标平台只靠 cargo 构建。引入依赖时用 `cargo tree --workspace -e all` 核对完整依赖图（`-e build` 只列出根 crate 的直接构建依赖，看不到依赖自身的构建脚本）。
- **gRPC / protobuf**：用 tonic + prost。prost-build 默认依赖系统 `protoc`（C++ 程序），因此改用纯 Rust 的 protox 解析 proto，再经 `compile_fds` 交给 prost / tonic 生成代码。proto 源取与 pymilvus 3.0.2 对应的 milvus-proto commit（3.0 的 `MilvusService` 是 2.6 的超集，2.6 的消息与字段在 3.0 中都保留），随仓库固定（离线构建与 sdist 都需要），引入时记录 commit 并核对其许可、注明来源；生成物不入库。参照实现运行期直接用 `pymilvus.grpc_gen`，CI 也不钉 pymilvus 版本，Rust 侧没有这条捷径。背景：3.0 在 2.6 的 117 个 RPC 之上只增不改地新增 27 个（snapshot、namespace、`AlterCollectionSchema` 等），另新增独立的 `ClientTelemetryService`（4 个 RPC）；3.0 客户端的 `add_collection_field` 会先调 `AlterCollectionSchema`，失败再回退旧 RPC。
- **支持的 pymilvus 版本**：2.6.x 与 3.0.x 两条线都支持，兼容测试各跑一个最新版。
- **Python 打包**：PyO3 + maturin，产出 abi3 wheel，Python 下限与 3.x 一致（`abi3-py310`）。abi3 wheel 不能在 free-threaded 版 CPython 上加载，这一点作为已知限制。服务端在 Python 进程内的后台线程上运行多线程 tokio runtime，CPU 密集的检索交给专用线程池，调用时释放 GIL，形态与 3.x 相同（进程内服务、返回 `http://127.0.0.1:<port>`）。同路径复用、atexit 时的停止顺序与持久化语义、fork 后子进程中 runtime 的处理见 [0003](docs/decisions/0003-in-process-server-lifecycle.md)；WAL 与 flush 的 fsync 时机及持久化保证见 [0006](docs/decisions/0006-m2-scope.md) 第 19 项。PyPI 元数据与 README 都要声明非官方、与 Milvus / Zilliz 无隶属关系。发行包名为 `milvus-lite-rust`，包内提供 `milvus_lite` 模块（见 [pymilvus 接入点](#核心业务术语)）。首发平台与证据层级按 [0013](docs/decisions/0013-first-release-platform-and-packaging.md)：Linux / macOS，Windows 延后；首发 Python 包在构建期明确拒绝 Windows 目标，sdist 边界已由负责人确认；内部候选执行已按 0013 / [0021](docs/decisions/0021-release-build-in-ci.md) 将 Intel Mac 视为标准层，面向安装用户的支持声明随首个 CI 候选的版本切换同步。参照 3.x 的 CI 仍包括 Windows，这一首发差异见 [0008](docs/decisions/0008-first-release-scope.md)。
- **持久化格式**：数据文件与 3.x **双向互通**，索引文件各自独立。范围、约束与验证方式见 [0002](docs/decisions/0002-data-dir-compat.md)。
- **向量索引**：第一阶段只做 SIMD 优化的暴力检索（FLAT），同时作为召回与距离口径的正确性基准；HNSW 等 ANN 索引后期补齐，优先纯 Rust（依赖边界见上），届时再定具体方案。第一阶段对各类索引定义的处理见 [0002 «结论»](docs/decisions/0002-data-dir-compat.md#结论)，当前源码的定义创建、DropIndex 与删除意图协议见 [0014](docs/decisions/0014-index-definitions-and-filter-expressions.md)。
- **分词与 BM25**：在全文检索阶段定具体实现，但因为数据目录互通，必须与 3.x 逐位一致（范围与约束见 [0002](docs/decisions/0002-data-dir-compat.md#共享状态的语义约束) 约束 8）。
- **CLI**：先提供 `milvus-lite server`，参数与 3.x 一致（见 [CLI](#核心业务术语)）；`dump` 后续再做。
- **已引入的依赖**：
  - md5 0.8.1：partition key 的 3.x 固定哈希；纯 Rust、无传递依赖、无构建脚本，Apache-2.0 / MIT。只用于路由，不用于安全校验。
  - tonic 0.14（含 tonic-prost、tonic-prost-build）、prost / prost-types 0.14、protox 0.9：gRPC 与 proto 代码生成，全部纯 Rust（见上）。
  - tokio 1：tonic 的运行时，每个服务实例独占一个 runtime（[0003](docs/decisions/0003-in-process-server-lifecycle.md)）。
  - pyo3 0.29（`abi3-py310`）：Python 绑定。
  - thiserror 2：错误类型。tempfile 3：仅用于测试。
  - libc 0.2：索引变更的 Unix 权限预检、文件系统类型与内核启动标识查询；FFI 局限在 `storage::index_platform`，不引入额外系统库，合同见 [0014](docs/decisions/0014-index-definitions-and-filter-expressions.md#实施设计)。
  - serde_json 1：JSON 字段与动态字段的校验与存储，以及 gRPC 层解析 search_params、索引参数等 JSON 文本；开启 `float_roundtrip`、`preserve_order` 与 `arbitrary_precision`，JSON 字段保留键序与宽整数，`$meta` 由 [pyjson.rs](crates/core/src/pyjson.rs) 显式按嵌套 key 排序后输出。
  - parking_lot 0.12：collection 表、每个 collection 的读写锁，以及 M2 已接通的数据库写入闸门、目录互斥、名字锁、失败判定缓存、flush 锁与关闭等待。它的锁没有中毒语义；debug 锁序检查与当前别名维护的串行边界见 [0007](docs/decisions/0007-m2-storage-engine.md#取锁顺序)。
  - rayon-core 1.13：服务的计算池（[0005](docs/decisions/0005-search-thread-model.md)）。只用线程池部分，不引入 rayon 的并行迭代器；依赖只有 crossbeam-deque / epoch / utils，都是纯 Rust，构建脚本只做 cfg 探测（`cargo tree --workspace -e all` 核对）。可以使用的接口由 [clippy.toml](clippy.toml) 的 `disallowed-methods` 限定。
  - arrow-array / arrow-schema / arrow-ipc 与 parquet 60.0（Parquet 只开 `arrow`、`snap`）、serde 1：M2 与 3.x 的 Arrow IPC WAL、Parquet 和 JSON 持久化格式。macOS 上经 chrono 的 `clock` feature 链接系统自带的 CoreFoundation 框架，无需额外安装。版本及依赖图评估见 [0006 第 24 项](docs/decisions/0006-m2-scope.md#五工程)；实际引入后用完整 `cargo tree --workspace -e all` 核对。
  - LOCK 用 std 的 `File::try_lock`：它在 Unix 上用 flock、在 Windows 上用覆盖第 0 字节的 LockFileEx，满足 [0002](docs/decisions/0002-data-dir-compat.md) 约束 10，因此不引入 fs2 一类依赖，`rust-version` 相应定为 1.89。
  - 构建工具 maturin 1.15，用 uv 装在开发 venv 中，不是运行依赖。根 `pyproject.toml` 声明 PEP 517 后端与发行元数据；`rust-toolchain.toml` 固定开发/候选构建 Rust 1.98.1，sdist 用户使用自己的 Rust（MSRV 1.89）。`pyo3-build-config` 0.29.2 为 py crate 的直接构建依赖，用于拒绝 free-threaded、非 CPython 与 Windows 目标。

## 常用命令

本实现（在仓库根目录执行；基准对比随 M1 第 5 步补充）：

| 任务 | 命令 |
| --- | --- |
| 建开发环境 | `uv venv -p 3.12 .venv && uv pip install -p .venv/bin/python maturin==1.15.0 pymilvus pytest` |
| 构建并以可编辑方式安装扩展（改 Rust 后重跑） | `.venv/bin/maturin develop -m crates/py/Cargo.toml --release` |
| 格式化 | `cargo fmt --all` |
| lint | `cargo clippy --workspace --all-targets -- -D warnings` |
| Rust 单测 | `cargo test --workspace` |
| M2 持久化点故障注入单测（feature 默认关闭） | `cargo test -p milvus-lite-core --features fault-injection` |
| 子进程与关闭重开测试的隔离 / 默认并行 30 轮复验 | [进程内测试隔离记录与命令](docs/testing/process-test-isolation.md) |
| S1 语法真值核验（参照实现须可编辑安装、HEAD 为 `43d1257`） | `<3.x 的 venv>/bin/python tests/differential/generate_filter_parse_fixtures.py --verify` |
| 顶层动态过滤组件真值核验（真实 3.x / `43d1257`） | `<3.x 的 venv>/bin/python tests/differential/generate_dynamic_filter_fixtures.py --verify` |
| 完整布尔树三路线组件真值核验（真实 3.x / `43d1257`；Arrow 15/25 各执行） | `<3.x 的 venv>/bin/python tests/differential/generate_filter_tree_fixtures.py --verify`；重新生成须用 Arrow 25 并指定 `--arrow15-python <Arrow15 的参照解释器>` |
| 分区 / Query iterator 参照真值（仓库外生成、--verify 不覆盖） | 环境与命令见 [调查工具 README](tests/compatibility_audit/README.md#分区与-query-iterator-调查) |
| 0017 分区目录边界补测与 delta 转存数据原型（非产品验收） | [环境、文件系统变体与命令](tests/compatibility_audit/README.md#0017-分区边界补测)；工具自检：`python tests/compatibility_audit/selfcheck.py` |
| 0017 B key 单/多桶写入、Upsert、读取与峰值专项（仓库外新路径；顺序轮换） | [专项合同与命令](benchmarks/partition-key/README.md) |
| 0017 B partition key 参照真值（显式执行；非产品验收） | [环境与命令](tests/compatibility_audit/README.md#0017-b-partition-key-参照调查)；工具自检仍用 `python tests/compatibility_audit/selfcheck.py` |
| 0017 实际可见性组件 / 列式引擎资源 | [真实组件、26/35 组压力与冷重开命令](benchmarks/partition-visibility/README.md#实际组件与列式引擎)；`target/release/examples/partition_visibility_probe verify-core tests/compatibility_audit/fixtures/partition_visibility_v321.json` |
| 0017 产品分区目录边界、0.2.0rc1 回退、默认路径及分区读取性能 | [产品调查命令](tests/compatibility_audit/README.md#0017-产品边界与回退)、[读取/写入专项](benchmarks/partitions/README.md)；常规 Python 与差分套件仍分别运行，不能合为同一 pytest 进程 |
| 分区可见性隔离组件对照（不接入产品；含冻结代、快照与稀疏关系资源诊断） | `cargo test -p milvus-lite-core --example partition_visibility_probe`；构建、真值核验及 RSS 采样见 [组件 README](benchmarks/partition-visibility/README.md) |
| ISO 时间组件直接模糊对照（不经服务；参照环境为 CPython 3.12，报告放仓库外） | `cargo build -p milvus-lite-core --example filter_parse_probe`，再 `<3.x 的 venv>/bin/python tests/differential/fuzz_temporal.py --cases 200000 --seed 29 --output <work_dir>/temporal.json` |
| S1 Unicode 分类表核验（CPython 3.12 / Unicode 15.0） | `<CPython 3.12>/bin/python tests/differential/generate_filter_unicode.py --verify` |
| 端到端测试（真实 pymilvus；含 [tests/python/adapter/](tests/python/adapter/) 中复制自参照实现的用例，逐条状态见其 INVENTORY.md） | `.venv/bin/python -m pytest tests/python` |
| 与 3.x 的差分测试（3.x 的 venv 按 [互通 README](tests/storage_interop/README.md) 建 Arrow 25 参照环境；`benchmarks/baseline/setup.sh` 建出的环境不固定 pyarrow，arm64 上会被 S1 用例拒绝；不设环境变量时报错，不跳过；上一行的命令不包含它） | `MILVUS_LITE_3X_PYTHON=<3.x 的 venv>/bin/python .venv/bin/python -m pytest tests/differential` |
| ARRAY 元素转换、schema/default 与三路线组件真值（真实 3.x / `43d1257`，Arrow 15/25 各执行） | `<参照解释器> tests/differential/generate_array_fixtures.py --verify`；重新生成用 Arrow 25，随后以 Arrow 15 核验 |
| 固定兼容样本统计、nullable 反事实与 schema 真值（离线、显式执行，不进入默认测试） | [统计工具与命令](tests/compatibility_audit/README.md)；`<参照解释器> tests/compatibility_audit/schema_truth.py --reference references/milvus-lite --kind filters --output <仓库外报告> --verify-nullable-filter-fixture` 核验空值过滤组件 |
| M2 存储文件双向互通矩阵（环境准备见 [tests/storage_interop/README.md](tests/storage_interop/README.md)；Arrow 15/25 均须安装固定参照引擎，Arrow 15 固定 pyarrow 15.0.0 / NumPy 1.26.4） | `MILVUS_LITE_3X_PYTHON=<v3>/bin/python MILVUS_LITE_ARROW15_PYTHON=<arrow15>/bin/python MILVUS_LITE_ARROW25_PYTHON=<v3>/bin/python .venv/bin/python -m pytest tests/storage_interop` |
| 独立 CLI server | `.venv/bin/milvus-lite server --data-dir <data_dir> --host 127.0.0.1 --port 19530 --max-workers 10`；也可 `python -m milvus_lite server ...`，合同见 0012 |
| 构建 wheel / sdist | `.venv/bin/maturin build --release -m crates/py/Cargo.toml` / `.venv/bin/maturin sdist -m crates/py/Cargo.toml` |
| 核对依赖图中没有 C++ 构建与 protoc | `cargo tree --workspace -e all` |
| 发行物与安装验收（本地诊断，候选以发行工作流为准；根配置；工作目录在仓库外） | `.venv/bin/python tests/packaging/run_acceptance.py --work-dir <work_dir> --constraints <constraints.txt> --suite --suite-expect 473`；约束文件与完整步骤见 [打包验收](tests/packaging/README.md) |
| 发行物/平台脚本自检 | `.venv/bin/python -m pytest tests/packaging tests/platform` |
| CI 纯文档分类与汇总判定自检 | `.venv/bin/python -m pytest tests/ci` |
| 发行工作流：演练 / 正式运行（0021；从以冻结提交为最新提交的已推送分支触发，正式运行引用同一提交含 PID 1 的成功演练） | `gh workflow run release.yml --repo <owner/repo> --ref <冻结分支> -f commit=<完整 SHA> -f version=<版本> -f dry_run=true`；正式运行改为 `-f dry_run=false -f pid1_dry_run=<演练运行 ID>.<attempt>` |
| 签发环境配置只读核对（0021 第 19 条；改动 `release-signing` 后、下一次正式运行前） | `gh api repos/<owner/repo>/environments/release-signing`；`gh api repos/<owner/repo>/environments/dry-run-signing` |
| 版本台账校验（0021 第 3 条） | `python3 tests/release/ledger.py docs/releases/consumed-versions.json` |
| 发布准备：核对待发布的七个附件（0021 第 20 条；用候选冻结提交中的工具） | `python3 tests/release/release_set.py --dir <附件目录> --version <版本> --repo <owner/repo> --prepare` |
| 发布后公开下载核验（0021 第 20 条；只在 `main` 上触发） | `gh workflow run public-check.yml --repo <owner/repo> --ref main -f version=<版本>` |
| 签发前保全：本机保存候选证据集并输出审批行 / 复核已保存目录（0022；在冻结提交的干净 worktree 中运行，演练用 `--mode dry-run`） | `python3 <worktree>/tests/release/preserve.py save --mode release --repo <owner/repo> --run <运行 ID>.<attempt> --version <版本> --root <本地留存根目录>`；`python3 tests/release/preserve.py verify <保存目录>` |
| 候选测试的确定性分片（plan / run / verify，合同见 0019）及自检 | 用法见 [tests/sharding/README.md](tests/sharding/README.md)；自检：`.venv/bin/python -m pytest tests/sharding` |
| Linux PID 1、命名卷 LOCK 与强杀恢复 | `.venv/bin/python tests/platform/linux_pid1_probe.py --formal-wheel <linux-wheel> --fi-wheel <linux-fi-wheel> --image <image-id> --constraints .github/ci/requirements/sdk30.txt --output <work_dir>`；见 [平台流程](tests/platform/README.md) |
| Windows 准备检查（无运行证据） | `pwsh -NoProfile -File tests/platform/windows_run.ps1 -WorkDirectory <work_dir>`（PowerShell ≥7.2）；见 [平台流程](tests/platform/README.md) |

pymilvus 两条线都要测。2.6.x 线用单独的 venv 安装构建出的 wheel，再运行同一套用例（在仓库外的目录执行，避免从源码目录导入）：

```bash
.venv/bin/maturin build --release -m crates/py/Cargo.toml -o <work_dir>/wheels
uv venv -p 3.10 <work_dir>/v26 && uv pip install -p <work_dir>/v26/bin/python "pymilvus>=2.6,<2.7" pytest <work_dir>/wheels/milvus_lite_rust-*.whl
cd <work_dir> && v26/bin/python -m pytest <repo>/tests/python
cd <work_dir> && MILVUS_LITE_3X_PYTHON=<3.x 的 venv>/bin/python v26/bin/python -m pytest <repo>/tests/differential
```

性能基线（2.5.1、3.x 与本仓的 Rust 实现对照，工作目录须在仓库外；完整步骤见 [benchmarks/baseline/README.md](benchmarks/baseline/README.md)）：

| 任务 | 命令 |
| --- | --- |
| 建两套对照环境 | `benchmarks/baseline/setup.sh <work_dir>` |
| 跑一组配置 | `benchmarks/baseline/run.sh <work_dir> <N> <dim> <out.jsonl> v2:FLAT:inproc v3:HNSW:server ...`（须顺序执行） |
| 汇总 | `<work_dir>/v3/bin/python benchmarks/baseline/summarize.py <work_dir>/<out.jsonl>` |
| 并发吞吐 | `<work_dir>/{v2,v3}/bin/python benchmarks/baseline/concurrency.py <work_dir>/r_<env>_<idx>_inproc_<N>_<dim>.db <dim> 1,2,4,8`（用与库对应版本的 python） |
| 3.x 耗时归属 | `<work_dir>/v3/bin/python benchmarks/baseline/profile_v3.py <3.x 的库>` |
| 为本仓的 Rust 实现建第三套环境 | `benchmarks/baseline/setup_rs.sh <work_dir>` |
| M1 口径三方对照（每轮轮换顺序，跑 3 轮后汇总；完整命令见基准 README） | `benchmarks/baseline/run.sh <work_dir> <N> 128 m1.jsonl rs:FLAT:inproc:m1 v2:FLAT:inproc:m1 v3:FLAT:inproc:m1` |
| M2 持久化三方对照（轮换三轮；RSS 另跑 `memory`，完整口径见基准 README） | `benchmarks/baseline/run.sh <work_dir> <N> 128 m2.jsonl rs:FLAT:inproc:m2 v2:FLAT:inproc:m2 v3:FLAT:inproc:m2` |
| 基准脚本自检（失败传播、吞吐分母与多轮汇总） | `.venv/bin/python -m pytest tests/benchmarks` |
| 后台合并退出等待与删除重负载条件验收 | [benchmarks/compaction/README.md](benchmarks/compaction/README.md)；正式 wheel 与隔离 fault-injection wheel 分开记录 |
| 合并受控等待 / 提交点进程退出（隔离测试 wheel） | `<fi>/bin/python -m pytest tests/compaction`；需由 py → grpc → core 显式启用 fault-injection，正式 wheel 不含测试入口 |
| 独立删除表组件 RSS 诊断 | `cargo build -p milvus-lite-core --release --example committed_delete_memory`；过程与边界见合并验收记录 |
| 恢复内存与进程冷启动专项（已有健康库、独立服务；口径见 [专项 README](benchmarks/recovery/README.md)） | `<实现的 python> benchmarks/recovery/run.py <existing.db> --rows 1000000 --dim 128 --round 1 --build <artifact-identity>` |
| 批量 FLAT 检索专项（真值准备与计时分开；口径见 [专项 README](benchmarks/batch-search/README.md)） | `<实现的 python> benchmarks/batch-search/run.py --database <existing.db> --truth <truth_dir> --rows 1000000 --dim 128 --round 1 --build <artifact-identity>` |
| S1 过滤性能专项（优化前后与两代参照，顺序三轮、结果逐次校验） | 环境、数据与命令见 [benchmarks/s1/README.md](benchmarks/s1/README.md) |
| Range search / V1 与普通检索回归四方对照 | 环境、数据与轮换见 [专项 README](benchmarks/range-search/README.md) |
| Query iterator 首批、全量分页及普通读取回归 | 环境、轮换与命令见 [专项 README](benchmarks/query-iterator/README.md) |
| JSON 写入回归专项（预序列化 raw RPC / SDK 端到端，逐批校验） | 环境、轮换与命令见 [benchmarks/json-write/README.md](benchmarks/json-write/README.md) |
| JSON 原组件与分进程资源诊断（独立冻结副本；不修改产品） | [构建、运行与口径](benchmarks/json-write/diagnostics/README.md)；`python benchmarks/json-write/diagnostics/selfcheck.py` |
| Linux 容器的 LOCK 与 strace 验收 | 环境、非 root 执行与命令见 [tests/platform/README.md](tests/platform/README.md) |
| 已安装产物的 nullable／ARRAY／iterator／range 联合冒烟 | `<installed-wheel-python> tests/platform/feature_smoke.py --repo <source-root> --work-dir <new-outside-directory>`；见 [平台流程](tests/platform/README.md) |

参照实现侧（生成真值时用，在 [references/milvus-lite/](references/milvus-lite/) 内执行）：

| 任务 | 命令 |
| --- | --- |
| 安装开发环境 | `make dev`，再 `.venv/bin/pip install pymilvus`（dev extra 不含 pymilvus；缺了它 `tests/adapter` 会被 `importorskip` **整目录静默跳过**，`tests/compatibility` 则直接导入失败） |
| 常规功能测试 | `make test` |
| gRPC / pymilvus 兼容测试 | `make test-compat` |
| 参照实现自带的性能基准（不作为规则 7 的基线） | `make benchmark` |
| 起本地 gRPC 服务 | `make serve`（端口 19530，数据目录 `./data`） |

**改了什么就跑什么**：改存储 / 恢复 → 崩溃注入与恢复测试；改索引或距离计算 → 与 brute force 及参照实现的差分测试；改 filter → 解析与求值的差分测试；改 gRPC 层 → [tests/python/adapter/](tests/python/adapter/) 中复制自参照实现的用例与 [tests/differential/](tests/differential/) 的差分测试（`compatibility` 套件涉及的功能进入范围后再以本实现为后端运行）；改 Python 绑定 → 用真实 `pymilvus` 走 `MilvusClient("./x.db")` 的端到端测试；声称性能提升 → 用 [benchmarks/baseline/](benchmarks/baseline/) 同机同数据集对照 2.5.1 与 3.x。

## 核心心智

你是强大的 Agentic 模型，不是代码生成器。规则之外的灰色地带按此判断：

- **先调查再动手**：实现任何对外行为前，先读参照实现的对应代码、测试与设计文档；遇到“奇怪”的边界处理先 `git -C references/milvus-lite log -p <file>` 看历史，那通常在修一个你不知道的兼容性 bug。
- **对齐行为，不翻译代码**：Python 里为绕开 GIL、pyarrow 或 FAISS 限制而写的结构，在 Rust 里未必需要；但它们守住的不变量必须保留。动手前分清「这是语义」还是「这是 Python 的权宜之计」。
- **第一性原理 / 能减不加**：动手前问“真正要解的问题是什么、最小可行解是什么？”优先从根源修、让现有代码承担，最后才新增；禁止预留“未来扩展点”。
- **不硬做**：发现需求前提错误（参照实现本身不支持 / 违反架构不变量 / 与 pymilvus 契约冲突）立即反馈，不做到一半再说。
- **性能是目标，正确性是前提**：没有差分测试护住的优化不算完成；性能结论必须有可复现的基准数据，不凭直觉宣称“更快”。
- **高风险跨域改动做组件级收敛**：并发（读写锁、flush / compaction 与查询并发）、持久化（WAL、manifest 原子性、崩溃恢复）、`unsafe` 与 FFI 类改动的正确性是组合性的。同类问题反复冒出时停止逐点补丁，圈定组件、冻结不变量与接受的残余风险、按失败模式组合建测试矩阵。

## 全局强制规则（每次改动都要应用）

1. **语言**：对话回复跟随用户当次需求描述的主体语言，不被个别英文专名带偏。落库产物（commit message、文档、必要注释）固定中文；代码标识符英文。
2. **参照实现是行为真值**：对外可观察行为（RPC 语义、结果与排序、距离数值、错误码与信息、过滤语义）与参照实现不一致即为 bug，除非该差异已在本仓文档中**显式登记**（差异内容、理由、影响的用例）。不得为了“更合理”悄悄改变对外行为。
3. **继承参照实现的架构不变量**（[modules.md «Architecture Invariants»](references/milvus-lite/docs/modules.md)），按性质分两类，另有一条由 0001 引入的 Rust 侧约束：
   - **正确性不变量，必须保持**：1–5（`_seq` 全序、MemTable 互清 seq-aware、tombstone GC 规则、文件不可变、manifest 唯一真值且原子更新）、8（单进程独占 data_dir）、9（schema 不可变）、11–15（索引与 segment 1:1 且不可变、未 load 禁止检索、距离归一化在索引内、adapter 只翻译）。持久化保证不得弱于 10（WAL 关闭前 fsync）。
   - **Python 时代的实现选择，可重新设计**：6（同步 flush）、7（单 collection 单写者、无内部锁；3.x 代码已有 collection 级的锁，文档滞后）是 Rust 版提升性能的主要空间，但改动前须先写设计说明（锁 / 快照边界、与正确性不变量的关系、测试矩阵）并与用户确认，不在实现里顺手突破；16（proto 生成物入库）已与参照实现代码不符，proto 方案见[技术栈](#技术栈)。
   - **Rust 侧的性能约束**：查询路径不得做 O(总行数) 的逐行有效性判定、计算保持列式、检索可并行，细节与理由见 [0001 «对 Rust 实现的约束»](docs/decisions/0001-refactor-baseline.md#对-rust-实现的约束)；涉及并发的方案仍按上一条先写设计说明。
4. **测试是 ship 门槛**：改条件分支 / 状态流转 / 协议或持久化格式 / 修过的 bug 必须补或更新测试；提交前按“改了什么就跑什么”跑齐对应套件并全绿；不用 `#[ignore]`、skip 或放宽断言掩盖失败，也要确认套件没有因缺依赖被静默跳过。涉及对外行为的改动必须有差分测试或复用参照实现测试作证。测试不得依赖真实付费外部服务（OpenAI embedding、Cohere rerank 等）。
5. **根源修复，禁止伪修复**：不吞错误（不 `let _ =` 丢弃 `Result`、不用 `unwrap_or_default` 兜底真实缺失）、不改宽断言迁就坏代码、不用 retry / fallback 掩盖错误原因。持久化路径尤其警惕：I/O 与校验失败必须向上传递，不得“尽力而为”地继续写。
6. **`unsafe` 与 panic 纪律**：`unsafe` 块必须附 `// SAFETY:` 说明成立条件并有覆盖测试；库代码与服务路径不得因可预期的错误输入 panic，`unwrap` / `expect` 只用于能证明不会失败的地方。跨 FFI 边界的 panic 必须被捕获并转成错误。
7. **性能声明要有证据**：宣称提升必须附同机、同数据集、同参数下的基准对比，基线**同时包括 2.5.1 与 3.x**，用 [benchmarks/baseline/](benchmarks/baseline/) 测量并遵守其口径（2.5.1 没有 HNSW，按 FLAT 对 FLAT 及与召回无关的操作比较），迭代优化另附与上一版本的对比；目标见 [0001 «对 Rust 实现的约束»](docs/decisions/0001-refactor-baseline.md#对-rust-实现的约束)；不得通过降低召回、跳过校验或弱化持久化保证换取数字。
8. **持久化格式变更要显式**：WAL / segment / manifest / 索引文件格式的任何变更都要记录格式版本与兼容性影响，并说明旧数据目录如何处理；不得静默产生新旧实现都读不了的数据目录。
9. **参照实现只读**：[references/milvus-lite/](references/milvus-lite/) 不修改、不提交（已被 `.gitignore` 忽略）；需要改它来辅助对照时先告诉用户。从中借用的代码或测试数据须遵守其 Apache-2.0 许可并注明来源。
10. **文档与代码同源，禁止双源**：机制细节的权威源是本仓 README / docs 与参照实现的代码和设计文档；本文及代码他处需提及时**链接**，不拷贝。改代码同步更新被影响的文档（对外行为差异、CLI 参数、持久化格式、支持矩阵）。
11. **变更范围最小化**：不扩展用户给的任务边界；顺手能改的小问题报告而不动手；一次发现 N 个独立问题列出来让用户挑。
12. **探索性问题给 2-3 句 + tradeoff**：用户问“该怎么做 / 你觉得呢”时先给推荐方向与主要取舍，等确认再实现。[技术栈](#技术栈)中尚未细化的选型（如 HNSW 实现、分词器）尤其如此。
13. **不轻易 commit + 线性历史**：默认不主动 commit，仅用户明确要求时提交；一次 commit 一件事。拉远端用 `git pull --rebase` 或 `--ff-only`，不产生 merge commit；不 amend / force-push 已 push 的 commit。
14. **面向开源的落库卫生**：项目将对外开放。不提交 `.db` 数据目录、基准原始输出、API Key、`.env`；文档不引用个人绝对路径或内部链接；commit message、PR 描述、代码注释与文档中**不留 AI 协作痕迹**（不加 `Co-Authored-By` AI 署名、“Generated with …” 尾注或「由 AI 生成」类措辞）。发现机密已入库立即报告用户，不自行改写历史。
15. **路径用 markdown link 而非 backtick**：引用文件 / 目录 / 行号写 `[name](path#L42)`，IDE 里可点击。
16. **代码注释只写“为什么”**：只写非平凡的 why（隐藏约束 / 不变量 / 与参照实现对齐的原因 / 上游 bug workaround / 跨模块隐含契约），禁止“当前任务”、“为修复 X”类指向性注释。

### 规则冲突时的优先级

**正确性 > 行为对齐 > 性能 > 范围最小化 > 文档同步**：

- 参照实现本身有 bug → 不照抄错误行为也不静默修正，报告用户决定，并按规则 2 登记差异。
- 性能优化与对齐冲突（如结果顺序、浮点误差）→ 对齐优先；确需取舍时先与用户确认允许的误差边界。
- 改代码时发现被影响文档过时 → 文档同步（规则 10）是规则 11 的合理例外，算同一逻辑单元。
- 修 bug 时发现测试本身错了 → 规则 5「禁止改宽断言」的例外是**显式论证测试过时**（commit body 写清），不能仅因 fail 就静默放宽。
- commit 时发现**完全无关**的测试 fail → 报告用户决定，不自行扩展范围。

## 提交规范

- Conventional Commits：`feat:` / `fix:` / `perf:` / `refactor:` / `docs:` / `test:` / `chore:`，可带 scope（如 `feat(storage):`、`fix(grpc):`、`perf(index):`），中文描述。
- **Commit body 写 why 不写 what**（非平凡改动必写）：含验收依据（跑了哪些测试 / 差分对照 / 基准数据）、替代方案为何不选、跨文件影响、与参照实现的行为差异（如有）。**禁止使用** HEREDOC + `Co-Authored-By` 标注 AI 协作；PR 描述同理，不附任何 AI 生成标记（见规则 14）。
