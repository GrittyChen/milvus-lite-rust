# JSON 写入组件与分进程资源诊断

对应 [0018 合同草案](../../../docs/decisions/0018-json-write-normalization.md)与[调查记录](../../../docs/testing/0018-json-write-investigation.md)。这是诊断工具，不是产品优化或正式发行物；不进入默认测试，不联网下载依赖，不修改参照或当前产品源码。运行前准备好固定 Rust 工具链、Cargo 缓存和各自已安装的 Python 环境。

## 原组件分项

Python 3.12 或以上执行：

```bash
python benchmarks/json-write/diagnostics/build.py --work-dir <new-outside-directory>
```

工具核验已发布冻结提交 `27aabf26d9fbf00a51f489d723873a0550b5557c` 的完整归档 SHA256，在仓库外解包，仅追加三个诊断源码文件；既有函数体不修改。Cargo 使用独立 target、offline / locked / release。已有路径、软链、仓库内路径及不存在的父目录直接拒绝；构建失败保留原日志，不输出通过结果。

运行输出必须位于构建时的同一 `<work>` 下，且不在其源码树内；已有路径、其它工作目录或源码树内输出均拒绝。组件输入为 UTF-8 JSONL，每行一个 JSON 值，固定 5000 行。原 [JSON 专项](../run.py)的 `payload(i)` 是 mixed 语料；simple 语料是 `dict(body="x" * 2048, values=list(range(32)))`。每行以 Python `json.dumps(value, ensure_ascii=False)` 编码，换行分隔。只测有效语料；错误语义仍由产品差分与既有故障测试验收。

```bash
<work>/target/release/examples/json_write_diagnostic <input.jsonl> <new-output-directory> false
<work>/target/release/examples/json_write_diagnostic <input.jsonl> <another-new-output-directory> true
```

false 对应普通非空 JSON 列；true 对应完整布局的 `valid_data` 隔行无效，仍校验被掩码遮住的原始 JSON，区别于直接传入 JSON `null` 文本。每进程预热一次、记录五次。组件调用原字段校验 / 可选列转换 / 规范化 / WAL Arrow 构造及真实 WAL 写入；每批同步后重新读取 WAL，核对完整 JSON 和父有效位。

分项与端到端结果分开：仅一个 JSON 列，省略 PK、向量、gRPC、MemTable 发布、Upsert 合并与分桶。显式 WAL sync 是额外观察，不在普通 Insert 响应前强加 fsync。codec 子诊断逐行计时，包含计时器和分配释放开销，不能把分项之和当作完整 RPC CPU 占比。

RSS 是 `ps` 的进程当前值，快照在计时外；预热后分配器可能保留内存，数值平台不能证明阶段没有分配。Arrow 的内存字节计数也不是整个进程峰值。

## 分开服务端与客户端

以下工具的 `ru_maxrss` 字节口径只在 macOS 验证；使用已安装正式 wheel 或固定参照的 Python 环境：

```bash
<installed-python> benchmarks/json-write/diagnostics/memory.py \
  --database <new-outside-directory>/memory.db --mode default
<installed-python> benchmarks/json-write/diagnostics/memory.py \
  --database <new-outside-directory>/key-memory.db --mode multi
```

worker 只托管服务，客户端在控制进程。两种操作各重复六次同键写入，逐批验证状态、完整 ID / 成功位次及操作数量；每组后核对全部 JSON，再 Flush 并核对行数。保存 ready、请求准备、每批完成、完整读回及 Flush 后两进程的当前 RSS 和各自高水位。快照不在 RPC 计时区间；该计时仅作诊断，不加入三方轮换成绩。

数据库和同目录下由 `with_suffix('.server.log')` 派生的服务日志均须是仓库外的新路径，且不能指向同一位置。启动 worker 前检查完整输出集合，拒绝已有普通文件、目录、符号链接及断链；日志使用排他新建，检查后才出现的同名日志也不会被覆盖。失败时保留本次新日志作为旁证；重试须使用新的输出路径。

输出显示每个进程各自的峰值，不能相加当作同时峰值；也不是 allocator、活数据或单个缓冲的归因。该工具未比较外部 C++ 服务的内存，不扩展为跨平台持久化证明。

## 自检与执行顺序

```bash
python benchmarks/json-write/diagnostics/selfcheck.py
```

八项自检验证路径拒绝和已确认旁证不被覆盖，包含数据库 / 日志两种输出各四种已有路径、日志别名、检查后出现的日志及失败关闭文件；不构建或启动服务。全新路径的实际诊断另行执行。正式计时、诊断、构建、自检必须分阶段顺序执行。调查的三轮顺序、原始驱动、环境、输入、阶段日志及失败准备记录随独立证据包保存；复现先核验对应软件身份，不以当前开发安装代替正式 wheel。
