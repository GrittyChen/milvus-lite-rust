# 自动合并与 tombstone GC 实施验收

日期：2026-10-09。范围为负责人确认的 [0010](../decisions/0010-compaction-and-tombstone-gc.md) 与本轮追加确认的 [0011](../decisions/0011-committed-delete-state.md)。参照为 milvus-lite v3.2.1 / `43d1257`，checkout 保持干净。

这是本切片的实施及自验证记录，实现与修复均已通过评审，负责人已授权提交与推送；不代表首发或生产可用。M2 的历史性能表不改写为此版本的测量。原始数据库、JSON、wheel 与日志保存在仓库外，不入库。

## 实现与边界

- 每 Db 一条维护线程，空闲只持 Db 的弱引用。按实际句柄记录触发计数，一次触发一轮，不合并丢轮、不自动跑到收敛。create、首次成功懒打开、有效代发布各触发一次；仅删除的代算有效，空提交与重新提交不重复计数。
- 准备读取内存中的不可变列；临时文件在 data 目录内完成编码和文件同步，不持 flush 或状态锁。提交持 flush 锁，在短读锁下校验，放开状态锁后改名、同步目录并 replace manifest，最后在状态写锁内发布。发布使用最新位图与主键引用，保留准备期的插入、覆盖和删除。
- data 替换与 delta GC 一次提交。提交点前失败不改引用，提交点后按实际持久化状态发布或毒化；持久化未确认不清理旧文件。被 snapshot 钉住的 data/delta/index 文件保留，旧内存快照不读取这些磁盘文件。
- 独立已提交删除表在打开时取自已提交 delta，普通 flush 发布时更新。全局最大删除表继续负责逻辑可见性，活跃删除不会遮住物理回收所需的较早已提交删除。GC 严格按新 manifest 的最小 data seq 下界处理，保留未提交删除与遗留 WAL 风险。
- 维护不绕过前台 flush 的失败节流、不抢先重新提交。无合并候选时不额外清理保留的 WAL。create/drop 在等独占许可前通知取消；正常 close 接收最终有效代的触发并排空，异常收尾保留不再提交 manifest 的边界。
- ManualCompaction 返回 id 0，GetCompactionState 返回 Completed，沿用参照应答；Completed 不表示维护已经排空。GetCompactionPlans 等其它 RPC 仍未实现。

来源移到 manifest 末尾后，故障目录中“选中与未选中来源具有同一主键、同一最大 seq”的赢家可能改变。当前整轮拒绝这类歧义输入，保留旧 manifest 与诊断；不是静默改变平局语义。此限制及其它并发、编码器、取消差异见 0010 的差异表。

## 产物身份

下列为评审前测量产物，两类 wheel 均以 release 模式构建，abi3-py310 / macOS arm64；仅隔离测试产物启用 py → grpc → core 的 fault-injection。正式产物不带等待、暂停或故障入口。

| 产物 | native SHA256 |
| --- | --- |
| 正式 wheel | `a1ff65608d7c52abd3df81700ce5174106c177a9c6fb1e43befa55d20bdf320d` |
| 隔离 fault-injection wheel | `442ad5b436a84a628b442fdf3a2442e315c2dd88966831d1b4b6faef9c960ca6` |

```bash
.venv/bin/maturin build --release -m crates/py/Cargo.toml -o <work>/formal
.venv/bin/maturin build --release -m crates/py/Cargo.toml --features fault-injection -o <work>/fi
```

正式与测试 wheel 分别装进仓库外的 venv。需要精确暂停/等待的测试在服务所属进程里调用测试入口；参照侧运行期包裹原维护函数，没有修改参照源码。正式产物用正常停服排空验证目录，不能用 FI 结果代替它。

## 正确性证据

| 边界 | 本轮证据 |
| --- | --- |
| 正常关闭、打开/创建/删除代/空代触发 | core 合并集成用例检查关闭后引用、重开主键与触发次数；两个 SDK 的正式停服用例 |
| 准备期写入和 flush、旧快照 | 暂停提交时追加/覆盖/删除并 flush；新主键、有效位保留，旧视图 Search 结果原样 |
| 隐藏的 latest_data 与恢复后重建 | 未选中的旧行、被删的选中赢家；对运行期与重开状态执行相同 upsert/delete/get，结果一致 |
| 活跃删除遮住已提交删除、恢复混入新 WAL | 物理输出不保留被已提交删除覆盖的旧行；原 WAL 字节不变；后续 flush 与重开结果一致 |
| 空输出、snapshot 钉住 | 空 data 时保留活跃删除；data/delta/index 钉住集合跨 GC 与重开仍在 |
| worker 启动失败、panic、关闭与取消 | 启动失败释放 LOCK；六类任务故障后健康 collection 仍可推进；暂停轮在正常 close 时等待，目录变更可取消提交前输出 |
| 文件与提交顺序 | FI trace 检查临时文件同步 → 改名 → data 目录同步 → manifest replace → 旧文件 unlink；提交不重复同步输出内容 |
| 提交点两侧进程退出 | 两个真实子进程 exit 注入；重开前 manifest 分别保留四个输入/一个输出，重开后四行完整向量均正确 |
| 正常停服与 Python 信号 | FI 受控暂停两秒，SIGINT 在原生 stop 等待期间发送；KeyboardInterrupt 在等待返回后才被观察到 |
| 3.x 行为、来源序与格式 | Int64/VARCHAR、完整并列顺序、Query 分页、覆盖/删除与正常关闭重开；Rust/3.x × Arrow15/25 交替读取合并目录 |

完整并列顺序使用非截断检索断言；截断处按 0004 的 hits 口径允许并列互换，不能把 exact 的额外约束当合同。互通新增用例核对物理 `_seq`、字段值、更新/删除以及反向再次打开，不只比 count。

### 变异检验

临时修改源码执行目标用例，随后按修改前 SHA256 完整恢复；不在变异态构建 wheel 或计时。

| 变异 | 实际失败 |
| --- | --- |
| 回退到全局最大删除 ≤ current_seq | 活跃删除遮蔽用例失败：物理输出 4 行，正确期望 3 行 |
| 删除跨选中/未选中来源的同 pk/seq 保护 | 歧义来源用例失败：失败轮计数 0，正确期望 1 |

恢复后合并集成 14 条全部通过。前者证明 0011 不是仅增加一张未使用的表；后者证明故障目录的限制实际执行。

## 第 3 项附加条件：退出等待

正式产物，Apple M2 Pro / arm64 / macOS 27.0.1。使用真实 3.x 生成的同一百万行、dim 128 seed：20 个各 50000 行的 Parquet，每轮复制后单独启动。每次懒打开恰好合并四个文件、200000 行，最终 18 个文件，物理与存活行均为百万。

| 轮次 | 合并轮耗时（秒） | 正常 stop 等待（秒） |
| --- | --- | --- |
| 1 | 0.633042 | 0.630978 |
| 2 | 0.567373 | 0.565337 |
| 3 | 0.593028 | 0.589879 |

最大观测值 0.633042 秒，未触及负责人指定的“超过 10 秒重新评估”。这不是最坏 I/O 或任意平台的硬上界。合并计时包括选择、准备、提交和清理，不含前面的全量恢复；恢复已预热内存/页缓存，stop 开始时维护可能已经推进了一部分。三轮测量顺序执行，期间没有并跑其它基准或编译。

脚本 [run.py](../../benchmarks/compaction/run.py) SHA256：`04e5b95057e9f904e1a7da8d69ee971cdd05f679ebe5baa773cbc43c9116320c`。NumPy 2.5.3、pyarrow 25.0.1。没有参照侧同口径轮耗时，因此不据此计算速度提升。README 与 0003 已声明退出可能等待合并，RPC grace 不是 stop 总期限，Ctrl-C 可能延迟到原生等待返回后处理。

## 第 5 项附加条件：删除重负载与内存

受控 FI 负载：4000 行、dim 128，先提交 800 个删除，再在准备窗口中重插/删除相同主键；随后重复重插、删除、flush、确定等待，共五轮。两边顺序运行。仅统计 manifest 引用的已提交 data，不混入活跃 WAL。

| 版本 | 每轮物理行 | 每轮存活行 | 每轮失效比例 | data 文件/字节 |
| --- | --- | --- | --- | --- |
| 原保守方案 | 4000 | 3200 | 20% | 1 / 2716441 |
| 0011 最终实现 | 3200 | 3200 | 0% | 1 / 2297726 |
| 3.x | 3200 | 3200 | 0% | 1 / 2297622 |

五轮各自结果都相同。原差距触发负责人追加确认 0011；最终实现消除了本负载中的死行差距。104 字节的最终编码差异不能当作死行差距，也不能由该负载推广到任意删除比例或持续并发。

[delete_load.py](../../benchmarks/compaction/delete_load.py) 与 run.py 合并指纹：`9048d0db38bb3401c29728f09866ac22f962fc7856877c54234c13bd60e855c1`。复现命令与产物区分见 [专项 README](../../benchmarks/compaction/README.md)。

独立表会增加内存。用 [committed_delete_memory](../../crates/core/examples/committed_delete_memory.rs) 的 `HashMap<Pk,u64>` 克隆诊断，父进程在 BASE/CLONED 两个等待点采集 RSS：

| 删除主键数 | 类型 | 表容量 | 克隆额外 RSS（MiB） |
| --- | --- | --- | --- |
| 250000 | Int64 | 458752 | 16.53 |
| 250000 | VARCHAR，16 字符 | 458752 | 20.39 |
| 1000000 | Int64 | 1835008 | 66.03 |
| 1000000 | VARCHAR，16 字符 | 1835008 | 81.31 |

这是单独 map 的组件增量，不是整库 RSS、峰值或通用每键成本。诊断保留原表与克隆同时存活，不计引擎其它结构。GC 后已提交表在长度低于容量四分之一时缩容；更长字符串、不同分配器和高删除密度整库测量仍没有证据。

## 套件结果

下表为评审前产物及测试的实跑结果，没有 skip/ignore。原生 SHA 如上；后续修改仅为测试与文档时不将它记成新测量产物。

| 套件 | 结果 |
| --- | --- |
| cargo test --workspace | 245 passed，core lib 144、grpc 16 |
| core fault-injection 全套 | 318 passed，core lib 165、合并集成 14、并发故障 18 |
| fmt / 默认 workspace 与 core FI clippy -D warnings | 通过 |
| 正式产物 Python，pymilvus 3.0.2 / 2.6.17 | 各 239 passed |
| 正式产物差分，对照 3.x 3.2.1，两 SDK | 各 119 passed |
| 正式产物存储互通，Arrow15/25，两 SDK | 各 68 passed |
| benchmark + platform 脚本自检 | 49 passed |
| 隔离 FI wheel 提交点退出/信号，两 SDK | 各 3 passed |

上游 compaction 的两个 adapter 用例原样纳入，状态与统计同步到 INVENTORY。正式产物的 NativeServer 属性自检确认 FI 入口缺席；不能把带 FI 的库发布为正式 wheel。

## 扫描与未验证范围

扫描 `compact_handle/compaction_deletes/committed_deletes/latest_delete/latest_data/pk_index/source_order`，核对 prepare → 当前 manifest 校验 → replace → 新状态发布 → cleanup 全链；扫描 `schedule/accepted/stop/close_nowait/impl Drop/Weak/NamespaceIntent`，核对触发、计数、取消、关闭与所有权；扫描 `sync_file/rename/commit_manifest/remove_file/pinned_files/data_sources`，核对目录与文件共存；核对 IMPLEMENTED_METHODS 及两条 SDK 的真实 RPC。

确定性失败均在首次破坏性提交之前检查：选择器 metadata/文件类型/seq 范围、输入连续性、主键提取、旧 WAL 风险、提交时引用及映射完整性、输出 ID/名称、候选 manifest。提交后的并发状态检查若失败沿用 poison，不能回滚已替换的磁盘真值。单轮输出 fsync 在准备阶段完成；旧文件删除只在持久化确认和发布成功之后进行。

本轮没有验证 Linux/Windows、真实断电、任意 I/O 卡顿的等待上界、长期跨 collection 负载，也没有重测历史 S1/检索/恢复性能。进程退出注入不替代断电，FI trace 只证明调用顺序，不替代平台 strace。正式 wheel 的一般行为和隔离轮次实验分列；生产限制保持。

## 本轮评审修复与复核

本节替代前轮产物的“当前构建”口径；上面的测量数字与产物哈希作为历史记录保留。

### 修复范围与反证

| 项目 | 当前实现与证据 |
| --- | --- |
| 最终名孤儿 | 每次改名完成即保留该输出路径。promote/candidate/replace 的错误或 panic 在提交锁内分类；replace 前失败将已改名路径登记到 compaction_cleanup，下轮在同一 flush 锁内清理。部分 promote 的单测检查已改名路径不会丢失；集成用例先制造一个最终名孤儿，再触发有效 flush/下一轮，确认 data 目录与 manifest 引用完全一致 |
| 清理与文件复用 | 清理前按当前 manifest 过滤。单测把已由 flush 引用的路径放进清理计划，文件字节和存活行均保持；不能在 Prepared::drop 或锁外删除最终名。replace 后不将输出登记为提交前孤儿，原有六类故障及进程退出用例保持 |
| 成功诊断 | 默认不输出；启动 Db 时显式设置 MILVUS_LITE_COMPACTION_DIAGNOSTICS=1 才记录单轮与发布锁时间。正式产物的三个子进程分别验证未设置/0/1。计时脚本只设置子进程环境，自检确认父进程环境不变；失败诊断仍保留 |
| 恢复映射 | collection 在 mem::take 移走 segments 前构造 data_sources，并按值传给 runtime。删除原来 runtime 从已清空 segments 重算的入口，避免直接删除前一份造成映射缺失；恢复、合并、差分及跨实现读取保持 |

额外变异关闭提交前孤儿登记，新增集成用例在最终文件集合断言失败；恢复源码后全套通过。原两个变异的生产判据没有修改，不把前轮执行记成本轮重跑。检查/写入顺序保持，新增分类仅覆盖改名后的失败，不提前提交 manifest 或清理旧引用。

### 修复产物与套件

| 产物 | native SHA256 |
| --- | --- |
| 正式 wheel | `fdbc7399e75bd55a670f11dc3b47274f0957b7f054eb0b205c4e8edde9c74542` |
| 隔离 FI wheel | `aaea11c35fed9e2de7dc00ed4f5510f13e1142844b27ce6e4952f2b8e9a63fd9` |

| 套件 | 实跑结果 |
| --- | --- |
| cargo test --workspace | 247 passed，core lib 146 |
| core fault-injection 全套 | 321 passed，lib 167、合并集成 15、并发故障 18 |
| fmt / 默认 workspace 与 core FI clippy -D warnings | 通过 |
| 正式 wheel，两 SDK 的 Python / 差分 / 互通 | 各 242 / 119 / 68 passed |
| benchmark + platform 自检 | 59 passed（修复复审时，含后续移除的历史复原自检） |
| 隔离 FI wheel，两 SDK 的提交点退出与信号 | 各 3 passed |

正式 wheel 扫描及属性测试确认测试入口缺席。构建后追加的当前引用保护单测不改变生产代码；复审时的历史复原自检只在临时副本运行，未改工作树；收口时已移除该专用自检。没有 skip/ignore，Python 套件的七条警告是已有弃用 API 用例。

### 正式产物退出条件复跑

沿用同一百万行/dim128/20×50000 行 Int64 seed，顺序三轮。均完成恰好一轮 200000 行，18 个 data 文件、百万物理及逻辑行、0 失效行。

| 轮次 | 合并轮（秒） | 正常 stop 等待（秒） |
| --- | --- | --- |
| 1 | 0.742537 | 0.740087 |
| 2 | 0.709110 | 0.706415 |
| 3 | 0.894720 | 0.891944 |

最大观测 0.894720 秒，仍未触及 10 秒重新评估条件。与前轮并非同一时刻配对测量，不把两个最大值之差当作修复造成的性能变化。机器/页缓存/恢复预热及非硬上界的限制不变。

当前 run.py 指纹：`c64c9b6706bd4192ee651a6a7f538db30859967a639a22c185f0ff0058201fea`。本节三轮结果与当前脚本指纹一致，作为退出条件的验收依据；前轮数字仅保留为过程记录，不再提供历史脚本复原。后续修改脚本由新测量记录新指纹，不修改历史数字或指纹。

### 发布锁与并发 Query

隔离 FI 产物，百万行、dim128，Int64 和 16 字符十进制 VARCHAR；各三轮顺序执行。准备完成后暂停，先跑 200 次主键 Query 基线，再恢复维护并持续从一个客户端 Query 同一主键，至该轮提交、发布、清理完成。每次结果均校验，完整物理与逻辑行数再次校验。

| 主键/轮次 | 发布写锁（ms） | 基线 Query p99（ms） | 提交窗口样本 | 窗口 Query p99（ms） | 窗口 Query 最大（ms） |
| --- | --- | --- | --- | --- | --- |
| Int64 / 1 | 98.172 | 0.749 | 206 | 0.558 | 98.968 |
| Int64 / 2 | 91.080 | 1.612 | 78 | 56.941 | 91.674 |
| Int64 / 3 | 86.403 | 0.574 | 175 | 0.484 | 86.829 |
| VARCHAR / 1 | 194.428 | 1.121 | 421 | 1.108 | 195.619 |
| VARCHAR / 2 | 158.230 | 0.706 | 319 | 0.632 | 158.865 |
| VARCHAR / 3 | 174.863 | 1.285 | 298 | 1.420 | 176.386 |

获取写锁的等待六轮均记录为 0.000000 秒（诊断保留六位小数，不表示任何负载下无等待）。持有时间只计获取后的发布与计划登记；打印发生在释放写锁之后。发布仍包含逐行主键克隆与映射构造，当前确实会让同一 collection 的请求出现约 0.1–0.2 秒的单次阻塞，因此撤下“短写锁”的性能暗示，使用“发布阶段写锁”。不删除校验来换数字。

Query 窗口包含改名、manifest 同步、发布和清理，p99 不是仅写锁区间统计；稀少的阻塞可能落在最尾端，不能因多数 p99 低就声称没有停顿。样本数和桌面负载限制其统计强度；单客户端、全存活、无写入并发，不覆盖长字符串或多客户端。该结果作为已确认方案的成本记录和后续性能项，不增设未经负责人确认的首发延迟硬门槛。

脚本 [publication.py](../../benchmarks/compaction/publication.py) 与 run.py 合并指纹：`502708d1907a0deb1fff3eee7c6519f025f9fc09292cead25ae1befee79b5f57`。Python 3.12.14 / pymilvus 3.0.2；统计方法为线性插值的样本 p99，并同时报告最大值。FI 总轮耗时包含受控暂停与基线 Query，不用于正式产物的 10 秒条件或速度比较。

### 删除条件复跑与未验证边界

当前 FI 产物再跑五轮删除/重插/再删除，Rust 与 3.x 仍均为每轮 3200 物理/逻辑行、0% 失效行、1 个文件。data 字节仍为 2297726 / 2297622。新脚本合并指纹 `6937b6a81b23c1598a030a0139185bd6e27faedb3709170dc061f928003d222f`；恢复映射与清理修复没有改变该负载的逻辑结果。

独立删除表组件 RSS 沿用前轮诊断，没有把它改记为本轮重新测量。Linux/Windows、真实断电、其它索引/主键长度/删除比例、多客户端与写入并发的性能仍未测。扫描新增的 submission/phase/output.relative/compaction_cleanup、promote/drop/cleanup、diagnostics、recovered/data_sources 调用链；提交点分类、GC、正常/异常关闭和快照边界沿用本轮全套回归证据。

## 评审通过后的收口

实现及修复复审通过。按负责人确认的收口范围，删除历史复原补丁、其专用自检及维护流程；保留当前脚本已经执行的三轮 Int64 与五轮删除负载复测及完整指纹作为验收依据。早期结果保留为过程记录，不再承诺复原其脚本。引擎、基准执行脚本、产物身份与测量数字没有变化。

发布写锁进一步优化保留为后续独立性能切片；当前仍披露约 0.1–0.2 秒的单次发布停顿。收口后的 benchmark + platform 自检为 58 passed，文档相对链接及 diff 空白检查通过；这次只跑对应的文档/自检验证，不把前轮引擎与 SDK 结果记成此次重新执行。负责人已授权本切片提交与推送，发布仍需独立授权。
