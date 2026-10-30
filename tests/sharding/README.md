# 候选测试的确定性分片

合同、判据与试点见 [0019](../../docs/decisions/0019-test-sharding.md)。本工具只改变执行方式：用例、断言与 fixture 作用域不变，任何遗漏、重复、额外、乱序、跳过或失败都使核对不通过。只支持 POSIX 平台。

## 用法

`plan` 与 `run` 的输出目录都必须是新目录，会解析为绝对路径。收集与执行使用同一解释器、`--cwd` 与额外参数（放在 `--` 之后）。`--cwd` 决定子进程的工作目录与 `python -m` 的导入路径；rootdir 与 nodeid 仍按 pytest 的规则确定，两个候选套件由仓库根的 `pyproject.toml` 锚定。

```bash
# 差分：使用会话级服务的用例整组放入同一片，按收集顺序执行。
python tests/sharding/shard.py plan --python <被测解释器> --suite tests/differential --out <plan_dir> \
    --shards 4 --group-fixture diff --group-fixture rust_uri --group-fixture ref_uri \
    --expect 431 --expect-group 118 \
    [--durations <上一轮的 shard-*.json> ...] [--order balanced|round-robin|reverse]
python tests/sharding/shard.py run --plan <plan_dir>/plan.json --out <run_dir>

# 互通没有会话级服务 fixture。
python tests/sharding/shard.py plan --python <被测解释器> --suite tests/storage_interop --out <plan_dir> \
    --shards 4 --expect 248 --expect-group 0

# 对已有输出离线重做核对，供候选门槛使用。
python tests/sharding/shard.py verify --plan <plan_dir>/plan.json --out <run_dir>
```

`--expect` 与 `--expect-group` 必填，取同一源码串行基线的收集数与会话组数；fixture 名写错或漏写时会话组数不符，计划被拒绝。差分仍需 `MILVUS_LITE_3X_PYTHON` 等原有环境变量，由调用方设置并传给子进程。环境中设置了 `PYTEST_ADDOPTS` 或 `PYTEST_PLUGINS` 时拒绝执行，避免在命令行之外改变收集。`--shards 1` 即串行执行，同时产出逐 nodeid 耗时，可作为下一轮 `--durations` 的输入。`--serial <nodeid>` 把用例放入串行组，在全部分片结束后单独运行；入组条件见 0019，会话组成员不能单独入组。

## 输出与门槛

- `plan_dir/collected.json`、`plan.json`：收集全集（含会话组标记）、预期值与分配，记录输入耗时和收集列表的哈希。
- `run_dir/shard-<n>.{log,json,xml}`：各进程的日志、插件执行记录与 JUnit；`shard-<n>-basetemp`、`shard-<n>-tmpdir` 为该进程独占的临时目录。
- `run_dir/processes.json`：驱动启动时的信号处理函数，以及各进程的 pid、命令、临时目录、退出码与起止时间；中断时另有 `interrupted` 行。
- `run_dir/summary.json`：每次 verify 先写入不通过结果，再覆盖为最终结果；任何输入缺失或损坏都记为问题。

候选门槛除 `passed` 为真外，还须核对 summary 回显的 `expect`、`counts.collected` 与串行基线一致，`expect_group` / `counts.group` 与 `group_fixtures` 符合 0019，`pytest_args` 为空，`plan_file_sha256` 与存档的计划文件一致。

`run` 并发启动全部非空分片：每个分片使用新进程组，SIGINT / SIGQUIT 复位为默认处理并恢复驱动原有的信号屏蔽集，加 `-p no:cacheprovider`。一个分片失败不提前终止其他分片；驱动收到 SIGINT、SIGTERM 或 SIGHUP，或启动中途失败时，终止已启动的全部子进程组并记为失败。

## 残余风险

- 加载插件所需的 `tests/sharding` 前置于 `PYTHONPATH`，会传给被测进程启动的孙进程；当前两个套件及参照环境中没有同名模块。
- 没有单分片看门狗：某个用例挂死时驱动一直等待，需由调用方中断。
- SIGKILL 无法捕获：驱动被强杀时子进程组残留，`processes.json` 与 `summary.json` 缺失，门槛因此判为不通过。
