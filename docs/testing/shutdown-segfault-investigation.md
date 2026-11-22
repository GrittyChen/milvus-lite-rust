# 停服路径段错误调查

- 状态：调查记录，经负责人评审通过；同日补记阶段 1、阶段 2 的结果与出口：**根因未查明，经负责人决定对当前产品源码与依赖谱系做显式发布级风险接受**，任何再现即撤回（见[「结果与出口」](#结果与出口)）。没有根因修复，没有进入阶段 3（ThreadSanitizer），调查全程没有任何产品代码变化。2026-11-22 第二轮（ASan 与停服间隙进程退出的测试）仍未查明根因，补充验证了停服的两个间隙中进程退出时已应答的写入在重开后仍在，见[「第二轮调查」](#第二轮调查2026-11-22)。
- 「已确认事实」至「定位方案」形成时未运行新证据，只分析已有的运行日志、失败记录产物、仓库记录与源码；阶段 1、2 的探针此后逐项授权执行。
- 日期：2026-10-26。
- 依据：[0020 试点记录「试点后的拓扑调整 · 失败与根因」第 2 项](0020-ci-pilot.md#失败与根因-1)（显式风险接受，正式候选前须取得根因与修复或再次做发布级风险接受）；[0021 试点记录「尚未执行」](0021-release-pilot.md#尚未执行)。
- 范围：对照层运行 39948671536 的 job `diff-3.0.2 (ubuntu-24.04)`（job 120397551186）的日志与失败记录产物 `diff-3.0.2-ubuntu-24.04`；0020 试点记录中的复现尝试；此后各工作流的运行统计；`09653c2` 至调查完成时 `main` 的 `2ddf5a7` 的产品源码。
- 证据：运行日志公开，有保留期限；失败记录产物需要仓库访问权限，2026-11-06 过期，其中的分片日志、进程记录与各用例数据目录是本记录唯一的现场材料。原始产物 zip（SHA256 与 GitHub 记录的 digest `264e95a4…4ee2b` 一致）、运行与 job 元数据及完整日志已由负责人本地留存。

## 已确认事实

1. **发生**：2026-10-23，`ci/release` 分支 `09653c2` 上的对照层运行 39948671536，attempt 1。`diff-3.0.2 (ubuntu-24.04)` 以 4 个分片并行执行差分套件（收集 421 项，分片 118 / 101 / 101 / 101），shard-3（101 项）进程以返回码 -11 退出，开始后约 233 秒；其余分片正常结束，汇总报告 `shard-3 退出码 -11`、shard-3 记录缺失、执行并集缺少 101 项。该运行共 11 个 job，9 个成功、2 个失败：失败的是这个差分 job 与汇总 `compare-pass`，其余 8 个对照工作 job 与 `changes` 成功。
2. **faulthandler 输出**（shard-3 日志）：`Fatal Python error: Segmentation fault`，`Current thread 0x00007f4729eb6b80`，Python 调用栈自顶向下为 `server_manager.py:85 _stop_locked`（即调用原生 `NativeServer.stop`）← `release_server` ← `tests/differential/test_reopen.py:67 close` ← `pair` fixture 的 teardown ← pytest。输出中只有这一个 Python 线程。已加载的扩展模块包括 `grpc._cython.cygrpc`、`google._upb._message` 与 numpy 各模块。
3. **崩溃在 teardown 中**：faulthandler 栈位于 `pair` fixture 的 teardown，即该用例的 call 阶段之后。分片日志在 `test_range_search.py` 打出 6 个点后崩溃；该分片的计划含 10 个 `test_range_search` 用例（5 个 `test_range_matrix_float64_window_output_and_sdk[*-COSINE]`、4 个 `test_search_iterator_empty_or_tied_scores_with_varchar_keys[*-COSINE]`、1 个 `test_v1_iterator_observes_changes_between_requests`）。basetemp 中 search iterator 用例只有一个编号目录 `test_search_iterator_empty_or_0`，另有 pytest 指向最新编号目录的 `test_search_iterator_empty_or_current`（上传产物时展开为目录，内容与 `_0` 相同），两者都是下一条所述的未 flush 状态。teardown 的顺序是先关闭两个 `MilvusClient`，再 `release_server` 停止本实现的服务，最后结束 3.x 参照子进程。
4. **崩溃时点早于最后一次 flush 的写盘**：上述目录的 `rust.db` 只有初始 manifest（version 1、无数据文件）与未清理的 `wal/wal_data_000001.arrow`；同一分片中正常停服的用例（如 `test_range_matrix_float64_wind0`、`test_query_iterator_order_pagi…`）都留下最后一次 flush 写出的 parquet，没有 WAL。正常停服在 `db.close()` 中完成最后一次 flush，因此崩溃发生在停服开始到 flush 写出第一个文件之间。
5. **当时没有原生栈**：崩溃现场采集在之后的 `dd86a57` 才加入，这次只有 faulthandler 的 Python 栈，没有 core。
6. **环境**：GitHub 托管 `ubuntu-24.04`（x86_64，4 vCPU / 16 GB），CPython 3.12.15，pymilvus 3.0.2，grpcio 1.84.0，protobuf 7.36.2，numpy 2.5.3；四个分片进程并行，每个差分用例另起一个 3.x 参照服务子进程。没有记录崩溃时刻的内存与 CPU 争用。`09653c2` 本身只改测试侧：测试驱动进程在导入 grpc 前关闭 grpcio 的 fork 处理（不涉及产品代码）。
7. **产品代码未变**：`09653c2` 与调查完成时的 `main`（`2ddf5a7`）之间 `crates/`、`Cargo.lock` 与 `rust-toolchain.toml` 没有任何差异，此后所有运行执行的都是同一份原生代码。
8. **停服路径**（源码）：`NativeServer.stop` 在 `py.detach` 中（释放 GIL）取服务互斥锁后调用 `Server::stop`，顺序为：发送停止信号；`runtime.block_on` 最多等宽限时间让服务任务结束，超时则中止；`db.begin_shutdown()`；`runtime.shutdown_timeout(0)` 丢弃连接与请求任务，已开始的阻塞任务在阻塞线程上继续运行；`db.close()` 等这些引擎任务结束、完成最后一次 flush 后释放 LOCK；随后析构。生产源码中唯一的显式 `unsafe` 在 `storage::index_platform`，停服路径不经过。
9. **已有的复现尝试均未复现**（0020 试点记录）：GitHub 相同环境约 184 次整套 4 分片 `diff-3.0.2`（其中 32 次保持 grpcio 默认 fork 处理作对照）、约 1280 轮 `test_range_search` 与 `test_reopen` 四进程定向压力；本机 macOS 4 进程循环该用例 1440 次。
10. **此后没有自然再现**：2026-10-23 12:00 UTC 之后，快速层 50 次成功、2 次失败、3 次取消，对照层 27 次成功、3 次取消，发行工作流 5 次成功、22 次失败（多为 A、B 阶段的失败注入），诊断工作流 4 次。逐一检索 25 次失败运行的失败 job 日志，出现段错误字样的只有 3 次刻意注入（39978220301 验证崩溃现场采集、40166358454 容器 core 诊断、40173299185 注入崩溃的演练）。崩溃现场采集已由注入验证可在 macOS、ubuntu-24.04 与 ubuntu-24.04-arm 的托管机器（39978220301）及发行工作流的容器 job（`b880fdd`）取得原生栈。

## 推断（待原生栈确认）

- **崩溃用例是 `test_search_iterator_empty_or_tied_scores_with_varchar_keys[False-False-COSINE]`**：依据是 6 个点、计划中的用例顺序与只有一个编号的 search iterator 目录，前提是执行顺序与计划顺序一致。该用例建表（INT64 主键、2 维向量、nullable 标量与 ARRAY），插入 20 行，经 pymilvus 的 search iterator 分页读取。
- **出错线程是主线程**：SIGSEGV 由出错线程同步接收，faulthandler 把执行处理函数的线程标为 `Current thread`；若出错的是 tokio、rayon 或 grpcio C-core 等没有 Python 线程状态的线程，主线程不会被标为当前线程。`0x00007f4729eb6b80` 的形态也与注入运行中 gdb 列出的主线程（`Thread 1 (Thread 0x7f58372e8b80 …)`）一致。即崩溃指令执行于主线程在 `NativeServer.stop` 内部（含其调用的依赖库与 libc）的这段时间。
- **位于停服顺序的前段**：结合事实 4，在停止信号、宽限等待、runtime 关闭或 `db.close()` 开始到 flush 写文件之前。
- 后两点只缩小范围，不说明是哪一层的缺陷：主线程可能是出错者，也可能只是撞上了其他线程此前造成的内存破坏。

## 候选假设

不互斥，按目前的先验排序：

| # | 假设 | 支持 | 反向或缺口 |
| --- | --- | --- | --- |
| H1 | 进程内其他原生代码造成内存破坏，主线程在停服的释放或分配中成为受害者 | 停服路径上本仓只有安全 Rust；同一进程有 grpcio C-core，且客户端通道刚在 teardown 中关闭，C-core 可能仍在后台线程收尾 | 没有原生栈与内存检查结果 |
| H2 | 依赖库在停服路径上的 `unsafe` 实现缺陷：tokio runtime 零超时关闭与仍在运行的阻塞任务、tonic / hyper 连接任务被丢弃、parking_lot、rayon-core 计算池的回收 | 崩溃时主线程正执行这些操作 | 都是常见用法，上千次重复未复现 |
| H3 | 主线程栈溢出：扩展模块中 Rust 不为主线程安装栈溢出检测，溢出表现为 SIGSEGV | 无法排除 | 停服路径调用层次很浅，先验低 |
| H4 | PyO3 / CPython 交界：`py.detach` 期间的线程状态问题 | `stop` 在释放 GIL 时执行 | 进程只有一个 Python 线程，先验低 |
| H5 | 托管机器的偶发硬件或内核问题 | 无法排除 | 无法检验，最后考虑 |

本仓逻辑层面的停服顺序问题（例如 runtime 关闭后阻塞线程仍在访问引擎）在安全 Rust 中不会直接产生段错误，只可能经 H2 表现出来，因此不单列。

## 未知项

- 原生栈：崩溃指令位于哪个库、哪个函数。
- 是否与紧前的客户端通道关闭、或宽限期内仍在途的请求在时间上相关。
- 出现概率：分子只有 1，分母只能按上面各类运行粗略估计，给不出可靠的概率。
- 是否只限于 Linux x86_64：其他平台没有出现过，但运行量较少。
- 崩溃时刻的资源状况。

## 定位方案（记录形成时只写方案；执行结果见「结果与出口」）

原则：一次只改变一个变量；每个阶段写明运行预算与停止条件，超出即停下报告；成功与失败判据事先写定。

| 阶段 | 做法 | 变量 | 预算 | 判据 |
| --- | --- | --- | --- | --- |
| 0 被动监测 | 已在运行：快速层、对照层与发行工作流的崩溃现场采集 | — | 零额外成本 | 任何自然再现即停止其他工作，取 core 的原生栈定位（沿用 0020 的处理） |
| 1 内存错误检测 | 在 Linux x86_64 上以 valgrind memcheck 运行崩溃所在的两个测试文件（`test_range_search.py` 与其依赖的 `test_reopen.py`），Python 使用 `PYTHONMALLOC=malloc`；不依赖崩溃复现 | 有无内存检查器，用例与构建不变 | 原生 x86_64 验收主机一次完整执行（预计 1–2 小时），不占用托管机器 | 失败：任何 invalid read / write 或释放后使用的报告，栈中含 `_native`、tokio、tonic / hyper、rayon 或 grpcio 帧 → 按栈定位；成功：一次完整执行无此类报告，记录后进入阶段 2 |
| 2 停服窗口定向压力 | 独立探针脚本（测试侧，不改产品）按崩溃用例的形态循环：起服务、建表、插入 20 行、search iterator 分页、关闭客户端、停服；不起 3.x 参照，去掉无关变量；ubuntu-24.04 托管机器 4 进程并行，开启崩溃现场采集 | 每轮只改一个时序变量：a. 基线；b. 停服前不关闭客户端（连接仍在）；c. 关闭客户端后立即停服与间隔 1 秒停服 | 每个变量最多一个托管 job、60 分钟，三个变量合计不超过 3 job·小时 | 失败：任何崩溃 → 取原生栈，停止其余变量；成功：该变量在预算内零崩溃，记录轮数 |
| 3 条件性：数据竞争检测 | 仅当阶段 1、2 指向线程竞争时，以 nightly 工具链的 ThreadSanitizer 构建诊断用扩展运行阶段 2 的探针 | 有无 TSan | 另行请示 | 另行确定 |

**出口**：

- 取得根因：按正常评审修复，补测试，修复本身不在本记录范围。
- 阶段 1、2 在预算内都没有发现问题：带着上述运行量与检查结果请负责人决定是否做发布级风险接受（0020 已允许的两种处理之一），或追加预算。
- 不论走哪个出口，阶段 0 的崩溃现场采集都保留。

## 结果与出口

阶段 0 持续运行，期间没有自然再现。阶段 1、2 均按上表的判据为「成功」，即在预算内没有发现问题；按出口第二条，由负责人决定。

### 阶段 1：内存错误检测

- 环境：原生 x86_64 验收主机上的 manylinux_2_28 容器（AlmaLinux 8.10、glibc 2.28），Rust 1.98.1、CPython 3.12.15、valgrind 3.22.0、pymilvus 3.0.2、grpcio 1.84.0；`bd9b171` 的 release 构建，另加 `CARGO_PROFILE_RELEASE_DEBUG=1` 只为取得调试信息；`PYTHONMALLOC=malloc`；memcheck 只检查 pytest 进程，3.x 参照服务等子进程 exec 后不在检查范围。不占用托管机器，没有 GitHub 运行号。
- 完整执行：`test_range_search.py` 与 `test_reopen.py` 共 45 例全部通过，退出码 0，986 秒。被测的 pytest 进程（本实现服务所在进程）报告 3 个错误 context，均为 `Conditional jump or move depends on uninitialised value(s)`；没有 invalid read / write / free，没有释放后使用；停服路径、grpcio、rayon 没有任何报告。另有 65 份日志来自 fork 后、exec 前的子进程，没有错误报告。
- 归因：以 `--track-origins=yes` 复跑前 7 例，全部通过，595 秒，出现同样 3 个 context。未初始化值的来源都是栈上分配：1 个在 hyper HTTP/2 服务端把 content-length 转为解码长度处（`Option<u64>`），2 个在 `ReadSnapshot::filtered_chunks_compiled` 的有效行迭代（`FlatMap` 前端迭代器 `Option<FromFn<..>>`）。两者都是安全 Rust 中 `Option` 为 `None` 时未初始化的负载字节，优化代码的条件判断读到了它们，memcheck 无法区分，判为误报；不据此修改产品代码。
- 预算：完整执行预计 1–2 小时，归因复跑不超过 30 分钟，均在预算内。

### 阶段 2：停服窗口定向压力

- 运行 [40290467268](https://github.com/GrittyChen/milvus-lite-rust/actions/runs/40290467268)，attempt 1，3 个 job 全部成功。诊断分支只含诊断工作流与探针（基于 `bd9b171`，删除其余工作流，不合入），用后删除，删除前生成并验证 git bundle。
- 环境：GitHub 托管 `ubuntu-24.04`（4 vCPU / 16 GB），CPython 3.12.15；依赖与对照层相同（[dev.txt](../../.github/ci/requirements/dev.txt)：grpcio 1.84.0、pymilvus 3.0.2、protobuf 7.36.2、numpy 2.5.3）；扩展与对照层同样以 `maturin develop --release --locked` 构建，不加调试信息。
- 探针（测试侧，不改产品）：每个 job 4 个工作者并行，每个进程最多 50 轮。每轮新建 `ServerManager` 与数据目录，按崩溃用例的形态起服务、建表（INT64 主键、2 维向量、nullable 标量与 ARRAY，COSINE FLAT）、插入 20 行、search iterator 每页 7 条读完，再按变量停服，最后核对最后一次 flush 已写出 parquet 且 WAL 已清理（崩溃现场恰好缺这两点）。不起 3.x 参照；探针进程与测试驱动进程一样关闭 grpcio 的 fork 处理，并开启 faulthandler；常设崩溃现场采集开启，进程卡住 15 分钟以 SIGABRT 结束以留下 core。任一进程非零退出或出现 core 即停止该 job 的全部工作者并取消其余变量。
- 结果（job 开始后 45 分钟不再开始新的轮次，实际压力窗口约 42 分钟）：

| 变量 | job | 轮数 | 进程数 | 崩溃 / core |
| --- | --- | --- | --- | --- |
| a. 基线：关闭客户端后立即停服 | 121464053682 | 17835 | 359 | 0 / 0 |
| b. 客户端仍连接时停服 | 121464053947 | 17228 | 347 | 0 / 0 |
| c. 关闭客户端后等待 1 秒再停服 | 121464053987 | 6505 | 132 | 0 / 0 |

- 合计 41568 轮、838 个进程，每轮都完成了 flush 与 WAL 清理；三个 job 各约 45 分钟，合计约 2.25 job·小时（预算 3）。三个变量在预算内零崩溃，按授权停止，没有扩大压力。

### 出口：显式发布级风险接受

负责人决定：**根因未查明，接受为当前产品源码与依赖谱系的发布级残余风险**，不再追加压力预算，也不进入阶段 3。

依据：

- 阶段 1：45 个完整用例及其停服路径没有非法访问、释放后使用或相关原生栈；仅有的 3 个未初始化值 context 已归因为安全 Rust 优化代码中栈上 `Option` 的判别，属于 memcheck 误报。
- 阶段 2：三个停服时序变量合计 41568 轮、838 个进程，零崩溃、零 core，每轮完成 flush 与 WAL 清理。
- 自然发生仍只有一次；现有证据不支持指定根因，追加同形态压力或与现象不匹配的 ThreadSanitizer 搜索边际收益很低。

适用范围：当前谱系，以 `2ddf5a7` 的 `crates/` 与 `Cargo.lock` 为准（与崩溃时的 `09653c2` 没有差异；停服相关的 Rust 依赖为 tokio 1.53.1、tonic 0.14.6、hyper 1.11.1、h2 0.4.19、PyO3 0.29.2、rayon-core 1.13.0、parking_lot 0.12.5），工具链为 [rust-toolchain.toml](../../rust-toolchain.toml) 固定的 Rust 1.98.1，与本实现同一进程运行的 grpcio 为 CI 固定的 1.84.0。

失效与继承：

- `crates/`、`Cargo.lock`、Rust 工具链，或 grpcio、PyO3、tonic、tokio 等停服相关依赖发生变化时，本接受不再自动适用。
- 首个真实候选相对当前谱系的差异若不涉及停服、服务生命周期、原生交界（PyO3、FFI、`unsafe`）或上述依赖，候选记录须写明所比较的提交与差异范围，经负责人明确确认后可以继承本结论；涉及其中任何一项，须重新调查或再次做发布级风险接受。

停止条件：

- 候选的 10 / 5 计数或任何运行再次出现停服路径 SIGSEGV，立即停止快进与发布，撤回本接受，以常设采集取得的 core 或诊断报告转回根因调查；不得按偶发失败重跑掩盖。
- 常设崩溃现场采集（阶段 0）继续保留。
- 最终 Release 记录须明确写「根因未查明、显式风险接受」，不得写成已修复。

证据：阶段 2 的运行日志公开，有保留期限。阶段 1 与归因复跑的完整日志、错误 context 与容器内脚本，阶段 2 的运行与 job 元数据、完整日志、探针、工作流、补丁与诊断分支的 git bundle，均由负责人本地留存。

## 第二轮调查（2026-11-22）

负责人要求再查一次根因，授权按 A、B、C 三段依次进行，找到真因即停止。基线为 `main` 的 `350cd31`。第二轮同样不改产品代码，只增加测试。

### 自然再现与谱系变化

- 2026-10-26 至 11-22 共 350 次运行（快速层 177、对照层 132、发行工作流 28、其他诊断与公开核验 13）。失败或重跑过的运行逐个 attempt 取失败 job 的日志，共 45 个 attempt 含失败 job，检索不到段错误、SIGSEGV 或退出码 -11；其中的 7 份崩溃现场采集都是 0 个 core。分片崩溃时汇总会因执行记录缺失而失败，成功的运行不需另查。
- 停服相关的依赖与第一轮的适用范围相同：tokio 1.53.1、tonic 0.14.6、hyper 1.11.1、h2 0.4.19、PyO3 0.29.2、rayon-core 1.13.0、parking_lot 0.12.5。`Server::stop`、`NativeServer.stop` 与 `Db::close` 的函数体与 `2ddf5a7` 相同，但 `Db::close` 调用的 flush、维护与存储收尾此后有改动（`db.rs` 相对 `2ddf5a7` 改动约 3000 行），停服时执行的代码并非不变。
- 此后新增的 `unsafe`：[allocator.rs](../../crates/core/src/allocator.rs) 在维护轮次后调用 `malloc_trim`，[flat.rs](../../crates/core/src/flat.rs) 按 CPU 分派 AVX2 副本。两者都晚于那次崩溃，不是它的原因，但属于此后的残余风险范围，B 段覆盖了它们。

### A：上游变更与静态审查

- 据各自的变更说明，tokio 1.53.2 与 h2 0.4.20 有关闭相关的修复（阻塞池拒绝任务时的锁顺序、定时器在关闭时的处理、h2「关闭时的罕见竞争」），变更说明都不涉及内存安全；tokio 唯一的释放后使用修复在不稳定特性中，本实现不启用。hyper 1.12.0 的发布包不含变更记录，没有逐项核对。grpcio 1.84 没有检索到匹配的公开报告。
- 停服路径（发送停止信号、宽限等待、`begin_shutdown`、`shutdown_timeout(0)`、`Db::close`）都是安全 Rust。AVX2 副本的函数体是与基线副本相同的安全切片代码，`unsafe` 只用于调用前确认 CPU 特性；`index_platform` 只做 `faccessat`、`geteuid`、`fstatfs` 这类查询。没有找到可疑点。

### B：AddressSanitizer

- 环境：原生 x86_64 验收主机上的 manylinux_2_28 容器，CPython 3.12、pymilvus 3.0.2、grpcio 1.84.0（[dev.txt](../../.github/ci/requirements/dev.txt)），参照环境按 ref25 建立并从固定哈希的 sdist 安装 jieba 0.42.1。扩展用 `rustc 1.101.0-nightly (f9ceb4a4d 2026-10-31)` 以 `-Zsanitizer=address -Cforce-frame-pointers=yes` 构建，只插桩目标代码（本仓与全部 Rust 依赖），std 未插桩；release 加调试信息。运行时为预加载的 libasan8 16.2.1，`PYTHONMALLOC=malloc`，`ASAN_OPTIONS=detect_leaks=0:allocator_may_return_null=1:allow_user_segv_handler=1`。扩展中有 41 个未定义的 `__asan_` 符号，不预加载运行时则无法导入，确认插桩生效。
- `tests/python`：765 项全部通过，409 秒，没有 ASan 报告。
- `tests/differential`（与 `tests/python` 分开的 pytest 进程，参照服务同样在预加载的进程中运行）：874 项全部通过，4799 秒，没有 ASan 错误报告。
- 合计约 1.6 小时，在 4 小时的预算内。不占用托管机器，没有 GitHub 运行号。构建与运行脚本不入库，本节结论只有本地证据：脚本、pytest 输出与 ASan 输出由负责人本地留存。
- 唯一的 ASan 输出是 6 行「failed to allocate 0x4000000000000029 bytes」警告，不是内存错误：来自 3.x 参照服务执行 `test_errors_fail_every_consumer_and_never_publish_a_delete` 中的 `j['s'] * 4611686018427387904`（字符串重复 2^62 次），3.x 按预期报 MemoryError（[0032](../decisions/0032-arithmetic-filters.md) 登记的差异），本实现在 64 MiB 上限处拒绝，不做这次分配。
- 覆盖范围：ASan 检测已插桩代码中的堆栈越界与释放后使用，不检测数据竞争，也不检测 std、CPython 与 grpcio C-core 内部的错误。为不干扰 faulthandler 设了 `allow_user_segv_handler=1`，野指针引起的 SIGSEGV 可能先由 faulthandler 处理而没有 ASan 报告，但会表现为测试失败；两套测试都没有失败。

### C：停服间隙进程退出时已应答的写入不丢

- 依据：WAL 追加直接 `write_all` 到文件，写入在应答客户端之前已交给内核；进程崩溃不会丢失内核中的数据。第一次崩溃的现场正是只有初始 manifest 与未清理的 WAL。
- 新增 [server.rs](../../crates/grpc/src/server.rs) 的 `process_exit_inside_stop_keeps_every_acknowledged_write`：子进程起服务，经 gRPC 应答 3 行写入，第 1 行之后 flush，后 2 行只在 WAL 中；然后在停服的两个间隙以 `process::exit` 直接退出（宽限等待中，由一个卡在引擎入口的在途请求保证服务任务在宽限内结束不了；runtime 关闭之后、`Db::close` 之前），不走析构与最后一次 flush。父进程确认退出码、WAL 非空（最后一次 flush 没有发生），重开后 3 行都在。子进程 60 秒内没有按预期退出时以另一个退出码自行结束，用例失败而不是挂起。flush 中 manifest 替换前后与 WAL 删除后的进程退出由 core 的 [crash_recovery](../../crates/core/tests/crash_recovery.rs) 覆盖。
- 测试二进制派生子进程，按[进程内测试隔离](process-test-isolation.md#后续grpc-库内单元测试2026-11-22)的做法加守卫。
- 验证（本地 macOS arm64，变异改动不提交）：子进程中把 WAL 截断为空时用例失败（只读回第 1 行）；让观察者不退出时子进程 60 秒后以超时退出码结束、用例失败；用例单独连跑 30 次通过；grpc 库测试按默认并行、普通与 fault-injection 构建各 20 轮通过。
- 范围：用例以干净退出模拟进程在这两个间隙死亡，覆盖的是「已应答的写入在内核中、进程随后消失」这一点。不覆盖内存破坏本身：若崩溃源于 H1、H2 那样的内存破坏，破坏发生在最后一次 flush 写文件的过程中时，对数据文件的影响无法由本用例推出。操作系统崩溃或断电时 WAL 在关闭前才 fsync（架构不变量 10，与 3.x 相同），也不在范围内。

### 结论

- **根因仍未查明。** A、B 都没有发现问题：上游没有对应的内存安全修复，停服路径没有可疑代码；ASan 下 `tests/python` 与差分共 1639 项、包括其中全部停服与重开，没有内存错误。与第一轮一样，现有证据不支持指定根因，自然发生仍只有 2026-10-23 的一次。
- **新增的证据**：进程在停服的两个间隙死亡时，已应答的写入在重开后仍在（C）；第一次崩溃的现场（最后一次 flush 之前，只有 WAL）属于这一情形。残余风险是：宿主进程在停服时崩溃（嵌入使用时用户的 Python 进程随之退出），下次打开时回放 WAL；若崩溃源于内存破坏且发生在最后一次 flush 写文件的过程中，对数据文件的影响没有覆盖。
- 常设的崩溃现场采集与第一轮的停止条件不变：任何再现即停止快进与发布，取原生栈转回根因调查。今后候选的风险接受是否据此改写，由负责人决定。
