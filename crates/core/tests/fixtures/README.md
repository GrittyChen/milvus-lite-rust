# 3.x 存储格式测试数据

这些文件由参照实现 milvus-lite v3.2.1（`43d1257`）生成，原项目按 Apache-2.0 许可发布；来源代码分别为 `milvus_lite/schema/persistence.py`、`milvus_lite/storage/manifest.py`、`milvus_lite/storage/wal.py`、`milvus_lite/storage/data_file.py`、`milvus_lite/storage/delta_file.py` 与 `milvus_lite/storage/snapshots.py`。

- `schema_3x.json`：collection `c`，INT64 主键 `id`、二维 FLOAT_VECTOR `vector`。
- `manifest_3x.json`：新 collection 的首次 manifest，版本 1，无数据文件。
- `wal_data_3x.arrow`：同一 WAL stream 中连续写入两个 batch，`(_seq, id)` 分别为 `(1, 10)` 与 `(2, 20)`，向量均为 `[1.0, 2.0]`。
- `data_3x.parquet`：一个 data 文件，`(_seq, id)` 为 `(1, 10)` 与 `(2, 20)`，向量分别为 `[1.0, 2.0]` 与 `[3.0, 4.0]`。
- `wal_delta_3x.arrow`：删除主键 10，`_seq=3`，`_partition=_all`。
- `delta_3x.parquet`：同一次删除落盘后的 delta 文件，列为 `id` 与 `_seq`。
- `snapshot_3x.json`：对含上述 data 与 delta 文件的 collection 创建的 snapshot 元数据，钉住两份文件。

只保存格式测试必需的小文件；测试不会修改参照实现的 checkout。

[generate.py](generate.py) 直接调用固定 checkout 中的 schema、manifest、WAL、Parquet 与 snapshot 写入 API 重建样本，并逐项核对字段与内容；snapshot 的随机 ID 和创建时间不比较。用参照实现的独立 Python 环境执行 `<v3>/bin/python crates/core/tests/fixtures/generate.py --verify`，不会覆盖已入库的样本。
