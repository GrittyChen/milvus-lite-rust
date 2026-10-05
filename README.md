# milvus-lite-rust

The unofficial milvus-lite developed with Rust.

以官方 [milvus-lite](https://github.com/milvus-io/milvus-lite) 为行为规范，用 Rust 重写一个更高效的 milvus-lite：`pymilvus` 用户的代码无需修改，官方 3.x 的数据目录也能直接打开。本项目非官方，与 Milvus / Zilliz 无隶属关系。M1 工程骨架已完成；M2 已接通建表、insert、主键 Delete、Upsert、Flush、关闭、重开与基础读取，并已加入与 3.x 的关闭重开差分用例；Search 与主键 Query/Get 已改为短锁取得不可变视图、放锁后计算与编码；完整 M2 验收仍在进行。目前尚不可用于生产，也未发布到 PyPI。

M2 崩溃矩阵已覆盖混合 data/delta WAL 写入后的进程退出、flush 与 CreateIndex 的 manifest replace 前后，以及同一代两类 WAL 删除之间及 create/drop 目录改名两侧的崩溃；其他持久化点仍待验收。

## 文档

| 内容 | 位置 |
| --- | --- |
| 项目上下文、术语与全局规则（人与 coding agent 共用） | [AGENTS.md](AGENTS.md) |
| 设计决策 | [docs/decisions/](docs/decisions/)：[0001 重构基线选择](docs/decisions/0001-refactor-baseline.md)、[0002 数据目录格式](docs/decisions/0002-data-dir-compat.md)、[0003 进程内服务的生命周期](docs/decisions/0003-in-process-server-lifecycle.md)、[0004 M1 范围与验收](docs/decisions/0004-m1-scope.md)、[0005 检索线程模型](docs/decisions/0005-search-thread-model.md)、[0006 M2 范围与验收](docs/decisions/0006-m2-scope.md)、[0007 M2 存储引擎](docs/decisions/0007-m2-storage-engine.md) |
| 与官方 2.5.1 / 3.x 对照的基线基准 | [benchmarks/baseline/](benchmarks/baseline/README.md) |

## 许可

[Apache License 2.0](LICENSE)
