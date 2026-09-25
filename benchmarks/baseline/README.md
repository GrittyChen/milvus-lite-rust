# 基线基准

对照 milvus-lite **2.5.1**（C++，内嵌 Milvus segcore + knowhere）与 **3.x**（纯 Python，LSM + Parquet + FAISS）的同负载基准。两者都由同一版本的 `pymilvus` 驱动，指标口径一致。重构基线的决策与数据解读见 [0001-重构基线选择](../../docs/decisions/0001-refactor-baseline.md)。

本仓的 Rust 实现作为第三个对照臂接入（M1 口径，见下文），性能声明以本目录的测量结果为准（见 [AGENTS.md](../../AGENTS.md) 规则 7）。

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

M1 口径（[0004「验收」第 4 项](../../docs/decisions/0004-m1-scope.md#验收)）：Rust 实现在 M1 只在内存中，只跑 `m1` 阶段，三个实现同口径对照。结果汇总进 [0001「M1 部分验证」](../../docs/decisions/0001-refactor-baseline.md)。

```bash
# 8. 为本仓的 Rust 实现建第三套环境 $W/rs（用仓库开发环境里的 maturin 构建 wheel）
benchmarks/baseline/setup_rs.sh $W

# 9. 3 轮，每轮 10 万与 100 万行，各实现的先后顺序每轮轮换，结果追加到 $W/m1.jsonl
for order in "rs v2 v3" "v2 v3 rs" "v3 rs v2"; do
  for n in 100000 1000000; do
    benchmarks/baseline/run.sh $W $n 128 m1.jsonl $(for e in $order; do echo "$e:FLAT:inproc:m1"; done)
  done
done
$W/v3/bin/python benchmarks/baseline/summarize.py $W/m1.jsonl
```

## 文件

| 文件 | 作用 |
| --- | --- |
| [setup.sh](setup.sh) | 建 v2 / v3 两套 venv |
| [setup_rs.sh](setup_rs.sh) | 用本仓构建的 wheel 建 rs 环境，并记下构建所用的 commit（工作区有未提交改动时附改动内容的指纹） |
| [run.sh](run.sh) | 顺序执行每个配置的各阶段：默认 `build` + `reopen`，配置可用第 4 段指定阶段（如 `rs:FLAT:inproc:m1`） |
| [bench.py](bench.py) | 单个配置的基准：写入、建索引、检索（含过滤 / 批量）、查询、`count(*)`、点查、召回、删除、upsert、冷启动、服务端内存；`m1` 阶段只测 M1 口径 |
| [concurrency.py](concurrency.py) | 服务端独立进程 + 多客户端进程的检索吞吐（0001 并发表所用，口径早于 `m1` 阶段，差别见 [0001](../../docs/decisions/0001-refactor-baseline.md) 该表的说明） |
| [profile_v3.py](profile_v3.py) | 3.x 嵌入式引擎的 cProfile，拆分计算与 Python 胶水的耗时 |
| [summarize.py](summarize.py) | jsonl → markdown 表；`m1` 阶段按配置汇总各轮的中位数与最小–最大 |
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

### M1 口径（`m1` 阶段）

- **与 build 相同的部分**：schema（含 `cat` / `tag` 两个标量字段）、随机数的消耗顺序、每批 1000 行的写入、FLAT 索引与 load、各项的采样次数，因此写入吞吐可以与 0001 的表对照。
- **只测 M1 支持的操作**：无过滤检索（nq=1 与 nq=100）、空 expr 的 `count(*)`、按主键 get 100 条，以及并发检索；不测过滤、删除、upsert 与 reopen。
- **空 expr 的 `count(*)`**（`query_count_empty_expr`）与 build 的 `query_count`（带过滤，0001 的口径）不是同一个操作：3.x 前者走 `num_entities`，不能与 0001 的 `count(*)` 放在同一行比较。
- **写入吞吐不是同口径**：2.5.1 与 3.x 每次写入都要落盘（SQLite、WAL），Rust 实现在 M1 不落盘，写入目标要到 M2 才能验证。
- **预热**：每项先调用一次再计时。
- **recall@10 必须为 1**：三个实现都是暴力检索，否则 `bench.py` 报错，不输出结果行。
- **并发**：
  - 同一进程内 1 / 2 / 4 / 8 个线程（0004 的口径）与 1 / 2 / 4 / 8 个进程（每个客户端一个进程，与 0001 并发表的客户端形态相同，作为补充），每档 8 秒，报吞吐与 p50 / p99。
  - 每个客户端用 `dedicated=True` 自建连接。连接建好后在栅栏上等齐，一起起跑，进程启动与建连的耗时不计入。
  - 吞吐 = 完成的请求数 ÷ 从放行到最后一次返回的实际时长：截止前发出、截止后才返回的请求也计入，分母不用固定的 8 秒，否则单次越慢吞吐越被高估。
  - 任何一个客户端失败，`bench.py` 即报错退出，这个配置不输出结果行。
  - 客户端连接的是进程内服务的 URI，由 `server_manager_instance.start_and_get_uri` 取得。
  - 单个 Python 进程内的多线程受客户端自身限制，在极小的 collection 上约 6000–7000 QPS 封顶，接近这个量级时测到的是客户端上限。
- **轮次与顺序**：各跑 3 轮，每轮轮换各实现的先后，汇总报中位数与最小–最大；每行结果记录 load average、版本号与 Rust 实现的构建 commit。
- **代理环境变量**：grpc 对 127.0.0.1 也遵守 `http_proxy` 等变量（未设 `no_proxy` 时），经 TCP 连接的 3.x 与 Rust 实现会多一跳代理，2.5.1 走 Unix socket 则不会；`bench.py` 启动时清掉这些变量，客户端子进程继承清过的环境。
- **2.5.1 的 socket 路径**：2.5.1 在临时目录下创建 UDS socket，macOS 限制 socket 路径不超过 104 字节，`TMPDIR` 过长时 2.5.1 无法启动，保持默认的 `TMPDIR`。
