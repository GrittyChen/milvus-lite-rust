# milvus-lite-rust

以 Rust 实现的非官方 milvus-lite，与 Milvus / Zilliz 无隶属关系。目标是让 `pymilvus` 用户继续使用 `MilvusClient("./demo.db")`，并读取官方 3.x 的数据目录。

**`0.2.0rc1` 为预发布试用版本，不建议用于生产。** 本文对应该版本的源码与发行候选，下方安装命令在对应 GitHub Release 发布后使用。已发布版本以仓库 Releases 页面为准；本项目目前未发布到 PyPI，PyPI 上若出现同名包，并非本项目发布。平台表沿用已确认的验收范围，各行实际达到的证据层级以 Release 说明为准。

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
python -m pip install --no-index --only-binary :all: --find-links https://github.com/GrittyChen/milvus-lite-rust/releases/expanded_assets/v0.2.0rc1 milvus-lite-rust==0.2.0rc1
```

需要先核对文件时，从 [v0.2.0rc1 Release](https://github.com/GrittyChen/milvus-lite-rust/releases/tag/v0.2.0rc1) 下载本平台的 wheel 与 `SHA256SUMS`，在同一目录核对后安装本地文件：

```bash
shasum -a 256 -c --ignore-missing SHA256SUMS && python -m pip install --no-index ./milvus_lite_rust-0.2.0rc1-*.whl
```

核对失败时不会执行安装。Linux 上也可以用 `sha256sum -c --ignore-missing SHA256SUMS` 代替 `shasum -a 256 -c --ignore-missing SHA256SUMS`。

**不要使用 `pymilvus[milvus-lite]`**：该 extra 会引入官方发行包。若两个发行包已经同时存在，应卸载二者后只重新安装本包。绑定层会拒绝检测到的共存，但官方包后装覆盖本模块的情况无法由本包检测；pymilvus 的导入错误提示也可能把用户引向官方包。

开发者可在当前源码根目录构建安装：

```bash
python -m pip install .
```

源码构建需要 Rust 1.89 或以上、目标平台的正常链接工具；仓库开发工具链固定为 1.98.1。sdist 用户使用自己的 Rust 工具链，MSRV 是源码发行的必过门槛。无需 FAISS、CMake 或系统 protoc。

## 平台与 Python 边界

| 首发候选 | 系统下限 | 证据层级 |
| --- | --- | --- |
| Linux x86_64 | glibc 2.28 | 完整验收目标；已在原生 x86_64 的 glibc 2.28 环境实跑 |
| Linux arm64 | glibc 2.28 | 完整验收目标；较新 Linux 容器的结果不等于最低 glibc 已验证 |
| macOS arm64 | macOS 11 | 完整验收目标 |
| macOS x86_64 | macOS 11 | 构建与安装/读写恢复冒烟；实际原生或 Rosetta 方式单独记录，证据弱于完整验收 |

Intel Mac 采用分层验收，不宣称已有完整原生验证。

Python 元数据保持 `>=3.10`，不预先封顶；首发验证窗口是 **GIL CPython 3.10–3.15**，未来未测版本不据此宣称通过。3.10 保留为兼容下限，不以此承诺解释器的安全维护。

继续提供 sdist：矩阵内纳入验收；musl、其它架构及较低 glibc 的自行构建仅为实验性尝试，无支持承诺。**Windows、free-threaded 与非 CPython 在构建时明确拒绝**；Windows 当前仍有目录/句柄同步缺陷，首发既无 Windows wheel，也不允许 pip 从 sdist 安装出该已知不可用版本。Windows 未安装 Rust 时会先得到 cargo 缺失错误，本项目的拒绝检查可能尚未执行。

abi3 wheel 供普通 GIL CPython 使用。正式包没有维护暂停/故障注入接口，FI 测试包单独构建，不用于发行。

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

本候选在已发布 rc3 的基础上增加标量 nullable / 默认值、八种标量元素 ARRAY、Query iterator 和 range search / search iterator V1；范围与验收依据见源码树中的 0015、0016 决策。

`0.2.0rc1` 的范围包括基本 CRUD、Upsert/Delete、Flush/关闭重开、FLAT 的 L2/IP/COSINE、标量与动态字段比较、and/or/not、in/not in、LIKE、is null/exists、JSON 与动态字段路径、json_contains/array_contains 系列及 array_length、后台 compaction 与 tombstone 回收，以及下述新增能力。过滤按官方 3.x 的整条表达式路线执行，各路线对缺失值、短路和类型错误的处理不同；范围与参照缺陷见 0014。算术运算、text_match、几何与时间谓词、分区、alias/rename、BM25/function chain、稀疏向量等范围外能力保持明确的 UNIMPLEMENTED，不静默忽略条件。官方 3.x 数据互通仍受类型、索引与已登记历史目录限制约束，不宣称兼容它的全部功能。

- nullable / 默认值支持 BOOL、INT8/16/32/64、FLOAT/DOUBLE、VARCHAR、JSON；主键与向量的 nullable 仍不支持。
- ARRAY 支持 BOOL、INT8/16/32/64、FLOAT/DOUBLE、VARCHAR 元素及父列 null；不支持嵌套、JSON 或向量元素。存量数组中的元素 null 可保留，但经 Query/Search 投影时报错，与参照实现一致。
- Query iterator 按 INT64 / VARCHAR 主键排序，每个请求取得独立快照；不提供跨请求的一致性快照。
- range search 沿用 3.x 的有限候选窗口，不是全量范围扫描；阈值附近可能出现已登记浮点误差范围内的成员差异。服务端不实现 Search iterator V2，会忽略 V2 探测参数并返回不含 V2 结果字段的普通 Search 结果；SDK 据此自动回退 V1，与官方 3.x 一致。V1 的探测算法在部分批量和数据分布下可能提前结束，不保证任意场景都遍历全部数据。

新能力沿用 3.x 的 schema.json、Arrow WAL 和 Parquet 编码，不增加私有格式。已发布 rc1–rc3 不支持新增的 schema 属性，回退时会明确拒绝相应 collection；升级与回退的具体范围见源码树中的 0015 验收记录。

向量检索只有 FLAT（精确暴力检索）。新建索引时，FLAT 与 BRUTE_FORCE 原生支持；AUTOINDEX（pymilvus 快速建表使用）、HNSW、HNSW_SQ、IVF_FLAT、IVF_SQ8 可创建并原样保存定义，向量仍按 FLAT 执行，召回不低于 ANN 但数据量大时更慢。标量字段上的 INVERTED 可创建，过滤仍按扫描执行，结果可能与官方在已建索引数据上的结果不同。新建稠密向量索引时，AUTOINDEX、HNSW、HNSW_SQ、IVF_FLAT、IVF_SQ8 只接受 COSINE / L2 / IP；BM25 / NONE 返回 UNIMPLEMENTED，避免写出官方 3.x 无法 load 的定义。FLAT / BRUTE_FORCE 保持原度量校验。

DropIndex 的名字行为与 3.x 一致：普通字段名或匹配不到的名字会删除全部索引；`<字段名>_idx` 或明确传入 field_name 才定向删除。collection 需先 release，删完全部索引后自动变为 Loaded。

打开官方 3.x 写入的目录时，索引定义原样保留；load 成功后，向量仍按 FLAT、标量仍按扫描执行。官方 3.x 也无法构建的索引类型（如 IVF_PQ、DISKANN、向量字段上的 INVERTED），在 collection 尚未 load 且已有 flush 落盘的数据（包括已删除的行）时，与官方一样 load 报错。存量 AUTOINDEX、HNSW、HNSW_SQ、IVF_FLAT、IVF_SQ8 定义配 BM25 / NONE 时，collection 尚未 load 且至少一个有物理行的当前 segment 缺少匹配的 3.x 侧车，load 与官方一样报错；已删除的行仍计入。匹配路径全有、只有 WAL 或已经 loaded 时，度量检查不拒绝 load。侧车内容和构建参数仍不校验：官方因参数无效而 load 失败的目录，这里可能照常 load；参数与度量同时无效时，报错原因也可能不同，见源码 0002。

上述新建 ANN / INVERTED、DropIndex 和存量度量检查已从 rc3 提供，已发布的 rc2 不包含；rc2 也允许新建 AUTOINDEX 配 BM25 / NONE。or / not、not in、LIKE、is null / exists、JSON 路径与 contains 等过滤扩展同样从 rc3 开始提供。对于官方 3.x 也无法构建的索引类型（如 IVF_PQ、DISKANN）的 load 检查，rc1 不做，会照常 load。

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
