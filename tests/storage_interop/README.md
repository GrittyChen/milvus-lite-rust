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

缺少环境变量或解释器版本不符时套件直接失败，不跳过。覆盖：3.x 已 flush 目录与未提交 WAL → Rust；Rust 已 flush 目录与 WAL → 3.x；3.x 读取后继续写入 → Rust；pyarrow 15 与 25 各自读取 Rust 文件、重写同格式文件后由 Rust 读取。真实数据库路径还覆盖 3.x→Rust→3.x 交替写入、Rust 删除与部分 upsert 后由 3.x 重开、3.x 崩溃 WAL 的恢复、Rust 索引定义由 3.x 重开、3.x 自身无法 build 的索引定义（已提交、全部删除、只有 WAL）由两侧重开并 load、同号 data/delta 的应用顺序、截断 WAL 的告警，以及 JSON／`$meta` 浮点平局值在 Parquet 中的逐字节一致性。文件矩阵与真实路径只覆盖当前切片，不能证明完整崩溃矩阵、过滤或 Linux／Windows 的持久化语义。

WAL 故障矩阵另覆盖 data/delta 多批流的每个字节截断位置、中间消息格式损坏与零填充：直接运行参照实现 v3.2.1（43d1257）的 `_read_wal_file`，并以 pyarrow 15/25 完整校验，对照 Rust 的批前缀和内容。结构仍有效的向量符号位变化被双方接受，用例不把完整校验误称为校验和；真实 3.x 目录的 schema 整数宽度位翻转另证明未捕获的 ArrowNotImplementedError 会使 collection 访问失败。

真实客户端还读取 Rust 在 ENOSPC 部分追加失败后退出留下的两类目录：失败恢复 flush 完成、WAL 已清理，以及恢复 flush 预同步失败、半截 WAL 仍在盘上。用例先断言 WAL 文件集合，再分别检查 count/get/主键 query/search。ENOSPC 来自测试构建的 File 层注入，发布 wheel 不包含该接缝。
