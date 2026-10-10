# 0003 进程内服务的生命周期：同路径复用、进程退出与 fork

- 状态：已采纳
- 日期：2026-09-24
- 对照版本：milvus-lite 3.2.1（tag `v3.2.1`，`43d1257`），pymilvus 2.6.17 / 3.0.2
- 前置决策：[0001 重构基线选择](0001-refactor-baseline.md)、[0002 数据目录格式](0002-data-dir-compat.md)

## 背景

pymilvus 遇到 `.db` URI 时调用 `server_manager_instance.start_and_get_uri(path)`，按返回的 URI 走 gRPC（见 [AGENTS.md「pymilvus 接入点」](../../AGENTS.md#核心业务术语)）。服务运行在用户的 Python 进程里，因此它的生命周期由用户进程决定：同一路径可能被多次、多线程地打开；进程可能正常退出，也可能被杀；进程还可能 fork。

本文在写服务代码之前定下这三件事。Rust 实现和 3.x 有一处根本不同：3.x 的服务线程是 Python 线程，状态都是 Python 对象；Rust 的 tokio runtime 与检索线程池是原生线程，fork 之后在子进程里全部不存在，但它们持有的锁、文件描述符和内存状态都被复制了过去。

## 3.x 的做法与实测

3.x 的 [server_manager.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/server_manager.py) 维护一张 `os.path.abspath(path) → (server, db, port)` 的表：
- 命中表项时直接返回已有端口；
- 未命中时在表锁**之外**启动服务，失败则记日志、返回 `None`；
- 构造时执行 `atexit.register(release_all)`，退出时对每个服务依次执行 `server.stop(grace=2)` 和 `db.close()`。`stop` 不等待在途请求结束就返回，所以 `db.close()` 与在途请求并发执行。

`db.close()` 对每个 collection 先把 memtable flush 成 segment，再 fsync 并删除 WAL（[collection.py:2147](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L2147)），最后以 `LOCK_UN` 释放 LOCK（[db.py:711](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/db.py#L711)）。WAL 每次写入后都 flush 到 OS，只在关闭时 fsync。默认的 `sync_mode="close"` 外部不可配置（[wal.py:110](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/storage/wal.py#L110)）。

3.x 没有任何 fork 处理。pymilvus 2.6.17 与 3.0.2 从不调用 `release_server` 或 `release_all`，`MilvusClient.close()` 也不停服务，服务一直活到进程退出。

以下场景都在 macOS、Python 3.12、milvus-lite 3.2.1 + pymilvus 3.0.2 上实测，并由另一套独立编写的脚本复现。这套环境可以用 [benchmarks/baseline/setup.sh](../../benchmarks/baseline/setup.sh) 重建：

| # | 场景 | 3.x 的结果 |
| --- | --- | --- |
| E1 | 两个线程同时首次打开同一路径 | 每轮都是一个线程拿到 URI，另一个拿到 `None`：它自己构造的第二个 `MilvusLite` 抢 LOCK 失败，因为同一进程的两个 fd 之间 flock 也互斥 |
| E2 | 同一目录的不同写法 | 相对路径和绝对路径命中同一个服务；符号链接写法得到 `None`：`abspath` 不解析链接，于是另起一个服务，抢 LOCK 失败 |
| E3 | 父进程写入后调用 `os.fork()`，子进程用同一路径建 client 并查询 | 子进程命中复制来的表项，拿到父进程的端口，由父进程的服务应答，结果正确 |
| E4 | 同 E3，子进程以 `sys.exit(0)` 正常退出 | 子进程的 atexit 对复制来的服务执行 close，产生四个后果：用复制来的 memtable 写出一个 segment；改写 manifest，父进程内存中的 manifest 从此与磁盘不一致；删除父进程正在使用的 WAL；用 `LOCK_UN` 释放了父子进程共享的 flock。父进程仍在运行时，第三个进程已能拿到 LOCK |
| E5 | 同 E4，之后父进程再写 50 行 | 父进程随后被 SIGKILL 时，重启后只剩 100 行，后写的 50 行丢失，因为它们写进了已被删除的 WAL；不 fork 的对照组能从 WAL 恢复全部 150 行。父进程随后正常退出时不丢数据，但子进程写出的 segment 成了 manifest 不再引用的孤儿文件 |
| E6 | 同 E3，子进程以 `os._exit(0)` 退出。multiprocessing 的 fork 子进程就是这样退出的（`popen_fork.py`） | 子进程不执行 atexit，父进程的 LOCK 与数据都不受影响 |
| E7 | 同 E6，父进程先退出，子进程再访问继承来的 URI | 两种退出方式下，子进程都在约 5–7 秒后收到连接失败。父进程正常退出时 LOCK 随之释放；父进程被 SIGKILL 时，子进程继承的 LOCK fd 仍持有锁，第三个进程要等子进程退出才能打开 |

E4、E5 违反了架构不变量 8（单进程独占 data_dir）和不变量 5（manifest 是唯一真值），也使持久化保证弱于不变量 10。触发条件是直接调用 `os.fork()`，且子进程正常退出。

## 结论

### 所有权

- **一个服务一套资源**：每个服务实例独占下列资源，停止服务时一并回收：
  - tokio runtime；
  - CPU 检索线程池：由服务创建后注入引擎，不走全局；
  - 引擎实例；
  - LOCK 文件描述符。
- **`crates/core` 与 `crates/grpc` 中不得有进程级的全局线程池，也不得有持有锁的全局状态**，例如 rayon 的全局线程池、全局 tokio runtime。CPU 任务只提交到服务自己的线程池，不得调用会落到 rayon 全局池的接口。否则 fork 出的子进程无法再启动新服务。
- **内存分配器默认用系统分配器**：jemalloc、mimalloc 这类分配器有自己的进程级锁和可选的后台线程，fork 之后在子进程里的状态要单独验证。如果基准显示值得更换，更换前必须通过下文「验证」中的全部 fork 用例。
- **服务记录创建它的进程号**：Rust 侧的服务句柄在停止或析构时，如果当前进程不是创建者，就什么都不做，直接 `mem::forget`：
  - 不 shutdown runtime、不 join 线程：子进程里没有这些线程，tokio 多线程 runtime 析构时要等待的工作线程并不存在；
  - 不 flush：复制来的用户态缓冲与父进程共享文件偏移，flush 会往父进程的 WAL 追加重复内容；
  - 不删 WAL、不改 manifest、不 `LOCK_UN`：这些都是父进程的状态，E4 正是反例。
  - 析构在创建者进程中也不得阻塞或 panic；需要停止时走显式的停止接口，panic 在 FFI 边界捕获（[AGENTS.md](../../AGENTS.md) 规则 6）。
- LOCK 沿用 [0002 约束 10](0002-data-dir-compat.md#共享状态的语义约束) 的 flock，不得改用 fcntl 记录锁。fcntl 记录锁的 fork 继承和 close 语义与 flock 不同，在 Linux 上两种锁也互不感知，无法与 3.x 互斥。子进程只 close 继承来的 fd 不会释放父进程的锁，只有 `LOCK_UN` 会。

### 同路径复用

- **表的键沿用 3.x 的 `os.path.abspath`**，命中时返回同一个 URI。符号链接等不同写法与 3.x 一样得到 `None`，不改用 `realpath`。
- **服务启动放在表锁之内完成**：同一路径并发首次打开时只启动一次，所有调用方拿到同一个 URI。这修正了 E1，是与 3.x 的行为差异，见下文「与 3.x 的行为差异」。
  - 代价一：启动期间，其他路径的首次打开也要排队。
  - 代价二：fork 也要等启动完成（见「fork」）。
  - 启动只做打开目录与绑定端口。引入 WAL 后，collection 在第一次被访问时才打开并恢复（[0006](0006-m2-scope.md) 第 16 项），恢复不在启动路径上，所以不需要改为按路径加锁。
- **启动失败时与 3.x 一样**记录完整错误日志并返回 `None`，例如 LOCK 被占、目录无法打开。已有 collection 在第一次访问时才按 [0006 第 14–16 项](0006-m2-scope.md#三数据目录的兼容) 判定：不支持或损坏的 collection 单独 fail-closed，不能把它显示为空库。以下情况不算已有数据：
  - `LOCK` 文件；
  - 空的 `collections/`、`databases/` 与 `.database-staging/` 目录：被 3.x 打开过、但没写过数据的目录只有这几项（见 [0002 约束 10](0002-data-dir-compat.md#共享状态的语义约束)）。

  `.database-staging/` 中如果有残留条目，取得 LOCK 后按 [0006 第 15 项](0006-m2-scope.md#三数据目录的兼容) 清理，不再按 M1 的规则拒绝整个数据目录。
- **`args` 参数与 3.x 一样忽略**。`release_server(path)` 与 `release_all()` 两个公开方法都保留。

### 进程退出

- **atexit 停止**：Python 胶水层在 atexit 中，对本进程创建的每个服务依次执行：
  1. 停止接受新请求，最多等在途请求 2 秒，超时后取消；
  2. 宽限结束后先设置公开 Db.enter 闸门为 closed，再关闭 runtime。切点之后才尝试入场的排队请求不执行；切点之前已入场但未应答的写入仍可能生效，客户端取消不表示回滚。这一顺序由 0012 在 2026-10-10 确认。
  3. 关闭引擎：等已接受的请求结束；完成最后一次有效 flush，并排空已接受的后台维护（[0010](0010-compaction-and-tombstone-gc.md)），再同步保留 WAL。两秒只限制 RPC 宽限，不是整个停服时限；正常退出可能多等若干轮合并，进行中的 I/O 没有固定墙钟上界；
  4. 释放 LOCK。

  整个停止期间释放 GIL。维护排空新增的退出等待已由负责人在 2026-10-09 确认；单轮大数据耗时和 Python 信号处理边界按 0010 附加条件验收。Ctrl-C 可能在原生停服调用返回后才被 Python 处理，不能保证即时中断排空。

  3.x 用的也是 2 秒宽限，但 `stop` 不等待就返回，引擎关闭与在途请求并发执行。本实现先等请求、再关引擎，是一处更严格的差异。
- **关闭引擎的持久化语义**：按 [0007「关闭」](0007-m2-storage-engine.md#dropcreate-与关闭) 提交 memtable、同步或保留 WAL，最后释放 LOCK；create/drop 改名后的目录 fsync 失败使数据库停止写入时，关闭只同步并保留 WAL。
  - 引入 WAL 后，每个写 RPC 返回前，WAL 必须已经写到 OS，保证进程被杀后能恢复；E5 的不 fork 对照组就是 3.x 的这一保证。
  - fsync 的时机不弱于 3.x（[AGENTS.md](../../AGENTS.md) 规则 3），具体见 [0006](0006-m2-scope.md) 第 19 项：Linux 与 macOS 上还保证断电不丢已 flush 的数据。
- **非阻塞析构**：正常情况下 `Server::drop` 把 Db、WAL 与 LOCK 交给后台收尾线程；它等在途任务结束并同步保留的 WAL 后才释放 LOCK，不沿用 M1 在最后一个任务结束时直接解锁的路径。后台线程创建失败时，析构线程例外地同步完成 WAL 收尾以保持已确认写入（[0007](0007-m2-storage-engine.md#dropcreate-与关闭)）。
- **停止失败**：
  - 同一服务内，flush 或 manifest 提交一旦失败，就不再执行删除 WAL 等后续的破坏性步骤，WAL 留给下次打开时恢复；LOCK 照常释放：WAL 已经留在磁盘上，释放 LOCK 之后，本进程或其他进程才能重开并恢复。`release_server` 走的也是这条停止流程，此时进程并不退出。
  - 错误写入日志并设置收尾失败标志；正常关闭因目录持久化未确认而跳过最终 flush 同样报告失败，与维护积压无关；显式 Server.stop 收尾后返回存储关闭错误，CLI 退出 1。嵌入服务的 atexit/release 接收者记录错误并继续停止其余服务；Drop 无接收者仍只日志。3.x 在这里直接 `except: pass`。此报告扩展不改变提交和 WAL 保留协议。
- **不执行 atexit 的退出方式**：SIGKILL、`os._exit` 和未处理的 SIGTERM 都不执行 atexit，与 3.x 一样依赖下次打开时的 WAL 恢复。

### fork

- **表锁与 fork**：胶水层用 `os.register_at_fork`（Windows 上没有这个函数，按平台判断后再注册），在 fork 前持有表锁，fork 后在父子进程中释放。这样子进程看到的表是一致的，表锁也不会在子进程中永久处于加锁状态。如果 fork 时另一线程正在启动服务，fork 要等启动结束，因为等锁时 GIL 已释放，所以不会死锁。直接从 C 调用的 `fork()` 不经过这些钩子。
- **继承来的路径**：子进程返回父进程的 URI，与 3.x 的 E3 一致，multiprocessing 的 fork 子进程可以照常访问父进程打开的库。父进程退出后，这些请求会失败。
- **新路径**：子进程启动自己的服务，使用新的 runtime 与线程池，服务归子进程所有，退出时由子进程停止。
- **子进程中的 atexit 与 `release_server`**：跳过继承来的服务，并保留其表项。不关闭、不解锁、不删 WAL，之后对同一路径的调用仍返回父进程的 URI。这修正了 E4、E5，是与 3.x 的行为差异，见下文「与 3.x 的行为差异」。
- **平台差异**：Windows 没有 fork。macOS 上 multiprocessing 默认用 spawn，Linux 上 3.10–3.13 默认用 fork，3.14 起改为 forkserver；但 `os.fork()` 在这些平台上都可用，规则相同。Python 3.12 起，在多线程进程中 fork 会发出 `DeprecationWarning`，3.x 的 gRPC 线程同样会触发，不做处理。

### 接受的残余风险

- **父进程异常死亡**：子进程继承的 LOCK 与监听 socket 的 fd 会一直保持到子进程退出，期间别的进程打不开该目录（E7），与 3.x 相同。
- **依赖库的全局状态**：日志或 tracing 的全局 sink、stdout 锁等不在上面「所有权」的约束之内。如果 fork 时刻，某个已消失的线程正持有这类锁，子进程启动新服务时可能死锁。由验证中的 fork 用例兜底。

## 与 3.x 的行为差异

按 [AGENTS.md](../../AGENTS.md) 规则 2 与「规则冲突时的优先级」，参照实现的 bug 不照抄，也不静默修正，由负责人决定。负责人已决定三处都修正，在此登记为已知差异：

| 差异 | 3.x | 本实现 | 理由 | 影响的用例 |
| --- | --- | --- | --- | --- |
| 同路径并发首次打开 | 一方得到 `None`，pymilvus 报 “Open local milvus failed” | 都得到同一个 URI | 3.x 做双重检查，本意就是复用；失败是实现上的竞态，不会有用户依赖它 | 验证中的「并发首次打开」 |
| fork 子进程正常退出 | 关闭父进程的服务：释放共享 LOCK、删除父进程的 WAL、改写 manifest，可能丢数据或留下孤儿文件（E4、E5） | 不触碰继承来的服务 | 违反正确性不变量 5、8；照抄会让 Rust 实现同样丢数据 | 验证中的「fork 子进程正常退出」「子进程 release_server」 |
| 退出时的在途请求 | 引擎关闭与在途请求并发执行 | RPC 宽限 2 秒后先封公开入场，再关 runtime，等已入场引擎任务收尾 | 避免请求在关闭过程中读写已关闭的状态 | 验证中的「正常退出」 |

## 验证

都用真实 pymilvus 跑。fork 相关用例只在 Linux 与 macOS 上跑，其余用例三个平台都跑：

| 用例 | 断言 |
| --- | --- |
| 同一路径打开两次，相对路径与绝对路径各一次 | 得到同一个 URI |
| 符号链接写法 | 返回 `None`，日志中有 LOCK 被占的原因 |
| 并发首次打开：8 个线程同时首次打开同一路径，重复多轮 | 全部得到同一个 URI，只启动一个服务 |
| 另一个进程持有 LOCK | 返回 `None`，日志写明原因 |
| 目录中有不支持或损坏的 collection | 服务能启动，`ShowCollections` 仍能列出它；首次访问时只让该 collection fail-closed，其余照常。取得 LOCK 后清理 `.database-staging/` 残留；`LOCK` 和空目录仍可正常打开 |
| `Server::drop` 非阻塞析构 | 正常情况下析构线程立即返回；后台收尾同步保留的 WAL 之前，另一进程仍拿不到 LOCK；线程创建失败的故障注入下析构线程同步完成 WAL 收尾后才返回，重开仍能恢复 |
| `release_server` 后再次打开同一路径 | 旧服务已停、LOCK 已释放，新服务可用 |
| 正常退出 | 最后一次有效 flush 的维护计数不丢，已接受维护全部完成后才释放 LOCK；不以 RPC 宽限作为总退出时限；重开数据完整，单轮大数据耗时与信号处理另按 0010 验收 |
| 正常退出时 collection 正在 flush | Parquet 与 manifest 提交、保留 WAL 的 fsync 尚未结束时，另一进程拿不到 LOCK；收尾完成后才能打开并恢复。此并发故障用例随完整 M2 验收执行 |
| 进程被 SIGKILL（引入 WAL 后） | 重开后从 WAL 恢复出全部已返回成功的写入 |
| 同时开两个服务，其中一个停止失败（故障注入） | 另一个照常停止；失败的那个保留了 WAL，重开后能恢复；错误写入日志 |
| 解释器退出时服务句柄析构 | 不 panic、不挂起 |
| fork 后，子进程访问父进程打开的路径 | 查询结果与父进程一致 |
| fork 子进程正常退出：子进程以 `sys.exit` 退出 | 父进程仍持有 LOCK，第三个进程拿不到；父进程的 WAL 与 manifest 不变；引入 WAL 后，父进程之后再写并被 SIGKILL，重开时数据完整（E5 的反例） |
| 子进程 release_server：对继承来的路径调用 `release_server` | 父进程不受影响；子进程之后再打开该路径，仍得到父进程的 URI |
| fork 后，子进程打开新路径 | 子进程内的服务可用；子进程正常退出后，该路径的 LOCK 被释放 |
| fork 发生在另一线程正在启动服务期间 | 子进程不死锁，能启动新路径的服务 |

## 放弃的方案

- **整个进程共用一个 runtime 与线程池**：线程更少，但 fork 后子进程继承的是一个没有线程的 runtime，要在子进程里检测并重建全局状态，还要保证没有任何代码碰到旧的全局对象。一个服务一套资源，「谁创建谁回收」就成了唯一规则。用户同时打开的路径通常只有一个，多出的线程可以忽略。
- **在 Rust 侧维护路径表**：可以少写一些 Python 代码，但 fork 前后的表锁处理需要 `pthread_atfork` 一类机制，而 Python 的 `os.register_at_fork` 已经解决了这件事。路径表只是胶水，不涉及引擎逻辑，放在 Python 侧也符合 [AGENTS.md「架构地图」](../../AGENTS.md#架构地图跨文件心智)「Python 绑定层只做胶水」的边界。
- **子进程在 fork 后关闭继承来的 LOCK 与监听 socket 的 fd**：可以消除第一条残余风险，但需要在 fork 钩子里跨过 Python 与 Rust 的边界操作原始 fd，而这个风险 3.x 同样存在，收益有限。
- **照抄 3.x 的行为**：见上文「与 3.x 的行为差异」。

## 独立 CLI 的停止策略

[0012](0012-cli-server.md) 复用宿主，但 CLI 的 RPC 宽限为 5 秒。首次 SIGINT/SIGTERM（Windows 支持的 SIGBREAK 单列验证）请求正常停服；进入原生 stop 前恢复本 CLI 接管信号的 SIG_DFL，之后的默认终止按进程崩溃恢复，PID 1/Windows 及监督器期限限制见 0012。嵌入用户进程不安装这些 handler，原有 fork 与路径表规则保持。

包装进程若转发 SIGINT，一次终端 Ctrl-C 可能导致后续默认终止，退出码非零、维护排空中断；不承诺严格按键次数。正常停服的收尾失败标志只报告已有错误，不把退出码 0 宣称为完整持久化证明。
