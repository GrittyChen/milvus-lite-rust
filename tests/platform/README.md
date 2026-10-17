# M2 Linux 验收

范围来自 [0006 第 7/19 项与验收](../../docs/decisions/0006-m2-scope.md#验收)：容器内执行 Rust 默认与故障注入测试、真实 pymilvus 入口、两实现的 LOCK 互斥，以及 Linux strace 的同步/提交/删除顺序。Windows 与真实断电另行验证，不从本记录推断。

0015／0016 的跨平台开发验收见[独立记录](../../docs/testing/0015-0016-cross-platform-acceptance.md)。安装 wheel 后，可在两条 SDK 环境中运行四个切片的联合冒烟：

```bash
<installed-wheel-python> tests/platform/feature_smoke.py --repo <source-root> --work-dir <new-outside-directory>
```

探针覆盖标量 null／默认值、ARRAY、Query iterator 的主键顺序、范围检索和 SearchIterator V1，分别在活跃写入、正常关闭重开、显式 Flush、再次重开后核对结果；输出解释器、SDK、原生库与探针哈希。它不代替差分、故障注入或 WAL 崩溃恢复矩阵。Intel Mac 使用 Rosetta 时须在记录中注明，不能写成原生 Intel 通过。

## 环境与执行

使用 ARM64 Linux 容器时，源码从 `git archive` 复制到容器自己的文件系统；数据、临时目录和构建产物同样位于容器内部，不使用宿主 macOS 的 bind mount。以非 root 用户执行测试，权限位造成的 EACCES 才有效。容器需要 Rust、Python 开发环境、maturin、pytest、pymilvus 和 strace；参考环境安装 `milvus-lite==3.2.1`，Rust 环境不能安装官方 `milvus-lite`。

```bash
# 源码复制示例；container 是已启动且带 SYS_PTRACE 的 Linux 容器名。
git archive HEAD -o /tmp/m2-source.tar
docker cp /tmp/m2-source.tar "$container":/tmp/source.tar
docker exec "$container" sh -c 'mkdir -p /work/repo; tar -xf /tmp/source.tar -C /work/repo'

# 容器内先建立 /opt/rs 与 /opt/ref 两套 venv，再把 /work 与 /opt/rs 交给非 root 测试用户。
# 以下命令在容器内、该用户下执行；先结束性能测量，避免编译影响基准。
cd /work/repo
export PYO3_PYTHON=/opt/rs/bin/python
export CARGO_BUILD_JOBS=4
cargo test --workspace
cargo test -p milvus-lite-core --features fault-injection
/opt/rs/bin/maturin develop -m crates/py/Cargo.toml --release
/opt/rs/bin/python -m pytest tests/python
/opt/rs/bin/python tests/platform/linux_lock_probe.py /opt/rs/bin/python /opt/ref/bin/python /work/lock-probe
```

`linux_lock_probe.py` 分别让 Rust/3.x 持锁，断言另一方明确报锁错误；同目录 `flock` 也必须被拒绝，而 `lockf`（POSIX record lock）可以独立取得。持锁者停止后另一方必须成功打开，避免把目录格式错误误当作锁互斥。

## 系统调用顺序

```bash
cargo test -p milvus-lite-core --features fault-injection --test concurrency_faults --no-run --message-format=json > /work/binaries.jsonl
# 从 compiler-artifact 的 executable 字段取得该测试二进制，赋给 binary。
strace -ff -ttt -yy -e trace=fsync,fdatasync,rename,renameat,renameat2,unlink,unlinkat,flock \
  -o /work/fs.trace "$binary" \
  inherited_and_new_files_are_synced_before_replace_and_wal_unlinks_stay_ordered --exact --test-threads=1
/opt/rs/bin/python tests/platform/check_strace.py /work/fs.trace
```

分析器按时间合并线程记录，仅接受成功调用；核对继承文件、新 Parquet、schema、manifest tmp、两类原 WAL 及相关目录在 replace 前的同步，replace 后 collection 目录同步，已提交 WAL 在各自 unlink 前再次同步，以及 data→delta unlink 与每次 unlink 后的 wal/ 同步。它依赖 strace 的 fd 路径注解；日志缺失或格式无法识别直接失败，不默认为通过。分析器自身的反证用例为 [test_strace_checker.py](test_strace_checker.py)。

该系统调用证据只覆盖上述指定路径，不等于验证所有操作、文件系统固件或掉电后持久化。原始 trace、容器日志和数据库不入库；实际环境、结果与剩余缺口写入最终验收记录。

## 首发 PID 1 与监督器停止

合同与矩阵见 [0013](../../docs/decisions/0013-first-release-platform-and-packaging.md)。准备两份同源码的 Linux wheel：默认正式构建，以及显式 `--features fault-injection` 的隔离构建。探针创建自己的镜像、容器、数据/控制命名卷，结束后清理这些资源；不修改已有容器，不用宿主 bind mount 证明 Linux flock/fsync。

```bash
.venv/bin/python tests/platform/linux_pid1_probe.py \
  --formal-wheel <formal-linux-wheel> --fi-wheel <fi-linux-wheel> \
  --image <base-image-id-or-digest> --output <new_outside_directory>
```

正式场景用 console script 的 exec ENTRYPOINT；FI 场景用 Python 包装脚本作为 PID 1。Docker logs 的 PAUSED/DEFAULT 标志确定暂停及 SIG_DFL 切点，独立控制卷的 resume 文件放行。另一个容器争抢同数据卷的 LOCK，必须在放行前失败、停服后成功。正常关闭退出 0；PID 1 后续 SIGTERM 被忽略、监督器超时 SIGKILL 退出 137 且不是 OOM。重开逐项核对已确认行、值与 `_seq`。原始日志与 kill 事件放在输出目录。

记录 Docker VM 内核、镜像与 wheel 身份；`--platform linux/amd64` 在 ARM 宿主上的结果必须写成模拟执行。部署时考虑 `--init` 与足够的 stop timeout；PID 1 在 SIG_DFL 后没有二次 SIGTERM 强制退出的退路。

## Windows 准备流程

首发构建期拒绝 Windows Python 包；当前不运行 Windows 持久化/信号用例。准备 Rust 1.98.1 的 rustfmt/clippy 与 `x86_64-pc-windows-msvc` target 后，`pwsh -NoProfile -File tests/platform/windows_run.ps1 -WorkDirectory <new_outside_directory>`（PowerShell ≥7.2）执行 core/grpc 默认与 FI 的 check/clippy，并确认 py crate 以本项目的 Windows 拒绝消息失败。使用 `-NoProfile` 排除用户 profile 对错误偏好的修改；最低版本来自 [PowerShell 7.2 的原生命令重定向规则](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_preference_variables#erroractionpreference)。Unix 上可按脚本中的 cargo 参数交叉检查；这只有编译证据。

后续 Windows 切片先按 0013 修复目录校验与写权限同步句柄，再解除构建拒绝、构建 wheel。真实 runner 逐项验证第 0 字节 LockFileEx、替换文件、关闭失败留存 WAL、提交点前后恢复、CTRL_BREAK_EVENT（独立进程组）和 TerminateProcess。管道读取采用线程与队列；POSIX fork/flock/权限/信号用例单独登记不适用，不以删掉共享数据/提交断言替代平台验证。

迁移到根 `pyproject.toml` 后，上文 `-m crates/py/Cargo.toml` 仍指定 Rust crate，需在仓库或 sdist 解包根执行；当前打包命令与验证见 [发行物流程](../packaging/README.md)。
