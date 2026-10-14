# 存量 FAISS 定义的 load 度量检查：调查与冻结修复合同

日期：2026-10-14。对照为 milvus-lite v3.2.1 / `43d1257`；Rust 为切片 A 基线 `a10477f` 对应的开发 wheel。调查与合同已评审通过；接受状态锁内只读元数据检查，按本合同实施，rc2 发行物不变。

## 已验证的触发条件

参照 [Collection.load](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py) 对每个索引定义、每个有物理行的 segment 调用 [Segment.build_or_load_index](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/storage/segment.py)。匹配路径存在时调用 `load_index_from_spec`，缺失时才调用 `build_index_from_spec`。四种显式 FAISS 类型的 load 读取已有文件并保留传入 metric，不校验它；AUTOINDEX 同样先尝试 HNSW 文件，失败后尝试暴力文件。

因此“FAISS 类型配 BM25 / NONE 就在 load 拒绝”过宽：需要区分是否必须构建。读取已有文件时，参照即使收到这两个 metric 也能 load；是否能使用该 metric 检索是另一项行为，不由本合同改变。

探针先由真实 3.x 写入 L2 定义、两段各 150 行的数据和两个有效侧车，再复制目录，分别将 metric 改成 BM25 / NONE 并控制侧车存在性。两侧都在新进程里重开，观察 load 结果、状态和成功时的 Query 行数。五种类型 × 两个 metric × 三种侧车形态，共 30 组，每组各跑两侧：

| 对当前两个非空 segment 的侧车 | 3.x load | Rust load | 组合数 |
| --- | --- | --- | --- |
| 两个都存在 | 成功，Loaded，Query 300 行 | 同左 | 10 |
| 只剩一个 | code 1，NotLoad | 成功，Loaded | 10 |
| 全部缺失 | code 1，NotLoad | 成功，Loaded | 10 |

3.x 失败原文分别为 `unsupported metric 'BM25'; expected COSINE / L2 / IP` 和 `unsupported metric 'NONE'; expected COSINE / L2 / IP`。

已有文件的成功也存在正常 API 的生产路径：真实 3.x 在 HNSW / L2 数据上建 snapshot、release、DropIndex，再创建 HNSW / NONE。snapshot 钉住的旧文件被保留，创建和冷重开 load 都成功，Query 返回 300 行。创建时的这类冲突已由 0014 在 Rust 侧拒绝；存量目录的 load 仍须保留参照结果。

这轮没有复跑全部 WAL / 全删 / 提交状态及平台矩阵，不把调查探针当作修复验收。

## 最小修复合同（已评审通过）

### 新增判定

只扩展 LoadCollection 的存量 metric 维度：

1. 已 loaded 时直接成功，不重新检查文件，保持当前合同。
2. 按原索引定义顺序检查。旧的索引类型判定及错误不变；稠密向量上的 AUTOINDEX、HNSW、HNSW_SQ、IVF_FLAT、IVF_SQ8 配 BM25 / NONE 时，检查当前已提交数据源。
3. 仅物理行数大于 0 的 segment 参与检查；删除隐藏的行仍计入。对应路径使用参照的 `<source_stem>.<field_name>.<index_type 小写>.idx`。至少一个这样的 segment 缺少匹配路径时，返回上面的 metric 原文，`InvalidArgument` 映射为 `code=1, error_code=0`。
4. 全部相关路径存在、只有 WAL、或全部已提交 segment 物理行数为 0 时，仍 load 成功。不能用“目录内有一个该类型文件”代替逐 segment 判定；不统计 snapshot 中不属于当前数据源的旧文件。
5. 失败后保持 released，Query / Search / get 走既有 101；判断过程不写文件、不改变定义、manifest、snapshot、删除意图或其他 collection。

### 实现边界与代价

- metric 分类放在 index；当前定义、数据源与物理行数仍由 `Collection::load` 的状态临界区读取。仅上述存量非法组合读取侧车路径的元数据；正常度量不新增目录 I/O。
- 使用每次 load 的实际路径状态，不增加缓存、持久化字段、boot 状态或通用文件管理框架。`NotFound` 是缺失；其他真实元数据 I/O 失败按 Storage 向上传递并保持 released，不吞成“缺失”。存在性使用会跟随符号链接的 fs::metadata：悬空链接为缺失，目录为存在。本检查不验证路径的文件内容和可加载性。
- **保留现有 load 状态锁，在其中做只读元数据检查**，不新增 flush 锁或锁外准备 / 版本重试协议。代价是该稀有非法组合的 load 会在检查期间占用状态锁；已接受这一边界；每个非空 segment 最多一次 stat，不扫描目录，不把它隐藏成纯计算。
- 0014 的 CreateIndex / DropIndex 仍在 flush 锁中处理文件、在状态锁中发布；load / release 不取 flush 锁。并发 load 导致 DropIndex 删除确认后再拒绝的已登记路径保持，不为这项修复顺手串行化两个入口。
- FLAT / BRUTE_FORCE 的 BM25 / NONE 与所有正常度量保持原行为；新建 FAISS 非法组合的 UNIMPLEMENTED 不变。load 成功后新增 segment 不立即重新判定，release 之后再 load 才检查。
- 沿用“Rust 不读 3.x 索引内容”的差异：损坏的现成侧车可能让 3.x load 失败，本实现仍按 FLAT；不验证内容。元数据错误的严格上报会与参照 `os.path.exists` 将部分错误当缺失的理由原文不同，需在 0002 明确登记。
- 构建参数继续不解析。若 metric 与参数同时无效，参照可能先在 `M` / `nlist` 转换时报错，Rust 报 metric 错误；这是既有参数差异的组合影响，需在 0002 写明并测试，不能宣称所有失败原文都一致。

### 验证矩阵

| 维度 | 验收观察 |
| --- | --- |
| 类型 / 度量 | 五种类型 × BM25 / NONE 原文；各正常 metric、FLAT / BRUTE_FORCE 及旧三类类型错误保留 |
| 数据源 / 文件 | 一个和多个 segment；全有、全无、一个缺失、无关 stem 文件；零行 segment 无文件不误拒绝 |
| 生命周期 | WAL-only 先 load，flush 后重复 load 成功，release 后 load 才失败；已有文件先 load，增加新 segment 后同一状态规律 |
| 删除 / compaction | 全删但物理行仍在时检查；flush / compaction 生成缺少侧车的新非空 segment 后，下一次 release / load 失败；物理行消失后可成功 |
| 失败后恢复 | load 失败保持 released，再 DropIndex（没有文件可删）成功、定义移除、自动 Loaded，正常 load / 读取恢复 |
| 符号链接 / 目录 | 有效链接为存在，悬空链接为缺失，目录为存在；真实元数据错误上抛 |
| 失败与隔离 | 失败后 101、重复失败、文件和 manifest 字节不变、健康 collection 正常；元数据 I/O 错误向上传递 |
| 既有差异 | 损坏但存在的侧车仍不读内容；非法参数与非法 metric 的组合明确登记；创建与 DropIndex 协议不变 |
| 并发 | 两个 load、load / release、load / flush、load / drop collection、与 DropIndex 第 8 / 9 步的受控交错；删除确认但 manifest 替换前失败仍保留定义，侧车缺失时下一次 load 失败；核对旧句柄退役，无反向取锁，不回滚其他已完成的状态变化 |
| 跨实现 | 真实 3.x 写定义和有效侧车，各自复制后冷重开；比较 load 状态及后续读取错误；覆盖 snapshot 旧文件的正常 API 生产路径 |

本修复独立收口，不并入已提交的切片 A；锁边界已接受，按合同实现并跑 core、gRPC、两条 SDK 与完整存储互通，随后做实现复审。本文件的调查结果不作为尚未执行的修复验收。

## 实现与验证（2026-10-14，实现复审通过）

当前源码在 `Collection::load` 中逐非空来源读取跟随链接的 metadata；类型错误仍先按旧规则处理，正常 metric 不新增 stat。创建与存量分类共用五种 FAISS 类型集合。新增只读 I/O 之前确认句柄未被 drop 退役，旧句柄不能在同名重建后访问新目录。没有新增持久化状态或 flush 锁。

core 的 `index_load.rs` 覆盖十个非法组合、全部 / 部分侧车、损坏内容仍忽略、有效 / 悬空链接与目录、真实 metadata 错误、WAL / flush / 已 loaded 幂等、全删后的物理行、健康 collection 及移除定义后恢复。fault-injection 额外控制删除之前 / 之后的 load、删除确认但 manifest 替换前失败，以及 compaction 生成新来源：load 抢先成功时 DropIndex 拒绝，定义保留，下一次 release / load 失败；文件已删时并发 load 失败，DropIndex 可继续提交。

零行的原语跳过由 collection 内部单测核验。恢复层本来拒绝没有 RecordBatch 的空 Parquet，且 compaction 要求合法序号范围文件名；本修复不改变这些格式边界，也不使用非法夹具迁就测试。

跨实现覆盖在 `test_index_load_metrics.py`：由真实 3.x 写两个 segment / 侧车或 WAL，再各自复制冷重开，直接核对 code / error_code / 原文、状态、三个读取入口的 101 和 DropIndex 恢复。五种类型 × 两个 metric × 五种形态为 50 项，另外两项固定参数与 metric 同时无效时的错误理由差异，一项用真实 snapshot / DropIndex / CreateIndex API 生产 HNSW / NONE 定义而非手工改写 manifest。

macOS arm64，CPython 3.12.14 / pymilvus 3.0.2，以及 CPython 3.10.21 / pymilvus 2.6.17，都安装当前开发 wheel。原生库 SHA256 为 `1e9b0cbae366f852c2f46577f6decc5f8394b5013853a24234a1fa086724ccfa`；仍标识 0.1.0rc2 的开发构建，未替换任何公开发行物。

| 检查 | 实跑结果 |
| --- | --- |
| Rust workspace 默认 | 280 passed |
| core fault-injection 最终全套 | 365 passed（顶层 363，子进程 2） |
| grpc all-features | 22 passed |
| 两条 SDK 端到端 | 各 287 passed |
| 两条 SDK 存储互通 | 各完整 157 passed，补充的组合参数 / snapshot 三项各 3 passed；共覆盖 160 项 |
| 索引定义 / 重开 / 跨来源检索差分 | 27 passed（pymilvus 3.0.2）；本轮不宣称两条 SDK 全量差分已复跑 |
| Linux arm64 manylinux_2_28 | load 矩阵 13、索引变更矩阵 25 passed |
| 打包 / 平台脚本自检 | 170 passed |
| fmt、两种 Clippy、wheel / sdist 与原生库检查 | 通过 |

反证在仓库外副本和独立 target 上执行，确认编译来源：切片 A 旧实现不能通过逐非空来源的 metric 测试；将 metadata 改成 symlink_metadata 后，悬空链接反例失败。实际修复及 Linux 对应反例通过，未放宽旧期望。

**单独记录（已与本修复分离）：** 首轮 core fault-injection 在未修改的 `interrupted_deletion_blocks_both_endpoints_and_same_boot_reopen` 用例里，关闭后重开一次遇到 LOCK busy。独立重跑和最终全套均通过；清理隔离 core 产物、确认源码后，切片 A 基线的同一套件先通过一次、连续六次也通过，尚不能称为已经复现的历史问题或归因于 metric。未修改底层 LOCK 或原测试断言，原始失败日志保留在开发工作目录。随后复审方以并行 / 单线程 / 排除子进程三组对照确认测试间派生子进程的干扰，并确认与 load 修复无因果；测试隔离另行修复，flock 在 exec 前可能保留继承引用的机制仍与测试层改动区分。

未实跑真实系统重启 / 断电、Linux x86_64 或 Intel Mac 的本修复完整候选验收。切片 B 引擎尚未实施；本次复审只覆盖独立的 load 修复及直接关联状态、失败和恢复路径。
