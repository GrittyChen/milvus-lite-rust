# 恢复内存与进程冷启动专项

[run.py](run.py) 复用 [baseline](../baseline/README.md) 的独立服务启动、正常停止与 RSS 采样。只接收已有健康库，不执行 prepare、Insert、Delete 或 Flush；服务自身的打开维护及正常关闭仍照常发生，因此不把它称为文件系统只读操作。

输入必须来自 baseline/S1 的 seed 42 数据：先生成 200 个查询向量，再生成 N 个向量；Int64 主键为 `0..N`，FLAT L2，全部存活且已显式 Flush。其他数据集不能用这个脚本衡量。2.5.1、3.x 各读自己的库，优化前后 Rust 读同一份物理目录。三方相同的是逻辑行、参数和请求；文件布局保持各自真实形态。

```bash
# 使用该实现的独立 Python 环境，库、日志和输出放仓库外。
<python> benchmarks/recovery/run.py <existing.db> \
  --rows 1000000 --dim 128 --round 1 --build <artifact-identity>
```

每个进程只输出一条 JSON。顺序跑 10 万与百万行、三轮轮换优化前后 Rust / 2.5.1 / 3.x 的先后；不同时构建、测试或运行诊断。保留每条记录的 native 和脚本 SHA256、依赖版本、负载及构建身份。基准新增或改动脚本后，指纹改变的记录应另组统计。

## 口径与失败门

- `connect_s` 包含新服务进程启动和客户端连接；`load_s` 是首次 load RPC；`ready_s` 为两者之和，不含两者之间单独采 RSS 的耗时。它与历史 reopen 的请求序列不同，不合并历史表。
- 页缓存未清空，是进程冷启动，不是磁盘冷启动。没有重复执行同进程 load 作为样本。
- RSS 为 MiB，沿用 baseline：Rust/3.x 计服务辅助进程及后代，包含不可分离的 Python/绑定开销；2.5.1 只计 C++ 子进程。客户端真值生成及数组不计入服务 RSS。
- 分别记录连接后、load 后、首次 Search 后 RSS，都是时点采样，不是全过程峰值。`first_search_ms` 每进程只有一个样本，不推导 p99。
- float64 FLAT 真值在服务启动前生成，计时后检查首次 Search 的候选、顺序、有限距离及 0004 的 `max(1e-5*abs(expected), 1e-6)` 误差门。count 必须严格等于 N；任何失败不输出结果行。真值与 RSS 采样不进入 load/Search 计时。
- load 及其 RSS 采样之后，在计时区间外调用 `list_indexes` / `describe_index`，要求唯一索引且实际定义为 `FLAT` / `L2`；HNSW、其他度量、缺失定义和多个索引均拒绝，不会凭 top-10 相同就当作 FLAT。通过的定义写入 `index_described`。调用失败沿正常失败路径退出并停止服务，不输出记录。
- 正常 EOF 关闭与退出码验证沿用 baseline。客户端 close 失败时仍尝试停止服务；失败不输出记录。`dedicated=True` 沿用现有基准，不将其推广为共享连接长期会话结论。
- 结果只覆盖全存活、已提交、单 collection、无并发写入的两种规模。WAL 及覆盖/删除/快照的正确性另由测试固定，不在这组性能结论内。

当前测量、所有权边界与局限见 [恢复性能记录](../../docs/testing/recovery-performance.md)。

## 历史测量脚本

原表使用 `5e17c0ec…`，当时脚本没有主动核验索引定义；索引校验新增后的脚本指纹为 `6fbc7a22…`。原始记录不回填、不改标签，补验同一批库的实际定义也不等于当时逐轮已经执行了这条校验。

[measured-20261008.patch](measured-20261008.patch) 只保存当前 `run.py` 到原测量版本的源码差异。在仓库外的新空目录以完整提交 `e7632ad8e38e226e3efcf69cc24baac2271e0ba4` 的 Git 树取得 baseline 九个 `.py/.sh` 的名字与字节（方法同 [baseline 历史恢复](../baseline/README.md#历史测量脚本的指纹复核)），另外复制当前 `run.py` 到相邻 `recovery/`；在该 `recovery/` 目录执行 `git apply <补丁绝对路径>`，按本脚本的目录名、文件名与字节算法拼 baseline/recovery 即恢复完整历史指纹。自检使用固定历史输入，不随工作区 baseline 文件修改漂移，也不在用户工作树原地应用补丁。

以后改 `run.py`，须先在仓库外用旧文件及旧补丁保存原测量源码，再从新文件向该冻结源码重新生成补丁（unified diff、三行上下文、路径 `a/run.py` / `b/run.py`；空上下文行去掉空格前缀），并运行历史指纹自检。不得改历史指纹迁就测试；浅克隆需要先包含上述固定提交，测试不访问远端，也不回退到当前 baseline 文件。
