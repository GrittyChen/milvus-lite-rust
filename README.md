# milvus-lite-rust

以 Rust 实现的非官方 milvus-lite，与 Milvus / Zilliz 无隶属关系。目标是让 `pymilvus` 用户继续使用 `MilvusClient("./demo.db")`，并读取官方 3.x 的数据目录。

**当前是开发候选，尚未发布到 PyPI，也尚不可用于生产。** 以下平台表是首发候选及验收边界，不表示所有行已经通过。公开源码和文档地址尚未确定，开发与验收材料在源码树中维护。

## 安装与包冲突

本包的发行名为 `milvus-lite-rust`，提供 `milvus_lite` 模块及 `milvus-lite` 命令。它与官方发行包 `milvus-lite` **不能同时安装**，pip 不会阻止同名模块被覆盖；后装者覆盖文件，卸载任一方也可能损坏另一方。

先移除官方包：

```bash
python -m pip uninstall milvus-lite
```

发布后使用以下安装方式；当前尚未发布，不能将这些命令当作现有 PyPI 包已可用：

```bash
python -m pip install pymilvus milvus-lite-rust
```

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
    client.insert("docs", [{"id": 1, "vector": [1.0, 0.0], "category": "tech"}])
    print(client.query("docs", filter="category == 'tech'", output_fields=["id"]))
    print(client.search("docs", [[1.0, 0.0]], limit=1))
    client.flush("docs")
finally:
    client.close()
```

`.db` 的父目录必须存在。同路径服务复用，`MilvusClient.close()` 关闭客户端而非本地服务；正常脚本退出会收尾服务并排空已接受的后台合并。

当前范围包括基本 CRUD、Upsert/Delete、Flush/关闭重开、FLAT 的 L2/IP/COSINE、S1 标量比较与 and、主键 in、顶层动态字段比较/in、后台 compaction 与 tombstone 回收。ANN、BM25/function chain、iterator、嵌套 JSON 等范围外能力保持明确的 UNIMPLEMENTED，不静默忽略条件。官方 3.x 数据互通仍受类型、索引与已登记历史目录限制约束，不宣称兼容它的全部功能。

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
