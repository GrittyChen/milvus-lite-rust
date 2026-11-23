# HNSW 专项

合同见 [0041](../../docs/decisions/0041-hnsw.md)，性能记录见 [0041 HNSW 性能对照与参数标定](../../docs/testing/0041-hnsw-performance.md)。同机对照四个臂：本实现 HNSW、本实现 FLAT、3.x HNSW，以及 2.5.1 FLAT（2.5.1 没有 HNSW，按规则 7 只作 FLAT 对 FLAT 与召回无关操作的参照）。只写仓库外的新目录，不下载、安装依赖，不进入默认测试；规模测量只在 x86 服务器上做。

```bash
# 每个臂一个独立环境（Python 3.12 + pymilvus 3.0.2 + numpy，再分别装 milvus-lite 2.5.1 / 3.2.2 或本实现的 release wheel）
<env>/bin/python benchmarks/hnsw/bench.py <v2|v3|rs> <FLAT|HNSW> <uniform|hard> <行数> <工作目录> <结果.jsonl> [main|sweep]
```

- 服务端经 [baseline/server_process.py](../baseline/server_process.py) 在独立进程中运行，RSS 为服务端进程树（2.5.1 的 Python 辅助进程不计入）。检索一律 `output_fields=["id"]`：3.x 与本实现默认返回向量字段、2.5.1 不返回，大批量时会把测量变成传输。
- 数据：128 维、L2、每批 1 万行写入，写完后 flush 一次；标量字段 `tag = i % 1000` 用来构造通过率 10% / 1% / 0.1% 的过滤。`uniform` 为基线口径的均匀随机（seed 42）；`hard` 为 0041 的门槛夹具，从 `MLR_HARD_DIR` 读取 `data.f32`（100 万 × 128 的小端 float32）与 `queries.f32`（100 条），按行数截取前缀。
- HNSW 为 `M=16, efConstruction=200`，检索 `ef=64`，top10；召回以 float64 精确结果为真值（并列按行号）。
- **就绪**：3.x 为 release + load 的耗时（load 时同步为各段建 faiss 索引）；本实现 load 不等图，脚本读 manifest，等每个 1 万行以上的段都写出图文件。
- `main` 阶段依次测：写入与 flush、就绪、就绪时 RSS、nq1（200 次）与 nq100（20 次）、召回、三档过滤（各 100 次，另测召回）、nq1000 的 limit 100 与 1000（10 ms 采样峰值 RSS）、1 / 4 / 8 个客户端进程并发（每档 `MILVUS_BENCH_SECONDS` 秒，默认 8），最后重启服务测 load、首次检索、nq1 与 RSS。
- `sweep` 阶段只测通过率 100% 到 1% 的过滤曲线：建一次库，对 `MLR_SWEEP_BUDGETS`（逗号分隔，默认 `1`）的每个取值重启服务、load 读入已有的图后测；FLAT 臂也在重启后测一次。重启时把取值放进环境变量 `MLR_GRAPH_BUDGET`，只有另行构建、会读这个变量的本实现才会改变距离预算，正式构建忽略它。对照构建必须用单独的 `CARGO_TARGET_DIR`，装好后核对 .so 的指纹，并带一个必然改变结果的取值（如 `0.0001`，此时召回应全部为 1）。

门槛夹具的生成器在第 1 步的原型里，不在仓库中：隐空间 128 维、1024 个簇，簇中心取标准正态，簇内标准差 1.2，经固定随机矩阵（元素为标准正态除以 √128）投到 128 维后加标准差 0.05 的噪声；簇中心与投影矩阵的种子为 1234，数据与查询的种子分别为 42 与 7。性能记录所用文件的 SHA256：`data.f32` 为 `defd4559…9240661`，`queries.f32` 为 `6a537c1b…c0db8d3`。

各配置顺序执行，不并发计时；轮换各臂的先后（性能记录中 10 万行三轮、100 万行两轮）。结果每行一条 JSON，原始输出不入库。
