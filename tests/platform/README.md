# M2 Linux 验收

范围来自 [0006 第 7/19 项与验收](../../docs/decisions/0006-m2-scope.md#验收)：容器内执行 Rust 默认与故障注入测试、真实 pymilvus 入口、两实现的 LOCK 互斥，以及 Linux strace 的同步/提交/删除顺序。Windows 与真实断电另行验证，不从本记录推断。

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
