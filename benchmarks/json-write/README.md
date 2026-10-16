# JSON 写入回归专项

用于 [0015](../../docs/decisions/0015-nullable-defaults-and-array.md) 的含 JSON 写入回归。默认每批 5000 行、每行约 2.7 KB 的普通非空 JSON、二维向量、不启用动态字段，schema 不声明 nullable/default。每个进程预热一批，再测五批，每批使用新 collection；输入 JSON、SDK 数据对象及原始 protobuf 请求均预先准备。

```bash
<python> benchmarks/json-write/run.py --database <work_dir>/run-raw.db \
  --rows 5000 --samples 5 --mode raw --round 1 --build <artifact-identity>
<python> benchmarks/json-write/run.py --database <work_dir>/run-sdk.db \
  --rows 5000 --samples 5 --mode sdk --round 1 --build <artifact-identity>
```

数据目录和结果必须在仓库外，已有目录直接拒绝。rc3、变更前、变更后、3.x 各使用独立环境及目录；顺序运行三轮，轮换实现顺序。Rust 使用正式构建形态的独立 wheel，按原生库哈希区分；原始样本全部保留，报告各轮最小值和中位数的轮间范围。不在测量期间构建或运行测试。

raw 模式直接发送已序列化 bytes，计时包括本机 gRPC 传输、服务端解码/写入和响应；不能把它称为仅服务端 CPU 时间。sdk 模式计时另外包含 pymilvus 的校验、JSON/protobuf 编码。建表、预热、验证、Flush、Drop 和关闭均在计时外；不改写 WAL 持久化语义。

当前计时操作为 Insert；不把结果推广到 Upsert、任意 null 密度或其它 JSON 形状。Upsert 的错误顺序、掩码和默认值由本切片的正确性矩阵独立覆盖。

每一批，包括预热批，都检查返回状态、写入数量、主键和行数，并分批读回全部 JSON，与独立构造的输入逐项比较。验证失败不输出测量结果。输出记录输入/请求/脚本/原生库指纹、版本、机器负载和全部时延，防止用少写数据或隐藏错误换取数字。

SDK 输入对象与核验真值分开，读回比较保留 JSON 类型，不能把 true 与 1 混同。`raw_request_sha256` 是预构造原始请求的指纹；SDK 模式用它标识等价输入，不将其当作 SDK 实际发送字节的抓包记录。

本专项验证恢复到 rc3 的回归目标，不作为 0001“写入追平”的完成证据。宣称跨实现性能提升时，仍须按规则 7 补齐同机 2.5.1 / 3.x 基线。rc3 与 3.x 的既有差距单独记录，不随本修复扩展实现范围。
