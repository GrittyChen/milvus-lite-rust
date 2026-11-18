# 差分测试与组件真值

本目录有两类内容：

- 差分测试 `test_*.py`：同一输入分别喂给参照实现与本实现，经真实 pymilvus 比对结果。运行命令与参照环境要求见 [AGENTS.md「常用命令」](../../AGENTS.md#常用命令)。
- 组件真值的生成与核验 `generate_*.py`，以及不经服务的直接模糊对照 `fuzz_*.py`：真值存为固定文件，供 Rust 单元测试读取，见 [crates/core/tests/fixtures/README.md](../../crates/core/tests/fixtures/README.md)。

改了某个组件就跑对应的核验。`--verify` 只核对、不覆盖已入库的真值；CI 中由 Compare 执行的项目见 [0020](../../docs/decisions/0020-continuous-integration.md)「对照层」第 3 条。

公共前提：
- `<3.x 的 venv>` 指参照实现以可编辑方式安装、HEAD 为 `1320d26` 的 CPython 3.12 解释器，按[互通 README](../storage_interop/README.md) 建成 Arrow 25 环境，装有 jieba 0.42.1；`<Arrow 15 的 venv>` 是同一 README 中 Arrow 15 的参照环境。
- 标「Arrow 15/25 各执行」的项目，还要用 `<Arrow 15 的 venv>` 再跑一次 `--verify`。其中布尔树、算术与动态列推断重新生成时，须在 Arrow 25 上执行并加 `--arrow15-python <Arrow 15 的 venv>/bin/python`；ARRAY 不接受这个参数，用 Arrow 25 重新生成后，再以 Arrow 15 核验。
- 本目录的差分测试与 [tests/python](../python/) 分别运行，不能合为同一个 pytest 进程。
- 模糊对照的报告放在仓库外。

## 固定真值核验

| 组件 | 命令 | 说明 |
| --- | --- | --- |
| S1 语法 | `<3.x 的 venv>/bin/python tests/differential/generate_filter_parse_fixtures.py --verify` | |
| 顶层动态过滤 | `<3.x 的 venv>/bin/python tests/differential/generate_dynamic_filter_fixtures.py --verify` | |
| 完整布尔树三路线 | `<3.x 的 venv>/bin/python tests/differential/generate_filter_tree_fixtures.py --verify` | Arrow 15/25 各执行 |
| 过滤算术 | `<3.x 的 venv>/bin/python tests/differential/generate_filter_arith_fixtures.py --verify` | Arrow 15/25 各执行（[0032](../../docs/decisions/0032-arithmetic-filters.md)） |
| hybrid 动态列推断 | `<3.x 的 venv>/bin/python tests/differential/generate_dynamic_infer_fixtures.py --verify` | Arrow 15/25 各执行，版本分歧条目分别核对（[0033](../../docs/decisions/0033-hybrid-dynamic-inference.md)） |
| ARRAY 元素转换、schema/default 与三路线 | `<3.x 的 venv>/bin/python tests/differential/generate_array_fixtures.py --verify` | Arrow 15/25 各执行；重新生成见上 |
| S1 Unicode 分类表与小写表 | `<3.x 的 venv>/bin/python tests/differential/generate_filter_unicode.py --verify` | 任一 CPython 3.12 均可（Unicode 15.0）；在仓库内执行，以使用固定工具链的 rustfmt |
| 时区换算 | `<3.x 的 venv>/bin/python tests/differential/generate_timezone_fixtures.py --verify` | 任一 CPython 3.12 均可，用真实 zoneinfo，含 `datetime` 范围两端（[0025](../../docs/decisions/0025-timestamptz-and-timezone.md)） |
| standard 分词 | `<3.x 的 venv>/bin/python tests/differential/generate_analyzer_fixtures.py --verify` | |
| jieba 内置数据与固定真值 | `<3.x 的 venv>/bin/python tests/differential/generate_jieba_data.py --verify`；`<3.x 的 venv>/bin/python tests/differential/generate_jieba_fixtures.py --jieba-test <sdist>/test/test.py --verify` | `test/test.py` 取自 sha256 固定的 jieba 0.42.1 sdist（[0036](../../docs/decisions/0036-jieba.md)） |
| GEOMETRY | `<3.x 的 venv>/bin/python tests/differential/generate_geometry_fixtures.py --verify` | 码位穷举约半分钟，读取它的 Rust 单测在 debug 下约 45 秒（[0034](../../docs/decisions/0034-geometry.md)） |
| 公共 function chain | `<3.x 的 venv>/bin/python tests/differential/generate_function_chain_fixtures.py --verify` | 同时需要 pymilvus 3.0.2；核对解码、校验与执行的结果，以及测试用的线格式编码与 SDK `to_proto()` 等价（[0035](../../docs/decisions/0035-function-chains.md)） |
| 空值过滤组件 | `<3.x 的 venv>/bin/python tests/compatibility_audit/schema_truth.py --reference references/milvus-lite --kind filters --output <仓库外报告> --verify-nullable-filter-fixture` | Arrow 15/25 各执行；工具见 [compatibility_audit](../compatibility_audit/README.md) |
| 分区可见性组件 | `cargo build -p milvus-lite-core --release --example partition_visibility_probe`，再 `target/release/examples/partition_visibility_probe verify-core tests/compatibility_audit/fixtures/partition_visibility_v321.json` | 不需要参照解释器；资源诊断见 [partition-visibility](../../benchmarks/partition-visibility/README.md) |

## 直接模糊对照

先构建探针，再用参照解释器跑对照脚本。

| 组件 | 构建 | 对照 |
| --- | --- | --- |
| ISO 时间 | `cargo build -p milvus-lite-core --example filter_parse_probe` | `<3.x 的 venv>/bin/python tests/differential/fuzz_temporal.py --cases 200000 --seed 29 --output <work_dir>/temporal.json` |
| 过滤算术（随机表与表达式覆盖三条路线） | `cargo build -p milvus-lite-core --example filter_arith_probe` | `<3.x 的 venv>/bin/python tests/differential/fuzz_arith.py --output <work_dir>/arith.json` |
| standard 分词 | `cargo build -p milvus-lite-core --example analyzer_probe` | `<3.x 的 venv>/bin/python tests/differential/fuzz_analyzer.py --cases 200000 --seed 29` |
| jieba（含用户词） | `cargo build -p milvus-lite-core --release --example jieba_probe` | `<3.x 的 venv>/bin/python tests/differential/fuzz_jieba.py --cases 200000 --groups 150 --seed 36 --output <work_dir>/jieba.json` |
| GEOMETRY（WKT 解析、五个谓词与 `hypot`；验收用种子 34 与 35 各跑一次） | `cargo build -p milvus-lite-core --example geometry_probe` | `<3.x 的 venv>/bin/python tests/differential/fuzz_geometry.py --cases 200000 --seed 34 --output <work_dir>/geometry.json` |
