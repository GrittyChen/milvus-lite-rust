# 关闭时显式解锁的验收

日期：2026-10-15。基线为测试隔离提交 `1e5ae42`；参照实现为 3.2.1 / `43d1257`。本项独立于过滤表达式切片 B，不改变数据格式、锁原语或依赖。

## 依据与取舍

参照实现 MilvusLite 的正常 close 先 `flock(LOCK_UN)` 再 close fd。本实现此前只关闭自身 fd，Unix 子进程尚未 exec 时持有同一个打开文件描述，可能使同库重开暂时返回 Locked。

采用 std 的 File::unlock，在既有排空和持久化收尾完成后显式解锁，再关闭 fd。接口自 Rust 1.89 稳定；没有增加等待重试或换成 fcntl 记录锁。测试隔离守卫保留，继续约束同一测试进程内无关用例的派生与关闭重开；不会串行化单个用例内部的并发。

## 出口与失败边界

| 收尾入口 | 解锁前必须完成 | 最终动作 |
| --- | --- | --- |
| Db::close | 等在途、正常存储收尾、维护排空、最后存储收尾 | 共用 release_data_dir，之后通知关闭等待者 |
| close_nowait 后台线程 | 等在途、停止维护、同步保留 WAL | 共用最终出口 |
| close_nowait 线程创建失败 | 同步完成同样的在途等待与 WAL 收尾 | 共用最终出口 |
| 最后一个 Db 引用析构 | 停止维护、同步保留 WAL | 共用最终出口 |

解锁失败写明数据目录和原错误，设置 shutdown_failed，然后关闭自身 fd；不吞错、不重试。显式 Server.stop 使用既有 StorageShutdown 返回错误，CLI 的退出处理与 atexit / release 的日志接收方式不变。3.x 忽略解锁错误，这项既有收尾报告差异仍保留。关闭 fd 有时也能释放锁，不能以此把显式解锁失败当作成功；仍有继承引用时，错误后的锁可能继续被持有。

显式解锁不放进 DataDir 析构。服务创建者的 Server.stop 在任何收尾前校验 PID，非创建者的 Server 析构泄漏继承状态；子进程不会通过这两个入口解开父进程的锁。裸 DataDir 析构只关闭自身 fd，共享引用仍在时保持锁。直接在 fork 后使用继承的 core Db 不属于进程内服务的 fork 契约，不能绕过宿主的所有权规则调用。

## 用例的判别能力

- core 单元测试保留一份 dup 描述符，覆盖正常 close、后台 close、Db 析构和线程创建失败回退；解锁前开库被拒，完成收尾后保留 dup 仍可重开。dup 与 fork 共享打开文件描述，能拦住只 close 的实现。
- 另一测试固定裸 DataDir 析构的边界：共享引用仍存活时不能拿到锁，最后一份引用关闭后才成功。
- 解锁失败随下一次成功打开的 DataDir 附带注入，使后台线程也能确定地遇到故障；覆盖同步 / 异步 / 同步回退的失败标志，并在 Server.stop 验证 StorageShutdown。注入接口与状态只存在于测试或 fault-injection 构建。
- 独立 Python 探针让真实 fork 子进程通过管道保持存活、不 exec；创建者停止服务后立即测试锁和重开。另一分支让子进程直接调用 NativeServer.stop 并析构句柄，必须拒绝 stop，父进程仍持锁；父进程关闭后才可重开。等待与子进程退出均有超时约束，不靠 sleep 或重试断言。
- 相同关闭探针在真实 3.x 的 MilvusLite.close 和 Rust NativeServer.stop 上逐项比较。修改前 wheel 在「停止之后锁应已释放」断言失败；修改后两端成功。原有在途 flush / RPC 停服测试继续检查收尾前不能提前释放 LOCK。

## 验证与边界

| macOS arm64 检查 | 结果 |
| --- | --- |
| Rust workspace 默认 | 284 passed |
| core fault-injection | 370 passed（顶层 368，子进程 2） |
| grpc all-features | 23 passed |
| fmt、两种 Clippy、MSRV 1.89 core --locked | 通过 |
| pymilvus 3.0.2 / 2.6.17 端到端 | 各 289 passed |
| 与真实 3.x 差分 | 两条 SDK 各 132 passed |
| 打包 / 平台脚本自检 | 170 passed |
| 当前 wheel / sdist 内容与原生库检查 | 通过 |

用于 Python 回归与探针的 macOS wheel 原生库 SHA256 为 `c08e006e048899753400c7362baf79a51c285a1f3498b138186decf6d5df7e98`；Linux 探针用的原生库为 `8272a56de69bc81171716de33e45d09353755e11096bea3ca3c3f3d946d77b79`。两者均为本次源码新构建的开发产物。

Linux arm64 在 manylinux_2_28 容器中使用独立源码，强制重编译 core；运行三项相关 core 单测、18 项 concurrency_faults、grpc all-features，并以当前新建 wheel 跑父进程关闭、子进程 stop / 析构保护和真实 3.x 三个探针。原始输出留在仓库外。

显式解锁改动之后，再以当前三个测试二进制按默认并行各连续运行 30 轮；没有设置单线程或使用重试。每轮核对退出码、实际 case 数、失败及忽略数。两个平台共 180 次运行全部通过：

| 二进制 | macOS arm64 | Linux arm64 |
| --- | --- | --- |
| index_changes | 25 项，30/30 轮通过 | 26 项，30/30 轮通过 |
| crash_recovery | 6 项，30/30 轮通过 | 6 项，30/30 轮通过 |
| concurrency_faults | 18 项，30/30 轮通过 | 18 项，30/30 轮通过 |

复验命令沿用 [测试隔离记录](process-test-isolation.md#重复验证命令)，本轮输出与测试隔离提交前的输出分别保留。有限次数的通过不能证明永远不会有偶发失败。

本轮没有实跑 Windows、Linux x86_64、真实断电或 rc3 完整候选矩阵；不把开发 wheel 当作已发布或已完成发布验收。切片 B 的引擎实现另行推进，公共 rc2 的 tag 和发行物保持不变。
