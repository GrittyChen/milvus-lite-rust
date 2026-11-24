# 兼容性说明

本项目以官方 milvus-lite 3.x（下称 3.x）为行为规范：结果与排序、距离、错误码与错误信息、过滤语义以 3.x 为准。3.x 本身的缺陷不照抄，按真实 Milvus 的语义处理；所有差异都登记在 [0006 差异表](decisions/0006-m2-scope.md)与各功能的设计决策中。本文按主题汇总用户需要知道的部分，细节以链接的决策为准。安装与运行见[使用说明](usage.md)。

## 支持范围

| 类别 | 支持 |
| --- | --- |
| 数据操作 | Insert、Upsert（含 partial update）、Delete、Query、Get、count、Flush；Query iterator 与 Search iterator（V1） |
| 字段类型 | 标量、JSON、ARRAY、TIMESTAMPTZ、GEOMETRY；稠密向量（可多个、可 nullable）、稀疏向量；动态字段 |
| 检索 | FLAT（L2 / IP / COSINE）、过滤、range search、分组、HybridSearch 与 rerank、function chain |
| 全文 | BM25（standard、jieba 分词）、`text_match` |
| 管理 | 分区与 partition key、别名、改名、清空、多数据库 |

各项的细节与边界见下文；本表随功能增加而更新，README 只给概括。

## 数据目录

- 长期数据文件沿用 3.x 的 `schema.json`、Arrow IPC WAL、Parquet 与 manifest format 2 编码，不增加长期私有格式，与 3.x 双向互通；范围与约束见 [0002](decisions/0002-data-dir-compat.md)。互通受下文各功能的差异与「降级」一节的版本限制约束，不宣称兼容 3.x 的全部功能。
- 改名与清空过程中各有一个短暂存在的私有意图文件（文件名与格式见 0002），由本实现重开时收尾。**降级到不含该功能的版本之前，须先用本实现正常打开并关闭一次。**
- `schema.json`、`aliases.json`、`database_properties.json` 等共享 JSON 按与 3.x 相同的键排序格式写出。
- 索引文件不互通：本实现不写 3.x 的索引侧车，也不读取其内容（只检查是否存在，见「索引」）。HNSW 的图写在 `indexes/<段>.<字段>.hnsw.mlrs`，3.x 会忽略它、按自己的方式重建索引；它只是缓存，丢失或损坏时本实现从数据重建。
- nullable 向量的空值存为 Arrow null，3.x 需要 pyarrow 26 及以上（因此需要 Python 3.11 及以上）才能打开含空值的集合，详见下文「nullable 稠密向量」。
- Parquet 的行组更小（每组最多 65536 行、约 32 MiB），以限制写入时的内存峰值；两边的读取都不依赖行组数量（[0002 约束 9](decisions/0002-data-dir-compat.md)）。

## 索引

- **HNSW** 建图检索（[0041](decisions/0041-hnsw.md)），纯 Rust 实现，参数 `M`、`efConstruction`、`ef` 的含义与默认值（16、200、64）与 3.x 相同。结果是近似的：返回的每一行都满足过滤与删除可见性，距离与 FLAT 对同一行算出的值逐位相同，但可能漏掉真正更近的行。与 3.x 的差异：
  - 实际束宽取 `max(ef, 候选窗口)`；某段的图返回不足窗口条数时（如过滤后剩下的行在图上走不到），该段改走 FLAT，因此与 FLAT 一样返回足 k 条。3.x 在 k 或窗口大于 ef 时召回明显下降，甚至返回不足 k 条。
  - 行数不足 1 万的段、内存中尚未落盘的数据、尚未建好图的段，以及过滤后有效行不多于实际束宽 800 倍的段，按 FLAT 精确检索；图搜索的距离计算超过该段有效行数时也改走 FLAT。3.x 只在有效行不多于 k 时才精确检索。
  - 图在后台构建：load 与 CreateIndex 不等待，建好之前按 FLAT 检索，建好后自动切换；3.x 首次 load 与 loaded 状态下的 CreateIndex 会同步建完。构建失败时记录日志并重试，期间按 FLAT 检索，DescribeIndex 照常显示，load 不报错。
  - `M`、`efConstruction`、`ef` 只接受整数、有限浮点数（截断）、ASCII 十进制整数字符串与布尔；其他写法（包括 Python `int()` 也接受的下划线分隔与非 ASCII 数字）以及 `M` 超过 512、`efConstruction` 超过 100000 时，不建图或不用图，按 FLAT 执行，不报错。3.x 对无法解析的值报错，对合法的大值照常构建。
- 其余向量索引按 FLAT（精确暴力检索）执行，FLAT 同时是召回与距离口径的正确性基准。新建索引时，FLAT 与 BRUTE_FORCE 原生支持；AUTOINDEX（pymilvus 快速建表使用）、HNSW_SQ、IVF_FLAT、IVF_SQ8 可以创建并原样保存定义，向量仍按 FLAT 执行，召回不低于 ANN，但数据量大时更慢；3.x 的 AUTOINDEX 实际走 HNSW。需要 ANN 时请显式建 HNSW。这五种新建定义只接受 COSINE / L2 / IP；BM25 / NONE 返回 UNIMPLEMENTED，避免写出 3.x 无法 load 的定义。FLAT / BRUTE_FORCE 保持原有的度量校验。
- INVERTED 可以在 BOOL、INT8/16/32/64、FLOAT/DOUBLE、VARCHAR、TIMESTAMPTZ 字段上创建（JSON、ARRAY、GEOMETRY 不支持），过滤仍按扫描执行，结果可能与官方在已建索引数据上的结果不同。
- DropIndex 的名字行为与 3.x 一致：普通字段名或匹配不到的名字删除全部索引；`<字段名>_idx` 或明确传入 field_name 才定向删除。集合需要先 release，删完全部索引后自动变为 Loaded。
- 打开 3.x 写入的目录时，索引定义原样保留；load 成功后，HNSW 字段在后台建本实现自己的图（不读 3.x 的 `.idx`），其余向量索引按 FLAT、标量仍按扫描执行。3.x 自己也无法构建的索引类型（如 IVF_PQ、DISKANN、向量字段上的 INVERTED），在集合尚未 load 且已有 flush 落盘的数据（包括已删除的行）时，与 3.x 一样 load 报错。
- 存量的 AUTOINDEX、HNSW、HNSW_SQ、IVF_FLAT、IVF_SQ8 定义配 BM25 / NONE 时，如果集合尚未 load，且至少一个有物理行的当前 segment 缺少匹配的 3.x 侧车，load 与 3.x 一样报错（已删除的行仍计入）；匹配的侧车全有、只有 WAL 或已经 loaded 时不拒绝。侧车内容与构建参数不校验：3.x 因参数无效而 load 失败的目录，这里可能照常 load；参数与度量同时无效时，报错原因也可能不同。设计见 [0014](decisions/0014-index-definitions-and-filter-expressions.md)。

### 索引变更后的人工恢复

索引变更时，如果旧索引文件的删除没有确认落盘，只封锁该集合的 CreateIndex / DropIndex。服务重开或容器重启不能证明已经恢复；可信的内核重启可以自动解除封锁，overlayfs、共享目录或启动来源不可信时需要人工恢复：

1. 确认承载数据目录的系统已经真实重启；
2. 停止对应服务或退出 Python 进程；
3. 删除下面的最终标记，再启动服务。

最终标记：`<data_dir>/collections/<collection>/milvus-lite-rust.index-intent.json`。

最终标记如果是目录等异常条目，把它移出这个路径，不要递归删除内容；符号链接只移除链接本身。不要改 manifest、索引文件或 snapshot，也不要只修改标记里的启动标识。如果只是已安全完成的操作的标记清理失败，可以在**同一个服务实例内**重试，无需重启系统；实例退出后会按磁盘上的标记重新判定，仍可能保守封锁。完整协议见 [0014](decisions/0014-index-definitions-and-filter-expressions.md)。

## 按功能的说明与差异

- **过滤表达式**：按 3.x 的整条表达式路线执行，各路线对缺失值、null、短路与类型错误的处理不同（[0014](decisions/0014-index-definitions-and-filter-expressions.md)）。`%` 与 `**` 与 3.x 一样是解析错误。
- **过滤中的算术**：`+`、`-`、`*`、`/` 与一元负号，按 3.x 的三条求值路线复刻（只引用 schema 字段时按 pyarrow 的类型提升，含 JSON 路径时按 Python，含动态字段键时先按列、类型不合适时改按 Python）。差异：字符串、列表与大整数的中间结果有上限（单个值与每行累计 64 MiB，整数 2^20 位），超出时报 code 1，3.x 没有上限；已删除的旧行不参与类型推断；动态字段是多键对象时，个别情形与 pyarrow 15 下的 3.x 一个报错、一个改按 Python 计算（[0032](decisions/0032-arithmetic-filters.md)、[0033](decisions/0033-hybrid-dynamic-inference.md)）。
- **分区**：Create / Drop / Has / Show / statistics，以及写入和读取的分区选择。读取先选分区再判版本，删除按主键全局生效；Drop 保留其他分区仍需要的全局删除记录，避免重开后已删除的行复活（[0017](decisions/0017-partitions.md)）。
- **partition key**：支持非主键的 INT64 / VARCHAR，按 3.x 的固定哈希路由到 16 个桶，另保留 `_default`。auto_id 按分桶后的行序号生成，返回顺序仍对应原请求，可能得到 `[1,3,2]`。确定性的校验或转换失败在写入任何 WAL 之前整批拒绝，自动 ID 会覆盖可读旧行时整批拒绝；真实 I/O 故障按已校验前缀恢复，不保证整批原子写入。
- **Upsert**：非 partition key 集合中，同一批里重复主键的每一行都从请求前的旧值独立合并，与 3.x 一致。例如旧行 `dict(id=1,x=0,y=0)`，同批 partial upsert `[dict(id=1,x=1),dict(id=1,y=2)]` 的结果是 `dict(id=1,x=0,y=2)`；分成两个请求时依次合并。partition key 集合中，每一行同样从请求前的状态取旧值，但合并后按桶顺序写入，同批重复主键的胜者由分桶后的 `_seq` 决定，不能简单认为输入的最后一行生效（[0017](decisions/0017-partitions.md)）。
- **集合管理与多数据库**：别名、改名（不支持跨数据库）、清空（保留分区、索引定义、别名与 load 状态，auto_id 从 1 重新分配）、集合属性；数据库的创建、删除、列表、描述与属性，按请求的 `db_name` 路由。差异：数据库属性值只接受字符串；创建与打开集合不读数据库属性文件，因此能打开 3.x 因该文件损坏而打不开的库；运行期由外部创建或删除的 `databases/<db>` 重开后才可见；`databases/` 与 `.database-staging/` 须在同一文件系统，`databases/` 不可读时启动失败（3.x 照常启动、按请求失败）；只差大小写的数据库名按精确名字处理（[0023](decisions/0023-aliases-rename-truncate-databases.md)）。
- **HasCollection 与 CheckHealth**（raw gRPC）：HasCollection 与 3.x 一样先确认数据库、再解析别名，只看集合目录与 `schema.json` 是否存在、不打开集合（打不开的集合也返回 true），错误一律 code 1；含路径分隔符或只差大小写的名字、指向目录的符号链接、`schema.json` 是目录时按不存在处理（3.x 按拼出的路径判断）。CheckHealth 总是报告健康。pymilvus 的 `has_collection` 走 DescribeCollection，不受影响。
- **分组检索**：分组字段支持 INT8 / INT16 / INT32 / INT64 / BOOL / VARCHAR（含可空，空值为独立一组），候选窗口与 3.x 相同，排名靠前的命中被少数几组占满时后面的组取不到。差异：按主键（名字不是 `id` 时）或名为 `distance` / `id` 的可空字段分组时，3.x 结果错误，本实现按字段值正常分组（[0024](decisions/0024-group-by-search.md)）。
- **TIMESTAMPTZ 与 timezone**：内部为 UTC 微秒，读出为以 `Z` 结尾的 ISO 字符串；无偏移的时间按请求、集合、数据库的 `timezone` 优先级换算，与 3.x 一致；Query 与 Search 支持 `time_fields`，`AlterCollection` / `AlterDatabase` 的 `timezone` 即时生效，已保存的数据不变；时区库内置，不依赖运行平台的 tzdata。差异：原始 gRPC 写入超出 `0001-01-01..9999-12-31` 的整数微秒时整批拒绝（3.x 写入成功，之后读到该行的查询都失败）；手工编辑的 `schema.json` 里 TIMESTAMPTZ 的字符串默认值按范围外处理。用大小写不规范的时区名（如 `utc`）写入属性的目录，搬到 Linux 上的 3.x 可能无法使用（[0025](decisions/0025-timestamptz-and-timezone.md)）。
- **HybridSearch 与 rerank**：RRFRanker、WeightedRanker，以及 `function_score` 的 boost 与 rrf / weighted / decay；合并、打分、同分次序、候选窗口与错误先后与 3.x 一致。HybridSearch 支持各路自己的过滤、分区（子请求的分区优先于顶层）、度量与 `timezone` / `time_fields`。与 3.x 一样，HybridSearch 忽略 `round_decimal`、`strict_group_size` 与 `rank_group_scorer`，子请求里的 `radius` / `range_filter`、`offset` 与分组参数只校验不生效，boost 过滤读不到动态字段。差异：`rank_params` 的整数参数超出 int64 时按饱和值执行（3.x 报错）；子请求的 `params` 不是严格 JSON 时拒绝（3.x 忽略）；rerank 算出的得分写进同名的 TIMESTAMPTZ / ARRAY / 向量字段时返回 UNIMPLEMENTED；分组排序键含 NaN 时组内顺序不逐位复现（[0026](decisions/0026-rerank-and-hybrid-search.md)）。
- **公共 function chain**：Search 的 `function_chains`（pymilvus 3.0），支持 L2 rerank 阶段的一条链，数值与错误与 3.x 一致。差异：`output_fields` 含 `*` 或整个动态字段时返回真实值（3.x 返回类型默认值）；分组字段与链里的分数列或主键列同名时不被覆盖（[0035](decisions/0035-function-chains.md)）。
- **`text_match` 与分析器属性**：`text_match(field, 'query')` 按 3.x 的 standard 分词（`lower()` 后按 `\w+` 切词，多个词是 OR），可用于 Query、Search、count、Delete、boost 与 HybridSearch；不要求字段开 `enable_match`，分词器固定 standard。`enable_analyzer`、`enable_match`、`analyzer_params` 可以建表、回显并与 3.x 互通。含 `NaN`、`Infinity` 或嵌套超过 124 层的 `analyzer_params` 返回 UNIMPLEMENTED（[0029](decisions/0029-text-match.md)）。
- **稀疏向量**：写入、读回与 `SPARSE_INVERTED_INDEX` + IP 检索，分数与次序与 3.x 逐位一致；倒排在首次检索时构建并缓存，不写索引文件。差异：带稀疏默认值的集合打开时报错（可以 drop）；nullable 稀疏列有空值时 pymilvus 读出真实值（3.x 把有效行读成空向量）；指定其他分区、尚未 flush 的删除在稀疏检索中立即全局生效（[0030](decisions/0030-sparse-vectors.md)）。
- **全文检索（BM25）与 jieba**：BM25 function 在写入时生成稀疏向量；分析器支持 standard、`stop` 过滤器与 jieba（内置 jieba 0.42.1 的词典与 HMM 模型，不需要另装 jieba；wheel 因此增大约 2.5 MB，首次使用加载约 0.1 秒，常驻内存约多 45 MB，每个带用户词的集合另持有一份词典，约 20–40 MB，macOS arm64 实测）。分数按 segment 统计，与 NumPy 2 环境中的 3.x 逐位一致。差异：jieba 用户词每个集合一份（3.x 加进进程全局词典，会互相影响）；用户词合计超过 100 万个字符或词频总和超出 64 位整数时建表报错；3.x 建的带用户词的 jieba 集合照常打开，但新写入与查询按本实现的切法，召回可能下降；segment 布局不同（自动 flush 或合并的时机）时分数与次序随之不同（[0031](decisions/0031-bm25.md)、[0036](decisions/0036-jieba.md)）。
- **多向量字段**：多个 FLOAT_VECTOR 字段各自独立，维度、索引与度量按字段；`anns_field` 未指定时取第一个。非 nullable 的向量字段写入时必须有值；3.x 接受第二个及之后的向量字段缺失但 flush 后读取出错，本实现直接拒绝，已含这种行的 3.x 集合无法打开（[0028](decisions/0028-multi-vector-fields.md)）。
- **nullable 稠密向量**：写入时给 None 或省略该字段即为空值，空值读出为 None，各类检索都不返回该字段为空的行；partial upsert 省略该字段时保留原值。差异：3.x 把空值写成零向量并让它参与检索，本实现保留空值；3.x 对整批为空的写入报错。一次写入的行数 × dim 不能超过 6400 万。3.x 需要 pyarrow 26 才能打开含空值的集合；即使在 pyarrow 26 上，3.x 的多行输出中排在空值之后的向量会错位（[0038](decisions/0038-nullable-dense-vectors.md)）。
- **GEOMETRY**：只接受 `POINT` 与单环、不自交的 `POLYGON`，不能作主键或 partition key，不支持索引与分组；几何谓词复刻 3.x 的平面近似算法（容差 1e-9），不是 OGC 语义。差异：`ARRAY<GEOMETRY>` 返回 UNIMPLEMENTED，已有这种集合的 3.x 目录中该集合无法打开（可以 drop）；3.x 运行在 CPython 3.12 以外时，恰好落在 `ST_DWITHIN` 阈值上的行可能不同（[0034](decisions/0034-geometry.md)）。
- **nullable、默认值与 ARRAY**：nullable 与默认值支持 BOOL、INT8/16/32/64、FLOAT/DOUBLE、VARCHAR、JSON；主键不支持 nullable。ARRAY 支持 BOOL、INT8/16/32/64、FLOAT/DOUBLE、VARCHAR 元素与父列 null，不支持嵌套、JSON、向量、TIMESTAMPTZ 与 GEOMETRY 元素；存量数组中的元素 null 可以保留，但经 Query / Search 投影时报错，与 3.x 一致（[0015](decisions/0015-nullable-defaults-and-array.md)）。
- **iterator 与 range search**（[0016](decisions/0016-query-iteration-and-range-search.md)）：Query iterator 按 INT64 / VARCHAR 主键排序，每个请求取独立快照，不提供跨请求的一致快照。range search 沿用 3.x 的有限候选窗口，不是全量范围扫描。服务端不实现 Search iterator V2，忽略 V2 探测参数，SDK 据此回退 V1，与 3.x 一致；V1 的探测算法在部分批量和数据分布下可能提前结束。
- **距离误差**：距离沿用 [0004](decisions/0004-m1-scope.md) 的浮点误差预算，范围阈值落在误差带内时两端可能有成员差异。BM25 与稀疏 IP 的分数逐位对齐，不使用这一预算。

## 未实现

以下返回 UNIMPLEMENTED 或不在范围内：

- LoadPartitions / ReleasePartitions、snapshot 相关 RPC、AddCollectionField / AlterCollectionSchema，以及其他不在已实现清单里的 RPC；
- 真实的 ANN 检索（索引定义只保存，见「索引」）；
- TEXT_EMBEDDING function、Search 与 HybridSearch 顶层实际执行的 model rerank（外部 provider；HybridSearch 子请求里的 model 与 3.x 一样被忽略，不报错）、候选真正用到的 boost `random_score`；
- `ARRAY<TIMESTAMPTZ>`、`ARRAY<GEOMETRY>`；
- 向量默认值、在向量字段上过滤（如 `vec is null`），与 3.x 相同；
- 跨数据库改名；
- Search iterator V2（按上文回退 V1，不报错）。

## 已知限制

- **写入峰值内存**：大批量写入时进程内存峰值高于 2.5.1，见[使用说明](usage.md#后台维护与内存)。
- **HNSW 的内存**：建了 HNSW 时，百万行服务端内存比 2.5.1 的 FLAT 高约 7%–18%（按 [0041 的基准口径](testing/0041-hnsw-performance.md)；另一测量口径下 2.5.1 自身的读数高约三分之一，两者的大小关系因此取决于口径，差异原因未查明）。
- **JSON 写入优化**：收益限于 [0018 已审的同机工况](testing/0018-json-write-acceptance.md)，不外推为任意 JSON、并发吞吐或全部写入路径。
- **稀疏检索**：nq 较大时仍慢于 2.5.1（nq=32 约慢 27%），单条查询与过滤检索追平或更快（[0030 验收](decisions/0030-sparse-vectors.md#验收)）。
- **批量检索**：性能结论只在原生 x86_64 服务器上测得，aarch64 与 macOS 没有同等测量（[批量检索记录](testing/batch-search-performance.md)）。
- **停服路径的一次段错误**：持续集成中出现过一次，此后的定向压力与运行都没有再现，根因未查明，按显式风险接受处理，见各版本发行说明。
- **fork 的使用限制**：在已运行服务的进程中 fork 时，若父进程恰有线程在内部锁上等待（如并发请求争用同一集合），子进程为另一路径启停服务时仍可能挂起或崩溃；建议在没有在途调用时 fork，或改用 spawn / forkserver，见 [0003「fork」](decisions/0003-in-process-server-lifecycle.md#fork)。
- **SDK 的端点 schema 缓存**：同一进程中显式释放后复用端口，或 CLI 在固定端口更换数据目录重启，可能使 SDK 沿用旧 schema；3.x 上同样存在。
- **dump 的限制**：见[使用说明](usage.md#导出-dump)。
- **Windows**：不支持，构建时拒绝。

## 降级到旧版本

降级前先用本实现正常打开并关闭一次，收尾改名与清空的意图文件。下表列出含新功能的集合在旧版本中的表现；表中的版本是引入该功能的版本，降级到它之前的任何版本都适用。

| 功能或集合内容 | 引入版本 | 降级到更早版本时 | 数据 |
| --- | --- | --- | --- |
| nullable / 默认值 / ARRAY 等 schema 属性 | 0.2.0rc1 | 0.1.0rc1–rc3 明确拒绝该集合（[0015](decisions/0015-nullable-defaults-and-array.md)） | 保留 |
| 非默认分区、partition key | 0.3.0rc1 | 0.2.0rc1 明确拒绝相应集合；删除全部非默认分区且没有 key 等属性后，只剩 `_default` 的目录可能回到旧版能力范围（[0017](decisions/0017-partitions.md)） | 保留 |
| 多数据库 | 0.4.0rc1 | 数据库 RPC 返回 UNIMPLEMENTED；经 `db_name` 访问非默认库报数据库不存在（错误码随 RPC 而异），库内集合不可访问 | 保留 |
| 别名 | 0.4.0rc1 | 别名 RPC 返回 UNIMPLEMENTED，按别名访问报集合不存在；旧版 create / drop 集合时会剔除指向不存在目标或被删集合的别名 | 别名文件保留 |
| TIMESTAMPTZ 字段、`timezone` 属性 | 0.4.0rc1 | 明确拒绝打开该集合 | 保留 |
| `text_match` 分析器属性、稀疏向量、BM25 function、多向量字段 | 0.5.0rc1 | 明确拒绝（UNIMPLEMENTED） | 保留 |
| GEOMETRY 字段 | 0.6.0rc1 | 明确拒绝（UNIMPLEMENTED） | 保留 |
| jieba 分词器、nullable 稠密向量字段 | 0.7.0rc1 | load 时明确返回 UNIMPLEMENTED，可以 drop；升级回来后照常可用 | 保留 |
| HNSW 图文件（`.mlrs`） | 0.8.0rc1 | HNSW 字段按 FLAT 精确检索，图文件原样保留、不读取；旧版合并后，被合并段的图成为孤儿，升级回来时清理并在后台重建，其余图直接读入 | 保留 |

各版本实测的降级边界见对应的[发行说明](releases/)。
