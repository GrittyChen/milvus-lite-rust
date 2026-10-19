# 分区读取及默认写入回归

对应 [0017](../../docs/decisions/0017-partitions.md)。这是显式执行的本机专项，不进入默认测试；不联网、不安装依赖，只新建仓库外不存在的数据库，拒绝覆盖既有路径。

```bash
"$BENCH_PYTHON" benchmarks/partitions/run.py \
  --database "$BENCH_WORK/current-1.db" --rows 100000 --samples 5 \
  --build '<原生库身份>'
"$BENCH_PYTHON" benchmarks/partitions/run.py \
  --database "$BENCH_WORK/partitions-1.db" --rows 100000 --samples 5 \
  --partitioned --build '<原生库身份>'
```

默认模式与已发布的 0.2.0rc1 对照；分区模式与固定 3.x 对照，不让旧版本忽略分区来充当基线。三轮轮换实现先后，准备、测试和编译全部结束后顺序测量；原始输出保存在仓库外，每轮使用新的数据库。

- 10 万行、二维 L2；分区模式均匀分配至 16 个分区。测量全集、单分区、偶数分区子集的过滤 Query 和 32 查询向量批检索。逐次检查完整过滤行序、命中主键、数量及有限距离；这里的距离为已知二维输入的平方差。
- 默认分区分别写 INT64、约 2 KB 的 JSON、32 元素 INT64 ARRAY，每批最多 5000 行。写入使用预序列化原始 gRPC，输入构造、序列化及读回检查在计时外；逐次核对状态、返回主键、成功位次和完整读回内容。JSON/ARRAY 的核验保留 bool、整数、浮点和 null 的区别。另测实际 Flush，计时后读回。
- 每项先预热一次，再保存全部样本。脚本及原生库 SHA256、版本、数据规模、分区模式和系统负载随结果输出。`--build` 只是操作者标签，不能替代指纹。任一核验失败均不输出测量结果。

本专项不含跨分区重复键；重复度、256 分区的关系成本及百万行实际列式恢复分别由[可见性专项](../partition-visibility/README.md)核对。128 维随机向量的 S1、批检索及独立服务恢复 RSS 仍运行[既有专项](../s1/README.md)、[批检索](../batch-search/README.md)、[恢复](../recovery/README.md)，不把本专项的二维结果推广到高维检索。
