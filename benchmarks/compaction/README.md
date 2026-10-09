# Compaction 条件验收

对应 [0010 的确认附加条件](../../docs/decisions/0010-compaction-and-tombstone-gc.md#确认附加条件)。数据、wheel、原始 JSON 和日志全部放仓库外，不覆盖已有库。

## 正式产物的单轮与退出等待

用真实 3.x 环境准备同一批不可变文件：一百万行、dim 128，20 个各 50000 行的文件。参照选择器的一轮选中四个文件，共 200000 行；懒打开只触发一轮，不自动循环。

```bash
<v3>/bin/python benchmarks/compaction/run.py --prepare --database <work>/million.db --rows 1000000 --dim 128
<v3>/bin/python benchmarks/compaction/run.py --database <work>/million.db --rows 1000000 --dim 128 --python <formal-rust>/bin/python --work-dir <work>/timing-rust --implementation rust --rounds 3 --build <native-sha256>
```

必须顺序运行，测量时不同时跑 cargo、其它基准或大规模测试。每轮复制同一健康 seed，单独起服务；校验 schema 维度、物理/逻辑行数。Rust 的单轮时间来自正式产物的正常维护诊断日志，必须恰好有一个 200000 行的完成记录；退出等待通过正式 NativeServer.stop 测量。文件最终布局也独立校验。

输入恢复会预热内存和页缓存；计时包括该轮选择、合并、提交与清理，不包含前面的全量恢复。三轮最大值是该机器/负载的观测值，不是任意平台或阻塞 I/O 的绝对上界。单轮大于 10 秒时脚本拒绝输出通过记录，需按负责人条件重新评估。此脚本不声明相对 2.5.1/3.x 的性能提升。

reference 模式可用于相同输入的布局/正常关闭冒烟；没有与 Rust 相同的单轮日志，不能把它的 stop 时间当作参照的合并时间，或直接推导加速倍数。

## 删除/重插/再删除

```bash
<v3>/bin/python benchmarks/compaction/delete_load.py --rust-python <isolated-fi>/bin/python --reference-python <v3>/bin/python --work-dir <work>/delete-load --cycles 5
```

这是受控组件负载，使用隔离的 fault-injection wheel；报告必须与正式产物证据分开。Rust 在准备前暂停，3.x 只在进程内暂时包裹原合并调用并等待，不改参照 checkout。

4000 行、dim 128；先提交删除，再在准备窗口中重插并删除同一批 800 个主键，随后反复执行重插/删除/flush/确定等待。每轮核对逻辑行数和 manifest 引用的物理行、失效比例、data 字节及文件数。仅统计已提交 Parquet，不把活跃 WAL 当成文件回收结果。

跨编码器的字节差不能全部归因于死行；该负载也不能泛化成任意删除密度或长期并发的结论。本轮原保守方案的明显差距已触发负责人确认 [0011](../../docs/decisions/0011-committed-delete-state.md)，并由独立已提交删除表消除。后续不同负载若出现明显差距，仍须单独评估，不能默认永久接受。当前数值与组件内存代价见[实施验收](../../docs/testing/compaction-correctness.md)。

## 显式诊断与发布窗口

正式产物默认不输出合并成功日志。`MILVUS_LITE_COMPACTION_DIAGNOSTICS=1` 在创建 Db 时开启单轮及发布锁诊断；已复用的服务不因之后修改环境变量而切换。run.py 只给测量子进程设置该变量，不修改调用者环境。诊断输出发生在状态写锁释放之后；锁持有计时从获取守卫后到释放前，包含发布与清理计划登记，获取写锁的等待另记。

发布窗口采用隔离 FI 产物，先暂停在准备完成/提交之前。200 次单主键 Query 作无发布竞争的基线，再启动一个持续 Query 调用者并恢复维护；直到该轮提交、发布及清理完成。每次 Query 核对同一主键；另核对完整物理与逻辑行数、单轮预算和诊断次数。

```bash
<v3>/bin/python benchmarks/compaction/run.py --prepare --database <work>/million-varchar.db --rows 1000000 --dim 128 --pk-kind varchar
<v3>/bin/python benchmarks/compaction/publication.py --database <work>/million.db --pk-kind int64 --python <fi>/bin/python --work-dir <work>/publication-int64 --rounds 3
<v3>/bin/python benchmarks/compaction/publication.py --database <work>/million-varchar.db --pk-kind varchar --python <fi>/bin/python --work-dir <work>/publication-varchar --rounds 3
```

两种主键各一百万行/dim128，VARCHAR 是 16 位十进制字符串。三轮顺序运行，不并跑编译、其它测试或基准。记录每轮状态写锁时间、获取等待、Query 样本数/p99/最大值及实际 native 哈希。Query 窗口包含改名、manifest 同步、发布、清理；不是只在写锁持有期间采样。p99 的低值不能证明没有阻塞，最大值须同时披露。FI 的总轮耗时包含受控暂停与基线请求，不用于 10 秒条件，也不与正式 wheel 的单轮耗时比较。

## 当前验收依据

当前脚本已完成正式产物 Int64 单轮三轮测量、隔离 FI 产物发布窗口测量及删除重负载五轮复测，结果与脚本/产物身份见[实施验收](../../docs/testing/compaction-correctness.md#本轮评审修复与复核)。这些复测记录作为当前验收依据；早期数字只保留为过程记录，不再提供历史脚本复原。

后续修改脚本时，新测量记录新的脚本指纹与产物身份；原数字和指纹继续表示当时的测量，不用复原补丁约束日常修改。
