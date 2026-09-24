# milvus-lite-rust

The unofficial milvus-lite developed with Rust.

以官方 [milvus-lite](https://github.com/milvus-io/milvus-lite) 为行为规范，用 Rust 重写一个更高效的 milvus-lite：`pymilvus` 用户的代码无需修改，官方 3.x 的数据目录也能直接打开。本项目非官方，与 Milvus / Zilliz 无隶属关系。目前处于设计阶段，尚无引擎代码。

## 文档

| 内容 | 位置 |
| --- | --- |
| 项目上下文、术语与全局规则（人与 coding agent 共用） | [AGENTS.md](AGENTS.md) |
| 设计决策 | [docs/decisions/](docs/decisions/)：[0001 重构基线选择](docs/decisions/0001-refactor-baseline.md)、[0002 数据目录格式](docs/decisions/0002-data-dir-compat.md)、[0003 进程内服务的生命周期](docs/decisions/0003-in-process-server-lifecycle.md)、[0004 M1 范围与验收](docs/decisions/0004-m1-scope.md) |
| 与官方 2.5.1 / 3.x 对照的基线基准 | [benchmarks/baseline/](benchmarks/baseline/README.md) |

## 许可

[Apache License 2.0](LICENSE)
