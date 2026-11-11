# BM25 全文检索专项

合同见 [0031](../../docs/decisions/0031-bm25.md)。只写仓库外不存在的新目录，不下载、安装依赖或进入默认测试。

```bash
OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 VECLIB_MAXIMUM_THREADS=1 \
  "$BENCH_PYTHON" benchmarks/bm25/run.py --work-dir "$BENCH_WORK/<新目录>" --impl <标签> --out "$BENCH_WORK/results.jsonl"
```

同机、同 Python 与 SDK（pymilvus 3.0.2），同一脚本由三套环境的解释器分别运行：本实现的 release 构建、参照 3.x（`1320d26` 的可编辑安装，NumPy 2）与 milvus-lite 2.5.1。2.5.1 支持同样的写法（BM25 function、`SPARSE_INVERTED_INDEX` + BM25、以文本检索），已实测。文本由种子生成，三方相同：10 万行，每行 10–40 个词，词按幂律（指数 1.1）取自 2 万个形如 `w123` 的词；查询 2–5 个词。定义 `SPARSE_INVERTED_INDEX` + BM25，k1 / b 取缺省值。

计时项：分 10 批写入（含服务端生成稀疏向量）、flush、load 后的首次检索（本实现与 3.x 在此时为每个 segment 建倒排）、50 条 nq=1 的热查询（中位数与 p95，前两条预热）、9 次 nq=32、20 条带过滤（一半行）的 nq=1，结束时进程树的 RSS（2.5.1 的服务是子进程，一并计入）。

正确性：每次检索都要返回 10 条命中，输出字段与主键对应；全部检索结果（主键与 float32 距离的位模式）按顺序汇成 `results_sha256`。本实现与 3.x 的摘要必须相同，即每一条结果逐位一致。2.5.1 的 BM25 用全局统计、距离为正数，与 3.x 的口径不同，只参加计时。

本实现的计算池线程数等于机器核数（`available_parallelism`，结果里记 `cpu_count`），首次检索的建倒排与多条查询的打分在其上并行；3.x 与 2.5.1 按各自默认的线程配置运行。依次轮换三方的先后顺序，共三轮，每次新建目录，不并发计时。环境：2.5.1 用 [baseline/setup.sh](../baseline/setup.sh) 的做法单独建（需要 setuptools<81），3.x 用 [互通 README](../../tests/storage_interop/README.md) 的 Arrow 25 参照环境，本实现用 `maturin develop --release` 的开发环境。结果连同包版本、NumPy 版本与脚本 SHA256 写入 `--out`；这里的数字只说明这一数据形态下的相对量级，不推导其他分布、规模或平台。
