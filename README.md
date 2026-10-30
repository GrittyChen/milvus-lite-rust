# milvus-lite-rust

以 Rust 实现的非官方 milvus-lite，与 Milvus / Zilliz 无隶属关系。目标是让 `pymilvus` 用户继续使用 `MilvusClient("./demo.db")`，并读取官方 3.x 的数据目录。

**`0.3.0rc3` 为预发布试用版本，不建议用于生产。** 本文对应该版本的源码与发行候选，下方安装命令在对应 GitHub Release 发布后使用。已发布版本以仓库 Releases 页面为准；本项目目前未发布到 PyPI，PyPI 上若出现同名包，并非本项目发布。下方四个平台按同一标准验收层执行，实际结果以 Release 附件中的候选结果与 Release 正文链接的验收记录为准。

源码：<https://github.com/GrittyChen/milvus-lite-rust>；问题反馈：<https://github.com/GrittyChen/milvus-lite-rust/issues>。设计决策与验收记录在源码树的 docs 目录中维护。

## 安装与包冲突

本包的发行名即下方安装命令中的包名，提供 `milvus_lite` 模块及 `milvus-lite` 命令。它与官方发行包 `milvus-lite` **不能同时安装**，pip 不会阻止同名模块被覆盖；后装者覆盖文件，卸载任一方也可能损坏另一方。

先移除官方包：

```bash
python -m pip uninstall milvus-lite
```

pymilvus 从 PyPI 安装；本包只从 GitHub Release 的附件安装，`--no-index` 保证 pip 不向 PyPI 查询本包，`--only-binary :all:` 让不受支持的平台直接失败而不是尝试源码构建：

```bash
python -m pip install pymilvus
python -m pip install --no-index --only-binary :all: --find-links https://github.com/GrittyChen/milvus-lite-rust/releases/expanded_assets/v0.3.0rc3 milvus-lite-rust==0.3.0rc3
```

需要先核对文件时，从 [v0.3.0rc3 Release](https://github.com/GrittyChen/milvus-lite-rust/releases/tag/v0.3.0rc3) 下载本平台的 wheel 与 `SHA256SUMS`，在同一目录核对后安装本地文件：

```bash
shasum -a 256 -c --ignore-missing SHA256SUMS && python -m pip install --no-index ./milvus_lite_rust-0.3.0rc3-*.whl
```

核对失败时不会执行安装。Linux 上也可以用 `sha256sum -c --ignore-missing SHA256SUMS` 代替 `shasum -a 256 -c --ignore-missing SHA256SUMS`。

Release 附件共七个：四个平台的 wheel、sdist、候选结果 JSON，以及 `SHA256SUMS`。它们由本仓库的发行工作流在 GitHub 托管机器上从同一冻结提交构建与验收，在一次签发中取得构建来源证明，证明写入公开的透明日志。候选结果记录冻结提交、五个正式附件（四个 wheel 与 sdist）的哈希与各验收阶段的结果；`SHA256SUMS` 覆盖这五个附件与候选结果。

需要核对来源时，在下载目录用已登录的 GitHub CLI 执行本平台对应的一行（依次为 Linux x86_64、Linux arm64、macOS arm64、macOS x86_64），成功即说明该文件由本仓库的发行工作流签发：

```bash
gh attestation verify milvus_lite_rust-0.3.0rc3-cp310-abi3-manylinux_2_28_x86_64.whl --repo GrittyChen/milvus-lite-rust --signer-workflow GrittyChen/milvus-lite-rust/.github/workflows/release.yml
gh attestation verify milvus_lite_rust-0.3.0rc3-cp310-abi3-manylinux_2_28_aarch64.whl --repo GrittyChen/milvus-lite-rust --signer-workflow GrittyChen/milvus-lite-rust/.github/workflows/release.yml
gh attestation verify milvus_lite_rust-0.3.0rc3-cp310-abi3-macosx_11_0_arm64.whl --repo GrittyChen/milvus-lite-rust --signer-workflow GrittyChen/milvus-lite-rust/.github/workflows/release.yml
gh attestation verify milvus_lite_rust-0.3.0rc3-cp310-abi3-macosx_11_0_x86_64.whl --repo GrittyChen/milvus-lite-rust --signer-workflow GrittyChen/milvus-lite-rust/.github/workflows/release.yml
```

**不要使用 `pymilvus[milvus-lite]`**：该 extra 会引入官方发行包。若两个发行包已经同时存在，应卸载二者后只重新安装本包。绑定层会拒绝检测到的共存，但官方包后装覆盖本模块的情况无法由本包检测；pymilvus 的导入错误提示也可能把用户引向官方包。

开发者可在当前源码根目录构建安装：

```bash
python -m pip install .
```

源码构建需要 Rust 1.89 或以上、目标平台的正常链接工具；仓库开发工具链固定为 1.98.1。sdist 用户使用自己的 Rust 工具链，MSRV 是源码发行的必过门槛。无需 FAISS、CMake 或系统 protoc。

## 平台与 Python 边界

| 平台 | 系统下限 | 候选验收 |
| --- | --- | --- |
| Linux x86_64 | glibc 2.28 | 标准层：GitHub 托管 `ubuntu-24.04` 上固定的 manylinux_2_28 容器 |
| Linux arm64 | glibc 2.28 | 标准层：GitHub 托管 `ubuntu-24.04-arm` 上固定的 manylinux_2_28 容器 |
| macOS arm64 | macOS 11 | 标准层：GitHub 托管 `macos-15` |
| macOS x86_64（Intel Mac） | macOS 11 | 标准层：GitHub 托管 `macos-15-intel`，原生执行 |

四个平台执行相同的适用门槛：正式 wheel 的归档与二进制检查，Rust 默认与故障注入构建的单测，CPython 3.10 与 3.15 上两条 SDK 的端到端全套，3.11–3.14 的联合冒烟，正式 wheel 上与官方 3.x 的差分及 Arrow 15/25 数据互通，以及故障注入测试包的恢复专项；Linux 另验 LOCK、strace 与容器 PID 1。Linux 在 glibc 2.28 的容器中运行，宿主内核是托管机器的较新版本；托管机器没有 macOS 11，最低系统字段由二进制检查核对，未在 macOS 11 真机运行。真实断电不在验收范围内。

Python 元数据保持 `>=3.10`，不预先封顶；首发验证窗口是 **GIL CPython 3.10–3.15**，未来未测版本不据此宣称通过。3.10 保留为兼容下限，不以此承诺解释器的安全维护。

Linux 上使用 CPython 3.15 时，pymilvus 依赖的 orjson 目前只为 glibc 2.39 及以上提供预编译 wheel；在更低的 glibc 上安装 pymilvus 时可能需要从源码构建 orjson，因此需要 Rust 工具链。本包自身的 wheel 不受影响。

继续提供 sdist：矩阵内纳入验收；musl、其它架构及较低 glibc 的自行构建仅为实验性尝试，无支持承诺。**Windows、free-threaded 与非 CPython 在构建时明确拒绝**；Windows 当前仍有目录/句柄同步缺陷，首发既无 Windows wheel，也不允许 pip 从 sdist 安装出该已知不可用版本。Windows 未安装 Rust 时会先得到 cargo 缺失错误，本项目的拒绝检查可能尚未执行。

abi3 wheel 供普通 GIL CPython 使用。正式包没有维护暂停/故障注入接口，FI 测试包单独构建，只用于验收，不随发行分发。

## `.db` 使用示例

```python
from pymilvus import MilvusClient

client = MilvusClient("./demo.db")
try:
    client.create_collection("docs", dimension=2, metric_type="L2")
    client.insert("docs", [dict(id=1, vector=[1.0, 0.0], category="tech")])
    print(client.query("docs", filter="category == 'tech'", output_fields=["id"]))
    print(client.search("docs", [[1.0, 0.0]], limit=1))
    client.flush("docs")
finally:
    client.close()
```

`.db` 的父目录必须存在。同路径服务复用，`MilvusClient.close()` 关闭客户端而非本地服务；正常脚本退出会收尾服务并排空已接受的后台合并。

本候选的产品运行源码与依赖继承 0.3.0rc2，能力与已登记差异全部继承，不新增功能、RPC、数据格式或依赖；`crates/` 中相对 0.3.0rc2 的变化全部位于 `cfg(test)`，发行二进制不编译。仓库另包含持续集成、测试工具、发行工作流与文档改动，这些不进入运行时二进制或产品依赖。本版起候选改由发行工作流构建、验收并签发来源证明，范围与依据见源码树中的 0015–0018、0021 与 0022 决策。本版实际验收结果以对应 Release 链接的记录为准，开发验收不替代最终产物验收。

`0.3.0rc3` 的范围包括基本 CRUD、Upsert/Delete、Flush/关闭重开、FLAT 的 L2/IP/COSINE、标量与动态字段比较、and/or/not、in/not in、LIKE、is null/exists、JSON 与动态字段路径、json_contains/array_contains 系列及 array_length、后台 compaction 与 tombstone 回收，以及下述已有能力。过滤按官方 3.x 的整条表达式路线执行，各路线对缺失值、短路和类型错误的处理不同；范围与参照缺陷见 0014。算术运算、text_match、几何与时间谓词、BM25/function chain、稀疏向量、数据库 RPC 等范围外能力保持明确的 UNIMPLEMENTED（alias、rename、truncate 与集合属性在 0.3.0rc3 同为 UNIMPLEMENTED，开发线见下文「集合管理」一条），不静默忽略条件。官方 3.x 数据互通仍受类型、索引与已登记历史目录限制约束，不宣称兼容它的全部功能。

- 显式分区支持 Create / Drop / Has / Show / statistics，以及写入和读取的分区选择；LoadPartitions / ReleasePartitions 仍未实现。读取先选分区再判版本，删除按主键全局生效；Drop 保留其它分区仍需的全局删除记录，避免重开后已删除行复活。目录安全拒绝和参照差异见 0017。
- partition key 支持非主键 INT64 / VARCHAR，按参照固定哈希路由至 16 桶，另保留 `_default`。生成 auto_id 取分桶后的行序号，返回顺序仍对应原请求，可能得到 `[1,3,2]`；确定性校验 / 转换失败在追加任何 WAL 前整批拒绝，自动 ID 会覆盖可读旧行时整批拒绝。真实 I/O 故障仍按已校验前缀恢复，不保证整批原子写入。
- 非 key 同批重复主键 Upsert 的每一行现在都从请求前的旧值独立合并。旧行 `dict(id=1,x=0,y=0)`，同批 partial Upsert `[dict(id=1,x=1),dict(id=1,y=2)]` 的结果从已发布 0.2.0rc1 的 `dict(id=1,x=1,y=2)` 变为 `dict(id=1,x=0,y=2)`，与参照 3.x 一致；分成两个请求时仍依次合并。
- 集合管理（开发线，尚未随预发布交付）：别名的创建、改指、删除、查询与列表，以及所有集合 RPC 对别名的解析；RenameCollection（改名后按重新打开的状态，不支持跨数据库改名）；TruncateCollection（保留分区、索引定义、别名与 load 状态，auto_id 从 1 重新分配）；AlterCollection 的集合属性（`timezone` 仍为 UNIMPLEMENTED）。只开放默认数据库，已存在的非默认数据库与数据库 RPC 保持 UNIMPLEMENTED。`schema.json`、`aliases.json` 等共享 JSON 改为与 3.x 相同的键排序格式写出；改名与清空各有一个短暂存在的私有意图文件（文件名与格式见 0002），由本实现重开时收尾，因此降级到不含该功能的版本之前须用本实现正常打开并关闭一次。与 3.x 的差异见 0023。
- nullable / 默认值支持 BOOL、INT8/16/32/64、FLOAT/DOUBLE、VARCHAR、JSON；主键与向量的 nullable 仍不支持。
- ARRAY 支持 BOOL、INT8/16/32/64、FLOAT/DOUBLE、VARCHAR 元素及父列 null；不支持嵌套、JSON 或向量元素。存量数组中的元素 null 可保留，但经 Query/Search 投影时报错，与参照实现一致。
- Query iterator 按 INT64 / VARCHAR 主键排序，每个请求取得独立快照；不提供跨请求的一致性快照。
- range search 沿用 3.x 的有限候选窗口，不是全量范围扫描；阈值附近可能出现已登记浮点误差范围内的成员差异。服务端不实现 Search iterator V2，会忽略 V2 探测参数并返回不含 V2 结果字段的普通 Search 结果；SDK 据此自动回退 V1，与官方 3.x 一致。V1 的探测算法在部分批量和数据分布下可能提前结束，不保证任意场景都遍历全部数据。

数据目录沿用 3.x 的 schema.json、Arrow WAL、Parquet 和 manifest format 2 编码，不增加私有数据格式。回退到已发布 0.2.0rc1 时，非默认分区 manifest 或 partition key schema 会使相应 collection 被明确拒绝，不能降级后继续读写这些集合；拒绝不应改坏目录，健康 collection 仍可打开。删除全部非默认分区且不含 key 等旧版不支持属性后，仅剩 `_default` 的目录可能回到旧版能力范围，具体见 0017。更早的 0.1.0rc1–rc3 还会拒绝 nullable / ARRAY 等新 schema 属性，见 0015。

0.3.0rc2 起提供的 JSON 写入优化，收益限于 0018 已审的同机工作负载，不外推为任意 JSON、并发吞吐或全部写入路径的性能保证。进程峰值资源差距尚未解决，继续单独跟进。

向量检索只有 FLAT（精确暴力检索）。新建索引时，FLAT 与 BRUTE_FORCE 原生支持；AUTOINDEX（pymilvus 快速建表使用）、HNSW、HNSW_SQ、IVF_FLAT、IVF_SQ8 可创建并原样保存定义，向量仍按 FLAT 执行，召回不低于 ANN 但数据量大时更慢。标量字段上的 INVERTED 可创建，过滤仍按扫描执行，结果可能与官方在已建索引数据上的结果不同。新建稠密向量索引时，AUTOINDEX、HNSW、HNSW_SQ、IVF_FLAT、IVF_SQ8 只接受 COSINE / L2 / IP；BM25 / NONE 返回 UNIMPLEMENTED，避免写出官方 3.x 无法 load 的定义。FLAT / BRUTE_FORCE 保持原度量校验。

DropIndex 的名字行为与 3.x 一致：普通字段名或匹配不到的名字会删除全部索引；`<字段名>_idx` 或明确传入 field_name 才定向删除。collection 需先 release，删完全部索引后自动变为 Loaded。

打开官方 3.x 写入的目录时，索引定义原样保留；load 成功后，向量仍按 FLAT、标量仍按扫描执行。官方 3.x 也无法构建的索引类型（如 IVF_PQ、DISKANN、向量字段上的 INVERTED），在 collection 尚未 load 且已有 flush 落盘的数据（包括已删除的行）时，与官方一样 load 报错。存量 AUTOINDEX、HNSW、HNSW_SQ、IVF_FLAT、IVF_SQ8 定义配 BM25 / NONE 时，collection 尚未 load 且至少一个有物理行的当前 segment 缺少匹配的 3.x 侧车，load 与官方一样报错；已删除的行仍计入。匹配路径全有、只有 WAL 或已经 loaded 时，度量检查不拒绝 load。侧车内容和构建参数仍不校验：官方因参数无效而 load 失败的目录，这里可能照常 load；参数与度量同时无效时，报错原因也可能不同，见源码 0002。

上述新建 ANN / INVERTED、DropIndex 和存量度量检查已从 0.1.0rc3 提供，已发布的 0.1.0rc2 不包含；0.1.0rc2 也允许新建 AUTOINDEX 配 BM25 / NONE。or / not、not in、LIKE、is null / exists、JSON 路径与 contains 等过滤扩展同样从 0.1.0rc3 开始提供。对于官方 3.x 也无法构建的索引类型（如 IVF_PQ、DISKANN）的 load 检查，0.1.0rc1 不做，会照常 load。

索引变更在旧索引文件删除落盘未确认时，会只封锁该 collection 的 CreateIndex / DropIndex。服务重开或容器重启不能证明恢复；可信的内核重启可自动解除，overlayfs、共享目录或启动来源不可信时需要人工恢复：确认承载数据目录的系统已经真实重启，停止对应服务或退出 Python 进程，删除下面列出的最终标记，再启动服务。最终标记为异常目录等条目时移出这个保留路径，不递归删内容；符号链接只移除链接本身。不要改 manifest、索引文件或 snapshot，也不要仅修改标记里的启动标识。若只是已安全操作的标记清理失败，可在**同一个服务实例内**重试，无需系统重启；实例退出后按磁盘标记重新判定，仍可能保守封锁。完整协议见源码仓库中的 0014 设计决策。

最终标记：`<data_dir>/collections/<collection>/milvus-lite-rust.index-intent.json`。

## 独立 CLI server

```bash
milvus-lite server --data-dir ./demo.db --host 127.0.0.1 --port 19530 --max-workers 10
# 等价入口
python -m milvus_lite server --data-dir ./demo.db --host 127.0.0.1 --port 19530 --max-workers 10
```

客户端使用 `MilvusClient(uri="http://127.0.0.1:19530")`。data-dir 不要求 .db 后缀；port 0 自动分配，ready 行显示实际地址。hostname 只绑定解析结果中首个成功 bind 的地址。当前验证的 pymilvus 2.6.17/3.0.2 会丢失 IPv6 URI 方括号，用这两条 SDK 时应绑定 `127.0.0.1`。

默认 `0.0.0.0` 监听所有网卡，当前没有认证/TLS；仅本机使用应显式指定回环地址。max-workers 控制引擎 RPC 阻塞池，CLI 默认 10，嵌入服务保持 512 上限；计算池和维护线程独立。现在仅提供 server，dump 尚未纳入。

argparse 语法/数值错误退出 2；括号 host 格式、启动和停止错误退出 1。数值/格式校验在资源前完成，DNS/bind 失败可能已经创建目录。任何存储收尾失败（包括因目录持久化未确认跳过最终 flush）均使显式 stop 报错、CLI 退出 1；退出 0 不是全部文件已确认持久化的证明。

## 停止、恢复与限制

首次 SIGINT/SIGTERM 请求正常关闭。CLI 的 RPC 宽限为 5 秒，嵌入服务为 2 秒；随后仍等已入场引擎任务、flush 和后台维护。总退出时间没有硬上限，可能等待若干轮合并。宽限切点后的排队写入不执行，切点前已入场但客户端未收到结果的请求仍可能生效。

进入原生 stop 前信号切到 SIG_DFL，普通进程的后续信号可使其异常终止；包装进程转发 SIGINT 时，一次 Ctrl-C 可能成为两次信号，退出非零并打断维护排空。**容器 PID 1 此时没有二次 SIGTERM 强制退出的退路**；部署建议 `--init`，或配置足够的 stop timeout，由监督器到期后 SIGKILL。

SIGKILL 不执行正常停服。下次打开按 WAL/manifest 恢复已确认写入；这是进程崩溃恢复边界，不等于真实断电实验。真实断电、Windows 生命周期和各平台尚未完成的验收不由正常 CRUD 或容器运行推导。

后台维护按 Db 串行，成功诊断默认静默，可在启动前设置 `MILVUS_LITE_COMPACTION_DIAGNOSTICS=1` 观察。删除状态有额外内存成本，文件布局受编码器影响；历史故障目录可能延后或拒绝维护。性能结论仅适用于各自测量场景，不能保证任意负载追平官方 2.5.1。

## 许可

本项目使用 [Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0)。发行物随附本项目与 proto 的许可证副本，借用的协议和测试保留来源。
