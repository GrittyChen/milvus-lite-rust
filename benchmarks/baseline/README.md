# 基线基准

对照 milvus-lite **2.5.1**（C++，内嵌 Milvus segcore + knowhere）与 **3.x**（纯 Python，LSM + Parquet + FAISS）的同负载基准。两者都由同一版本的 `pymilvus` 驱动，指标口径一致。重构基线的决策与数据解读见 [0001-重构基线选择](../../docs/decisions/0001-refactor-baseline.md)。

Rust 实现落地后，在这里接入第三个对照臂，性能声明以本目录的测量结果为准（见 [AGENTS.md](../../AGENTS.md) 规则 7）。

## 前置条件

- [uv](https://docs.astral.sh/uv/)（用于创建隔离的 Python 3.12 环境）
- macOS 或 Linux（`rss_mb` 依赖 `ps` / `pgrep`）
- 一个**仓库外**的工作目录，用来放 venv、`.db` 数据目录和结果文件；运行产物不入库

## 用法

```bash
W=/path/outside/repo/milvus-lite-bench

# 1. 建两套环境：$W/v2 = milvus-lite 2.5.1，$W/v3 = milvus-lite 3.2.1，均装 pymilvus 3.0.2
benchmarks/baseline/setup.sh $W

# 2. 顺序跑一组配置（环境:索引:模式），结果追加到 $W/r100k.jsonl
benchmarks/baseline/run.sh $W 100000 128 r100k.jsonl \
  v2:FLAT:inproc v2:IVF_FLAT:inproc v3:FLAT:inproc v3:HNSW:inproc v3:IVF_FLAT:inproc v3:HNSW:server

# 3. 汇总成 markdown 表
$W/v3/bin/python benchmarks/baseline/summarize.py $W/r100k.jsonl

# 4. 并发吞吐（复用第 2 步产生的库）
$W/v2/bin/python benchmarks/baseline/concurrency.py $W/r_v2_FLAT_inproc_100000_128.db 128 1,2,4,8
$W/v3/bin/python benchmarks/baseline/concurrency.py $W/r_v3_HNSW_inproc_100000_128.db 128 1,2,4,8

# 5. 3.x 耗时归属（绕过 gRPC 直接 profile 引擎）
$W/v3/bin/python benchmarks/baseline/profile_v3.py $W/r_v3_HNSW_inproc_100000_128.db

# 6. 2.5.1 删除慢的归因：在副本上对比有无 milvus_id 索引（不改动原库）
$W/v3/bin/python benchmarks/baseline/sqlite_delete.py $W/r_v2_FLAT_inproc_1000000_128.db

# 7. Rust 下「每次查询逐行去重」的代价估算（需要 rustc，无第三方依赖）
rustc -O -C target-cpu=native benchmarks/baseline/dedup_microbench.rs -o $W/dedup_microbench
$W/dedup_microbench 100000 1000000
```

`setup.sh` 可用第 2、3 个参数换 3.x 与 pymilvus 版本。

## 文件

| 文件 | 作用 |
| --- | --- |
| [setup.sh](setup.sh) | 建 v2 / v3 两套 venv |
| [run.sh](run.sh) | 顺序执行每个配置的 `build` + `reopen` 两个阶段 |
| [bench.py](bench.py) | 单个配置的基准：写入、建索引、检索（含过滤 / 批量）、查询、`count(*)`、点查、召回、删除、upsert、冷启动、服务端内存 |
| [concurrency.py](concurrency.py) | 服务端独立进程 + 多客户端进程的检索吞吐 |
| [profile_v3.py](profile_v3.py) | 3.x 嵌入式引擎的 cProfile，拆分计算与 Python 胶水的耗时 |
| [summarize.py](summarize.py) | jsonl → markdown 表 |
| [sqlite_delete.py](sqlite_delete.py) | 在 2.5.1 库的副本上对比有无 `milvus_id` 索引时的删除耗时 |
| [dedup_microbench.rs](dedup_microbench.rs) | Rust 单线程微基准：每次查询逐行重算跨源去重的代价 |

## 口径与注意事项

- **模式**：`inproc` 即 `MilvusClient("x.db")`，是用户实际用法；`server` 仅 3.x，服务端独立进程，用于只统计服务端内存。2.5.1 的服务端本来就是 C++ 子进程，`inproc` 下内存已只含服务端。
- **server 模式的退出**：3.x 服务端只在收到 SIGINT 时才正常 close（落盘 memtable、写索引）。脚本正常结束时发 SIGINT 并等待其退出；如果用 SIGTERM 或直接 kill，reopen 阶段就会测到 WAL 回放后的未落盘状态。
- **内存**：3.x `inproc` 的 RSS 含客户端进程自身（测试数据等），数值偏大；比较内存时用 3.x `server` 模式。
- **reopen**：新起进程后执行 `load_collection`。`reopen_total_to_ready_s` = `connect_s`（含拉起服务端子进程）+ `reopen_load_s`，不含首次检索（首次检索单列为 `first_search_ms`）。页缓存仍是热的，测到的是进程级冷启动，不是磁盘冷启动。3.x 会在这一阶段为末尾未建索引的 segment 补建索引。
- **N 的取值**：3.x 每 1 万行 flush 一次 memtable。N 不是 1 万的整数倍时，build 阶段有部分数据留在 memtable，检索会明显变慢，不同 N 之间不能直接比较。
- **数据**：均匀随机向量。它对 ANN 索引是最坏情况，召回偏低，因此不同 ANN 索引之间的延迟不可直接比较；FLAT 对 FLAT 与召回无关的操作可以直接比较。
- **索引参数**：HNSW `M=16, efConstruction=200, ef=64`；IVF_FLAT `nlist=1024, nprobe=16`，build 与 reopen 两阶段相同。2.5.1 不支持 HNSW；据 2.5.1 的 README，其 IVF_FLAT 在不足 10 万行时内部退回 FLAT。3.x 的 `create_index` 会原样登记未知的 `index_type`，要到某个 segment 需要建索引时才报错；数据还在 memtable 时，build 阶段会按暴力检索跑完。因此 `bench.py` 只接受 2.5.1 的 FLAT / IVF_FLAT 与 3.x 的 FLAT / HNSW / IVF_FLAT，并核对服务端登记的索引（结果里的 `index_described`）。
- **`create_index_s`**：pymilvus 以 0.5 秒为间隔轮询建索引进度，分辨率约 0.5 秒。
- **延迟**包含 pymilvus 客户端的序列化开销（约 0.3–0.5 ms 下限）。p50 / p99 按每项的采样次数 `n` 计算，`n` 较小时 p99 只是近似值。
- **配置校验**：`mode` 不是 `inproc` / `server`、索引类型不在上述范围内、或服务端登记的索引类型与请求不一致时，`bench.py` 直接报错，不输出结果行。
- **失败处理**：`run.sh` 把每个阶段的 stderr 写入 `<work_dir>/logs/`。单个配置失败时打印出来并继续执行后面的配置，最后以非零码退出。
- 必须顺序执行，不要并行跑多组配置。
