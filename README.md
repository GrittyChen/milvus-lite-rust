# milvus-lite-rust

用 Rust 重写的 milvus-lite：`pymilvus` 代码不用改，照常 `MilvusClient("./demo.db")`，并能直接打开官方 milvus-lite 3.x 的数据目录。

本项目是**非官方**实现，与 Milvus / Zilliz **无隶属关系**。当前版本是预发布试用版，不建议用于生产；本项目没有发布到 PyPI，那里的同名包不是本项目发布的。源码：<https://github.com/GrittyChen/milvus-lite-rust>

## 特点

- **pymilvus 代码不用改**：与官方相同的 `milvus_lite` 模块和 `milvus-lite` 命令，支持 pymilvus 2.6 与 3.0；对外行为以官方 3.x 为准，差异逐项登记。
- **与官方 3.x 的数据目录双向互通**：沿用 3.x 的文件格式，两边可以交替打开同一个目录。
- **更快、更省内存**：服务端不受 GIL 与 Python 解释器开销限制，与 2.5.1、3.x 的同机对照见[性能](https://github.com/GrittyChen/milvus-lite-rust/blob/main/docs/usage.md#性能)。
- **只依赖 Rust**：不需要 FAISS、CMake 或系统 protoc。

## 安装

本包与官方发行包 `milvus-lite` 提供同名的模块和命令，**不能同时安装**，先卸载官方包，也**不要使用 `pymilvus[milvus-lite]`**（它会装回官方包）：

```bash
python -m pip uninstall milvus-lite
```

本包只从 GitHub Release 安装（`--no-index` 不去 PyPI 查找本包，`--only-binary :all:` 让不受支持的平台直接失败）：

```bash
python -m pip install pymilvus
python -m pip install --no-index --only-binary :all: --find-links https://github.com/GrittyChen/milvus-lite-rust/releases/expanded_assets/v0.7.0rc1 milvus-lite-rust==0.7.0rc1
```

可选：从 [v0.7.0rc1 Release](https://github.com/GrittyChen/milvus-lite-rust/releases/tag/v0.7.0rc1) 下载本平台的 wheel 与 `SHA256SUMS`，核对哈希后安装；再用 GitHub CLI 核对构建来源证明（依次为 Linux x86_64、Linux arm64、macOS arm64、macOS x86_64）：

```bash
shasum -a 256 -c --ignore-missing SHA256SUMS && python -m pip install --no-index ./milvus_lite_rust-0.7.0rc1-*.whl
gh attestation verify milvus_lite_rust-0.7.0rc1-cp310-abi3-manylinux_2_28_x86_64.whl --repo GrittyChen/milvus-lite-rust --signer-workflow GrittyChen/milvus-lite-rust/.github/workflows/release.yml
gh attestation verify milvus_lite_rust-0.7.0rc1-cp310-abi3-manylinux_2_28_aarch64.whl --repo GrittyChen/milvus-lite-rust --signer-workflow GrittyChen/milvus-lite-rust/.github/workflows/release.yml
gh attestation verify milvus_lite_rust-0.7.0rc1-cp310-abi3-macosx_11_0_arm64.whl --repo GrittyChen/milvus-lite-rust --signer-workflow GrittyChen/milvus-lite-rust/.github/workflows/release.yml
gh attestation verify milvus_lite_rust-0.7.0rc1-cp310-abi3-macosx_11_0_x86_64.whl --repo GrittyChen/milvus-lite-rust --signer-workflow GrittyChen/milvus-lite-rust/.github/workflows/release.yml
```

## 快速上手

```python
from pymilvus import MilvusClient

client = MilvusClient("./demo.db")
try:
    client.create_collection("docs", dimension=2, metric_type="L2")
    client.insert("docs", [dict(id=1, vector=[1.0, 0.0], category="tech")])
    print(client.query("docs", filter="category == 'tech'", output_fields=["id"]))
    print(client.search("docs", [[1.0, 0.0]], limit=1))
    client.flush("docs")
finally:
    client.close()
```

独立运行服务（参数与官方 3.x 相同；服务没有认证与 TLS，默认监听所有网卡）与导出数据：

```bash
milvus-lite server --data-dir ./demo.db --host 127.0.0.1 --port 19530
milvus-lite dump -d ./demo.db -c docs -p ./dump
```

### 导出 dump

`dump` 把一个集合导出为 Milvus BulkWriter 的 JSON 文件，用于导入 Milvus 或 Zilliz Cloud，需要先安装 `pymilvus[bulk_writer]`。两个命令的参数与限制见[使用说明](https://github.com/GrittyChen/milvus-lite-rust/blob/main/docs/usage.md)。

## 支持范围

覆盖 pymilvus 的常用功能：数据增删改查与 flush、标量与向量字段、向量检索与过滤、全文检索与混合检索、分区与多数据库管理等。各类索引定义都可以创建：HNSW 在后台建图检索，其余索引按精确检索与扫描执行。不支持的功能返回 `UNIMPLEMENTED`，不会静默成功。完整的支持范围、与 3.x 的差异、未实现清单与降级影响见[兼容性说明](https://github.com/GrittyChen/milvus-lite-rust/blob/main/docs/compatibility.md)。

## 平台

Linux x86_64 / arm64（glibc 2.28 及以上），macOS arm64 / x86_64（Intel，macOS 11 及以上）；GIL CPython 3.10–3.15。不支持 Windows、free-threaded CPython 与非 CPython 解释器；其他平台可从 sdist 自行构建（需要 Rust 1.89 或以上），没有支持承诺。

## 文档

- [使用说明](https://github.com/GrittyChen/milvus-lite-rust/blob/main/docs/usage.md)：安装细节、命令行与导出、停止与恢复、内存、性能、平台验收。
- [兼容性说明](https://github.com/GrittyChen/milvus-lite-rust/blob/main/docs/compatibility.md)：支持范围、与官方 3.x 的差异、索引、未实现清单、降级与已知限制。降级前须先用本实现正常打开并关闭一次。
- [发行说明](https://github.com/GrittyChen/milvus-lite-rust/tree/main/docs/releases)与[设计文档](https://github.com/GrittyChen/milvus-lite-rust/blob/main/docs/README.md)。

## 开发

在源码根目录执行 `python -m pip install .` 构建安装；开发环境与测试命令见 [AGENTS.md](https://github.com/GrittyChen/milvus-lite-rust/blob/main/AGENTS.md)。问题反馈：<https://github.com/GrittyChen/milvus-lite-rust/issues>。

## 许可

[Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0)。发行物随附本项目、proto 与内置 jieba 数据（MIT）的许可证副本，借用的协议与测试保留来源。
