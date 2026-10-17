# 子进程与关闭重开用例的进程内隔离

日期：2026-10-14。基线为已评审的 load 修复 `17dd87a`。本记录只覆盖测试隔离与产品边界说明，不改变引擎、LOCK 原语或关闭协议。

本记录保留测试隔离提交时的证据。后续产品已改为排空后显式解锁，消除下面描述的正常关闭继承窗口；当前行为以 [0003](../decisions/0003-in-process-server-lifecycle.md#fork) 与 [显式解锁验收](explicit-unlock.md) 为准，完整用例守卫继续保留。

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
