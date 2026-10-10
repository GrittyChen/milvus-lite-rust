# 首发发行物验收

合同见 [0013](../../docs/decisions/0013-first-release-platform-and-packaging.md)。当前版本为开发候选，脚本不上传发行物。根 `pyproject.toml` 是唯一配置；从仓库根和 sdist 解包根执行 `maturin build`，也可以保留 `-m crates/py/Cargo.toml`。

## 构建与安装

```bash
# work_dir 必须是仓库外的全新目录。构建模式需要 uv、maturin 和实际目标解释器。
.venv/bin/python tests/packaging/run_acceptance.py --work-dir <work_dir> --suite
# 将这一轮构建出的同一个 wheel/sdist 复用于不同 Python / SDK；测试解释器不用装 maturin。
.venv/bin/python tests/packaging/run_acceptance.py --work-dir <test310> --python <python310> --sdk 2.6.17 --wheel <same.whl> --sdist <same.tar.gz> --suite
.venv/bin/python tests/packaging/run_acceptance.py --work-dir <test315> --python <python315> --sdk 3.0.2 --wheel <same.whl> --sdist <same.tar.gz> --suite
```

运行器使用目标默认 CPU、locked 构建，macOS 显式部署下限 11.0。实际 Linux 构建须在 glibc 2.28 环境执行，并使用 `maturin build --compatibility manylinux_2_28`；随后检查 ELF 版本依赖，不将重命名标签视为下限证明。

```bash
<python> tests/packaging/check_artifacts.py --wheel <wheel> --sdist <sdist> --repo <repo> --binary
<python> tests/packaging/check_artifacts.py --wheel <fi-wheel> --sdist <sdist> --repo <repo> --kind fi --binary
<python> -m pytest tests/packaging tests/platform
```

检查器核对版本、cp310/abi3 与平台标签、原生库架构及最低系统、两份许可证、console script、长描述源与安装/冲突说明、相对链接、未确认外链/项目 URL 元数据及禁入文件。正式 wheel 不得含测试入口，FI wheel 必须含隔离入口。归档与安装是两组检查；缺 SDK 的安装环境启动真实 CLI，由另一个 SDK 环境发 RPC，SDK 环境则验证 `.db` 写入、过滤、flush、删除、停止与重开。任一子步骤失败都不输出通过记录。复用模式要求 wheel/sdist 成对且已存在，先复制到本次独立目录，检查与安装都使用这份副本；不重建、不改变输入文件。输出中的 wheel SHA256 可用于核对各 Python / SDK 使用的是同一个产物。

## 源码与拒绝门槛

从 sdist 解包到仓库外，核对根配置、完整 Cargo 输入、Python 包及 proto 许可；分别由根入口、`-m` 入口构建，并以 pip/PEP 517 安装。检查器拒绝 sdist 中任何 `rust-toolchain.toml`；源码用户的工具链不由发行包固定，因此必须执行下面两套 MSRV 检查；用户的工具链不由开发固定版本替代。

```bash
PYO3_PYTHON=<gil-python> cargo +1.89.0 check --workspace --all-targets --locked
PYO3_PYTHON=<gil-python> cargo +1.89.0 check --workspace --all-targets --all-features --locked
PYO3_PYTHON=<free-threaded-python> cargo check -p milvus-lite-py --locked
PYO3_PYTHON=<pypy-python> cargo check -p milvus-lite-py --locked
PYO3_NO_PYTHON=1 cargo check -p milvus-lite-py --target x86_64-pc-windows-msvc --locked
```

后三条必须失败并匹配 `milvus-lite-rust build rejected:` 的对应原因，其它编译错误不算通过。Windows 未装 Rust 时，构建后端可能先报 cargo 缺失；用户需从长描述获知不支持。Windows core/grpc 静态门槛见 [平台流程](../platform/README.md)，不等于 Windows 可运行。

CPython 3.10、3.15 各跑两条 SDK 全套；3.11–3.14 用同一个正式 wheel 冒烟。Intel Mac 仅构建与两个端点冒烟，Rosetta 结果单独标注。资源缺口与实跑计数集中在 [首发验收记录](../../docs/testing/first-release-acceptance.md)，不将准备切片完成写成已发布。

公开项目地址尚未确认，完整 README（含代码示例、无协议 www. 地址）的 URL 主机仅接受 `apache.org` / `www.apache.org` 与本地 RPC 示例的 `127.0.0.1` / `localhost`。内联与引用式的文件相对链接均拒绝；出现 HTML `href` / `src` 属性即拒绝，元数据不接受 `Project-URL` / `Home-page`。以后确认公开地址时同步修订 0013、允许项和反证测试。
