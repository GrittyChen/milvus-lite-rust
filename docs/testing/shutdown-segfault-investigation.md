# 停服路径段错误调查

- 状态：调查记录，经负责人评审通过。**记录形成时未运行新证据**：只分析已有的运行日志、失败记录产物、仓库记录与源码，没有触发任何运行或探针。**本记录不等于实现或探针授权**：下文的定位方案和探针每个阶段都须逐项授权。
- 日期：2026-10-26。
- 依据：[0020 试点记录「试点后的拓扑调整 · 失败与根因」第 2 项](0020-ci-pilot.md#失败与根因-1)（显式风险接受，正式候选前须取得根因与修复或再次做发布级风险接受）；[0021 试点记录「尚未执行」](0021-release-pilot.md#尚未执行)。
- 范围：对照层运行 39948671536 的 job `diff-3.0.2 (ubuntu-24.04)`（job 120397551186）的日志与失败记录产物 `diff-3.0.2-ubuntu-24.04`；0020 试点记录中的复现尝试；此后各工作流的运行统计；`09653c2` 至 `4b1361e` 的产品源码。
- 证据：运行日志公开，有保留期限；失败记录产物需要仓库访问权限，2026-11-06 过期，其中的分片日志、进程记录与各用例数据目录是本记录唯一的现场材料。原始产物 zip（SHA256 与 GitHub 记录的 digest `264e95a4…4ee2b` 一致）、运行与 job 元数据及完整日志已由负责人本地留存。

## 已确认事实

1. **发生**：2026-10-23，`ci/release` 分支 `09653c2` 上的对照层运行 39948671536，attempt 1。`diff-3.0.2 (ubuntu-24.04)` 以 4 个分片并行执行差分套件（收集 421 项，分片 118 / 101 / 101 / 101），shard-3（101 项）进程以返回码 -11 退出，开始后约 233 秒；其余分片正常结束，汇总报告 `shard-3 退出码 -11`、shard-3 记录缺失、执行并集缺少 101 项。该运行共 11 个 job，9 个成功、2 个失败：失败的是这个差分 job 与汇总 `compare-pass`，其余 8 个对照工作 job 与 `changes` 成功。
2. **faulthandler 输出**（shard-3 日志）：`Fatal Python error: Segmentation fault`，`Current thread 0x00007f4729eb6b80`，Python 调用栈自顶向下为 `server_manager.py:85 _stop_locked`（即调用原生 `NativeServer.stop`）← `release_server` ← `tests/differential/test_reopen.py:67 close` ← `pair` fixture 的 teardown ← pytest。输出中只有这一个 Python 线程。已加载的扩展模块包括 `grpc._cython.cygrpc`、`google._upb._message` 与 numpy 各模块。
3. **崩溃在 teardown 中**：faulthandler 栈位于 `pair` fixture 的 teardown，即该用例的 call 阶段之后。分片日志在 `test_range_search.py` 打出 6 个点后崩溃；该分片的计划含 10 个 `test_range_search` 用例（5 个 `test_range_matrix_float64_window_output_and_sdk[*-COSINE]`、4 个 `test_search_iterator_empty_or_tied_scores_with_varchar_keys[*-COSINE]`、1 个 `test_v1_iterator_observes_changes_between_requests`）。basetemp 中 search iterator 用例只有一个编号目录 `test_search_iterator_empty_or_0`，另有 pytest 指向最新编号目录的 `test_search_iterator_empty_or_current`（上传产物时展开为目录，内容与 `_0` 相同），两者都是下一条所述的未 flush 状态。teardown 的顺序是先关闭两个 `MilvusClient`，再 `release_server` 停止本实现的服务，最后结束 3.x 参照子进程。
4. **崩溃时点早于最后一次 flush 的写盘**：上述目录的 `rust.db` 只有初始 manifest（version 1、无数据文件）与未清理的 `wal/wal_data_000001.arrow`；同一分片中正常停服的用例（如 `test_range_matrix_float64_wind0`、`test_query_iterator_order_pagi…`）都留下最后一次 flush 写出的 parquet，没有 WAL。正常停服在 `db.close()` 中完成最后一次 flush，因此崩溃发生在停服开始到 flush 写出第一个文件之间。
5. **当时没有原生栈**：崩溃现场采集在之后的 `dd86a57` 才加入，这次只有 faulthandler 的 Python 栈，没有 core。
6. **环境**：GitHub 托管 `ubuntu-24.04`（x86_64，4 vCPU / 16 GB），CPython 3.12.15，pymilvus 3.0.2，grpcio 1.84.0，protobuf 7.36.2，numpy 2.5.3；四个分片进程并行，每个差分用例另起一个 3.x 参照服务子进程。没有记录崩溃时刻的内存与 CPU 争用。`09653c2` 本身只改测试侧：测试驱动进程在导入 grpc 前关闭 grpcio 的 fork 处理（不涉及产品代码）。
7. **产品代码未变**：`09653c2` 与当前 `main`（`4b1361e`）之间 `crates/` 与 `Cargo.lock` 没有任何差异，此后所有运行执行的都是同一份原生代码。
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

## 定位方案（本轮只写方案，不触发）

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
