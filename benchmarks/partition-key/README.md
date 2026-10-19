# partition key 写入与读取专项

对应 [0017 B](../../docs/decisions/0017-partitions.md#b-合同设计已通过)，显式执行，不进入默认测试、不联网或安装依赖。只接收仓库外不存在的数据库；目录已存在、链接或位于仓库内时拒绝。

```bash
"$BENCH_PYTHON" benchmarks/partition-key/run.py \
  --database "$BENCH_WORK/key-multi-json-1.db" --mode multi \
  --workload json --rows 5000 --samples 5 --build '<原生库身份>'
```

- `default` / `single` / `multi` 分别是普通标量 k、key 固定为 0、key 为 id mod 16；后者对固定 16 桶按真实 MD5 路由，不保证每桶同样多。key 模式先核对 Describe 标志和完整桶集合，拒绝以忽略 key 的实现充当基线。
- `plain` / `json` / `array` 各在独立进程创建一个集合，预序列化原始 gRPC Insert / 全行 Upsert；计时外逐批核对完整 ID、成功位次、全部数据和 JSON/ARRAY 类型。JSON 约 2 KB、ARRAY 为 32 个 INT64；向量二维。每种操作一次预热再保存全部样本。
- `read` 用 128 维确定向量和严格存活行，显式 Flush 后测 typed S1 Query、32 向量批量 L2 检索。逐次检查完整 Query 结果，Search 主键、顺序、条数及既有距离预算；核验不进入计时。该确定数据集不能传给 seed 42 的 recovery 专项。
- 写入模式的峰值是该进程的 `ru_maxrss`，在最后 Flush 之前取值，包含固定客户端输入与进程内服务。它是请求缓冲的进程级上界观察，不是 allocator 单独计数；C++ 服务为外部进程，不能用该数字比较 C++ 内存。冷恢复 RSS 继续使用 [recovery](../recovery/README.md) 的服务及后代采样口径。
- 固定输入、参数、环境与线程设置；相同指纹按三轮顺序轮换。正式测量期间不构建或运行测试。默认模式比较同机 A、已发布 rc1、3.x 与 2.5.1；key 模式只比较明确通过标志/桶预检的实现。能力不等价的拒绝只记为诊断，不记性能成绩。

输出包括实际 native、脚本、Python/依赖、负载及线程变量；`--build` 是标签，不能替代指纹。任何正确性失败均不输出成绩。已有 S1、批量检索和冷恢复专项按其原输入合同另行运行，不把本工具的结果混入历史矩阵。
