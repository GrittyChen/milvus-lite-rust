# M2 存储文件互通矩阵

本套件独立于 [差分测试](../differential/)：直接调用参照实现 v3.2.1（`43d1257`）的引擎 API、本仓 [Rust 文件测试入口](../../crates/core/examples/storage_interop.rs)及真实的 `MilvusClient("./x.db")` 入口。所有数据目录由 pytest 建在临时目录中；测试不修改 [参照 checkout](../../references/milvus-lite/)。

准备仓库外的两个 Python 环境；`<work_dir>` 也应在仓库外：

```bash
uv venv -p 3.12 <work_dir>/v3
uv pip install -p <work_dir>/v3/bin/python -e references/milvus-lite 'pymilvus==3.0.2' 'pyarrow==25.0.1' pytest
uv venv -p 3.12 <work_dir>/arrow15
uv pip install -p <work_dir>/arrow15/bin/python 'pyarrow==15.0.0' 'numpy==1.26.4'
```

运行（从仓库根目录）：

```bash
MILVUS_LITE_3X_PYTHON=<work_dir>/v3/bin/python \
MILVUS_LITE_ARROW15_PYTHON=<work_dir>/arrow15/bin/python \
MILVUS_LITE_ARROW25_PYTHON=<work_dir>/v3/bin/python \
.venv/bin/python -m pytest tests/storage_interop
```

缺少环境变量或解释器版本不符时套件直接失败，不跳过。覆盖：3.x 已 flush 目录与未提交 WAL → Rust；Rust 已 flush 目录与 WAL → 3.x；3.x 读取后继续写入 → Rust；pyarrow 15 与 25 各自读取 Rust 文件、重写同格式文件后由 Rust 读取。真实数据库路径还覆盖 3.x→Rust→3.x 交替写入、3.x 崩溃 WAL 的恢复、Rust 索引定义由 3.x 重开、同号 data/delta 的应用顺序、截断 WAL 的告警，以及 JSON／`$meta` 浮点平局值在 Parquet 中的逐字节一致性。文件矩阵与真实路径只覆盖当前切片，不能证明删除、upsert、过滤或 Linux／Windows 的持久化语义。
