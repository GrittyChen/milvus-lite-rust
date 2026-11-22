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

仓库结构：README / LICENSE（Apache-2.0）、文档（[docs/](docs/)：设计决策、验收与调查记录、发行文档，入口为 [docs/README.md](docs/README.md)）、代码（[crates/](crates/)）、测试（[tests/](tests/)）与基准（[benchmarks/](benchmarks/)）。各切片的进展以对应决策文档开头的「状态」与验收记录为准，各版本以[版本台账](docs/releases/consumed-versions.json)与候选及发布验收记录为准，本文不记录进展。已发布的版本都是 GitHub Release 预发布，未发布到 PyPI，未声明生产可用。新增设计决策时在 [docs/README.md](docs/README.md) 的索引加一行；新增 module、依赖或常用命令时同步更新本文的[架构地图](#架构地图跨文件心智)、[技术栈](#技术栈)与[常用命令](#常用命令)；专项命令写进所在目录的 README。

## 核心业务术语

| 术语 | 含义 |
| --- | --- |
| **参照实现** | [references/milvus-lite/](references/milvus-lite/)，被 `.gitignore` 忽略、不随本仓提交的本地 checkout。当前对照版本为 v3.2.2 之后的 `1320d26`（升级结论见 [0027](docs/decisions/0027-reference-3.2.2-upgrade.md)；此前的决策与验收记录中出现的 `43d1257` 记录当时的基准），升级时同步改这里与差分测试的版本守卫（[tests/differential/conftest.py](tests/differential/conftest.py) 与 [tests/dump/diff/conftest.py](tests/dump/diff/conftest.py) 的 `REF_VERSION`）；记录对照结论时写明所依据的 commit（`git -C references/milvus-lite log -1`） |
| **pymilvus 接入点** | `pymilvus` 遇到以 `.db` 结尾的 URI 时，先检查父目录存在，再执行 `from milvus_lite.server_manager import server_manager_instance` 并调用 `start_and_get_uri(uri)`（3.x 的签名为 `start_and_get_uri(path, args=None)`）；返回 `None` 即报 “Open local milvus failed”，否则按返回的 `http://127.0.0.1:<port>` 走普通 Milvus gRPC（参照实现 [server_manager.py](references/milvus-lite/milvus_lite/server_manager.py)）。以上在 pymilvus 2.6.15 / 2.6.17 / 2.6.18 / 3.0.2 / 3.0.3 中逐行一致；pymilvus 升级时重新核对。**模块路径、实例名、方法签名、返回约定都是对外契约**。pymilvus 的 `milvus-lite` extra 依赖 PyPI 上的 `milvus-lite`（`>=2.4.0`，非 Windows），这个名字归官方所有，因此本项目以 `milvus-lite-rust` 发布：用户安装 `pymilvus` 与 `milvus-lite-rust`，不用 `pymilvus[milvus-lite]`。它与官方 `milvus-lite` 争用同一个 `milvus_lite` 模块与 `milvus-lite` 命令，**不得同时安装**：pip 不会阻止，后装者覆盖同名文件，卸载任一方都会弄坏另一方；pymilvus 导入失败时的提示还会把用户引向官方包。因此绑定层导入时须检测已安装的官方发行包 `milvus-lite` 并明确报错，README 的安装说明须先 `pip uninstall milvus-lite`。官方包后装覆盖本包的情形检测不到，只能靠文档说明 |
| **CLI** | 参照实现提供 `milvus-lite server --data-dir <dir> [--host] [--port] [--max-workers]` 与 `milvus-lite dump`（[cmdline.py](references/milvus-lite/milvus_lite/cmdline.py)）。本实现先提供 `server`，参数与之一致，按对外契约对待（`--max-workers` 控制 RPC 阻塞池，计算池与维护线程独立；见 [0012](docs/decisions/0012-cli-server.md)）；`dump` 见 [0037](docs/decisions/0037-dump.md)：只经 pymilvus 公共接口导出 BulkWriter JSON，`pymilvus[bulk_writer]` 由用户自装；它依赖的 `MilvusClient` 接口、BulkWriter 的校验与私有名 `LocalBulkWriter._flush` / `_working_thread` 在 pymilvus 升级时重新核对 |
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
| 专题设计：WAL 与 flush / recovery、索引、过滤表达式、gRPC 兼容面与 RPC 映射、全文检索与 BM25、function chain 与 rerank、search / query iterator | 参照实现 docs/ 下对应的 [wal](references/milvus-lite/docs/wal-design.md)、[index](references/milvus-lite/docs/index-design.md)、[filter](references/milvus-lite/docs/filter-design.md)、[grpc-adapter](references/milvus-lite/docs/grpc-adapter-design.md)、[fts](references/milvus-lite/docs/fts-design.md)、[funcchain](references/milvus-lite/docs/funcchain-design.md)、[search-iterator](references/milvus-lite/docs/search-iterator-design.md) 各一份 `*-design.md` |
| 近期兼容性改动的设计与计划 | 参照实现 [docs/superpowers/](references/milvus-lite/docs/superpowers/) |
| 参照实现的演进顺序（可借鉴的垂直切片划分） | 参照实现 [docs/roadmap.md](references/milvus-lite/docs/roadmap.md) |
| 某行为的真值 / 边界用例 | 参照实现 [tests/](references/milvus-lite/tests/)。`adapter/` 与 `compatibility/` 经 pymilvus 走 gRPC，是复用价值最高的一批，但它们用 `milvus_lite.adapter.grpc.server.start_server_in_thread` 在进程内起服务、部分用例还直接引用参照实现内部模块，**复用时须替换起服务的 fixture，不能直接指向本实现** |
| 上游 Milvus client 用例子集 | 参照实现 [tools/run_upstream_milvus_client_supported.sh](references/milvus-lite/tools/run_upstream_milvus_client_supported.sh)（依赖 Milvus 源码中的 `tests/python_client`，路径由 `MILVUS_CLIENT_TEST_DIR` 指定） |
| 本仓的设计决策（0001 起按编号）、验收与调查记录、各版本的发行说明与候选方案、发行流程命令 | [docs/README.md](docs/README.md)：按编号列出每份决策「查什么时看」及其验收记录，先看它再打开具体文档，不必枚举目录 |
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

本实现沿用这套**职责边界**，按 cargo workspace 拆成三个 crate。每个 module 的职责写在它自己的 `//!` 文档注释里，下表只列分组；新增 module 时写好 `//!` 注释，并把名字补进对应分组。

| crate | 对应参照实现 | 职责与 module |
| --- | --- | --- |
| `crates/core` | [db.py](references/milvus-lite/milvus_lite/db.py) 及其下各层 | 引擎，不依赖 protobuf 与 Python。生命周期与并发：`db`（数据库与 collection 生命周期、写入许可、关闭闸门）、`lock_order`、`maintenance`、`compute`、`data_dir`、`alias`、`database_properties`。集合与数据：`collection`、`data`、`schema`、`projection`、`partition_key`、`flush_budget`。存储：`storage::*`（WAL、manifest、Parquet、memtable 与可见性、提交与 flush、恢复、compaction、snapshot、分区目录、索引文件与各类私有意图）。检索：`filter`、`flat`、`hnsw`、`index`、`sparse_index`、`rerank`。类型与文本：`timestamptz`、`timezone`、`geometry`、`analyzer`、`jieba`。通用：`error`（与 3.x 异常一一对应）、`pyjson`、`json_text`、`allocator` |
| `crates/grpc` | [adapter/grpc/](references/milvus-lite/milvus_lite/adapter/grpc/) | proto 代码生成（protox）、协议翻译与错误映射；作为服务宿主，负责服务实例（runtime、计算池、引擎、端口）的启停。module：`service`、`server`、`guard`、`status`、请求解码与响应编码（`schema` / `insert` / `search` / `query` / `index` / `output` / `function_score` / `hybrid` / `function_chain`）、`pyint`。RPC 的处理体在 tokio 的阻塞线程池中执行，检索在其中经计算池并行（[0005](docs/decisions/0005-search-thread-model.md)） |
| `crates/py` | [server_manager.py](references/milvus-lite/milvus_lite/server_manager.py) | PyO3 扩展，加上 Python 包 `milvus_lite`：`server_manager.py` 维护路径表，导入时检测官方发行包；`cmdline.py` / `__main__.py` 是独立 CLI（[0012](docs/decisions/0012-cli-server.md)）。根 [pyproject.toml](pyproject.toml) 是唯一发行配置；[build.rs](crates/py/build.rs) 校验目标平台与解释器（[0013](docs/decisions/0013-first-release-platform-and-packaging.md)） |

服务实例的所有权、同路径复用、进程退出与 fork 的处理见 [0003](docs/decisions/0003-in-process-server-lifecycle.md)。跨文件的隐含约定：

- **已实现 RPC 的唯一清单**在 [crates/grpc/build.rs](crates/grpc/build.rs) 的 `IMPLEMENTED_METHODS`：只为清单中的方法生成 trait 方法（编译器保证都有实现），其余 RPC 与其他 service 由 [guard.rs](crates/grpc/src/guard.rs) 统一返回 `UNIMPLEMENTED`。新增 RPC 只改这份清单。
- **引擎的每次使用都经过关闭闸门**：[service.rs](crates/grpc/src/service.rs) 中每个使用引擎的 RPC 处理体先调用 `Db::enter`，[Server](crates/grpc/src/server.rs) 停止时 `Db::close` 等所有在途任务结束才释放 LOCK（0003「进程退出」）。绕过它直接使用引擎，停止时 LOCK 可能先于引擎任务释放。
- **持久化写入只经 Db 入口**：insert、Flush、CreateIndex / DropIndex、CreatePartition / DropPartition 与 create/drop 从 [db.rs](crates/core/src/db.rs) 取得写入许可；持久化 collection 的直接 `Collection::insert` / `create_index` 会拒绝调用。完整的锁序、读快照与按名字并行打开见 [0007](docs/decisions/0007-m2-storage-engine.md#取锁顺序)；debug 构建由 `lock_order` 检查锁序。
- **检索的计算任务不拿锁、不阻塞**：[compute.rs](crates/core/src/compute.rs) 的 `Compute::for_each` 只能从计算池之外的线程（RPC 所在的阻塞线程）调用，叶子任务只写自己独占的数据；rayon-core 只在 `compute` 中调用，其余提交任务的接口由 [clippy.toml](clippy.toml) 禁止。违反时会死锁或优先级反转（[0005](docs/decisions/0005-search-thread-model.md)）。
- **JSON 文本解析只经 [json_text.rs](crates/core/src/json_text.rs)**：serde_json 开了 `arbitrary_precision`，自带的 `Value` 反序列化会把首键为 `$serde_json::private::Number` 的用户对象当作数字；类型化结构的 `Value` 字段用它的 `deserialize_with`，clippy 禁止直接调用 `serde_json::from_*`（[修复记录](docs/testing/json-private-number-key-fix.md)）。
- **fork 安全由两层共同保证**：[server_manager.py](crates/py/python/milvus_lite/server_manager.py) 按进程号跳过继承来的服务；Rust 的 [Server](crates/grpc/src/server.rs) 在非创建者进程中拒绝 `stop`、析构时直接泄漏。

已确定的边界约束：

- **Python 绑定层只做胶水**：提供 [pymilvus 接入点](#核心业务术语)要求的模块与入口，启动 Rust gRPC 服务并返回 URI；不在 Python 侧实现任何引擎逻辑。
- **gRPC 层只翻译**：除服务宿主职责（runtime、线程池、端口、启停）外，只做协议解码 / 编码与错误映射，业务语义全部在引擎层。
- **引擎层与协议无关**：不引用 protobuf 生成类型，便于独立测试与基准。

## 技术栈

方向已定，具体依赖的版本在首次引入时补在这里，并说明理由；确切版本以 [Cargo.toml](Cargo.toml) 与 [Cargo.lock](Cargo.lock) 为准。

- **依赖边界：不引入 C++、CMake 或系统库依赖**（不绑定 FAISS 等），允许经 `cc` crate 编译、不需要额外安装任何东西的纯 C 代码，优先选纯 Rust。官方 3.x 删除了整套 C++ 构建（见 [0001](docs/decisions/0001-refactor-baseline.md)），本项目同样避免这类构建与分发负担：wheel 与 sdist 必须能在各目标平台只靠 cargo 构建。引入依赖时用 `cargo tree --workspace -e all` 核对完整依赖图（`-e build` 只列出根 crate 的直接构建依赖，看不到依赖自身的构建脚本）。
- **gRPC / protobuf**：用 tonic + prost。prost-build 默认依赖系统 `protoc`（C++ 程序），因此改用纯 Rust 的 protox 解析 proto，再经 `compile_fds` 交给 prost / tonic 生成代码。proto 源取与 pymilvus 3.0.2 对应的 milvus-proto commit（3.0 的 `MilvusService` 是 2.6 的超集，2.6 的消息与字段在 3.0 中都保留；3.0.3 / 2.6.18 的新增未跟进，理由见 proto README），随仓库固定（离线构建与 sdist 都需要），来源与许可见 [crates/grpc/proto/README.md](crates/grpc/proto/README.md)；生成物不入库。3.0 客户端的 `add_collection_field` 会先调 `AlterCollectionSchema`，失败再回退旧 RPC。
- **支持的 pymilvus 版本**：2.6.x 与 3.0.x 两条线都支持，兼容测试各跑一个最新版。
- **Python 打包**：PyO3 + maturin 产出 abi3 wheel（`abi3-py310`，与 3.x 的下限一致；不能在 free-threaded CPython 上加载，作为已知限制）。服务以进程内后台线程上的 tokio runtime 运行，形态与 3.x 相同（[0003](docs/decisions/0003-in-process-server-lifecycle.md)）。PyPI 元数据与 README 都要声明非官方、与 Milvus / Zilliz 无隶属关系。首发 Linux / macOS，构建期拒绝 Windows（[0013](docs/decisions/0013-first-release-platform-and-packaging.md)，与 3.x 的差异见 [0008](docs/decisions/0008-first-release-scope.md)）。
- **持久化格式**：数据文件与 3.x **双向互通**，索引文件各自独立。范围、约束与验证方式见 [0002](docs/decisions/0002-data-dir-compat.md)。
- **向量索引**：检索为 SIMD 优化的暴力检索（FLAT），同时是召回与距离口径的正确性基准；HNSW 自行以纯 Rust 实现、不引入依赖，复用 FLAT 的距离核，按段在后台建图并写本实现私有的图文件（[0041](docs/decisions/0041-hnsw.md)）；其余 ANN 定义仍按 FLAT 执行。各类索引定义的处理见 [0002 «结论»](docs/decisions/0002-data-dir-compat.md#结论) 与 [0014](docs/decisions/0014-index-definitions-and-filter-expressions.md)。
- **分词与 BM25**：因为数据目录互通，必须与 3.x 逐位一致（[0002](docs/decisions/0002-data-dir-compat.md#共享状态的语义约束) 约束 8）。standard 分词（[0029](docs/decisions/0029-text-match.md)）与 BM25（[0031](docs/decisions/0031-bm25.md)）自行实现，不引入依赖；jieba 以 jieba-rs 为依赖加包装层，约束 8 对它放宽为 [0036](docs/decisions/0036-jieba.md) 的口径。
- **已引入的依赖**（引入理由与评估见括号中的决策；标「版本写死」的升级前须重跑对应的对照）：
  - tonic / prost 0.14 与 protox 0.9（gRPC 与 proto 代码生成）；tokio 1（每个服务实例独占一个 runtime，[0003](docs/decisions/0003-in-process-server-lifecycle.md)）；thiserror 2；md5 0.8.1（partition key 的 3.x 固定哈希，只用于路由）；tempfile 3（仅测试）。
  - pyo3 0.29（`abi3-py310`）：Python 绑定；`pyo3-build-config` 0.29.2 为 py crate 的直接构建依赖，用于拒绝 free-threaded、非 CPython 与 Windows 目标。
  - rayon-core 1.13：服务的计算池，只用线程池部分，可用接口由 [clippy.toml](clippy.toml) 的 `disallowed-methods` 限定（[0005](docs/decisions/0005-search-thread-model.md)）。
  - parking_lot 0.12：collection 表与各类锁，没有中毒语义（[0007](docs/decisions/0007-m2-storage-engine.md#取锁顺序)）。
  - arrow-array / arrow-schema / arrow-ipc 与 parquet 60.0（Parquet 只开 `arrow`、`snap`）、serde 1：3.x 的 Arrow IPC WAL、Parquet 和 JSON 持久化格式；macOS 上经 chrono 链接系统自带的 CoreFoundation，无需额外安装（[0006 第 24 项](docs/decisions/0006-m2-scope.md#五工程)）。
  - serde_json 1：开启 `float_roundtrip`、`preserve_order` 与 `arbitrary_precision`，JSON 字段保留键序与宽整数，`$meta` 由 [pyjson.rs](crates/core/src/pyjson.rs) 按嵌套 key 排序后输出；文本解析的约定见[架构地图](#架构地图跨文件心智)。
  - libc 0.2：索引变更的 Unix 权限预检、文件系统类型与内核启动标识查询，以及 Linux glibc 上维护轮次后的 `malloc_trim`；FFI 局限在 `storage::index_platform`（[0014](docs/decisions/0014-index-definitions-and-filter-expressions.md#实施设计)）与 `allocator`（[分配器保留内存](docs/testing/allocator-retention.md)）。
  - num-bigint 0.5.1（只开 `std`）：过滤 Python 路线算术的精确整数，已随 arrow-buffer 在依赖图中（[0032](docs/decisions/0032-arithmetic-filters.md)）。
  - jiff 0.2.38（只开 `std` 与 `tzdb-bundle-always`）：TIMESTAMPTZ 的时区换算，内置 tzdata；版本写死，升级须重跑[时区 fixture](tests/differential/generate_timezone_fixtures.py)（[0025](docs/decisions/0025-timestamptz-and-timezone.md)）。
  - jieba-rs 0.11.0（`default-features = false`，不开会引入 zstd-sys 的 `default-dict`）：jieba 分词，词典与全精度模型由本仓提供（[crates/core/data/jieba/](crates/core/data/jieba/)）；版本写死，升级须重跑 jieba 的对照工具（[0036](docs/decisions/0036-jieba.md)）。
  - LOCK 用 std 的 `File::try_lock`（Unix flock、Windows 覆盖第 0 字节的 LockFileEx，满足 [0002](docs/decisions/0002-data-dir-compat.md) 约束 10），不引入 fs2 一类依赖，`rust-version` 因此为 1.89。
  - 以上依赖都不编译 C / C++ 代码，新增时须核对完整依赖图与许可。
  - 构建工具 maturin 1.15，用 uv 装在开发 venv 中，不是运行依赖。`rust-toolchain.toml` 固定开发与候选构建的 Rust 1.98.1，sdist 用户使用自己的 Rust（MSRV 1.89）。

## 常用命令

本实现（在仓库根目录执行）：

| 任务 | 命令 |
| --- | --- |
| 建开发环境 | `uv venv -p 3.12 .venv && uv pip install -p .venv/bin/python maturin==1.15.0 pymilvus pytest` |
| 构建并以可编辑方式安装扩展（改 Rust 后重跑） | `.venv/bin/maturin develop -m crates/py/Cargo.toml --release` |
| 格式化 | `cargo fmt --all` |
| lint | `cargo clippy --workspace --all-targets -- -D warnings` |
| Rust 单测 | `cargo test --workspace` |
| M2 持久化点故障注入单测（feature 默认关闭） | `cargo test -p milvus-lite-core --features fault-injection` |
| 端到端测试（真实 pymilvus；含 [tests/python/adapter/](tests/python/adapter/) 与 [tests/python/compatibility/](tests/python/compatibility/) 中复制自参照实现的用例，逐条状态见 adapter 的 INVENTORY.md） | `.venv/bin/python -m pytest tests/python` |
| 与 3.x 的差分测试（参照环境见 [tests/differential/README.md](tests/differential/README.md) 的公共前提，`benchmarks/baseline/setup.sh` 建出的环境不固定 pyarrow，不能用；不设环境变量时报错，不跳过；与上一行分别运行，不能合为同一 pytest 进程） | `MILVUS_LITE_3X_PYTHON=<3.x 的 venv>/bin/python .venv/bin/python -m pytest tests/differential` |
| dump 的端到端与差分（需要 `pymilvus[bulk_writer]`，环境与运行方式见 [tests/dump/README.md](tests/dump/README.md)；差分不设环境变量时报错，不跳过） | `.venv/bin/python -m pytest tests/dump/e2e`；`MILVUS_LITE_3X_PYTHON=<3.x 的 venv>/bin/python .venv/bin/python -m pytest tests/dump/diff` |
| M2 存储文件双向互通矩阵（Arrow 15/25 两套参照环境，nullable 向量另需 Arrow 26，准备见 [tests/storage_interop/README.md](tests/storage_interop/README.md)） | `MILVUS_LITE_3X_PYTHON=<v3>/bin/python MILVUS_LITE_ARROW15_PYTHON=<arrow15>/bin/python MILVUS_LITE_ARROW25_PYTHON=<v3>/bin/python MILVUS_LITE_ARROW26_PYTHON=<arrow26>/bin/python .venv/bin/python -m pytest tests/storage_interop` |
| 组件真值核验（`generate_*.py --verify`）与直接模糊对照（`fuzz_*.py`）：改了过滤、时间、分词、几何、function chain 等组件时跑对应项 | [tests/differential/README.md](tests/differential/README.md) |
| 合并受控等待 / 提交点进程退出（隔离的 fault-injection wheel；需由 py → grpc → core 显式启用，正式 wheel 不含测试入口） | `<fi>/bin/python -m pytest tests/compaction` |
| 子进程与关闭重开测试的隔离 / 默认并行 30 轮复验 | [进程内测试隔离记录与命令](docs/testing/process-test-isolation.md) |
| 独立 CLI server | `.venv/bin/milvus-lite server --data-dir <data_dir> --host 127.0.0.1 --port 19530 --max-workers 10`；也可 `python -m milvus_lite server ...`，合同见 0012 |
| 构建 wheel / sdist | `.venv/bin/maturin build --release -m crates/py/Cargo.toml` / `.venv/bin/maturin sdist -m crates/py/Cargo.toml` |
| 发行物与安装验收（本地诊断，候选以发行工作流为准；工作目录在仓库外） | `.venv/bin/python tests/packaging/run_acceptance.py --work-dir <work_dir> --constraints <constraints.txt> --suite --suite-expect 767`；约束文件与完整步骤见 [打包验收](tests/packaging/README.md) |
| 工具自检：发行物与平台脚本 / CI 脚本 / 发行工具 / 分片 / 基准脚本 | `.venv/bin/python -m pytest tests/packaging tests/platform`；`tests/ci`；`tests/release`；`tests/sharding`（用法见 [分片 README](tests/sharding/README.md)，合同见 0019）；`tests/benchmarks` |
| 平台验收：Linux PID 1 与命名卷 LOCK、容器 LOCK 与 strace、已安装产物联合冒烟、Windows 准备检查 | [tests/platform/README.md](tests/platform/README.md) |
| 兼容性调查与参照真值（固定样本统计、schema 真值、分区与 Query iterator、partition key；离线显式执行，不进入默认测试） | [tests/compatibility_audit/README.md](tests/compatibility_audit/README.md) |
| 发行工作流的演练与正式运行、台账、发布准备、公开核验与签发前保全 | [docs/README.md「发行」](docs/README.md#发行) |

pymilvus 两条线都要测。2.6.x 线用单独的 venv 安装构建出的 wheel，再运行同一套用例（在仓库外的目录执行，避免从源码目录导入）：

```bash
.venv/bin/maturin build --release -m crates/py/Cargo.toml -o <work_dir>/wheels
uv venv -p 3.10 <work_dir>/v26 && uv pip install -p <work_dir>/v26/bin/python "pymilvus>=2.6,<2.7" pytest <work_dir>/wheels/milvus_lite_rust-*.whl
cd <work_dir> && v26/bin/python -m pytest <repo>/tests/python
cd <work_dir> && MILVUS_LITE_3X_PYTHON=<3.x 的 venv>/bin/python v26/bin/python -m pytest <repo>/tests/differential
```

性能：三方基线（2.5.1、3.x 与本实现；规则 7 的口径）见 [benchmarks/baseline/README.md](benchmarks/baseline/README.md)；各专项基准在 [benchmarks/](benchmarks/) 下按专项分目录（目录名即专项，如 `recovery`、`sparse`、`bm25`、`json-write`），环境、数据、轮换与命令见各目录的 README。工作目录须在仓库外，各配置须顺序执行。

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
12. **探索性问题给 2-3 句 + tradeoff**：用户问“该怎么做 / 你觉得呢”时先给推荐方向与主要取舍，等确认再实现。[技术栈](#技术栈)中尚未细化的选型（如 ANN 索引的实现）尤其如此。
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
