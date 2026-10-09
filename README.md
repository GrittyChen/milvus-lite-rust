# milvus-lite-rust

The unofficial milvus-lite developed with Rust.

以官方 [milvus-lite](https://github.com/milvus-io/milvus-lite) 为行为规范，用 Rust 重写一个更高效的 milvus-lite：`pymilvus` 用户的代码无需修改，官方 3.x 的数据目录也能直接打开。本项目非官方，与 Milvus / Zilliz 无隶属关系。M1 工程骨架已完成；M2 已接通建表、insert、主键 Delete、Upsert、Flush、关闭、重开与基础读取，并已加入与 3.x 的关闭重开差分用例；Search 与主键 Query/Get 已改为短锁取得不可变视图、放锁后计算与编码；相邻小块合并与冻结代到 segment 的读取映射也已接通；不同名字的 collection 可并行懒打开，同名打开由名字锁协调，确定的 fail-closed 判定按名字缓存；S1 标量比较与 and 已接入 Query（含分页与 count）、Search 和 Delete，过滤求值使用不可变视图；既定 M2 范围的验收与性能阶段结果已有记录，正在补齐首发功能。目前尚不可用于生产，也未发布到 PyPI。

M2 崩溃矩阵已覆盖混合 data/delta WAL 写入后的进程退出、flush 与 CreateIndex 的 manifest replace 前后，以及同一代两类 WAL 删除之间及 create/drop 目录改名两侧的崩溃；并发、panic 与文件操作顺序的证据见 [M2 功能与故障验收记录](docs/testing/m2-correctness.md)。真实断电、平台行为与性能基准不由进程崩溃测试推断。

M2 最终验收候选与 Linux LOCK/strace 证据见[验收记录](docs/testing/m2-acceptance.md)，三方性能及修后复测见 [0001](docs/decisions/0001-refactor-baseline.md#m2-三方基准)。性能目标只部分达到，当前候选已通过范围内复审；这不代表首次发布或生产可用。

M2 性能阶段已按负责人确认的顺序收口，各专项结果和未达目标见[阶段总结](docs/testing/m2-performance-summary.md)。接下来补齐[首发最小范围](docs/decisions/0008-first-release-scope.md)：当前切片增加[顶层动态字段过滤](docs/decisions/0009-top-level-dynamic-filter.md)，支持裸字段比较/in 与 `$meta["key"]` 比较；嵌套 JSON 路径等仍显式拒绝。compaction、CLI、Windows 和完整发布验收尚未完成，生产限制保留。

## 文档

| 内容 | 位置 |
| --- | --- |
| 项目上下文、术语与全局规则（人与 coding agent 共用） | [AGENTS.md](AGENTS.md) |
| 设计决策 | [docs/decisions/](docs/decisions/)：[0001 重构基线选择](docs/decisions/0001-refactor-baseline.md)、[0002 数据目录格式](docs/decisions/0002-data-dir-compat.md)、[0003 进程内服务的生命周期](docs/decisions/0003-in-process-server-lifecycle.md)、[0004 M1 范围与验收](docs/decisions/0004-m1-scope.md)、[0005 检索线程模型](docs/decisions/0005-search-thread-model.md)、[0006 M2 范围与验收](docs/decisions/0006-m2-scope.md)、[0007 M2 存储引擎](docs/decisions/0007-m2-storage-engine.md) |
| 与官方 2.5.1 / 3.x 对照的基线基准 | [benchmarks/baseline/](benchmarks/baseline/README.md) |

## 许可

[Apache License 2.0](LICENSE)
