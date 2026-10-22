# CI 辅助文件

范围、判据与不纳入 CI 的内容见 [0020](../../docs/decisions/0020-continuous-integration.md)；工作流为 [ci.yml](../workflows/ci.yml)（快速层）与 [compare.yml](../workflows/compare.yml)（对照层）。

## 固定依赖

[requirements/](requirements/) 下每个 `.in` 是一个环境的直接依赖，同名 `.txt` 是由它解析出的完整版本集合，CI 以 `uv pip install --no-build -r <name>.txt` 安装，只用 wheel。

| 环境 | 解释器 | 用途 |
| --- | --- | --- |
| `dev` | CPython 3.12 | 构建并安装本实现、pymilvus 3.0.2 线、工具自检、驱动分片工具 |
| `sdk26` | CPython 3.10 | 安装 wheel 后的 pymilvus 2.6.17 线 |
| `ref25` / `ref15` | CPython 3.12 | 参照 v3.2.1 的 Arrow 25 / Arrow 15 环境；参照实现的依赖取自其 `pyproject.toml`，checkout 另以 `--no-deps -e` 安装 |
| `build` | — | 可编辑安装参照时的构建依赖（hatchling 及其可编辑安装额外请求的 editables），经 `--build-constraint` 使用 |

更新任一 `.in` 或参照版本后，在仓库根目录按各 `.txt` 文件头记录的命令重新生成，并随同一提交评审。生成使用 CI 固定的 uv 版本，并以 `--exclude-newer` 截止到生成日前两周，避免刚发布的版本进入环境；`--universal` 使同一份结果在三个平台通用，但不保证每个平台都有 wheel：例如参照环境的 faiss-cpu 1.15.1 在三个平台都有 wheel，但 macOS arm64 的最低目标是 macOS 14，更换托管机器或依赖版本时须重新核对。

## 检查脚本

- [rust_results.py](rust_results.py)：cargo test 的汇总须存在，且失败与 ignored 均为 0；可选的第二个参数要求 passed 总数恰为该值，用于按名称筛选的运行。
- [pytest_summary.py](pytest_summary.py)：仅用于分片工具自身的自检，核对普通 pytest 的末行汇总。
- [unittest_summary.py](unittest_summary.py)：用于以 subtests 组织的自检，核对运行数量与末行 `OK`。
- [cargo_fresh.py](cargo_fresh.py)：互通预构建后复跑的全部产物须为 fresh。
