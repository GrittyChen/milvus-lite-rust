# 批量 FLAT 检索专项

复用 baseline 的 seed 42、128 维、全部存活且已 Flush 的健康库。优化前后 Rust 使用同一物理库；2.5.1 / 3.x 使用各自按相同逻辑数据准备的库。不执行写入、删除或重新 prepare。查询为固定的前 1 / 100 条随机向量，FLAT L2、top10、默认输出字段。

```bash
# 真值只准备一次；目录必须不存在，数据与输出放仓库外。
<python> benchmarks/batch-search/run.py --prepare-truth \
  --truth <work_dir>/truth-1000000 --rows 1000000 --dim 128

# 每个实现使用独立 Python 环境；顺序跑两规模、三轮，并轮换顺序。
<python> benchmarks/batch-search/run.py --database <existing.db> \
  --truth <work_dir>/truth-1000000 --rows 1000000 --dim 128 \
  --round 1 --build <artifact-identity>
```

## 校验与计时

- float64 真值按块逐元素相减再平方求和，不能使用范数展开。每块按距离与主键的全序取 top10，再合并全局 top10；数组与真值 JSON 的指纹一并记录，测量前核对形状、dtype 和文件字节。
- 实际索引必须是唯一 FLAT/L2，空过滤 count 严格等于 N。其他库和参数不在这个基准的输入合同内。
- nq1 100 个样本、nq100 10 个样本，沿用 baseline 的 `lat`，额外一次预热。每项还先执行一次结果校验；此请求序列与历史 M2 不完全相同，不合并旧表。
- 转 Python list、RPC、距离计算、TopK、返回编码均在计时内；真值准备、索引核验和结果校验在计时外。仅额外记录返回结果的 list append 进入计时，三方同样如此。
- 每个查询都检查数量、主键类型/范围/唯一性、距离有限且符合 `max(1e-5*abs(expected), 1e-6)`、结果按距离排序。仅允许容差内的截断并列互换；少了真正更近的候选不能通过。返回结果校验失败不输出测量记录。
- 输出记录构建与 native / 脚本 / 真值 SHA256、依赖版本及系统负载；只有相同输入与指纹才能按轮次汇总。`round=0` 为冒烟，不进入正式三轮统计。
- 顺序测量，不并行构建、测试或采样。进程内 RSS 不用于内存比较。nq100 的十个样本不足以声称稳定 p99。

计算池、4 查询分组、行块、分波及公平性边界沿用 [0005](../../docs/decisions/0005-search-thread-model.md)。性能记录见[批量检索专项](../../docs/testing/batch-search-performance.md)；不得为数字放松 L2 精度、召回或持久化。
