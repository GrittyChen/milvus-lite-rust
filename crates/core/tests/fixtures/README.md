# 3.x 参照实现导出的测试数据

本目录的文件均由参照实现 milvus-lite v3.2.1（`43d1257`）生成，原项目按 Apache-2.0 许可发布。存储格式样本的来源代码分别为 `milvus_lite/schema/persistence.py`、`milvus_lite/storage/manifest.py`、`milvus_lite/db.py`、`milvus_lite/storage/wal.py`、`milvus_lite/storage/data_file.py`、`milvus_lite/storage/delta_file.py` 与 `milvus_lite/storage/snapshots.py`。

- `schema_3x.json`：collection `c`，INT64 主键 `id`、二维 FLOAT_VECTOR `vector`。
- `manifest_3x.json`：新 collection 的首次 manifest，版本 1，无数据文件。
- `wal_data_3x.arrow`：同一 WAL stream 中连续写入两个 batch，`(_seq, id)` 分别为 `(1, 10)` 与 `(2, 20)`，向量均为 `[1.0, 2.0]`。
- `data_3x.parquet`：一个 data 文件，`(_seq, id)` 为 `(1, 10)` 与 `(2, 20)`，向量分别为 `[1.0, 2.0]` 与 `[3.0, 4.0]`。
- `wal_delta_3x.arrow`：删除主键 10，`_seq=3`，`_partition=_all`。
- `delta_3x.parquet`：同一次删除落盘后的 delta 文件，列为 `id` 与 `_seq`。
- `snapshot_3x.json`：对含上述 data 与 delta 文件的 collection 创建的 snapshot 元数据，钉住两份文件。
- `schema_canonical_3x.json`、`manifest_canonical_3x.json`、`aliases_3x.json`：排序写出的字节样本，分别覆盖带默认值、非 ASCII 字符与 ARRAY / JSON / partition key 字段的 schema，多分区、文件列表与带浮点索引参数的 manifest，以及三个别名；`aliases_3x.json` 经 `milvus_lite/db.py` 的 `create_alias` 写出。

只保存格式测试必需的小文件；测试不会修改参照实现的 checkout。

[generate.py](generate.py) 直接调用固定 checkout 中的 schema、manifest、WAL、Parquet 与 snapshot 写入 API 重建样本，并逐项核对字段与内容；snapshot 的随机 ID 和创建时间不比较，三个排序写出样本逐字节比较。用参照实现的独立 Python 环境执行 `<v3>/bin/python crates/core/tests/fixtures/generate.py --verify`，不会覆盖已入库的样本。

过滤表达式的组件真值来自该版本的 `milvus_lite/search/filter/parser.py`、`semantic.py` 与 `eval.py`，供 core 的单元测试以 `include_str!` 读取。放在 crate 内，sdist 解包后才能独立编译和运行这些测试：

- `filter_parse_v321.json`：真实 parser 对语法分支、截断与固定种子词法变异的接受结果及诊断原文；部分用例种子取自参照实现的 `tests/search/filter/test_parser.py`。由 [generate_filter_parse_fixtures.py](../../../../tests/differential/generate_filter_parse_fixtures.py) 生成。
- `filter_dynamic.json`：顶层动态字段过滤在各类取值上的掩码与错误。由 [generate_dynamic_filter_fixtures.py](../../../../tests/differential/generate_dynamic_filter_fixtures.py) 生成。

两个生成脚本都要求可编辑安装的参照 checkout，加 `--verify` 时只核对、不覆盖；完整命令见 [AGENTS 常用命令](../../../../AGENTS.md#常用命令)。
