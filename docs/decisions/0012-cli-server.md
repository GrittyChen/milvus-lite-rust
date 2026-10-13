# 0012 CLI server 的入口、工作者与退出

- 状态：已确认（2026-10-10）；实施完成，冻结范围复审通过（2026-10-10）
- 日期：2026-10-09（调研启动）
- 复核修订：2026-10-10
- 确认：前五项按草案；第 2、4 项附同步文档条件，第 6 项改为收尾失败标志与 stop 错误
- 参照：milvus-lite v3.2.1 / `43d1257`
- 前置：[0008 首发范围](0008-first-release-scope.md)、[0003 生命周期](0003-in-process-server-lifecycle.md)、[0005 检索线程模型](0005-search-thread-model.md)、[0007 存储协议](0007-m2-storage-engine.md)、[0010 关闭排空](0010-compaction-and-tombstone-gc.md)

## 背景与范围

动态字段过滤和 compaction 已落库，首发的下一切片是独立进程 CLI。提供 `milvus-lite server` 与 `python -m milvus_lite server`，让进程外的 pymilvus 客户端连接同一个 Rust gRPC 宿主。

本切片覆盖参数、帮助、实际监听地址、信号、正常关闭与失败诊断。dump、新的 RPC、daemon 化、PID 文件、日志框架、认证/TLS、通用配置文件不进入此切片。原有 `.db` 接入、同路径服务表、fork 处理、检索池和磁盘格式保持。共享启动路径补齐线程参数错误处理；共享停止路径已收紧宽限期结束后的入口边界，两项按已确认的矩阵实施。

以下是负责人已确认的实施合同。涉及 RPC 阻塞池配置、停止入场切点及 CLI 信号语义；对外差异按规则 2 登记。能力与验收完成状态以实施记录为准。

## 参照事实与本地探针

权威代码为 [cmdline.py](../../references/milvus-lite/milvus_lite/cmdline.py)、[server.py](../../references/milvus-lite/milvus_lite/adapter/grpc/server.py)、[模块入口](../../references/milvus-lite/milvus_lite/__main__.py)及 [test_dump_tool.py](../../references/milvus-lite/tests/test_dump_tool.py)。CLI 最近变更来自 `04fb87a` 的纯 Python 重写；文档中的并发描述不替代当前代码。

- console script 指向 `milvus_lite.cmdline:main`，模块入口使用 `SystemExit(main())`。
- 没有子命令时打印帮助、返回 0。缺少 data-dir、非整数 port 等 argparse 错误返回 2。
- server 的 data-dir 必填；host 默认 `0.0.0.0`，port 默认 19530，max-workers 默认 10。max-workers 控制 grpcio ThreadPoolExecutor，不限制整个进程的线程数。
- `run_server` 先打开 Db，再创建工作池、bind、start；打印监听信息，等待终止。KeyboardInterrupt 打印 `Shutting down...`，finally 调 `server.stop(grace=5)` 和 `db.close()`。
- SIGTERM 没有自定义处理。参照 `db.close()` 吞掉 collection.close 的异常；不能把进程退出码 0 当作逐项持久化确认。

在 macOS / CPython 3.12.14 / 真实 3.2.1 环境运行独立空库子进程，未修改参照 checkout。原始 JSON/脚本在仓库外。

| 探针 | 实际结果 |
| --- | --- |
| 无子命令 / 缺少 data-dir | 退出 0 / 2 |
| port 非整数 | 退出 2；指定目录未创建 |
| max-workers 为 0 / -1 | 退出 1；指定目录已创建；ValueError: max_workers must be greater than 0 |
| 启动后 SIGINT | 退出 0；打印关闭信息 |
| 启动后 SIGTERM | 子进程 returncode -15；未打印关闭信息 |
| port 0 | 启动成功，但监听行显示 0 |
| port -1 / 65536 | 启动成功，监听行显示原请求值；本探针不据此断言实际绑定端口 |
| host `::1` / `[::1]`，port 0 | 前者绑定失败，退出 1；后者成功 |

另用当前正式 Rust native 对照：裸 `::1` 可绑定实际临时端口，原样 `[::1]` 输入则地址解析失败，说明 CLI 的括号规范化确有必要。

探针设置了 PYTHONUNBUFFERED=1 以观察启动行。这些空库结果不证明有写入时的 flush/维护排空或故障持久化；后面的实施矩阵必须单独验证。

当前实现相关事实：

- tokio 1.53.1 的阻塞池在 BUSY 循环中继续取队列任务；shutdown_timeout(ZERO) 不保证所有排队闭包取消。仓库外最小 Rust 探针用 worker=1、受控释放，实跑确认排队闭包可在 shutdown_timeout 返回后执行。真实 RPC 在闭包里才调用 Db.enter，当前 runtime 关闭与 Db.close 之间存在入场窗口。
- 本机 localhost 同时解析到 ::1 和 127.0.0.1。真实 3.x 对两种地址均可 TCP 连接；当前 Rust 只可连接 ::1，127.0.0.1 被拒绝。只检查 `TcpListener::bind` 能成功不足以证明 hostname 的两侧监听一致。
- tonic 0.14.6 的 io_stream 将 EMFILE 一类错误向上返回，外层 server/mod.rs 的 Some(Err) 分支记录后 **continue**，不因此退出 serve 任务。RLIMIT_NOFILE 耗尽行为没有实测，不作其它结论。
- 普通 macOS 子进程探针在 FI 维护暂停时接收首个 SIGTERM 请求，设置 SIG_DFL 后以明确检查点通知父进程；之后的 SIGTERM 使进程退出 -15，重开仍有全部四个已确认主键。它证明该默认信号策略与进程恢复边界，不是 PID 1/Windows 或真实断电验证。
- 当前 serve_internal 的退出条件是停止信号或 incoming 返回 None；TcpListenerStream 的 poll_next 只返回 Some(Ok)、Some(Err) 或 Pending，MakeSvc 的 poll_ready/call 也固定成功。连接/请求在各自任务中处理；在当前具体配置下没有核实到监听任务自然结束的路径，不为未来组件失败预留观察 API 或专用终止注入点。
- ServerConfig 当前只在绑定与 server.rs 的配置 helper 两处构造。

## 推荐方案

### 1. 包入口与胶水边界

在 crates/py 的 Python 包新增 cmdline.py、__main__.py，在 pyproject.toml 注册 `milvus-lite = milvus_lite.cmdline:main`。仅使用 Python 标准库解析参数和处理信号；调用已有 NativeServer，不引入 numpy、grpcio、pymilvus 或新的 Rust 依赖作为 CLI 的运行要求。

帮助与参数解析先完成，server 分支再导入 native。包级的官方发行包冲突检测照旧：同时安装官方 milvus-lite 与本包仍拒绝导入，不能为了帮助命令绕过冲突策略。本包仅列出实际支持的 server 子命令；dump 明确不可用、非零退出，不放成功占位。

cmdline 保留 `run_server(data_dir, host, port, max_workers)` 同名同签名入口，server_func 只转交解析结果；上游 monkeypatch 的分发用例可原样复用。模块入口用例也保留原测试主体。

帮助和参数解析可从任何线程调用；真实 run_server 必须在 Python 主线程运行，检查发生在导入 native、安装 handler 或打开目录之前。非主线程的 server 分支返回解释明确的错误，main 返回 1；直接调用 run_server 抛出普通 Exception。参照允许非主线程阻塞调用，这条限制须登记，不能让 signal.signal 的 ValueError 在创建资源之后才冒出。

启动时新增缓存的 bound_address getter，仅返回实际监听地址；不碰运行期互斥锁，不新增进程 pid 检查。

复用服务启动/停止路径。CLI 直接拥有一个 NativeServer，不经 server_manager 的 `.db` 路径表，不依赖 pymilvus 客户端或 stdin 来维持服务。stdin EOF 不表示停止。CLI 主循环只等停止请求，使用 Event.wait(0.2) 的短周期，避免在 Windows 上依赖无超时 Event.wait 被 Ctrl-C 打断；不将该周期当作 OS 调度硬上界。

### 2. 参数与输出

| 参数/输入 | 方案 |
| --- | --- |
| data-dir | 必填，使用用户给出的路径；沿用 DataDir 的创建、LOCK、类型与恢复检查；不要求后缀为 .db |
| host | 默认 0.0.0.0；接受 IPv4、hostname、IPv6。括号 IPv6 规范化后交给共享宿主；hostname 只绑定解析序列中首个能成功 bind 的地址，不绑定全部地址。ready 使用实际 local_addr，IPv6 加括号；其它解析地址不承诺可连接 |
| port | argparse int，允许 0..65535；0 使用实际分配端口。负数/越界在打开目录前返回参数错误 2 |
| max-workers | argparse int，默认 10，必须为正且能表示为目标平台 usize；非法值在创建资源前拒绝。实际线程容量的安全检查见下一节 |
| 帮助/缺少参数/非法整数 | 沿用 argparse 的退出约定；server 参数名与默认值一致。仅支持 server 导致顶层帮助和子命令 choices 与参照不同 |
| ready 输出 | 原监听句式与 data-dir repr，但地址取实际 local_addr，不继续显示可能不匹配的 hostname；port 0 输出实际端口。立即 flush stdout，不依赖 PYTHONUNBUFFERED 或 TTY |

确定性的数值错误必须在首次资源创建之前检查；真实目录/LOCK、DNS/bind、线程启动错误仍由实际操作返回。保留现有打开目录先于 bind 的启动顺序和错误优先级，不声称所有启动失败均无目录副作用。

### 3. max-workers 的 Rust 含义

推荐对应 CLI 所属 tokio runtime 的 `max_blocking_threads`。引擎操作 RPC 的处理体经过 service.run → spawn_blocking，因而限制这些处理体的同时执行数。tokio I/O 工作者、Rayon 检索池与单 Db 维护线程均不受这个参数限制；GetVersion、compaction 应答等纯 async 处理和未实现 RPC guard 不占该池。它不是总线程数或连接数上限，也不限制等候请求队列长度。

NativeServer.start 在现有四个参数后追加可选 max_workers。ServerConfig 直接加入 `max_blocking_threads: Option<NonZeroUsize>`，CLI 填 Some，绑定省略参数及现有测试 helper 填 None；维持一个启动实现。None 显式使用当前已有的 512 个阻塞线程上限，CLI 默认 Some(10)。计算池仍取 available_parallelism；这个字段不改变计算任务的提交方式。

CLI 和 `.db` 共享启动路径，在打开目录/创建池之前一次性解析最终 I/O 工作者数：沿用 tokio 的 TOKIO_WORKER_THREADS 或 available_parallelism（不可获取时为 1）规则，必须是正 usize。将同一个解析值显式交给 builder，避免校验和构建分别读取环境。

当前 Cargo.lock 的 tokio 1.53.1 在构建阻塞池时相加 `max_blocking_threads + worker_threads`；两个正数须 checked_add 成功。无效环境、usize 溢出或总容量溢出返回启动错误，不传入会 panic/wrap 的值，也不新增任意的业务线程上限。有效环境下的 `.db` I/O 工作者数和阻塞上限不变。非法 TOKIO_WORKER_THREADS 在两条入口均返回普通启动错误，绑定转成 RuntimeError，由 server_manager 的 except Exception 记录并返回 None；不能留下 PanicException 逃逸。此修复只收敛启动参数，不修改 URI 缓存失效策略。

### 4. 宽限结束、排队请求与入口闸门

推荐共享 Server.stop 使用明确切点：先停止接收并等待 RPC 宽限；在宽限正常结束或超时之后，**先关闭 Db 的公开 enter 闸门，再 shutdown_timeout(ZERO)，最后 Db.close**。新增内部拒绝入场操作仅设置 gate.closed，必须幂等、非阻塞，不设置 cleanup_started、不等待在途任务、不 abort 维护、不先释放 LOCK；最终 flush/排空仍由原 close 完成。

该顺序适用于 CLI 的 5 秒与嵌入服务的现有 2 秒。修改 0003 的停止顺序须由负责人确认，不能仅在 CLI 中靠 Python 标志阻止 Rust 闭包进入。宽限期间仍可进入的旧请求继续按原协议处理。

按真实 enter 的时间分三类：

1. 已向客户端返回成功：正常关闭重开后效果必须保留。
2. 切点之前已经取得 InFlight，但客户端未收到结果/被取消：可完成写入，恢复态允许包含该操作；网络失败不等于引擎回滚。
3. 切点之后才尝试 enter 的排队请求：必须返回 Closed，不能消耗 seq、追加 WAL 或改变数据。客户端连接可能已经取消，不能承诺它一定收到某个完整 RPC 错误码/原文。

worker=1 的测试放在 grpc crate 的 Rust 层，用 cfg(test) 的受控 RPC 钩子与 Server 测试构造注入，沿用 after_search_snapshot 的风格；不向 fault-injection wheel 新增 Python 接缝。测试卡住一个已入场任务、排入写请求、耗尽宽限后确认切点，再释放前者。确认已入场候选的处理、后入场队列不生效、LOCK 直到收尾完成才释放；再覆盖队列任务恰在切点之前取得 enter 的交错。门内已开始的操作结果使用候选集合，不写“所有客户端失败的写入都不存在”。

### 5. 信号与正常退出

推荐 POSIX 的第一次 SIGINT 或 SIGTERM 请求正常停服。主线程 handler 仅设置停止标志；不调用 stop、操作引擎或抛异常。Windows 若有 SIGBREAK，也接管为正常停止请求，供新进程组的 CTRL_BREAK_EVENT 使用，必须在 Windows 真机/CI 验证。

停止输出固定为参照原文 `\nShutting down...`，只输出一次并 flush；SIGTERM 和 SIGBREAK 的这条输出属于新增行为。调用 NativeServer.stop(5.0)：五秒仅是 RPC 宽限，随后等待已有引擎任务、最终 flush 和维护排空，再同步 WAL、释放 LOCK。

启动期间收到请求时，原生启动返回后立即收尾，不发送误导的 ready 行。进入原生 stop **之前**，把本 CLI 接管的停止信号改成 SIG_DFL，而不是恢复可能仍会抛 KeyboardInterrupt 的 Python 旧 handler。普通 POSIX 进程此后的停止信号走 OS 默认终止，作为进程崩溃；不需要 Python handler 获得 GIL。正常收尾完成后才恢复调用前保存的 handler；不重入半次 stop。

信号设置有过渡窗口：first handler 生效期间多次到达的信号可能被合并，只承诺“安装 SIG_DFL 后到达的信号按默认处理”，不承诺严格第几个键击。原生 start/stop 的 Python handler 仍可能延迟；SIGKILL 和其它强制终止不执行正常排空，沿用崩溃恢复边界，不推广为真实断电保证。

Linux PID namespace 的 PID 1 有特殊信号规则，SIG_DFL 不能被当成容器内可靠的二次终止手段；可使用外部 init/监督进程或最终 SIGKILL。本设计不为 PID 1 引入原生信号处理器，边界见 [pid_namespaces(7)](https://man7.org/linux/man-pages/man7/pid_namespaces.7.html)。

监督器的期限是外部期限，不是服务的总退出保证：Docker 默认先发 SIGTERM，未另配置时 Linux 等 10 秒、Windows 等 30 秒再强制终止，可由 stop timeout/stop signal 配置更改；见 [Docker stop](https://docs.docker.com/reference/cli/docker/container/stop/)。systemd 默认 DefaultTimeoutStopSec 为 90 秒，实际单位设置、停止动作及最终信号可能覆盖，见 [systemd-system.conf(5)](https://man7.org/linux/man-pages/man5/systemd-system.conf.5.html) 与 [systemd.service(5)](https://man7.org/linux/man-pages/man5/systemd.service.5.html)。部署文档要求为 RPC、在途引擎任务、flush 和维护留出总时间；超过外部期限就是异常终止，不把 5 秒 grace 或单轮测量当作不被强杀的保证。

Windows 的 TerminateProcess/os.kill(SIGTERM) 不是可处理的 POSIX SIGTERM；CTRL_BREAK_EVENT 依赖控制台与进程组，未具备该环境前不能声称已验证。Windows 的二次 SIGBREAK 默认终止也须实测，不用 POSIX 的 returncode=-signal 推导退出码。

### 6. 错误传播及现有关闭边界

CLI 单项数值/argparse 解析错误退出 2；host 校验错误（例如 [bad]）退出 1，校验仍在资源创建前；Rust 环境或组合线程容量配置错误退出 1，仍发生在创建资源之前；目录/LOCK/bind/资源启动及 native 返回的错误退出 1 并保留原因。收到正常停止请求并完成当前 stop 协议时退出 0。任何已启动的服务都在 finally 中尝试停止；主体与 stop 都失败时保留主体错误，另写出 stop 原因，不能让第二个错误掩盖第一个。

Db.close 继续返回 ()，新增原子收尾失败标志：flush、WAL 扫描/清理/同步及关闭 panic 的已发生错误均设置标志并保留原诊断；正常关闭排空期间的维护错误及维护线程异常 join 也记录，不能因为发生在 worker 上而漏报。正常关闭因数据库目录持久化未确认而跳过最终 flush 也算收尾失败，不依赖维护队列是否有积压；仍禁止 manifest 提交并同步保留 WAL。仅拒绝入场和有意取消不算存储收尾失败。

Server.stop 在所有收尾结束、LOCK 释放后检查标志；失败返回 StorageShutdown，若 transport 同时失败则把原原因放入错误链。CLI 返回 1；嵌入 server_manager 按已有 except Exception 记录 stop 错误并继续停止其它实例。标志是失败事实，重试成功不擦掉本次关闭中已发生的失败。

这一扩展不回滚提交、不修改 WAL 保护或关闭闸门。Drop/close_nowait 没有同步错误接收者，继续记录日志而不报告调用结果。退出 0 仍不代表任何平台、故障或真实断电下所有文件都已确认持久化，CLI 不输出“全部保存成功”。

## 与不变量及已有决策的关系

- 增加 CLI 胶水、收敛共享启动参数检查和正常停止的入场切点，不改变 seq 分配、列/位图、WAL、manifest、Parquet、删除或 GC。
- 正常关闭保留 0003/0007/0010 的 LOCK/flush/维护顺序；新增的公开入场拒绝切点在 runtime 关闭之前，不以五秒 grace 提前释放 LOCK，不停止维护后丢最终 flush 的触发。
- 计算池的提交约束、四查询分组、分波及快照继续按 0005；CLI 参数不减少计算线程或使叶子拿锁。
- 官方包冲突、abi3 与 fork 的已知限制照旧。更改信号策略仅作用于独立 CLI 进程。

## 与参照的差异（已确认）

| 场景 | 参照/已有行为 | 推荐方案 | 理由与影响用例 |
| --- | --- | --- | --- |
| POSIX SIGTERM | 直接终止，不打印关闭行 | 首次请求正常关闭，通常返回 0，并打印固定关闭行 | 监督进程停止；WAL/维护/LOCK 与外部超时用例 |
| 进程继承 SIGINT=SIG_IGN（如非交互 shell 中以 `&` 启动的后台作业） | Python 只在 SIGINT 为默认处置时安装 KeyboardInterrupt 处理器；SIGINT 不起作用，服务持续运行，直到 SIGTERM 等默认终止（2026-10-13 对 3.2.1 / `43d1257` 实测：10 秒后仍在运行，SIGTERM 后退出 -15） | 显式接管 SIGINT，与正常处置相同：正常停服、退出 0，并打印固定关闭行 | 与 SIGTERM 取向一致，明确的停止请求按正常停服处理，不取决于继承的处置；rc1 候选验收中发现，实测记录见[首发验收](../testing/first-release-acceptance.md#候选过程中的修复)；差分 CLI 用例以测试进程的默认 SIGINT 处置为前提 |
| 停止时新的排队任务 | 现有 Rust runtime shutdown 与 Db.close 之间仍可能取得 enter | 宽限结束后先拒绝入场，再关 runtime | 已入场未应答仍可能生效，后入场不得写入；共享 stop 修改另行确认 |
| 停止期间信号 | 参照 Python 等待可能被后续信号打断 | stop 前 SIG_DFL，之后的信号按默认动作处理 | 普通 POSIX 进程可强制终止；PID 1 与 Windows 特殊规则分列 |
| 无效 worker/port | 0/-1 worker 先建目录、退出 1；越界 port 探针能启动 | 提前参数错误，退出 2 | 无资源副作用，不能用截断/空转接受无效数值 |
| 大整数 workers | ThreadPoolExecutor 接受超过 usize 的正整数 | 超过 usize 或与 I/O 线程总容量相加溢出即拒绝 | 目标平台计数边界，不新增任意业务上限；边界反证 |
| TOKIO_WORKER_THREADS | 3.x 不使用；现有 Rust 非法时 panic，PanicException 会逃逸 | 两个入口共享解析，非法值普通 RuntimeError | `.db` 按已有返回 None/日志约定失败，不隐藏 BaseException |
| hostname 监听 | grpcio 对本机 localhost 两个解析地址都监听 | 保留 std 首个成功地址；ready 显示实际 bound_address | 另一解析地址可能不可连，客户端用实际 IP；双地址用例明确对照 |
| port 0/输出刷新 | 显示请求值 0；普通 print | 实际端口并立即刷新 | 管道读取可用地址，不设置 PYTHONUNBUFFERED 验证 |
| IPv6 写法 | 裸 ::1 探针失败，括号形式成功 | 规范化后接受两种形式、显示括号地址 | 复用现有 socket 能力，不声称 hostname 全地址绑定 |
| worker 拓扑 | grpcio executor | CLI 阻塞池，计算/维护线程独立 | 实际 RPC 并发上限；不是总线程/队列容量上限 |
| 非主线程 server 调用 | 参照可阻塞调用 | 创建资源前拒绝，main 返回 1；其它帮助/解析仍可用 | 不安装跨线程信号方案，真实线程用例 |
| Windows SIGBREAK | 参照未接管 | 作为首次正常停止请求 | CTRL_BREAK_EVENT 新进程组测试；TerminateProcess 不承诺优雅 |
| 启动错误呈现 | 参照未捕获异常会输出 traceback | CLI 简短说明完整原因，退出 1 | 系统错误原因保留，stdout 不输出虚假 ready |
| CLI 能力面 | server/dump | 只列出 server，dump 非零拒绝 | 首发范围，帮助不虚构能力 |
| 关闭存储错误 | 参照吞 collection 异常，退出码不报告 | 保留日志/WAL 保护；标志使 stop 返回错误，CLI 退出 1 | 覆盖全部收尾错误、因持久化未确认跳过最终 flush 与 panic；嵌入停止也记录，Drop 仍只日志 |

## 放弃的方案

- 新建 Rust CLI binary、再维护 Python subprocess 启动层：增加分发与生命周期两份实现；当前 wheel 的原生宿主已经满足需求。
- 让 max-workers 控制 Rayon 池或所有线程：改变 0005，与参照的 RPC executor 含义不同。
- 忽略 max-workers 只接受参数：不能证明该参数有实际效果。
- 只处理 KeyboardInterrupt、stop 期间持续保留 Python handler、或在 handler 直接关闭：前者缺少推荐的监督进程正常停止，后者可能重入/打断半次关闭。
- hostname 绑定全部解析地址：需要协调多 listener、共享临时端口、重复/双栈重叠与部分失败清理；本切片登记首个成功地址差异，输出实际地址。
- 新增通用信号线程、共享 runtime、daemon/PID 文件、通用配置类：当前单 CLI 宿主不需要。

## 实施与验收

实施顺序：共享线程数解析、可选 worker 配置、入场拒绝切点、收尾失败标志及实际监听地址 getter → Python 参数与信号胶水 → 打包入口、子进程对照与文档。同步 AGENTS 架构/命令、README 能力/限制、0003 的停止顺序/CLI 信号差异、0008 状态；能力与实跑结果在验收结束时同步。README 同步说明默认 0.0.0.0 监听所有网卡，当前没有认证/TLS；仅本机使用应显式给 --host 127.0.0.1，不能把默认值描述为仅回环监听。

| 组合 | 必须固定的结果 |
| --- | --- |
| console script / python -m、无子命令/帮助、解析错误 | 返回码/文本与默认值正确，帮助不启动服务，错误不静默成功；两套正式 wheel 的安装后入口 |
| data-dir/host/port/workers，省略及重复参数、非整数/0/负数/大整数 | 适用 argparse 规则和已登记差异，确定性错误发生在创建资源之前；usize/Tokio 容量边界不 panic |
| I/O worker ENV 缺失/正常/0/非法/非 Unicode/超范围 | 共享解析与两个入口的实际 builder 一致；无效环境下 `.db` 返回 None/普通异常，不能逃逸 PanicException；有效默认数不变 |
| worker 1/3，多于上限的阻塞 RPC | 用受控开始/释放断言实际并发上限，不只 mock 参数；有等待者的关闭不早释放 LOCK，计算池独立且无叶子阻塞死锁 |
| worker=1、宽限超时、队列写入、切点前后 enter | 已成功操作保留；已入场未应答按候选集合；切点后排队操作不消耗 seq/WAL/行，LOCK 收尾后才释放 |
| hostname 双地址解析、其中一个地址无法 bind | 首个成功规则、实际 ready 地址与另一个地址的不可连接差异分别固定；不推导与 grpcio 全地址一致 |
| main(argv) 非主线程、run_server 同签名 monkeypatch | 真正 server 提前拒绝，无目录/线程；上游分发和模块入口用例本体可原样复用 |
| port 0/固定/已占用、IPv4/hostname/IPv6、LOCK 冲突、目录为文件 | 只在成功启动后输出 ready，真实端口可连；错误有原因/非零码，无遗留自有线程/LOCK；不关闭外部拥有的 listener 或 Db |
| EOF、启动/运行/停止期间 SIGINT/SIGTERM、后续默认信号 | EOF 不停服；固定关闭行只打印一次；stop 前 DFL 的检查点后发第二信号并验证崩溃恢复，不能仅靠延时猜时序；PID 1 分列 |
| Windows SIGBREAK、新进程组、强制终止 | CTRL_BREAK_EVENT 正常/再次终止实际验证；无控制台/TerminateProcess 按强制终止，不伪造 POSIX 返回码 |
| 写入未 flush、已有/在途 flush、超过四文件触发维护、正在 Query/Search | 正常停止后重开完整，LOCK 全程保护；复用原故障/关闭矩阵，不以空库探针代替 |
| 主体失败 + stop 失败、存储关闭失败 | 不覆盖原错误；所有收尾存储失败均报告 stop 错误/CLI 1，保留 WAL；闸门封锁时分别固定维护队列有积压/无积压，均须报告失败；Drop 的日志式边界单列 |
| SIGKILL / Windows 进程终止 | 单独按异常终止恢复；不与正常 SIGTERM 混称，真实断电范围不变 |

两条 SDK 使用当前正式 wheel，通过 CLI 监听地址做 CRUD、顶层过滤、FLAT/分页、flush/关闭/重开；对照 3.x 的正常参数/错误/启动/信号语义，登记差异分别断言。源码层复用上游两个入口用例；Rust 默认/FI、Clippy、Python 与差分为受影响回归门槛。该切片不宣称性能提升，不重测历史性能矩阵。

## 已确认的取舍与附加条件

1. **线程配置与启动错误收敛**：ServerConfig 直接增加可选阻塞上限；CLI 默认 10、`.db` None 保持 512，计算/维护线程独立。两个入口共用 I/O 线程解析与容量检查，无效环境返回普通错误而非 panic。
2. **宽限结束的拒绝入场切点**：共享 Server.stop 在 RPC 宽限结束后，先关闭 Db.enter，再关 runtime、最终 close。改动覆盖 CLI 和嵌入服务，需同步 0003；已入场但未应答的写入仍可能生效，切点后未入场的队列不得改变数据。
3. **hostname 与 ready**：本切片保持首个成功解析地址，不做多地址 listener；打印实际 bound_address。接受与 3.x 全地址监听不同，双地址解析时另一地址可能不可连，用户可明确给 IP。
4. **信号与强制退出**：第一次 POSIX SIGINT/SIGTERM 正常停服；原生 stop 前置 SIG_DFL，随后信号按进程崩溃处理。Windows 增加 SIGBREAK 的实测方案；PID 1、监督器期限和强制终止的特殊边界明示，不承诺所有平台二次信号都能强杀。
5. **CLI 参数/调用差异**：非法数值提前拒绝，过大整数不截断；port 0 实际端口/刷新，IPv6 规范化，帮助仅 server。保持 run_server 同名同签名，真正 server 分支只允许主线程；解析和帮助不受该线程限制。
6. **关闭失败报告**：沿用日志与 WAL 保护；增加收尾失败标志，任何存储收尾失败都映射为 stop 错误，CLI 退出 1。退出 0 仍不宣称所有文件均已确认持久化；Drop 无接收者继续只记录日志。

附加条件：第 2 项的共享停止顺序与 0003 在同一次改动同步；第 4 项在 README 明示包装进程转发 SIGINT 可使一次 Ctrl-C 产生两次信号，进程异常终止、非零退出并打断维护排空，数据依赖崩溃恢复。

设计与实施已授权；提交、推送或发布仍需另行授权。

## 当前实现与验收

实现、自验证产物身份及未验证平台边界集中在 [CLI 实施记录](../testing/cli-correctness.md)。2026-10-10 冻结合同范围内复审通过，证据覆盖 macOS arm64 与普通进程方式的 Linux 容器；Linux PID 1、Windows 与真实断电仍未验证。第 2、4 项附加条件与第 6 项修订都在代码/文档同一改动内落实；CLI 不宣称首发或生产验收完成。
