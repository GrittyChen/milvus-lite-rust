# 0014 rc2 之后的兼容面：索引定义与过滤表达式

- 状态：草案。负责人已于 2026-10-14 确认范围（本文两个切片）、索引方案（接受 ANN 定义、按 FLAT 执行）与[已确认的三项取舍](#已确认的取舍)；首轮评审的三处阻断已修订，待复审。各切片实施前另做设计评审，发布另行授权
- 日期：2026-10-14
- 参照：v3.2.1 / `43d1257`；pymilvus 3.0.2 / 2.6.17
- 前置：[0002 结论与约束 1、5、7](0002-data-dir-compat.md)、[0006 第 13、14 项](0006-m2-scope.md)、[0007 提交协议](0007-m2-storage-engine.md)、[0009 顶层动态过滤](0009-top-level-dynamic-filter.md)、[0013 发布流程](0013-first-release-platform-and-packaging.md)

## 依据：rc2 的缺口统计

目标是让常见的 pymilvus 代码在本实现上不再碰到 UNIMPLEMENTED。范围按实测缺口排序，而不是按印象挑选。统计对象为 `0.1.0rc2` 的 macOS arm64 正式 wheel 与 pymilvus 3.0.2，对照同机的 3.x（`43d1257`，含 faiss）：

| 来源 | 3.x 通过 | rc2 通过 |
| --- | --- | --- |
| 参照实现 tests/adapter 与 tests/compatibility，起服务入口换成本包的 server_manager；排除依赖 3.x 内部模块的 139 条 | 760 | 237 |
| 参照实现 tools 中列出的上游 Milvus python_client 子集（上游 `dcda428`，4 条用例的类名已在上游改名，按新名运行） | 90 | 38 |
| 按 Milvus 文档常见写法编写的 49 项 MilvusClient 调用（只检查调用是否成功，不比较返回结果） | 43 | 11 |

参照用例中，3.x 通过而 rc2 未通过的共 523 条：522 条在执行中遇到明确的 UNIMPLEMENTED，1 条在 rc2 上被跳过；没有因结果与 3.x 不同而失败的。这 522 条按首个阻断原因归类：
- schema 类型 189：nullable 120（集中在少数共享 fixture）、ARRAY 42、TIMESTAMPTZ 16、GEOMETRY 7、默认值 3、多向量字段 1；
- 显式 ANN 建索引 110；
- 全文、rerank 与 hybrid 62；
- partition 与 partition key 55；
- 过滤表达式 39；
- database 37；
- iterator、range search 与 group_by 17；
- rename、alias、truncate 与 alter 13。

只计首个阻断，因此过滤表达式的实际覆盖面大于 39 条。上游子集的失败以 DropIndex 与 HNSW 建索引为主。

在客户端把 HNSW / IVF 改写为 FLAT 后重跑参照用例，rc2 通过数由 237 升到 325；另有 4 条只因 describe 返回 FLAT 而失败，保留原定义即可通过。

据此本阶段只做两个切片：A 索引定义与 DropIndex，B 过滤表达式。nullable / 默认值 / ARRAY 涉及 Parquet 与 Arrow 编码的双向互通，单独作为下一个切片；partition、alias、rename、database 涉及 manifest 与目录元数据；全文、稀疏、hybrid、iterator 体量最大。这些都不在本文范围内。

## 切片 A：索引定义与 DropIndex

### 创建

- 稠密向量字段在现有 FLAT、BRUTE_FORCE、AUTOINDEX 之外，接受 HNSW、HNSW_SQ、IVF_FLAT、IVF_SQ8。定义按 3.x 的形态持久化（类型大写、metric 原样、params 与 search_params 原样），DescribeIndex / list_indexes 照实返回；检索一律按 FLAT 精确执行，不建 ANN、不写索引文件。
- 标量字段接受 INVERTED，类型范围沿用现有校验（3.x 另支持的 TIMESTAMPTZ 随该类型另行接入）；过滤按扫描执行。
- 创建的校验顺序与错误原文不变，只解除 FLAT 阶段对上述类型的限制。3.x 自身无法构建的定义（IVF_PQ、DISKANN 等未知类型、向量字段上的 INVERTED）和稀疏字段上的任何索引，继续在创建时返回 UNIMPLEMENTED（见[已确认的取舍](#已确认的取舍) 2）。
- load / release 与 rc2 的 load 检查合同不变；已 load 时创建不改变状态。检索请求中的 ef、nprobe 等参数照常接受，不解析。

### 创建时的旧索引文件

3.x 加载索引只看文件名（`<data_stem>.<字段>.<类型小写>.idx`，标量为 `.sidx`），不核对 metric 与参数（约束 5、7）。

本实现创建任何类型的索引定义时（包括已支持的 FLAT 与 AUTOINDEX），该字段此前没有定义，因此同字段、同类型的 3.x 索引文件都是旧文件。它们的来源有两种：3.x 的 drop_index 删除失败被忽略后遗留的，以及被 snapshot 钉住而保留的。

不处理时，3.x 下次 load 会把旧文件当作新定义的索引加载。实测：COSINE/HNSW 的文件被 snapshot 钉住，drop 后重建 L2/HNSW，3.x 重开检索返回错误的主键与距离。这是 0002「继承的 3.x 已知风险」中已记录的情形，而本实现放开 HNSW 等类型的创建后，会成为新的触发入口。

处理方式：

- **未被钉住的旧文件**：在提交 manifest 之前删除。
- **被钉住的旧文件**：读取钉住它的 snapshot 中的 manifest 副本，取该字段的定义（类型、metric、参数）。只要有一份与新定义不同，或无法判定，就拒绝创建并登记差异；全部相同时保留文件并照常创建。
- snapshot 文件与 3.x 用它们恢复的能力保持不变。

提交顺序与失败语义见[下文](#提交顺序与失败语义)，与 DropIndex 共用。

### DropIndex

按 3.x 的 [collection.py:1491](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/collection.py#L1491) 与 servicer 的 DropIndex 对齐：

- 名字解析：先用 field_name；为空时只把 index_name 按 `<字段名>_idx` 匹配，匹配不到则视为删除全部索引。pymilvus 的 `drop_index(c, "<字段名>")` 因此会删除该 collection 的全部索引，包括标量 INVERTED；实测 `drop_index(c, "nope")` 同样删除全部。只有 `<字段名>_idx` 或显式的 field_name 才定向删除（见[已确认的取舍](#已确认的取舍) 1）。
- 错误：没有任何索引时报 `no index to drop`；指定的字段没有索引时报对应原文；collection 已 load 时报 `index cannot be dropped on loaded collection; call release() first`。错误码按现有映射。
- 删除后没有剩余索引时，collection 变为 Loaded，与 3.x 相同。
- 持久化走 Db 的写入许可，与 CreateIndex 相同。约束 7 要求删除被删定义对应的 3.x 索引文件：逐个分区扫描 `indexes/`，删除后缀为 `.<字段>.<类型小写>.idx`（标量为 `.sidx`）且未被 snapshot 钉住的文件。

### 提交顺序与失败语义

创建与 DropIndex 共用以下顺序：

1. **先完成全部可确定的检查，再做任何删除。** 包括：确定要删的文件；检查分区与 indexes 目录的形态（沿用 0006 的目录形态规则）；创建时还要做被钉住文件的定义比对。任一项不通过都直接报错，不删任何文件。
2. **删除文件，再同步 indexes 目录。**
3. **提交 manifest**：创建时加入新定义，DropIndex 时去掉被删的定义。

失败时按 manifest 是否已经替换分两种情况，与 CreateIndex 现有的提交处理一致：

- **替换之前失败**（含第 1、2 步）：向上报错，内存中的定义与 load 状态不变。已删的 3.x 索引文件本来就是旧文件，或由 3.x 在下次 load 时重建，不影响正确性。
- **替换成功、目录同步失败**（`AfterReplace`）：磁盘上的定义已经改变，内存同样发布提交后的定义与相应的 load 状态（DropIndex 删完全部索引时为 Loaded）。同时记录「持久化未确认」，沿用既有的 manifest 重试协议，RPC 的结果也与 CreateIndex 遇到同一阶段失败时相同。

故障注入分别验证这两种结果。3.x 的 drop_index 忽略删除失败（OSError），也不同步目录；本实现报错，登记为差异。

先删文件、后提交 manifest，是为了不让「定义已经变化、旧文件却还在」的状态落盘：否则同类型、不同 metric 的新定义会让 3.x 加载到错误的旧索引（约束 7 的实测风险）。

### 登记的差异（规则 2）

| 行为 | 3.x | 本实现 |
| --- | --- | --- |
| 新建的 HNSW / HNSW_SQ / IVF_FLAT / IVF_SQ8 上的检索 | 近似检索，召回取决于参数 | 精确 FLAT，召回不低于 ANN；结果与顺序可能不同，大数据量时更慢 |
| 构建参数与检索参数 | build 时解析 build_params、检索时解析 ef / nprobe，非法值报错 | 不解析，照常执行（扩展 0002 第 28 行已登记的参数差异） |
| 新建的标量 INVERTED | 已建索引的 segment 与 memtable 的结果可能不同 | 按扫描求值（扩展 0006 已登记的 INVERTED 差异） |
| 创建时遇到同字段、同类型的旧 3.x 索引文件 | 不处理，load 时直接加载；定义不同时结果错误（0002 已记录的继承风险） | 未被钉住的删除；被钉住且与钉住它的 snapshot 中的定义不同（或无法判定）时拒绝创建 |
| DropIndex 删除旧索引文件失败 | 忽略，照常提交 | 报错；manifest 替换之前失败时，定义与 load 状态不变 |

### 验证要求

- core：
  - 创建的接受与拒绝矩阵：各类型 × 字段类型 × 已 load / 未 load；
  - DropIndex 的名字解析、错误与 Loaded 转换；
  - 创建与 DropIndex 各自的故障注入：删除、目录同步、manifest 替换前、manifest 替换后（`AfterReplace`）四处失败与崩溃点，断言定义、load 状态与文件状态，以及替换前后两类结果的区分。
- 差分：create / describe / list / drop 的结果与错误原文。检索：对 3.x 使用 FLAT 定义的相同数据断言结果一致；对 HNSW 只断言本实现返回精确结果。
- 互通：
  - 本实现建 HNSW，3.x 打开后 load 建出 `.idx` 并检索；
  - 3.x 带 `.idx` 的目录由本实现 DropIndex：钉住的文件保留，其余删除；
  - 以不同 metric 重建同类型定义：没有钉住的旧文件时，3.x 重开后距离正确；有钉住且定义冲突的文件时，本实现拒绝创建，目录不变，snapshot 仍可由 3.x 恢复；
  - 钉住的定义与新定义相同时允许创建，3.x 重开后结果正确；
  - 模拟 3.x 删除失败后遗留的未钉住旧文件：本实现创建前将其删除，3.x 重开后结果正确。
- 复用：恢复 tests/python/adapter 中登记为「HNSW 换成 FLAT」的原用例，INVENTORY 同步；两条 SDK 线都要跑。

## 切片 B：过滤表达式

### 范围

解析器已覆盖完整语法（7791 条语法真值），本切片只扩展求值：

- 布尔组合：`or`、`not`（含 `||`、`!` 等写法）与任意嵌套；
- schema 标量字段与动态键的 `in` / `not in`，以及两个字段之间的比较；
- `like`；
- `is null` / `is not null` / `exists`；
- JSON 字段与嵌套 `$meta` 的路径访问（`meta['a']['b']`、下标），以及在路径上的比较、like、`is null` / `exists`；
- `json_contains` / `json_contains_all` / `json_contains_any` 与 `array_contains*`，作用于 JSON 字段、路径或动态值；`array_length` 只作用于 JSON 字段或动态字段名。

参照语法本身不允许的写法，仍报 code 6 与原文，不扩展解析器（rc2 的解析器已与之一致，实测两边原文相同）：
- 路径或 `$meta[...]` 出现在 `in` / `not in` 左侧，报 `left side of 'in' must be a field reference`；
- `array_length` 的参数是路径，报 `array_length: expected ')'`。

仍保持 UNIMPLEMENTED：算术运算（见[已确认的取舍](#已确认的取舍) 3）、ARRAY 字段（类型本身未接入）、`text_match`、几何谓词、TIMESTAMPTZ 与时间区间。

### 语义合同

- 求值路线与 3.x 一致，并且在编译时按整条表达式静态确定（[semantic.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/search/filter/semantic.py)）：
  - 只引用 schema 字段的走 Arrow 路线：Kleene and / or、invert、`is_in`、`match_like`，顶层 null 视为不匹配；
  - 含顶层 `$meta` 键的走 hybrid 路线，类型或转换失败时整条表达式回退 Python，按 0009 扩展到完整布尔树；
  - 含路径访问或 JSON / 数组函数的，全部走 Python 逐行语义。
  - 各路线对异构值、null、宽整数、NaN 与类型错误的处理不同，必须按路线分别复刻，不能统一成一种求值。
- 错误的原文与优先级与 3.x 一致；解析与语义检查（code 6）不变。顶层主键 `in` 的快路径（顺序、重复、类型转换）保持不变。
- 沿用 0006 第 13 项与 0009 的有效位边界：只对活行求值，已登记的差异扩展到新增的形式。失败的 Delete 不改变序号、WAL 或行集。
- 求值仍在状态锁之外、不做 I/O，不改变锁、快照、计算池与提交协议，不引入过滤索引或表达式缓存。

### 验证要求

- 组件真值：仿照 0009 的生成器，直接调用真实 3.x 的 parse / compile / evaluate，按三条路线分别生成语料（布尔组合与 null、in / not in、两字段比较、like 的通配与转义、各类路径与缺失键、JSON 函数在异构值上的表现、错误优先级），`--verify` 逐条一致，并分别在 Arrow 15 与 25 下核对。
- core：语料单测；生命周期组合（多块、重开、旧快照、失败的 Delete）。
- 差分：Search / Query / count / Delete、分页、发布与重开、两条 SDK 线；恢复 INVENTORY 中因过滤而列为范围外的用例。
- 性能：本切片重写了求值结构，需按 [S1 专项](../../benchmarks/s1/README.md)复测既有形式不退化；新增形式不声明性能。

## 实施顺序与验收

1. A 先于 B：A 改动小，并且能解开上游子集的主要失败。
2. 每个切片依次走：实施设计 → 评审 → 实现与测试 → 评审 → 单独提交，按 AGENTS 的「改了什么就跑什么」跑齐对应套件。
3. 两个切片完成后，按本文「依据」的同一方法重跑缺口统计作为验收证据，再按 0013 的流程发 rc3；版本切换、tag、发布逐项授权。候选构建前先清理首发残留，腾出磁盘空间。
4. 需要同步的文档：
   - README：当前范围、索引段落与 DropIndex 的名字行为；
   - 0002：「结论」（新建的 ANN / INVERTED 不再报错），以及「继承的 3.x 已知风险」中被 snapshot 钉住旧 `.idx` 的一条（本实现创建时的处理）；
   - 0004 / 0006 的差异条目；
   - tests/python/adapter/INVENTORY.md；
   - AGENTS 的知识索引与架构地图。

## 已确认的取舍

负责人于 2026-10-14 确认：

1. **DropIndex 的名字行为照 3.x 对齐。**
   - 3.x 的这一行为属于缺陷，规则要求由负责人决定，决定是对齐而不修正。
   - README 写明：普通字段名与匹配不到的名字都会删除全部索引；`<字段名>_idx` 或显式的 field_name 才定向删除。
2. **3.x 无法构建的定义继续在创建时拒绝**，保留现有的错误与校验顺序。3.x 会持久化它们、直到 load 时才报错；0002 约束 1 要求本实现不写出 3.x 无法 load 的定义。
3. **算术运算不纳入切片 B，继续明确拒绝**，后续单独评估。Arrow 路线的整数溢出与 Python 精确运算的语义不同，复刻成本高；统计中涉及的用例很少。
