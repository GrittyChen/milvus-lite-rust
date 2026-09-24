# milvus-lite-rust

The unofficial milvus-lite developed with Rust.

以官方 [milvus-lite](https://github.com/milvus-io/milvus-lite) 为行为规范，用 Rust 重写一个更高效、对 `pymilvus` 用户无感替换的 milvus-lite。目前处于设计阶段，尚无引擎代码。

## 文档

| 内容 | 位置 |
| --- | --- |
| 项目上下文、术语与全局规则（人与 coding agent 共用） | [AGENTS.md](AGENTS.md) |
| 设计决策 | [docs/decisions/](docs/decisions/)，首篇为 [0001 重构基线选择](docs/decisions/0001-refactor-baseline.md) |
| 与官方 2.5.1 / 3.x 对照的基线基准 | [benchmarks/baseline/](benchmarks/baseline/README.md) |

## 许可

[Apache License 2.0](LICENSE)
