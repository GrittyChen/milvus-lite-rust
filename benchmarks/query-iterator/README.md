# Query iterator 回归专项

冻结范围见 [0016](../../docs/decisions/0016-query-iteration-and-range-search.md)。脚本仅接受仓库外不存在的工作目录，绝不打开已有数据库。无网络依赖，不进入默认测试。

```bash
"$BENCH_PYTHON" benchmarks/query-iterator/run.py --work-dir "$BENCH_WORK/new-1" --rows 100000 --ordered
"$B3_PYTHON" benchmarks/query-iterator/run.py --work-dir "$BENCH_WORK/b3-1" --rows 100000
```

B3 尚不支持 iterator，不带 `--ordered` 只测普通过滤 Query 和主键 Get；新构建也测同样的两项，另外测全量/带 null 过滤的 iterator 首批，以及 SDK 4096 行逐页读取全部数据。加 `--varchar` 测含非 ASCII 的 VARCHAR 主键，默认 INT64。数据以 seed 37 洗牌、批次写入后 Flush；每次写入核对返回主键，每个计时样本逐次核对行序；全量分页逐行核对主键和 nullable 值。RPC 请求在计时外构造，计时不含建表和写入；全量分页单独记录 SDK 端到端时间。

所有测试、编译和其它基准结束后顺序执行，三轮轮换新/B3 的先后顺序，使用相同行数和主键类型。每次输出独立 result.json，记录原生库、脚本 SHA256、版本、逐样本时延及进程累计 maxrss 原始单位（macOS 为字节，Linux 为 KiB；含数据准备，不等于查询新增 RSS）。10 万与百万行分开报告，普通读取与新增有序读取分开比较；本专项只评估回归与资源边界，不宣称相对两代官方实现的性能提升。
