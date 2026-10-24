# 子进程与关闭重开用例的进程内隔离

日期：2026-10-14。基线为已评审的 load 修复 `17dd87a`。本记录只覆盖测试隔离与产品边界说明，不改变引擎、LOCK 原语或关闭协议。

本记录保留测试隔离提交时的证据；2026-10-23 在 core 库内单元测试中发现的同类窗口及其修复见文末[后续：库内单元测试](#后续库内单元测试2026-10-23)。后续产品已改为排空后显式解锁，消除下面描述的正常关闭继承窗口；当前行为以 [0003](../decisions/0003-in-process-server-lifecycle.md#fork) 与 [显式解锁验收](explicit-unlock.md) 为准，完整用例守卫继续保留。

2026-10-17 的两条 Python SDK 线端口复用与 schema 缓存隔离另见[跨平台验收记录](0015-0016-cross-platform-acceptance.md#本轮发现两条-sdk-的跨用例-schema-缓存污染)；它由 [pytest 函数级隔离](../../tests/conftest.py)处理，与本文的 Rust 进程内锁守卫是两个独立边界。

## 问题与边界

复审方以单线程、默认并行、排除派生子进程用例三组对照定位了偶发 Locked：同一测试二进制中的其他线程派生子进程，可能在 exec 关闭 CLOEXEC 描述符之前保留已关闭数据库的 flock 引用，干扰关闭后的立即重开。已通过六次基线运行不能排除这个短窗口。

本轮独立的管道受控探针在 macOS / Linux 验证了原语行为：父进程关闭描述符之后、子进程尚未 exec 时，重新加锁返回 Locked；子进程 exec 后仍存活，但 CLOEXEC 描述符已关闭，重新加锁成功。探针使用独立临时文件，不作为完整服务或任意 subprocess 实现的时序证明。原语依据及产品说明统一见 [0002 约束 10](../decisions/0002-data-dir-compat.md#共享状态的语义约束)、[0003 fork](../decisions/0003-in-process-server-lifecycle.md#fork)。

在上述基线中，生产也可能出现这个窗口。本实现当时通过关闭自身描述符收尾，3.x 的正常 close 显式执行 LOCK_UN，二者在继承窗口里不同；测试隔离提交保持各自原行为，不增加隐式重试、不删除 LOCK、不改为其他锁原语。

## 修复与清扫

- `index_changes.rs`、`crash_recovery.rs` 为整个用例取得进程内 CASE_LOCK，覆盖建库、所有派生子进程、关闭和重开；用例内部写者 / 读者 / 提交者及受控等待不取这把守卫，继续真实并发。
- `concurrency_faults.rs` 已有同样的守卫，逐个测试入口核对后保持代码不变。
- 使用 parking_lot::Mutex，不因一个断言 panic 使后续用例因中毒跳过；两份新增守卫各有失败后再次取得守卫的测试。
- exec 后测试子进程拥有重新初始化的守卫，父子进程之间不使用它通信；故障退出的状态码和原有断言不变。`id`、`chmod` 及权限 / 崩溃 child 入口也纳入完整用例范围。

搜索 `Command::new|process::Command` 于 `crates/core/tests/*.rs`、`crates/core/src/*.rs` 与 `crates/core/src/storage/*.rs`，命中仅这三个 integration test 文件。逐个 `[test]` 入口核对第一条守卫，失败后无中毒自测作为无数据库操作的明确例外。对照差异确认没有移除用例、放宽断言或添加 retry / sleep。隔离单位是整个用例，未只锁一次 spawn 或一次 close。

## 实跑证据

先以 Cargo 的 `--no-run --message-format=json` 取得三份当前测试可执行文件，再分别连续启动 30 次。未设置 `--test-threads=1`，移除环境中的 RUST_TEST_THREADS 覆盖，保留各二进制默认并行调度；每轮核对退出 0、实际测试数量、失败 0、忽略 0。三个二进制作为独立进程运行，不共享描述符表或守卫。

| 二进制 | macOS arm64 | Linux arm64 / manylinux_2_28 |
| --- | --- | --- |
| index_changes | 25 项，30/30 轮通过 | 26 项，30/30 轮通过 |
| crash_recovery | 6 项，30/30 轮通过 | 6 项，30/30 轮通过 |
| concurrency_faults | 18 项，30/30 轮通过 | 18 项，30/30 轮通过 |

两个平台共 180 次完整二进制运行。Linux 使用独立源码与 target，构建并行数 4，强制重新编译 core 并确认编译路径；旧日志、隔离前产物和模拟值未混入表中。权限用例包含普通用户子进程，case 数量的平台差别来自既有 macOS ACL 与 Linux 凭据探测。

其他检查：Rust workspace 默认 281 passed；core fault-injection 367 passed（顶层 365，子进程结果 2）；fmt、两种 Clippy 通过；打包 / 平台脚本自检 170 passed；实际 wheel / sdist 与原生库检查通过，原生库 SHA256 与 load 修复构建一致。没有重跑 Python 功能与差分套件：本轮产品代码和运行期依赖未改，README 的发行长描述另经实际归档检查。

未实跑 Windows 或 Linux x86_64 的本轮重复矩阵，不把这里的有限重复次数声明为永远不存在偶发失败。以上属于测试隔离的基线；后续产品对齐与其验证边界见 [显式解锁验收](explicit-unlock.md)，切片 B 的引擎实现另行推进。

## 重复验证命令

从仓库根目录执行下列命令；默认并行，任意一轮失败立即报错，不使用重试隐藏失败：

```python
import os
import subprocess

env = dict(os.environ)
env.pop("RUST_TEST_THREADS", None)
for round_number in range(1, 31):
    print(f"round {round_number}/30", flush=True)
    subprocess.run([
        "cargo", "test", "-p", "milvus-lite-core", "--features", "fault-injection",
        "--test", "index_changes", "--test", "concurrency_faults", "--test", "crash_recovery",
    ], env=env, check=True)
```

该命令复验相同范围；本轮实跑使用已编译的三份可执行文件，省去每轮 Cargo 的构建检查，原始输出保留在仓库外。

## 后续：库内单元测试（2026-10-23）

### 发现

0021 快速层计数中，提交 `59d3a5f` 的第 4 次运行（GitHub Actions 39819137221）在 rust-fi（ubuntu-24.04）失败：core 库内单元测试 `collection::partition_key::tests::old_snapshot_survives_bucket_write_while_close_and_new_reads_wait_for_permission` 关闭后重开数据目录时得到 `Locked`。同一次运行的其余 10 个 job 成功；计数按 0020 停止，没有重跑。

上文的清扫只覆盖当时的进程派生点，库内单元测试那时没有派生子进程。之后 partition key 的崩溃边界用例 `crash_boundaries_recover_only_actual_wal_prefix` 在库内以 `Command` 重新启动测试二进制，库内测试从此处在与上文相同的窗口中：该用例派生子进程时，子进程在 exec 之前继承其他用例此刻打开的描述符；失败用例先以裸 `DataDir` 打开并直接析构（只关闭自身描述符、不解锁），随后的重开撞上子进程持有的 flock。

### 确定性复现

在失败用例的裸 `DataDir` 打开期间，由另一线程派生一个在 exec 前停留 500 ms 的子进程，再析构、重开：macOS arm64 上 3/3 次在同一位置得到与 CI 相同的 `Locked`。复现改动不提交。

### 修复与清扫

- 新增仅测试编译的进程内读写锁 [`data_dir::SUBPROCESS_SPAWN`](../../crates/core/src/data_dir.rs)。崩溃边界用例改为 `stdin` 空、`stdout` / `stderr` 管道后，只在写锁内调用 `spawn()`，退出写锁后再 `wait_with_output()`；std 在子进程 exec 之后才从 `spawn()` 返回，写锁覆盖 fork 到 exec 的窗口，不覆盖子进程运行的数秒。原有退出码与输出断言不变。
- 依赖裸句柄析构释放锁、随后重开的 5 个用例，从首次打开到最后一次重开持读锁：partition key 的上述失败用例；`data_dir` 的 `second_open_in_same_process_is_locked_until_first_dropped`、`dropping_a_bare_handle_does_not_unlock_a_shared_description`、`lock_release_does_not_depend_on_existing_entries`；`db` 的 `unlock_failure_is_reported_by_synchronous_and_asynchronous_close`（注入解锁失败后只能靠关闭全部描述符释放锁，按循环的每次迭代取读锁）。读锁只让这些用例与派生互斥，用例内部的线程与真实并发不变。
- 经 `Db` 正常关闭的用例不需要守卫：`Db` 收尾时显式解锁（[db.rs](../../crates/core/src/db.rs) 的 `release_data_dir`），对子进程继承的同一打开文件描述同样生效；`all_close_paths_unlock_while_a_duplicated_descriptor_remains_open` 的解锁成功路径即为这一点的覆盖。
- 搜索范围：`Command::new|process::Command|current_exe|fork(` 于整个 workspace 的 `crates/**/*.rs`。库内只有 [partition_key.rs](../../crates/core/src/collection/partition_key.rs) 一个写者；集成测试中 `index_changes.rs`、`crash_recovery.rs`、`concurrency_faults.rs` 由上文的 CASE_LOCK 覆盖；`partitions.rs` 有两个派生点，但该二进制只经 `Db` 打开数据目录、不使用注入的解锁失败，`Db` 关闭总是显式解锁，不存在依赖裸句柄析构的重开，因此不加守卫。grpc 与 py crate 没有派生点。
- 没有增加 retry、sleep 或放宽断言；产品代码未改。

### 验证

以下均在 macOS arm64、修复后的源码上执行：

- 可控探针：沿用上面的复现时序，探针的子进程改为在写锁内 `spawn()`，10/10 次通过；不加写锁的同一探针作为对照，10/10 次得到 `Locked`。
- `cargo test -p milvus-lite-core --lib --features fault-injection --locked` 连续 30 轮，保留默认测试线程数并移除 `RUST_TEST_THREADS` 覆盖：30/30 轮 219 passed、0 failed、0 ignored。
- `cargo test --workspace --locked` 默认并行：361 passed、0 failed、0 ignored。
- `cargo fmt --all --check`、默认与完整 FI（`--features milvus-lite-py/fault-injection`）两种 Clippy、`git diff --check` 通过。

Linux 上的结果由修复提交推送后 CI 的 rust-fi job 验证；本地 30 轮只是 macOS 的结果，不作为跨平台结论，也不声明窗口在任何调度下都不再出现。

## 后续：测试驱动进程中 grpcio 的 fork 处理（2026-10-23）

### 发现

0020 试点后的拓扑调整（提交 `a01c4cb`）验证中，临时分支 `tmp/docs-after-code`（head `6087aeb`，在 `a01c4cb` 上追加一个纯文档提交）的快速层运行 39931684921 在 `python-3.0.2 (macos-15)` 失败：`tests/python/test_server_manager.py::test_lock_held_by_other_process_returns_none` 启动的持锁子进程没有输出 `ready`，父进程 64 秒后读到空行；持锁子进程本身没有任何输出。同一运行其余 job 成功。计数随即停止，`a01c4cb` 上已有的运行不计入。

用例结束时，一个刚 fork 出的进程在 3 毫秒内向捕获的 stderr 打印 333 行 grpcio 的 `FD from fork parent still in poll list`。持锁子进程不加载 grpcio，这只能来自 pytest 进程 fork 之后、exec 之前的子进程。

### 机制

pytest 进程经 pymilvus 加载 grpcio（1.84.0）。grpcio 核心在 `grpc_init` 时读取一次 `GRPC_ENABLE_FORK_SUPPORT`，默认注册 `pthread_atfork`；Python 层自己的 fork 支持默认关闭，不影响核心这一层。于是测试进程每次经 `subprocess` 派生子进程，子进程在 exec 之前都会恢复 gRPC 的线程、轮询父进程的描述符。这些线程可能触发致命断言而 abort；abort 早于 exec 时子进程无声退出，`subprocess` 把它当作 exec 成功，父进程读到空输出。fork 发生在 64 秒的末尾，fork 前的等待推断来自 gRPC 的 prefork 处理，未单独证实。

### 本机对照

macOS arm64，带活动服务与 channel 的进程每轮 600 次 `Popen`（探针在仓库外）：

| 配置 | 子进程中的致命断言 `Check failed: wakeup_fd_->ConsumeWakeup().ok()` | `FD from fork parent` 日志 |
| --- | --- | --- |
| 默认，空闲（两轮） | 6、0 | 8879、5419 行 |
| 默认，60 个遗留服务与客户端 | 0 | 74251 行 |
| 默认，后台并发调用 | 0（gRPC 跳过 fork 处理） | 5256 行 |
| 本进程关闭 fork 处理后从环境删除变量 | 0 | 0 |

本机没有观察到子进程死在 exec 之前（abort 都晚于 exec），也没有复现 64 秒的等待；以上对照证明 fork 处理在 exec 前的子进程中运行并能触发致命断言，关闭后消失，不是失败瞬间的确定性复现。

### 修复与清扫

- 新增 [tests/grpc_fork.py](../../tests/grpc_fork.py)：在首次导入 grpc 之前设置 `GRPC_ENABLE_FORK_SUPPORT=0`、导入 grpc 完成核心初始化，随后恢复变量原值。只改变本进程；exec 出的子进程（包括验证 fork 行为的探针）看不到该变量，按默认配置初始化自己的 gRPC。grpc 已先导入时报错，不假装生效。
- 根 [tests/conftest.py](../../tests/conftest.py) 先于各子目录 conftest 与测试模块执行它，覆盖 tests 下全部 pytest 进程；不需要 SDK 的工具自检环境可以没有 grpcio。
- 同类清扫：以 AST 找出自身导入 pymilvus 或 grpc、又派生子进程的模块。pytest 进程由根 conftest 覆盖；独立驱动 [linux_pid1_probe.py](../../tests/platform/linux_pid1_probe.py)（发行工作流直接执行）与 compatibility_audit 的 `partition_boundaries`、`partition_key_truth`、`partition_truth`、`partition_visibility_truth`、`query_iteration_truth`、`range_truth`、`rollback_probe` 在导入任何会加载 grpc 的模块之前调用同一函数。[install_smoke.py](../../tests/packaging/install_smoke.py) 只在 SDK 分支导入 pymilvus，之后不派生子进程，不改。benchmarks 下的驱动不在本次范围：`baseline/bench.py` 与 `concurrency.py` 的 multiprocessing 子进程 fork 后直接使用 gRPC，需要 fork 处理；`compaction` 与 `json-write/diagnostics` 的驱动另行评估。
- 测试 [tests/ci/test_grpc_fork.py](../../tests/ci/test_grpc_fork.py)：pytest 进程按是否安装 grpcio 走对应路径（开发环境须实际完成初始化），环境中没有该变量、exec 的子进程看不到它；未安装时 `required=False` 返回不可用、`required=True` 报错；顶层包存在而包内模块缺失时原样失败，不当作未安装；grpc 已先导入时失败；调用前的原值原样恢复。
- 没有增加 retry、sleep 或放宽断言；产品代码未改。

### 验证

macOS arm64（修复后的源码，开发环境 CPython 3.12、pymilvus 3.0.2、grpcio 1.84.0）：

- 同一探针改为先调用 `grpc_fork.disable_fork_handlers()`：空闲与 60 个遗留服务两种模式各 600 次 `Popen`，致命断言与 fork 日志均为 0；同轮默认配置分别有 5419 与 74251 行 fork 日志。
- 由修复后的进程以 `subprocess` 启动默认探针，30 次 fork 产生 655 行 fork 日志：exec 出的子进程恢复默认 fork 支持；同样启动的修复版探针为 0。
- 按 CI 的方式经分片工具运行 tests/python：473 项通过。
- `tests/python/test_server_manager.py` 整个文件默认并行连续 30 轮（含目标用例、全部 `test_fork_*` 与继承 LOCK 场景）：30/30 轮 21 passed，全部日志中没有 fork 日志或致命断言。
- tests/ci 43 项、tests/release 200 项、tests/platform 13 项通过；只装 pytest、没有 grpcio 的隔离 venv 中 tests/ci 43 项同样通过。去掉恢复原值、未安装时总是报错、去掉先导入检查、把包内模块缺失当作未安装，四个变异均被检出。

Linux arm64（本机 Docker 中固定摘要的 manylinux_2_28 容器，glibc 2.28，内核 7.0.14-linuxkit；CPython 3.12、pymilvus 3.0.2、grpcio 1.84.0；安装 `78d1f6f` 演练构建的 Linux aarch64 正式 wheel，此后产品代码未变）：

- 同一探针在默认与修复两种配置下均没有 fork 日志或致命断言：Linux 上 grpcio 使用 epoll 引擎，不走打印该日志的 poll 引擎，现象在此观察不到，Linux 结果只说明修复不改变其他行为。
- 在仓库外目录运行 tests/python：473 项通过；`test_server_manager.py` 默认并行连续 30 轮：30/30 轮 21 passed。

修改过的 8 个独立驱动以 `--help` 经过初始化与全部模块导入，compatibility_audit 自检 22 项通过；这些驱动的真值生成需要 3.x 参照环境，本次没有实际重跑。

修复后的 [linux_pid1_probe.py](../../tests/platform/linux_pid1_probe.py) 在本机 arm64 Docker 上以开发环境为驱动、本地 `python:3.10-slim-bookworm` 为基础镜像，安装同一演练的 Linux aarch64 正式与 FI wheel：normal / drain / kill 退出码 0 / 0 / 137，三项均通过，驱动日志中没有 fork 日志。容器内的服务进程不经过本函数，按默认配置运行。

CI 上的结果以修复提交在最终拓扑上的 10 / 5 计数为准，计数完成后记入 [0020 试点记录](0020-ci-pilot.md)。本地重复只是有限次数，不声明该窗口在任何调度下都不再出现。
