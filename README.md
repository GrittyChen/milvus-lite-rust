# milvus-lite-rust

The unofficial milvus-lite developed with Rust.

以官方 [milvus-lite](https://github.com/milvus-io/milvus-lite) 为行为规范，用 Rust 重写一个更高效的 milvus-lite：`pymilvus` 用户的代码无需修改，官方 3.x 的数据目录也能直接打开。本项目非官方，与 Milvus / Zilliz 无隶属关系。M1 工程骨架已完成；M2 已接通建表、insert、主键 Delete、Upsert、Flush、关闭、重开与基础读取，并已加入与 3.x 的关闭重开差分用例；Search 与主键 Query/Get 已改为短锁取得不可变视图、放锁后计算与编码；相邻小块合并与冻结代到 segment 的读取映射也已接通；不同名字的 collection 可并行懒打开，同名打开由名字锁协调，确定的 fail-closed 判定按名字缓存；S1 标量比较与 and 已接入 Query（含分页与 count）、Search 和 Delete，过滤求值使用不可变视图；既定 M2 范围的验收与性能阶段结果已有记录，正在补齐首发功能。目前尚不可用于生产，也未发布到 PyPI。

M2 崩溃矩阵已覆盖混合 data/delta WAL 写入后的进程退出、flush 与 CreateIndex 的 manifest replace 前后，以及同一代两类 WAL 删除之间及 create/drop 目录改名两侧的崩溃；并发、panic 与文件操作顺序的证据见 [M2 功能与故障验收记录](docs/testing/m2-correctness.md)。真实断电、平台行为与性能基准不由进程崩溃测试推断。

M2 最终验收候选与 Linux LOCK/strace 证据见[验收记录](docs/testing/m2-acceptance.md)，三方性能及修后复测见 [0001](docs/decisions/0001-refactor-baseline.md#m2-三方基准)。性能目标只部分达到，当前候选已通过范围内复审；这不代表首次发布或生产可用。

M2 性能阶段已按负责人确认的顺序收口，各专项结果和未达目标见[阶段总结](docs/testing/m2-performance-summary.md)。接下来补齐[首发最小范围](docs/decisions/0008-first-release-scope.md)：当前切片增加[顶层动态字段过滤](docs/decisions/0009-top-level-dynamic-filter.md)，支持裸字段比较/in 与 `$meta["key"]` 比较；嵌套 JSON 路径等仍显式拒绝。后台 compaction 与整份 tombstone GC 已接入，采用[0010](docs/decisions/0010-compaction-and-tombstone-gc.md) 的单 Db 维护线程和[0011](docs/decisions/0011-committed-delete-state.md) 的独立已提交删除状态，本轮实现与修复已通过评审，证据见[实施验收记录](docs/testing/compaction-correctness.md)；CLI `server` 已接入，Windows 和首次发布验收仍未完成，生产限制保留。

## 已知限制

- 正常停服/脚本退出会排空已接受的后台合并，可能额外等待若干轮；RPC 的两秒宽限不是整个停服时限。Ctrl-C 可能直到原生等待返回后才由 Python 处理。退出等待测量口径见[合并条件验收](benchmarks/compaction/README.md)，观测值不能作为阻塞 I/O 的绝对上界。
- SIGKILL 不执行正常停服；按 WAL/manifest 协议，下次打开恢复已返回成功的写入。这是进程崩溃恢复保证，不等于真实断电证据。
- 维护线程串行处理同一 Db 的 collection；文件大小桶受 Parquet 编码器影响，已登记的并发/布局差异见 0010。历史旧 WAL 风险和跨选中/未选中来源的同主键同序号歧义会延后或拒绝维护，保留诊断。
- 合并成功诊断默认静默；需要观察单轮及发布锁耗时时，在服务启动前设置 `MILVUS_LITE_COMPACTION_DIAGNOSTICS=1`，口径见[合并条件验收](benchmarks/compaction/README.md)。
- 独立已提交删除表增加与 tombstone 数和主键长度有关的内存；额外占用需与回收效果一起看，不能用普通无删除数据的 RSS 代表删除重负载。

## 文档

| 内容 | 位置 |
| --- | --- |
| 项目上下文、术语与全局规则（人与 coding agent 共用） | [AGENTS.md](AGENTS.md) |
| 设计决策 | [docs/decisions/](docs/decisions/)：[0001 重构基线选择](docs/decisions/0001-refactor-baseline.md)、[0002 数据目录格式](docs/decisions/0002-data-dir-compat.md)、[0003 进程内服务的生命周期](docs/decisions/0003-in-process-server-lifecycle.md)、[0004 M1 范围与验收](docs/decisions/0004-m1-scope.md)、[0005 检索线程模型](docs/decisions/0005-search-thread-model.md)、[0006 M2 范围与验收](docs/decisions/0006-m2-scope.md)、[0007 M2 存储引擎](docs/decisions/0007-m2-storage-engine.md) |
| 与官方 2.5.1 / 3.x 对照的基线基准 | [benchmarks/baseline/](benchmarks/baseline/README.md) |

## 独立 CLI server

安装本包后可启动独立服务，使用 0012 的已确认生命周期与线程配置：

```bash
milvus-lite server --data-dir ./demo.db --host 127.0.0.1 --port 19530 --max-workers 10
# 等价模块入口
python -m milvus_lite server --data-dir ./demo.db --host 127.0.0.1 --port 19530 --max-workers 10
```

客户端使用 `MilvusClient(uri="http://127.0.0.1:19530")`。data-dir 不要求 .db 后缀。`--port 0` 自动分配，ready 行显示实际地址；hostname 只绑定解析结果中首个成功 bind 的地址，IPv6 使用括号显示，应按实际地址连接。`--max-workers` 控制引擎 RPC 的阻塞处理并发，计算池与维护线程独立；嵌入服务省略该参数时保持 512 上限。

IPv6 监听已通过 Milvus 协议直连验证；本次验证的 pymilvus 2.6.17/3.0.2 存在 IPv6 URI 重组问题，用这两条 SDK 时请显式绑定 `127.0.0.1`。

默认 host 为 `0.0.0.0`，监听所有网卡；当前没有认证/TLS。仅本机使用应显式指定 `--host 127.0.0.1`。目前仅提供 server，dump 后续补齐。

首次 SIGINT/SIGTERM 请求正常关闭，Windows 的新进程组 CTRL_BREAK_EVENT/SIGBREAK 按单独的平台验收处理。RPC 宽限为 5 秒，之后仍等待已入场引擎任务、flush 和后台维护排空；这是总等待的一部分，不是退出期限。宽限结束后未入场的排队写入不执行，已入场但客户端未收到结果的请求仍可能生效。

进入原生 stop 前，CLI 将停止信号设为 SIG_DFL；普通进程后续信号可使其异常终止。若启动 CLI 的包装进程会转发 SIGINT，一次 Ctrl-C 可能变成两次，导致非零退出并打断维护排空，数据依赖进程崩溃恢复；这不是断电保证。Linux PID 1 和 Windows 的默认终止行为有特殊限制，监督器可能在外部期限后强杀，详见 [0012](docs/decisions/0012-cli-server.md)。

argparse 的语法与数值参数错误退出 2；host 格式校验失败（例如 `[bad]`）、启动和停止错误退出 1。数值与括号地址格式校验在创建资源前完成；DNS 解析和 bind 失败发生在打开数据目录之后，可能已创建目录。正常关闭因目录持久化未确认而跳过最终 flush，以及存储收尾的 flush、WAL 同步/清理或 panic，都会记录失败标志，显式 stop 返回错误、CLI 退出 1。正常停止退出 0 仍不宣称所有平台/故障情况下全部文件已确认持久化；Drop 无接收者仍记录日志。CLI 的代码验收与平台未验证范围见[CLI 实施记录](docs/testing/cli-correctness.md)。

## 许可

[Apache License 2.0](LICENSE)
