# 分区可见性组件对照与资源诊断

对应 [0017](../../docs/decisions/0017-partitions.md) 与 [组件记录](../../docs/testing/0017-visibility-component.md)。这是隔离 Rust example，不接入产品；不修改版本、协议、存储格式、锁或运行依赖。

```bash
cargo test -p milvus-lite-core --example partition_visibility_probe
cargo build -p milvus-lite-core --release --example partition_visibility_probe
target/release/examples/partition_visibility_probe verify \
  tests/compatibility_audit/fixtures/partition_visibility_v321.json
python benchmarks/partition-visibility/run.py \
  --binary target/release/examples/partition_visibility_probe \
  --work-dir "$PARTITION_COMPONENT_WORK/resources"
```

工作目录必须不存在且在仓库及参照外，binary 应为刚构建的 example。运行器不联网、不安装依赖，不进入默认 pytest；依赖系统 `ps` 读取被测进程的 RSS。macOS/Linux 均可使用。原始错误写入各项 stderr，超时/异常退出/计数不同立即失败，不重试。

`verify` 接收 [参照真值](../../tests/compatibility_audit/fixtures/partition_visibility_v321.json) 的物理源、活跃候选和两类删除，分别维护每分区候选、全集位图、局部位图及稀疏遮挡字。读取只组合位图；沿来源顺序物化主键/tag 与真实 Query 比较，同时持有上一阶段快照跨更新核验。参考输入中的直接引擎空列表诊断不作为 adapter 输入，原因见组件记录；gRPC 空列表已单独固定。合成冻结代与同序号平局的 4 项 Rust 单测和真实参照对照分开计数。

资源诊断按顺序运行 26 项：65536 物理行，1/16/64/256 分区，INT64/VARCHAR；完全不重复、所有分区共享全部键、只共享一个键三种负载；另测 16 分区、1048576 行的 INT64 无重复/全重复两项。每项独立子进程，在 empty、loaded、queried 三个确定同步点采样 RSS；每次采样后父进程发 continue，子进程再推进，无 sleep。全部 100 次选择偶数分区的读取均核对计数，再输出每次位图快照与 count 的平均微秒。

RSS 是同步点驻留量，**不是峰值或整库 RSS**；加载耗时包括从合成帧构建索引、位图及行式调查载体，不是 Insert RPC。这里没有向量、Arrow、WAL、恢复或真实读取过滤，也没有 2.5.1/3.x 的性能对照，不据此声明产品加速或达到 0001 的内存目标。JSON 记录全部点位、计数、字数、关系数及 binary/script 哈希；正式产品专项仍须按 0017/0013 执行。

模型采用单候选内联，只有多个实际候选时扩展位置数组；不重复缓存 winner。遮挡关系只存非零字，无重复键时不分配关系。全集/单分区共享既有位图；子集先合并局部位，再扣除选定分区的遮挡，遍历实际存在的关系，不扫描不存在的分区对。写侧对受影响键重算候选及关系；这是正确性对照模型，不应把它的整帧源替换、按键全关系重算或行式载体直接用于产品。工程接线必须复用现有列式数据/PkMap，收敛增量关系更新后再接 RPC。

## 实际组件与列式引擎

隔离模型保留为历史诊断，产品组件使用以下入口，不把模型的资源数字当作产品数字：

```bash
cargo build -p milvus-lite-core --release \
  --example partition_visibility_probe --example partition_engine_probe
target/release/examples/partition_visibility_probe verify-core \
  tests/compatibility_audit/fixtures/partition_visibility_v321.json
python benchmarks/partition-visibility/run.py \
  --binary target/release/examples/partition_visibility_probe --core \
  --work-dir "$PARTITION_COMPONENT_WORK/core-resources"
```

`verify-core` 使用真实 `PartitionVisibility`，1870 项当前读取及 1870 项旧快照均逐项物化核对。`--core` 以同一组 26 种规模和重叠度逐行增量更新真实组件，省去模型的行式调查载体；输出真实 PkMap、本地位图、稀疏遮挡、候选比较次数和 RSS。每次读取核对计数，再加入一次删除并核对新结果；不可变快照的完整内容由 `verify-core` 及核心测试负责。两种模式、不同二进制与脚本指纹分组报告。

实际 `Collection`、列、PkMap、WAL/重开及快照读取使用独立入口：

```bash
target/release/examples/partition_engine_probe seed \
  "$PARTITION_COMPONENT_WORK/seed.db" 1000000 128
python benchmarks/partition-visibility/run.py \
  --binary target/release/examples/partition_engine_probe \
  --engine-seed "$PARTITION_COMPONENT_WORK/seed.db" \
  --work-dir "$PARTITION_COMPONENT_WORK/engine-resources"
```

种子必须在仓库外且正常关闭、排空 WAL，所有引用文件存在且无符号链接；采样器取得 `LOCK` 非阻塞独占锁，在持锁期间复制三个独立读者。每个进程依次在 empty、恢复列完成、启用分区可见性、验证读取四个事件采 RSS，不用 sleep 估算阶段。每轮还核对一次真实插入及旧快照不变，再正常关闭。

这个种子使用恒值向量，目的是同进程比较移交既有主键索引前后的资源增量，不替代随机百万行、独立 Python 服务的恢复专项，不据此宣称达到 0001 的整体内存目标。所有 RSS 均为时点值，不能称为峰值。

在 `--core` 后追加 `--bidirectional`，另测 9 组键级交错顺序，使同一分区对在不同主键上有相反的新旧关系；总计 35 组。256 分区时可产生 65280 条非空有向关系，不只验证单方向的三角关系。仍逐次核对计数，不将这些时点资源数字推广为任意负载上界。
