# Range search 对照与普通检索回归

合同见 [0016 B](../../docs/decisions/0016-query-iteration-and-range-search.md#b冻结合同)。只写仓库外不存在的目录，不下载、安装依赖或进入默认测试。

```bash
OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 VECLIB_MAXIMUM_THREADS=1 \
  "$BENCH_PYTHON" benchmarks/range-search/run.py --work-dir "$BENCH_WORK/current-1" --range
```

同机、同 Python/SDK、10 万条、128 维，FLAT / L2。为使期望主键与阈值完全可核验，只有第一维按 id/65536 递增，其余为零；这是读取回归与路径对照，不代表真实向量分布。所有写入逐批核对主键；预序列化 raw RPC，普通/范围检索各 nq=1/32、topk=10，2 次预热、9 个样本，每次核对 topks、主键、分数有限性及既定误差。范围为 [0,1000)，所有点在范围内，确保与 2.5.1 真正的范围索引及 3.x 有限窗使用相同结果集；不以此推导任意范围的召回或吞吐。

`--range` 另测客户端 V1 每批 1000、总计 5000 条，构造至关闭计时，3 次逐项核对。已评审的 Query A 不支持 range，省略该开关，只测同样的普通 RPC 回归。2.5.1、3.x、新构建均带该开关。默认计算线程配置保持各实现原样；BLAS / OpenMP 统一为 1，环境、原生库与工具哈希随结果保留。

先完成软件测试和编译，再依次轮换 Query A / 新构建 / 3.x / 2.5.1 的先后顺序，共三轮，每次新建目录。不得并发计时，不覆盖中断结果；负载未完全隔离时保留离群值，单独说明。ABI、发行或其它平台结论不由此专项推导。

官方环境可用 [baseline/setup.sh](../baseline/setup.sh) 准备，固定参照 commit 与 SDK 版本；2.5.1 需要 setuptools<81 提供 pkg_resources。Rust 新构建及 A 基线使用各自 wheel 的独立环境，避免同名模块覆盖。原生库或官方版本、全部包版本、线程环境和脚本 SHA256 均写入每次结果。
