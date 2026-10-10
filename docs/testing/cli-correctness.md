# CLI server 实施与验收

日期：2026-10-10。合同为负责人确认的 [0012](../decisions/0012-cli-server.md)，参照 milvus-lite v3.2.1 / `43d1257`。冻结范围复审通过，覆盖 macOS arm64 与普通进程方式的 Linux 容器；不代表首发或生产可用。原始产物、数据和日志保存在仓库外。

## 实现落点

- Python 标准库的 cmdline/__main__ 与 console script 共用入口，run_server 的名称和参数保留。真实服务分支限制主线程；参数解析/帮助不启动宿主。
- NativeServer.start 追加可选 max_workers，现有四参数调用保持；ServerConfig 直接保存 Option。CLI 默认 10 个阻塞处理者，省略为 512；I/O 线程环境只解析一次，正数/usize/相加容量检查在资源创建之前完成。计算池与维护线程独立。
- bound_address 缓存实际 socket 地址，hostname 保留首个成功 bind；port 0 和 IPv6 的 ready 可用。没有终止观察、相关 pid 检查、人工结束 serve 的注入点或缓存 URI 重启策略。
- 共享 stop 在 RPC 宽限结束后先封 Db.enter，再关 runtime，最后关闭引擎、排空维护和释放 LOCK；0003 与代码一起更新。
- 首次停止信号只设 Event，主循环短周期等待；stop 前恢复 SIG_DFL，正常结束后还原旧 handler。固定关闭行与参照一致。包装进程转发、PID 1、Windows 及监督器超时的限制在 README/0012 披露。
- 收尾失败原子标志覆盖关闭的 flush、扫描/清理/同步、panic、关闭期间维护错误、目录持久化未确认导致跳过最终 flush，以及异常 worker join。Server.stop 在释放 LOCK 后报告 StorageShutdown，原 transport 原因保留在错误链；CLI 退出 1。Drop 无接收者仍只记录；退出 0 非断电/完整持久化证明。

## 产物身份

同一源码构建 macOS arm64 / abi3-py310 正式及独立 FI wheel。后者仅供已有的维护暂停/退出组合，排队请求的新增测试钩子仅为 Rust cfg(test)，没有 Python 接口。

| 产物 | native SHA256 |
| --- | --- |
| 正式 wheel | `7cfd2363d5b368d1a07fd19f4a81183cab3cbdae0db2b87d5abb52c329ccca64` |
| release + fault-injection wheel | `e7df3d5b80215f9cc37f6722bb952178f11c46eb659e6a2204ac6c0ff1a563bb` |

正式/FI 产物都不包含 Rust cfg(test) 的排队/停服钩子字符串；正式产物不提供维护 FI 方法。两条 SDK 装当前 wheel 后在仓库外执行；不是修改源码后沿用旧扩展。

## 关键证据

| 边界 | 证据与结果 |
| --- | --- |
| worker 1/3 | 真实 tonic RPC 进入受控钩子，确认仅 N 个处理体可同时入场，额外任务已排队；释放后全部返回正常，计算池大小不随参数改变 |
| 宽限超时与 queued write | 首个已入场 Insert 卡住，第二个已排队；runtime 关闭后的明确检查点保持，引擎入场已 Closed。释放首个后仅它可写入；第二个拒绝，不消耗 seq。恢复仅 id 1，current_seq=1，客户端两次都未收到成功不被误解为回滚 |
| 变异判别力 | 去掉 runtime 关闭前的 begin_shutdown，队列入场测试失败；把 shutdown_failed 恒改为 false，存储关闭错误测试失败；恢复静默跳过最终 flush 的旧逻辑，新增队列为空的回归在 pending=false 断言失败。两源文件按 SHA 完整还原，再跑 grpc 全套通过 |
| 关闭故障 | Rust FI 分别制造关闭 panic、prepare panic、manifest 目录同步失败、保留 WAL 同步失败；stop 错误、闸门关闭、LOCK 释放、重开确认行保持。异常 worker join 的结果传给关闭标志 |
| 封锁闸门与维护积压 | 先排空创建任务，再在写入闸门内固定积压 0/1 个维护任务，等 cleanup_started 后放行。两侧都 shutdown_failed=true；积压一侧明确完成一次维护失败。未提交 WAL 字节保持，manifest 未创建，释放 LOCK 后恢复一行；健康重开的关闭标志为 false |
| 正式 CLI 存储失败 | 未 flush 的已确认行，manifest.tmp 被目录占据；SIGINT 后真实 CLI 退出 1，诊断包含关闭 flush 失败及 storage shutdown failed，WAL 留存；修复测试障碍后重开可读 |
| 两信号 | 隔离 FI CLI 在维护暂停时通知 PAUSED，首次信号后 DEFAULT 检查点确认 SIG_DFL 已设置，再发后续信号。普通 macOS 子进程以信号终止，重开四个已确认主键都在；没有靠 sleep 猜切点 |
| 正常停止与 EOF | 正式进程不设 PYTHONUNBUFFERED，ready 立即可读且是实际地址。stdin EOF 后仍可 CRUD/动态过滤/检索；SIGINT/SIGTERM 退出 0，固定关闭行一次；重开覆盖/删除和字段值正确 |
| hostname 与 IPv6 | 当前 macOS localhost 解析为 ::1 / 127.0.0.1。ready 只报告实际监听地址，另一地址连接被拒；占用首个地址的固定端口后回退至另一地址；[::1] 子进程真实 ShowCollections RPC 后 SIGTERM 正常退出。测试按端口 0 的实际 bind 能力筛选地址并保留解析顺序；可绑定地址不足两个时验证占用后拒绝，不把它写成双地址回退证据；IPv6 不可用时明确验证 CLI bind 失败、无 ready |
| 错误与胶水 | 非法数字/环境不产生数据目录；两入口的坏 TOKIO_WORKER_THREADS 都普通异常而非 PanicException。非法括号 host [bad] 退出 1、不创建目录、无 ready；非主线程在资源前拒绝，主体/stop 双错都可观察且主体不被掩盖 |
| 与 3.x | 实际 CLI 两侧在 max-workers=1 下执行 CRUD/动态过滤、停止及重开比较；0/-1 workers 的资源创建/退出码差异按 0012 独立断言 |
| 包装产物 | console script 和 python -m 安装后可用；仅装 wheel 的 Python 3.10 环境无 pymilvus/numpy/grpcio，也能起 CLI，由外部客户端实际访问并正常停止。sdist 在仓库外解压后通过 maturin/cargo 构建 wheel，无系统 protoc/C++ 工具要求 |

两个上游入口测试原样复制，来源与 Apache-2.0 已标明；不混入 adapter 的原有 637 条统计。默认通配 host 无认证/TLS 的提示已同步 README。

## 套件结果

| 套件 | 当前构建实跑 |
| --- | --- |
| cargo test --workspace | 251 passed，core lib 147、grpc 19 |
| core fault-injection | 323 passed，lib 169 |
| grpc all-features | 20 passed，关闭故障单测包含四种注入 |
| 默认 / all-features workspace Clippy -D warnings、fmt | 通过 |
| 正式 wheel Python，pymilvus 3.0.2 / 2.6.17 | 各 272 passed（CLI/入口单独 30） |
| 正式 wheel 差分，对照 3.x，两 SDK | 各 122 passed |
| 正式 wheel 存储互通，Arrow15/25，两 SDK | 各 68 passed |
| benchmark + platform 脚本 | 58 passed |
| 独立 FI wheel 暂停/退出/信号，两 SDK | 各 5 passed |

既有七条 Python 弃用警告来自已存在 API 用例，没有 skip/ignore。mutation 之后的恢复源与构建源相同，后续修改只涉及测试/文档时不写成重新测量产物。

## 扫描、顺序与限制

扫描 ServerConfig/start/parse_io_threads/max_blocking_threads/bound_address、spawn_blocking/enter/begin_shutdown/shutdown_timeout/close，核对参数 → 宿主 → RPC → 切点 → 引擎收尾完整路径。扫描 finish_storage/eprintln/report_shutdown_failure/record_maintenance_shutdown_failure/stop/join，确保已发生收尾失败有标志和日志，不能仅捕获 Python Exception 伪造失败报告。扫描 signal/Event/wait/SIG_DFL/run_server，核对 handler、启动取消、最终关闭及还原的资源所有权。

确定性形态检查：主线程、单项 port/workers、括号 IPv6、I/O worker 环境、相加容量先于 DataDir/线程池/监听创建。操作能力失败：目录/LOCK、DNS/bind、资源创建按真实返回处理，保留既有打开目录先于 bind 的优先级；不以预检宣称消除实际 I/O 竞态。关闭切点只设公开入场 closed，不抢先设置 cleanup_started 或停止维护。

初版证据来自 macOS arm64；Linux CLI 与 IPv6 能力边界的补充实跑见下节。Linux/PID 1、Windows 控制台/新进程组、CTRL_BREAK_EVENT 和 Windows 默认终止码没有实跑；对应测试分支/矩阵尚须在平台验收执行，不从 POSIX 推导通过。EMFILE 的 CPU 忙等只是未验证线索，不写成事实，不用 fd 耗尽替代终止或退出证据。

没有真实断电、跨平台 strace、新性能基准或全部 3.x 能力的声明。PID 1 与包装进程可能改变信号交付；原生调用、在途引擎及维护的总等待没有硬期限。当前 hostname 只绑定首个成功地址的差异继续保留，完整首发验收/发布授权仍独立进行。

## 关闭判定与监听测试的修复复核

正常关闭请求最终 flush 时，若 write_gate 已标为目录持久化未确认，沿用禁止提交和保留 WAL 的协议，同时记录收尾失败。异常 close_nowait/Drop 有意不请求最终 flush，不因这个条件新增失败；真实 WAL 同步错误仍由原边界报告。失败标志不被维护队列是否为空改变，Server.stop 沿用原有 StorageShutdown 映射。没有调整闸门、runtime、维护 join 或 LOCK 的顺序。

新增 hostname 测试通过 pymilvus 的生成协议桩向 ready 地址直连，设置单通道 grpc.enable_http_proxy=0，不改环境代理。实测 SDK 2.6.17/3.0.2 的 URI 重组会将 IPv6 [::1]:port 变成 ::1:port；因此这些测试证明监听地址与真实 Milvus RPC 可用，不声明 MilvusClient 的 IPv6 URI 转换正确，也不修改第三方 SDK。其余端到端和 CLI 差分继续使用真实 MilvusClient。

README 的 argparse 语法/数值错误 2、host 校验与启动/停止错误 1 分别说明，CLI 章节置于许可之前。0012 和 0003 同步记录因持久化未确认而跳过最终 flush 的失败判定。

修复扫描覆盖 finish_storage(true/false)、write_gate、cleanup_started、维护错误报告、shutdown_failed 和 Server.stop；确认正常/有积压/无积压/异常收尾路径的提交、WAL 保留与入场切点未改变。Windows、Linux/PID 1 和真实断电仍未验证；普通 Linux 子进程的补充结果见下节，不把它推导为 PID 1 或其它平台结论。

## Linux IPv6 能力边界复核

Docker Engine 29.8.2，Linux aarch64 / 7.0.14-linuxkit。源码快照包含当前未提交实现与测试，复制到容器自身文件系统；未包含 references、构建目录或个人记录，测试以非 root 用户执行。正常容器为 Debian 13 / Python 3.13.5，禁用 IPv6 容器为 Debian 12 / Python 3.10.15，两者均使用 pymilvus 3.0.2。

Linux 正式 wheel 由当前 Rust 源码重新构建，native SHA256 为 `94e7fb3189b424c77d2ceef46595d9cb01b693433b60113cc8c23e80a8430575`。测试修复前后使用同一个产物，没有改 Rust、绑定层或 CLI 代码。

正常容器 net.ipv6.conf.all.disable_ipv6=0。另一容器以 --sysctl net.ipv6.conf.all.disable_ipv6=1 启动，并在容器自身 /etc/hosts 保留 ::1 localhost：这版 Docker 默认会移除该项，因此显式保留评审指出的目录配置组合。实测 getaddrinfo 仍返回 IPv4 与 IPv6，IPv4 bind 成功，::1 bind 返回 EADDRNOTAVAIL。

| 测试与状态 | 修复前 | 修复后 |
| --- | --- | --- |
| 正常 Linux：hostname / 地址回退 / 括号 IPv6 / 非法括号 host | 4 passed | 4 passed |
| 禁用 IPv6 且保留 hosts：同组测试 | 2 passed、2 failed（地址回退、括号 IPv6） | 4 passed，无 skip |

修复后的完整 tests/python：正常 Linux 和禁用 IPv6 的 Linux 各 272 passed；macOS 两条 SDK（3.0.2 / 2.6.17）也各 272 passed，均无 skip。七条弃用警告来自既有用例。Linux 两侧是上述不同 Python/发行版环境，不把它表述为仅改变 sysctl 的同环境对照；单独的 bind 能力与失败路径已实际确认。

地址候选先逐个执行端口 0 的 bind；只排除明确的地址或协议不支持错误，资源耗尽、权限等其它 OSError 继续抛出。hostname 只对可 bind 的地址集合建立期望；只有一个可绑定地址时，验证占用后 CLI 非零退出且没有 ready。显式 [::1] 在可用时验证真实 RPC 与正常停止，在不可用时验证启动失败，不跳过用例。

README 同步区分资源创建前的数值/括号格式校验，与打开数据目录后的 DNS/bind 失败；没有把所有 host 错误说成无目录副作用。该增量仅修改测试和说明，沿用已验证的 Rust、差分与互通产物，不将未重跑的套件算作此次新实跑。

容器里的 CLI 是普通子进程，不是 PID 1；本节不覆盖 PID 1 信号、Windows、真实断电或完整 Linux M2 故障矩阵。
