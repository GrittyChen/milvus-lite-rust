# 使用说明

本文是 [README](../README.md) 的补充：安装细节、平台与验收范围、嵌入使用与独立服务、导出、停止与恢复、后台维护与内存、性能。与官方 3.x 的差异和已知限制见[兼容性说明](compatibility.md)，各版本的变化见[发行说明](releases/)。

## 安装细节

### 与官方包共存

本包提供与官方发行包 `milvus-lite` 同名的 `milvus_lite` 模块和 `milvus-lite` 命令，两者不能同时安装（原因见 README「安装」）。绑定层在导入时检测已安装的官方发行包并明确报错；官方包后装、覆盖本包文件的情况无法检测。pymilvus 导入失败时的提示可能把用户引向官方包，按 README 的步骤卸载二者后只重新安装本包即可。

### 校验下载文件

README 给出的 `shasum -a 256 -c --ignore-missing SHA256SUMS` 在核对失败时不会执行后面的安装；Linux 上也可以用 `sha256sum -c --ignore-missing SHA256SUMS` 代替。

### Release 附件

每个 Release 有七个附件：四个平台的 wheel、sdist、候选结果 JSON，以及 `SHA256SUMS`。它们由本仓库的发行工作流在 GitHub 托管机器上从同一冻结提交构建与验收，在一次签发中取得构建来源证明，证明写入公开的透明日志。候选结果记录冻结提交、五个正式附件（四个 wheel 与 sdist）的哈希与各验收阶段的结果；`SHA256SUMS` 覆盖这五个附件与候选结果。

故障注入（FI）测试包单独构建，只用于验收，不随发行分发；正式包没有维护暂停与故障注入接口。

### Linux 上的 CPython 3.15

pymilvus 依赖的 orjson 目前只为 glibc 2.39 及以上提供 CPython 3.15 的预编译 wheel；在更低的 glibc 上安装 pymilvus 时可能要从源码构建 orjson，因此需要 Rust 工具链。本包自身的 wheel 不受影响。dump 所需的 bulk_writer 依赖 pyarrow，它在 CPython 3.15 上是否有预编译 wheel，取决于安装时 PyPI 上的版本。

### 从源码构建

源码构建需要 Rust 1.89 或以上和目标平台的链接工具；仓库开发与发行候选固定使用 Rust 1.98.1，sdist 用户使用自己的工具链，1.89 是源码发行的必过门槛。不需要 FAISS、CMake 或系统 protoc。

sdist 在验收矩阵内的平台上纳入验收；musl、其他架构与更低 glibc 的自行构建只是实验性尝试，没有支持承诺。Windows、free-threaded 与非 CPython 解释器在构建时明确拒绝：Windows 当前仍有目录与句柄同步缺陷，既不提供 wheel，也不允许从 sdist 装出已知不可用的版本。Windows 上没有安装 Rust 时会先得到 cargo 缺失的错误，本项目的拒绝检查可能还没执行。

## 平台与验收范围

| 平台 | 系统下限 | 发行候选的验收环境 |
| --- | --- | --- |
| Linux x86_64 | glibc 2.28 | GitHub 托管 `ubuntu-24.04` 上固定的 manylinux_2_28 容器 |
| Linux arm64 | glibc 2.28 | GitHub 托管 `ubuntu-24.04-arm` 上固定的 manylinux_2_28 容器 |
| macOS arm64 | macOS 11 | GitHub 托管 `macos-15` |
| macOS x86_64（Intel） | macOS 11 | GitHub 托管 `macos-15-intel`，原生执行 |

四个平台执行相同的适用门槛：正式 wheel 的归档与二进制检查；Rust 默认与故障注入构建的单测；CPython 3.10 与 3.15 上两条 SDK（pymilvus 2.6 与 3.0）的端到端全套；CPython 3.10 上两条 SDK 的 dump 端到端；3.11–3.14 的联合冒烟；正式 wheel 上与官方 3.x 的差分（含 dump）及 Arrow 15 / 25 / 26 的数据互通；故障注入测试包的恢复专项。Linux 另验 LOCK、strace 与容器 PID 1。

- Linux 在 glibc 2.28 的容器中运行，宿主内核是托管机器的较新版本；托管机器没有 macOS 11，最低系统版本由二进制检查核对，没有在 macOS 11 真机上运行。真实断电不在验收范围内。
- Python 元数据为 `>=3.10`，不预先封顶；验证窗口是 GIL CPython 3.10–3.15，未测的新版本不据此宣称通过。3.10 是兼容下限，不代表承诺解释器的安全维护。
- 每个版本的实际验收结果以 Release 附件中的候选结果与 Release 正文链接的验收记录为准。

## 嵌入使用

pymilvus 遇到以 `.db` 结尾的 URI 时在进程内启动本地服务：

- `.db` 的父目录必须存在；
- 同一路径复用同一个服务，`MilvusClient.close()` 只关闭客户端，不停止服务；
- 正常脚本退出时收尾服务，并排空已接受的后台合并。

### 自动 flush

每个集合的 MemTable 达到 128 MiB，或未提交的 WAL 达到 256 MiB 时自动 flush，与官方 3.x 相同。这是每个集合的 flush 时机，不是进程的内存上限。两个阈值可由环境变量 `MILVUS_LITE_MEMTABLE_MEMORY_LIMIT_BYTES`、`MILVUS_LITE_WAL_SIZE_LIMIT_BYTES` 设置（单位字节），在导入 `milvus_lite` 时读取一次；值不是正整数时导入抛 `ValueError`，pymilvus 打开本地 `.db` 也随之失败。

## 独立服务（CLI）

```bash
milvus-lite server --data-dir ./demo.db --host 127.0.0.1 --port 19530 --max-workers 10
# 等价入口
python -m milvus_lite server --data-dir ./demo.db --host 127.0.0.1 --port 19530 --max-workers 10
```

客户端使用 `MilvusClient(uri="http://127.0.0.1:19530")`。参数与官方 3.x 的 `milvus-lite server` 相同，合同见 [0012](decisions/0012-cli-server.md)。

- `--data-dir` 不要求 `.db` 后缀。`--port 0` 自动分配端口，启动后打印的 ready 行显示实际地址。
- 主机名只绑定解析结果中第一个成功 bind 的地址。当前验证的 pymilvus 2.6.17 / 3.0.2 会丢失 IPv6 URI 的方括号，使用这两条 SDK 时应绑定 `127.0.0.1`。
- 默认监听 `0.0.0.0`，即所有网卡；服务没有认证与 TLS，只在本机使用时请显式指定回环地址。
- `--max-workers` 控制引擎 RPC 的阻塞线程池，CLI 默认 10，嵌入服务的上限为 512；检索的计算池与后台维护线程独立。
- 退出码：argparse 的语法或数值错误退出 2；带方括号的主机格式错误、启动或停止失败退出 1。数值与格式校验在占用资源之前完成，DNS 或 bind 失败时可能已经创建了数据目录。任何存储收尾失败（包括因目录持久化未确认而跳过最终 flush）都会使显式停止报错、CLI 退出 1；退出 0 不能证明全部文件已确认持久化。

## 导出 dump

把一个集合的全部可见行导出为 Milvus BulkWriter 的 JSON 文件，用于导入 Milvus 或 Zilliz Cloud。只导出行，不导出 schema、索引、分区与别名，目标集合需要先在 Milvus 侧建好。设计见 [0037](decisions/0037-dump.md)。

```bash
pip install "pymilvus[bulk_writer]"
milvus-lite dump -d ./demo.db -c docs -p ./dump
milvus-lite dump --uri http://127.0.0.1:19530 -c docs -p ./dump
```

- **依赖**：需要 pymilvus 的 bulk_writer 附加依赖（pyarrow、minio、azure-storage-blob 等），本包不声明它，缺少时在打开数据目录之前报错。
- **数据来源**：`-d` 在 dump 进程内打开数据目录，路径必须以 `.db` 结尾（这是 pymilvus 的规则）；目录正被服务或其他进程占用时报 `Open local milvus failed`，此时改用 `--uri` 连接正在运行的服务。`--uri` 写成以 `.db` 结尾的本地路径时同样要求它已存在。
- **输出**：写在 `-p` 下新建的随机子目录中，文件依次为 `1.json`、`2.json`……；集合为空时不写文件。`-p` 的父目录必须存在，已有内容保留。
- **参数**：`--batch-size` 取 1–16384（默认 1000），`--chunk-size` 至少为 1 字节（默认 512 MiB）。chunk 越小文件越多，每多一个文件最多多等约 1 秒。
- **字段**：自增主键不导出，导入时重新分配；BM25 等 function 的输出字段不导出，导入时由 Milvus 按输入重新生成（因此 text embedding 的输出字段需要目标端有 provider 凭据）。
- **失败与信号**：失败时打印一行说明、退出 1，并删除本次创建的子目录；参数错误退出 2；SIGINT / SIGTERM 同样清理后退出 130 / 143。SIGKILL、断电、关闭终端或清理途中的第二次信号可能留下不完整的子目录；在容器 PID 1 上第二次信号不起作用，dump 照常清理后退出。
- **与官方 3.x 的差异**：带 BM25 function 或 ARRAY VARCHAR 字段的集合在 3.x 上导出失败，这里正常导出；3.x 在批大小为 0 时报告成功却导出 0 行、在写文件失败时报告成功却没有文件、遇到 NaN 或 Infinity 时写出不合法的 JSON，这里分别拒绝参数、报告失败、报错并指出字段与主键；3.x 失败时打印 traceback。
- **已知限制**：VARCHAR 主键含双引号或反斜杠时 pymilvus 的分页查询失败，无法导出；`--uri` 指向仍在写入的服务时，导出不是一致快照；`--uri` 会把已 release 的集合重新 load。

## 停止与恢复

- 第一次 SIGINT / SIGTERM 请求正常关闭。RPC 宽限时间 CLI 为 5 秒、嵌入服务为 2 秒；之后仍要等已进入引擎的任务、flush 与后台维护完成，总退出时间没有硬上限，可能等待若干轮合并。宽限切点之后排队的写入不执行；切点之前已进入、但客户端尚未收到结果的请求仍可能生效。
- 进入原生停止流程前，信号处理切回默认行为，普通进程收到后续信号可能异常终止。包装进程转发 SIGINT 时，一次 Ctrl-C 可能变成两次信号，使进程以非零码退出并打断维护排空。
- **容器 PID 1 没有“第二次 SIGTERM 强制退出”的退路**。部署时建议使用 `--init`，或配置足够长的 stop timeout，由监督器到期后 SIGKILL。
- SIGKILL 不执行正常停止。下次打开按 WAL 与 manifest 恢复已确认的写入；这是进程崩溃的恢复边界，不等于真实断电实验。生命周期合同见 [0003](decisions/0003-in-process-server-lifecycle.md)。

## 后台维护与内存

- 后台维护（flush 后的合并与删除记录回收）按数据库串行执行，成功时默认不输出诊断；可在启动前设置 `MILVUS_LITE_COMPACTION_DIAGNOSTICS=1` 观察。删除状态有额外的内存成本；历史故障目录可能使维护延后或被拒绝。设计见 [0010](decisions/0010-compaction-and-tombstone-gc.md)。
- **Linux（glibc）**：后台维护每轮结束后调用 `malloc_trim`，把 flush、合并与打开集合释放的空闲内存交还系统，作用于整个进程（[测量与代价](testing/allocator-retention.md)）。
- **macOS**：系统分配器会把已释放的内存块留作缓存，实测 flush 后比重开后多约 300 MiB（10 万与 100 万行），本实现无法在进程内关闭。需要时可在启动 Python 之前设置环境变量 `MallocLargeCache=0`，它作用于整个进程，对其他代码的性能影响未测。
- **写入峰值**：大批量写入时，进程内存的峰值出现在自动 flush 期间，百万行时约 1229–1309 MiB，高于 2.5.1 的约 940 MiB（[测量](testing/parquet-row-group-memory.md)）。
- 反复 upsert / delete 的负载下，被覆盖的旧版本行要等合并回收，内存随写删次数增长；同样负载下本实现的增长慢于 2.5.1 与 3.x（[测量](decisions/0040-reclaim-superseded-rows.md#搁置理由)）。
- 性能结论只适用于各自记录中的测量场景，不保证任意负载都优于官方 2.5.1。

## 性能

以下是同机对照（原生 x86_64 服务器容器，128 维 FLAT，各记录中的数据集与轮次），只适用于各自记录中的测量场景，不保证任意负载都优于官方 2.5.1 或 3.x。

| 场景 | 本实现 | 2.5.1 | 3.x | 记录 |
| --- | --- | --- | --- | --- |
| 百万行写删负载下的检索 p50（每个周期 upsert 5 万、删除 1 万并 flush） | 约 12 ms | 约 105 ms | 约 1.8 s（时间预算内只跑到第 17 个周期） | [0040 搁置理由](decisions/0040-reclaim-superseded-rows.md#搁置理由) |
| 同一负载每个周期的耗时 p50 | 2.7 s | 41.7 s | 60 s | 同上 |
| 百万行写入并 flush 后的服务端内存（Linux） | 约 742 MiB | 约 930 MiB | — | [分配器保留内存](testing/allocator-retention.md) |
| 百万行写入期间的进程内存峰值 | 约 1229–1309 MiB | 约 940 MiB | — | [Parquet 行组上限](testing/parquet-row-group-memory.md) |

写入期间的峰值仍高于 2.5.1，见上文「后台维护与内存」。批量检索的对照见[批量检索记录](testing/batch-search-performance.md)，只在原生 x86_64 上测得。

