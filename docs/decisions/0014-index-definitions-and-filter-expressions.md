# 0014 rc2 之后的兼容面：索引定义与过滤表达式

- 状态：范围与取舍已确认，切片 A 的实施设计见下文。负责人于 2026-10-14 确认本文两个切片、索引方案（接受 ANN 定义、按 FLAT 执行）与[已确认的取舍](#已确认的取舍)。实现与测试另做评审，发布另行授权
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
- 先保留 3.x 原有的校验顺序与错误原文，再做本实现的兼容性限制：解除 FLAT 阶段对上述类型的限制；AUTOINDEX 与这四种 ANN 类型只接受 COSINE / L2 / IP，BM25 / NONE 返回 UNIMPLEMENTED。3.x 自身无法构建的定义（IVF_PQ、DISKANN 等未知类型、向量字段上的 INVERTED）和稀疏字段上的任何索引，继续在创建时返回 UNIMPLEMENTED（见[已确认的取舍](#已确认的取舍) 2）。
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

1. **先完成实施设计清单规定的预检与 manifest 准备，再做任何索引删除。** 覆盖状态、版本、完整目录链、钉住定义、索引文件删除权限与 manifest 替换权限。实际创建并同步 tmp 只证明准备成功，不能代替目标替换能力检查。预检范围与未覆盖的系统限制在下文明确列出，失败时不删除索引文件。
2. **持久化删除意图标记，再删除文件、同步 indexes 目录。** 标记在删除开始之前落盘，删除同步确认后才清除；没有要删的文件时不写标记。
3. **提交 manifest**：创建时加入新定义，DropIndex 时去掉被删的定义。

失败按发生的阶段区分，manifest 替换前后的处理与 CreateIndex 现有的提交处理一致：

- **替换之前失败**（含第 1、2 步）：向上报错，内存中的定义与 load 状态不变。
  - 创建时已删的文件都是没有对应定义的旧文件，不影响任何现有定义。
  - DropIndex 时已删的文件属于仍保留的定义。3.x 下次 load 会按该定义重建；如果定义的构建参数 3.x 无法解析（例如 HNSW 的 `M="abc"`），原本靠现成文件可以 load 的目录会变成无法 load。3.x 自己保存 manifest 失败时也会留下同样的磁盘状态；差别在于内存状态，以及本实现与并发 load 竞争这一额外来源，见下表。
- **删除开始之后、目录同步确认之前退出**（包括部分删除后提前返回、同步失败、panic），以及删除前的目录同步能力检查失败：按替换之前的失败报错，持久化标记保留，并封锁该 collection 的 CreateIndex / DropIndex。扫描为空不能证明删除落盘。跨服务重开同样封锁；只有可信的系统重启依据才允许自动清除，无法取得该依据时须人工确认宿主系统已经重启后修复。删除已经确认、只剩安全的标记清理时，当前实例可重试清理，不能与未确认删除混为一类；协议见[实施设计](#实施设计)。
- **替换成功、目录同步失败**（`AfterReplace`）：磁盘上的定义已经改变，内存同样发布提交后的定义与相应的 load 状态（DropIndex 删完全部索引时为 Loaded）。同时记录「持久化未确认」，沿用既有的 manifest 重试协议，RPC 的结果也与 CreateIndex 遇到同一阶段失败时相同。

故障注入分别验证这三种结果。3.x 的 drop_index 忽略删除失败（OSError），也不同步目录；本实现报错，登记为差异。

先删文件、后提交 manifest，是为了不让「定义已经变化、旧文件却还在」的状态落盘：否则同类型、不同 metric 的新定义会让 3.x 加载到错误的旧索引（约束 7 的实测风险）。

### 登记的差异（规则 2）

| 行为 | 3.x | 本实现 |
| --- | --- | --- |
| 新建的 HNSW / HNSW_SQ / IVF_FLAT / IVF_SQ8 上的检索 | 近似检索，召回取决于参数 | 精确 FLAT，召回不低于 ANN；结果与顺序可能不同，大数据量时更慢 |
| 稠密向量 FAISS 类型的 BM25 / NONE | 保存定义，有已提交行且需要构建时 load 报错 | 创建时返回 UNIMPLEMENTED，防止写出不可互通的定义；包括已发布 rc2 允许的 AUTOINDEX 组合 |
| 构建参数与检索参数 | build 时解析 build_params、检索时解析 ef / nprobe，非法值报错 | 不解析，照常执行（扩展 0002 第 28 行已登记的参数差异） |
| 新建的标量 INVERTED | 已建索引的 segment 与 memtable 的结果可能不同 | 按扫描求值（扩展 0006 已登记的 INVERTED 差异） |
| 创建时遇到同字段、同类型的旧 3.x 索引文件 | 不处理，load 时直接加载；定义不同时结果错误（0002 已记录的继承风险） | 未被钉住的删除；被钉住且与钉住它的 snapshot 中的定义不同（或无法判定）时拒绝创建 |
| DropIndex 删除旧索引文件失败 | 忽略，照常提交 | 报错；manifest 替换之前失败时，定义与 load 状态不变 |
| DropIndex 已删除文件、磁盘 manifest 仍为旧定义 | 先删文件、后保存 manifest，保存失败时同样留下「文件已删、定义仍在」；此时内存中的定义已经删除，与磁盘不一致，重开后恢复。3.x 不会因并发 load 在删除之后拒绝 | 替换前失败时内存保留定义，与磁盘一致；来源也包括第 4 步之后的并发 load 导致删除后拒绝。另一个来源是 `AfterReplace` 的持久化仍未确认时断电：删除已落盘，manifest 却可能回退为旧版本，重开后同样留下「文件已删、定义仍在」。这些磁盘形态对 3.x 的影响相同：下次 load 需要重建，构建参数 3.x 无法解析时（如 `M="abc"`），原本能 load 的目录会 load 失败。本实现自身不建索引，不受影响 |
| CreateIndex / DropIndex 之前的 flush | 两者都不 flush，WAL 中的行保持未提交 | 先 flush，WAL 中的行成为已提交 segment，已有数据的查询结果不变。但 rc2 的 load 检查以「有已提交的行」为条件：只有 WAL 的 collection 若带 3.x 无法构建的向量定义（如 DISKANN），3.x 中创建或删除另一个索引后仍能 load，本实现之后 load 会失败 |
| 索引删除持久化未确认 | 无本实现的意图标记与封锁协议 | 只封锁该 collection 的索引变更；同一次可信启动内重开仍拒绝，不可信启动来源须人工恢复。删除已确认后的清理错误不改变已提交 RPC 的成功结果，当前实例可重试安全清理 |

### 验证要求

- core：
  - 创建的接受与拒绝矩阵：各类型 × 字段类型 × 已 load / 未 load，以及五种 FAISS 类型 × 五种 metric；FLAT / BRUTE_FORCE 的 BM25 / NONE 继续接受；
  - DropIndex 的名字解析、错误与 Loaded 转换；
  - 创建与 DropIndex 各自的故障注入以实施设计中的「实施验收矩阵」和「fault-injection 与证据」为准，覆盖准备、意图写入、删除、提交与清理的全部规定出口；断言 RPC、定义、load 状态、文件与重试状态，不限于原来的四个注入位置。
- 差分：create / describe / list / drop 的结果与错误原文。检索：对 3.x 使用 FLAT 定义的相同数据断言结果一致；对 HNSW 只断言本实现返回精确结果。
- 互通：
  - 本实现建 HNSW，3.x 打开后 load 建出 `.idx` 并检索；
  - 3.x 带 `.idx` 的目录由本实现 DropIndex：钉住的文件保留，其余删除；
  - 以不同 metric 重建同类型定义：没有钉住的旧文件时，3.x 重开后距离正确；有钉住且定义冲突的文件时，本实现拒绝创建，目录不变，snapshot 仍可由 3.x 恢复；
  - 钉住的定义与新定义相同时允许创建，3.x 重开后结果正确；
  - 模拟 3.x 删除失败后遗留的未钉住旧文件：本实现创建前将其删除，3.x 重开后结果正确。
- 复用：恢复 tests/python/adapter 中登记为「HNSW 换成 FLAT」的原用例，INVENTORY 同步；两条 SDK 线都要跑。

### 实施设计

状态：切片 A 的实施设计按完整失败流程收敛。采用已确认的方案乙；本节冻结入口、锁、预检范围、标记与提交协议。当前源码已实现切片 A，开发验证见 [实施记录](../testing/index-definitions-acceptance.md)；实现已通过约定范围内的评审，切片 B 尚未实施。

**冻结的不变量。**

1. **先完成下表规定的形态、能力与持久化前提检查，再删除任何索引文件。** 预检是明确的有限集合，不宣称能预测所有系统拒绝；操作时仍须处理 I/O 错误、权限变化与并发 load。
2. **索引删除未确认落盘时，不提交任何改变该 collection 索引定义的 manifest。** 删除之前先持久化意图标记；部分删除、同步失败、panic 或同步前进程退出，都不能绕过它。
3. **删除未确认时保持封锁，不靠原样重试 fsync 证明。** 一次错误可能只上报一次（0006 第 8 项、0007）。解除封锁须有可信的系统重启依据，或由操作者确认承载目录的系统已经重启后修复；容器或服务重启不能替代它。

**入口与顺序。** CreateIndex 与 DropIndex 共用以下流程；前两步沿用现有 [Db::create_index](../../crates/core/src/db.rs)：

1. **进入**：取数据库写入闸门，按名字取句柄。首次访问的懒打开与孤儿清理保持现状；孤儿清理按 source stem 是否仍被 manifest / snapshot 引用判断，不按字段与类型判断。
2. **flush**：`flush_handle(include_active)` 提交待 flush 的 WAL，并完成既有的 manifest 重试。DropIndex 沿用此入口。3.x 两者都不 flush；对后续 load 检查的影响已登记，不撤销已完成的 flush。
3. **取 flush 锁**，持有到本次操作及清理结束。它串行化 manifest、索引定义与意图标记的操作。标记 I/O 之前先以 `symlink_metadata` 确认 collection 根仍是真目录；子目录链在第 5 步继续检查。
4. **请求校验与标记判定**：创建沿用 `build_spec`；DropIndex 按 3.x 名字解析，依次检查「没有索引」「字段没有索引」「已 load」。检查 collection 未删除、持久化状态未损坏、不需重新提交、版本号不溢出。纯计算持状态读锁；标记 I/O 不持状态锁，按下文状态表处理。
5. **索引文件预检**（只持 flush 锁）：检查完整目录链；列出与目标字段、类型匹配的 `.idx` / `.sidx`；读取与 `load_pins` 共用的、已把反斜杠规范化为 `/` 的 snapshot 引用，区分钉住的文件并保留每份 snapshot 的归属。创建时，对被钉住且 source stem 仍在当前 manifest 中的文件，比较每一份钉住它的 snapshot manifest 中的类型、metric 与构建参数；不同、缺失或无法读取都拒绝。只把未钉住的匹配文件列为待删集合，并检查其删除能力。
6. **准备 manifest**（只持 flush 锁）：执行现有继承文件同步；校验 target / prev / tmp 的形态；创建、写入并同步候选 tmp，复制 prev。仅当待删索引文件非空时，再检查新 tmp 与 manifest 目标的替换能力；无删除时沿用实际 rename 的提交点与错误协议。`create_new` 成功后 tmp 归本次操作所有；准备本身或后续替换前失败、panic 都清理 tmp，清理错误附在原错误后。`prev_copy_error` 保持既有的非阻断语义。
7. **持久化删除意图**（仅待删集合非空）：按下文格式写 tmp、同步文件、rename、同步 collection 目录，全部成功后才进入删除阶段。失败时不碰索引文件，清理两种 tmp；若最终标记已经出现，当前实例记为 `CleanupPending` 并尝试清理。进程在此阶段退出后，新实例无法证明删除未开始，按磁盘标记保守判断。
8. **删除**：先同步 `indexes/` 做能力检查，再逐个删除未钉住的候选文件（NotFound 视为已删除），最后同步 `indexes/`。标记落盘后、最后一次同步确认前为 `Unconfirmed`；本阶段任何错误或 panic 都保留标记、清理 manifest tmp、报替换前错误，并封锁该 collection 的索引变更。局部 guard / `catch_unwind` 保证提前返回与 panic 同样收尾。最后一次同步成功后，立即转为 `CleanupPending`。
9. **提交**（持状态写锁）：重新确认第 4 步的状态前提。DropIndex 若被并发 load 抢先，按替换前错误返回，不回滚并发 load。否则执行 manifest rename 与 collection 目录同步，按下表发布定义及 load 状态；`AfterReplace` 沿用现有 `commit_index` 的重试与 RPC 语义。
10. **清理已安全的标记**（释放状态写锁，仍持 flush 锁）：第 7 步未开始删除，或第 8 步已经同步确认，才允许清理。第 9 步的所有出口都执行此收尾，包括重新校验失败、`BeforeReplace`、`AfterReplace` 与成功。清理失败只记录独立的清理错误、保留 `CleanupPending`，不覆盖 manifest 提交结果，也不改写其重试状态。

没有待删文件时还跳过第 6 步的替换权限预检，并跳过第 7、8 步，不写标记，也不为索引删除同步目录；manifest 本身的同步照常执行。

**删除前的形态与能力清单。** 两个入口共用一处检查，不以「目录能 fsync」推断写入、删除或替换权限。

| 对象 / 操作 | 删除前的检查 | 失败时结果 |
| --- | --- | --- |
| collection → `partitions/` → `_default/` → `indexes/` | 各级 `symlink_metadata`，不跟随链接；缺失及其下级按空集合，存在则必须是真目录；已有目录须能枚举 | 不删索引，不提交候选 manifest |
| 匹配的侧车与 snapshot 副本 | 候选必须为普通文件；先完成钉住判定和定义比对 | 同上 |
| `indexes/` 中待删文件 | 目录有效用户 W/X 权限、粘滞位所有权；macOS 还检查每个文件的实际删除权限与目录 / 文件 immutable、append-only 标志 | 同上，不写新标记 |
| manifest target / prev / tmp | target、prev 若存在必须为普通文件，tmp 异物规则沿用现状；实际创建、写入、同步 tmp；复制 prev 的错误仍按既有协议报告 | 同上；本次拥有的 tmp 清理 |
| manifest tmp → target 的 rename（仅保护后续索引删除时） | collection 目录的 W/X 与粘滞位；macOS 检查 tmp 的删除权限、已有 target 的删除权限及相关保护标志，不能仅凭 tmp 写入成功判定 | 同上，不写新标记 |
| 意图 tmp → 最终标记 | 形态与所有权规则同上；实际完成写入、同步、rename 与父目录同步 | 任一步失败均不删除索引；当前实例只处理安全的标记清理 |

- 通用目录权限使用 `faccessat(AT_EACCESS)`。此能力检查仅在存在待删索引文件时执行；没有删除时不因凭据差异或粘滞位的保守判定阻止普通索引变更。存在删除时粘滞位按目录或文件归有效用户所有判定，不假定 root 一定拥有绕过权限的能力。Linux 的旧 glibc 在部分凭据情形下会模拟此检查；遇到实际凭据不符合该检查前提时保守拒绝，不把模拟成功当作删除许可。普通服务的 real / effective UID、GID 相同路径须在 manylinux_2_28 环境实测。
- macOS 使用 `faccessat(..., _DELETE_OK, AT_EACCESS)` 检查文件删除权限，涵盖 ACL 的 `delete` 拒绝；目录 W/X 与保护标志另查。该检查同样用于 manifest tmp 与已有 target。`_DELETE_OK` 是 Darwin 扩展，不传给 Linux。[Apple 定义](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/unistd.h)
- Linux 的 chattr / ioctl 标志、LSM 与文件系统专有拒绝不在本切片的预检保证内；稳定的目录同步不支持由第 8 步能力检查发现。实际操作发生这些拒绝仍走持久化封锁，不静默，也不扩大为通用权限框架。
- 检查后目录、条目或权限被外部改变，属于持有 LOCK 期间的外部修改；预检不消除该竞态。实际错误照常处理，删除已完成的部分不恢复，可用性影响沿用上文登记。

**意图标记的格式与恢复依据。**

- collection 根目录的私有文件为 `milvus-lite-rust.index-intent.json`，同目录 tmp、文件同步、rename、目录同步。本切片冻结 `format_version = 1`，并包含 `operation`（`create_index` / `drop_index`）、`field_names`、`files`（待删文件相对 collection 的路径）、`boot_id`、`boot_source`、`auto_recovery`。
- `field_names`、`files` 是非空字符串列表；文件必须是 `partitions/_default/indexes/<侧车文件名>` 形式的合法相对路径，不允许绝对路径或 `..`。标记只记录本次操作，不按它的路径重放删除。`boot_id` 为规范化 UUID 字符串或 null，`boot_source` 为 `linux-kernel-procfs`、`darwin-kern-bootsessionuuid` 或 null；两者须同时有效或同时为 null。`auto_recovery` 为布尔值。只有有效来源、有效 UUID 与可信的缓存边界同时成立，才写 true；否则写 false。null 是明确的未知，不是字段缺失，不用随机 UUID 或空串代替。
- **Linux 来源**：打开 `/proc/sys/kernel/random/boot_id`，检查 `/proc/self/mountinfo` 排除该文件的单文件覆盖挂载，并对同一 fd 做 `fstatfs`；只有真实 procfs（`PROC_SUPER_MAGIC`）中的合法 UUID 才记为可信来源。普通文件、tmpfs / FUSE 覆盖、读取或检查失败均记为未知。不能以「在容器里」判断可信与否；systemd-nspawn 会用普通文件覆盖此路径，每次容器启动换值。[Linux 定义](https://github.com/torvalds/linux/blob/master/include/uapi/linux/magic.h)、[nspawn 实现](https://github.com/systemd/systemd/blob/main/src/nspawn/nspawn.c)
- **macOS 来源**：`sysctlbyname("kern.bootsessionuuid")`，读取失败或不是合法 UUID 时记为未知。Linux 与 macOS 来源不能互相比较以证明重启。
- **缓存边界**：自动恢复限于由服务所在内核管理的本地文件系统。本切片保守识别 Linux ext4、XFS、Btrfs，以及 macOS APFS、HFS；collection 与 indexes 所在文件系统都须符合。overlayfs 一律记为不可信，不解析或探测其 upperdir。网络、FUSE、9p、virtiofs 或无法识别的类型同样记为不可信。不可信只关闭自动恢复，不妨碍正常完成删除；删除未确认时按下文人工恢复。虚拟机内的本地文件系统按虚拟机内核判断，共享到宿主机的目录不据虚拟机重启自动解除。
- 这是独立的持久化安全状态，不能从当前目录扫描重建。同步 0002 时须同时更新格式、互通范围，以及约束 1 中「额外状态可重建」的表述，为本标记明确列出例外。

**当前实例的三种状态。** 状态由 flush 锁串行化；`CleanupPending` 还保存本实例写入或已经验证可清理的标记内容，不能据一个布尔值忽略任意磁盘标记。

| 状态 | 含义 | 下一次合法索引变更请求 |
| --- | --- | --- |
| `Clear` | 本实例没有待处理的意图 | 读取磁盘标记，按下表判定 |
| `Unconfirmed` | 索引删除的落盘状态无法确认，或标记损坏 | 拒绝，不原样重试索引目录同步；新实例重新按磁盘标记判断 |
| `CleanupPending` | 本实例知道尚未开始删除，或已经确认删除落盘；只剩标记清理 | 先核对磁盘标记仍为记录的内容，再清理；成功转 `Clear` 后继续，失败报清理错误并保留此状态，不要求系统重启 |

`CleanupPending` 看到相同的普通标记时删除它，再同步 collection 目录；看到 NotFound 时只补标记清理所需的 collection 目录同步。读取或清理因 I/O 失败时保留 `CleanupPending`，允许故障消除后重试；确实看到其他内容、损坏或形态变化时才转 `Unconfirmed`，不覆盖。该重试不用于证明索引删除落盘，也不能清掉既有的 manifest `retry_manifest` 标志。

**新实例或 `Clear` 状态的磁盘判定。**

| 磁盘状态 | 判定与动作 |
| --- | --- |
| 无最终标记 | 正常；旧目录和 3.x 目录无需迁移。未 rename 的意图 tmp 不是删除开始的证据，按普通文件形态与所有权规则清理 |
| 有效标记；来源相同、双方可信且 UUID 相同 | `Unconfirmed`，拒绝索引变更 |
| 有效标记；任一启动标识未知、来源不同、标记 `auto_recovery = false`，或当前缓存边界不可信 | `Unconfirmed`，不凭字符串差异自动清除；确认承载目录的系统已重启后，停止使用该目录的服务，手工删除 collection 根的 `milvus-lite-rust.index-intent.json`，再重开服务；具体边界见下文 |
| 有效标记；双方可信、来源相同、标记允许自动恢复，且 UUID 不同 | 所在内核已经重启，旧删除以重启后的实际目录为准。记为 `CleanupPending`，清理标记并同步 collection 目录后继续；清理失败仍可在当前实例重试清理 |
| 非普通文件、非法 JSON、未知版本、字段缺失或值不合法 | `Unconfirmed`，不自动删除或覆盖；确认承载目录的系统已重启后，停止服务，手工移除 collection 根的 `milvus-lite-rust.index-intent.json` 保留条目，再重开服务；异常条目的处理见下文 |

只有删除同步确认后才会提交候选索引定义。因此真实重启后，未确认的删除若回滚，manifest 仍是旧定义；已同步确认的删除不会回滚。不能用仅修改磁盘标记 UUID 的测试来证明这一前提。

损坏与 `Unconfirmed` 只限制该 collection 的 CreateIndex / DropIndex。读写、Flush、load、drop collection 与其他 collection 保持现有行为；它们不改变该 collection 的索引定义。正常完成后的清理失败与尚未确认的删除是两种状态，错误信息须区分「可重试清理」与「须重启或人工修复」。

**人工恢复动作。** 适用于删除未确认且不能自动恢复，或最终标记损坏的情况：确认承载数据目录的系统已经真实重启（容器 / 服务重启不算；共享目录须确认承载它的宿主系统），并停止持有该目录的服务或退出对应 Python 进程；手工删除 `<data_dir>/collections/<collection>/milvus-lite-rust.index-intent.json`，然后重新启动服务。只处理这个最终标记，不删除 manifest、索引文件或 snapshot。标记为符号链接时只移除链接本身；目录等异常条目移出这个保留路径，不递归删除内容。仅当前实例的 `CleanupPending` 可重试安全清理，无需系统重启；实例退出后这一内存依据不保留，重开按磁盘判定，可能仍需上述人工恢复。

**manifest 与清理结果。** 先确定提交结果，再独立收尾；清理不能把成功变失败，或把替换前错误掩盖掉。

| manifest 结果 | 内存定义与 load 状态 | 当次 RPC | 意图收尾 |
| --- | --- | --- | --- |
| 第 7 步失败，未开始索引删除 | 本次操作不改变 | 原准备错误；附清理错误 | 如有本实例拥有的最终标记，按 `CleanupPending` 清理 |
| 第 8 步退出且删除未确认 | 本次操作不改变 | 原删除 / 同步 / panic 错误 | `Unconfirmed`，保留标记 |
| 第 9 步重新校验失败或 `BeforeReplace` | 本次操作不改变，不撤销并发 load | 原替换前错误；附清理错误 | 删除已确认，尝试清理；失败为 `CleanupPending` |
| manifest 提交成功 | 发布候选定义；删完全部索引为 Loaded | 成功 | 清理失败记日志，保留 `CleanupPending` |
| `AfterReplace` | 发布候选定义及对应状态；保留现有完整重新提交协议 | 与现有 CreateIndex 一样成功，记录持久化未确认 | 清理独立处理，不清除 manifest 重试状态 |

第 9 步的替换原语保留现有 panic 的提交点分类；第 10 步的清理 guard 将错误或 panic 转成清理记录，不能阻止已提交状态发布。进程直接退出时无法运行 guard，最终标记保留，新实例按磁盘表判断；即使实际上已安全，同启动内也可能保守拒绝。

**互通与目录不变的边界。**

- 真实 3.x 使用正式文件名的探针已核实：原 collection 重开与查询正常，snapshot / restore 正常，原标记字节保留，恢复的新 collection 不复制标记。实施时仍补本实现与 3.x 双向测试；此探针不证明新恢复逻辑已经实现。
- 3.x 不执行本协议，会忽略标记并自行修改索引；本实现不能封锁 3.x 的操作，也不为其操作补充持久化保证。它的行为仍受 0002 已登记的继承风险约束。drop collection 与 truncate 随目录删除标记。
- 第 4–7 步拒绝时不删除索引文件，不提交候选 manifest，不改 snapshot；本次拥有的 tmp 清理，清理失败记录。第 2 步已完成的 flush 不撤销；意图已 rename 但准备失败时允许留下保守的最终标记，不能笼统写成目录逐字节不变。
- 文件扫描、snapshot 读取、权限检查、manifest 准备、意图写入、索引删除与标记清理不持状态写锁。仅第 9 步的 manifest rename / 同步与状态发布沿用现有提交锁边界。

**组件边界。**

- core index：解除已确认类型的创建限制、拒绝 FAISS 类型无法构建的 metric 组合，3.x 文件名解析统一一处，复用或替换 recovery 的 `sidecar_source_stem`。
- storage：`commit_manifest` 拆为准备与替换，旧调用方组合调用，保留提交点分类与 `prev_copy_error`；snapshot 增加逐份钉住 manifest 的读取；runtime 增加 DropIndex 提交与三种意图状态；一个模块负责意图格式、启动来源与清理；索引文件扫描与权限预检共用一处。
- collection / db：新增 `Db::drop_index`，定义顺序保持与 3.x 一致；所有持久化索引变更必须经过上述闸门和 flush 锁。
- grpc：`IMPLEMENTED_METHODS` 增加 DropIndex，只翻译 field_name / index_name、进入 Db 闸门及映射错误。
- libc 0.2 已在依赖图中，改为 core 直接依赖，用于 `faccessat`、`fstatfs`、平台标志及 macOS `sysctlbyname`。不引入 C++ 或额外系统库；实现时同步 AGENTS 技术栈，并核对完整 `cargo tree --workspace -e all`。

**错误。** DropIndex 既有错误与校验优先级按 3.x；IndexNotFound 为 code 700，SchemaValidation 为 code 6。冲突的 snapshot、目录形态、预检权限、意图封锁与损坏使用 InvalidArgument（`code=1, error_code=0`），信息写明路径和处理方式。存储 I/O 沿用既有映射。删除未确认提示重启 / 人工修复；安全的清理未完成提示重试，不要求重启。提交后的清理失败记日志，不更改当次 RPC 的成功结果。

**实施验收矩阵。** 下表同时适用于 CreateIndex 与 DropIndex；复用上文的类型、名字、钉住与互通测试，不只测内部状态位。

| 场景 | 必须观察的结果 |
| --- | --- |
| 没有 `_default/` 或 `indexes/`；没有待删文件；全部文件被钉住且创建定义相同 | 正常请求成功，无新意图、无索引删除或删除目录同步；DropIndex 的「无索引」错误仍保留 |
| 缓存句柄下三级目录各自为软链 / 文件；候选非普通文件；snapshot 定义冲突或读取失败 | 拒绝，库外文件、旧索引与候选 manifest 不变，snapshot 仍可恢复 |
| 索引目录只读 / 粘滞位；macOS 第二个候选的 delete ACL / immutable / append-only | 删除前拒绝，所有旧文件保留，无新最终标记 |
| 无待删文件时 real / effective 凭据不同；对照存在待删文件的操作 | 无删除的创建与 DropIndex 照常提交；有删除时仍在预检阶段保守拒绝，旧文件与 manifest 保留 |
| collection 目录只读；manifest target / prev / tmp 异物；版本溢出 / 损坏 / 未完成提交 | 删除前拒绝；tmp 所有权和清理按合同，旧索引与候选 manifest 不变 |
| macOS manifest target 的 delete ACL / immutable / append-only，及 tmp 继承删除拒绝 ACL | tmp 准备不能冒充替换能力；删除任何索引前拒绝，旧文件保留 |
| 意图 tmp 写入、文件同步、rename、父目录同步各点失败或退出 | 没有索引删除；当前实例对自己留下的标记只重试安全清理，新实例保守处理最终标记 |
| 第二次 unlink 失败、能力同步失败、删除中 panic、删除后同步失败、最后一次 unlink 后同步前进程退出 | 不提交候选定义；最终标记保留；当前及同可信启动内的新实例拒绝，扫描为空也不能绕过 |
| 启动标识读取失败、文件覆盖产生合法但不可信 UUID、缓存边界未知；写入时未知而后读取恢复 | 标记允许 null 且禁止自动恢复；正常删除成功可清理，未确认时绝不因后来得到不同 UUID 解锁 |
| 模拟双方可信且同来源的内核重启；对照容器 UUID 变化、内核启动未变 | 前者清理后继续，旧文件重新按规则处理；后者保持封锁。覆盖有 / 无旧文件的重启后形态，不只改标记字符串 |
| 损坏标记：目录、软链、非法 JSON、未知版本、字段缺失、错误类型 / 非法 UUID / 非法路径 | 有效请求拒绝，标记不变，不影响读写、Flush、load 与健康 collection |
| 删除已同步后，manifest 重新校验失败、rename 失败、`AfterReplace`、成功，分别叠加标记 unlink / 同步失败 | 逐项核对上表的 RPC、内存定义、load 状态、tmp、最终标记与 manifest 重试；当前实例可重试安全清理，新实例没有内存依据则保守拒绝 |
| 清理待完成时标记读取 I/O 失败、被改成其他内容，或清理中 panic | I/O 失败保留可重试清理；不覆盖不匹配的标记；清理 panic 不掩盖原提交结果，不能误清 manifest 重试状态 |
| 真实 3.x 读取正式标记、snapshot / restore；本实现写目录由 3.x 读取及反向重开 | 标记保留 / 不复制、定义及检索结果符合合同；未知启动依据保持封锁 |

已有两条反例保留：`M="abc"` 且已有 `.idx` 的 DropIndex 在删除后、替换前失败，3.x 重建报错；只有 WAL、带 DISKANN 与标量 INVERTED 的目录，定向删标量后本实现 load 失败而 3.x 成功。并发 load 在删除同步后、提交前插入，DropIndex 保留定义与并发 load 状态，按替换前错误返回并安全清理意图。

**fault-injection 与证据。** 复用 manifest 替换前 / 后注入；新增准备 tmp、意图写入各步、每次 unlink、目录同步、同步前进程退出及清理失败 / panic 落点。记录关键顺序并断言「标记持久化 → unlink → 删除目录同步 → manifest 替换」。启动测试覆盖已验证来源、未知来源与 UUID 变化，仅 fault-injection 构建可覆盖，正式 wheel 不含；替换测试值不是实际系统重启或断电证据。

文档阶段已执行 macOS 删除 ACL / manifest 保护标志的最小探针、真实 3.x 标记互通探针，并核对 Linux procfs 类型与上游来源实现。代码阶段须跑 core、故障注入、差分、互通及两条 SDK 线，Linux manylinux_2_28 与 macOS 的权限 / 启动来源分别实跑；这里不把未执行的实现测试写成通过。

**不改的部分。** load 检查、检索、过滤、compaction、恢复孤儿清理与 snapshot 钉住规则保持现状；不写本实现格式的索引文件，只支持 `_default` 分区。compaction 的祖先目录检查历史问题单独记录，不顺手修改。

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
   - README：当前范围、索引段落与 DropIndex 的名字行为；新增意图封锁出现的条件、仅影响索引变更的范围，以及人工恢复的完整路径与步骤，明确只有当前实例的 `CleanupPending` 无需系统重启；
   - 0002：「结论」（新建的 ANN / INVERTED 不再报错）；被 snapshot 钉住旧 `.idx` 的继承风险；新增意图标记的格式与互通范围（版本 1、可信 / 未知启动来源、3.x 保留、旧目录无标记），并明确它是不可从目录重建的私有安全状态；
   - 0004 / 0006 的差异条目；
   - tests/python/adapter/INVENTORY.md；
   - AGENTS 的知识索引、架构地图与技术栈（libc 改为直接依赖）。

## 已确认的取舍

负责人于 2026-10-14 确认：

1. **DropIndex 的名字行为照 3.x 对齐。**
   - 3.x 的这一行为属于缺陷，规则要求由负责人决定，决定是对齐而不修正。
   - README 写明：普通字段名与匹配不到的名字都会删除全部索引；`<字段名>_idx` 或显式的 field_name 才定向删除。
2. **3.x 无法构建的定义继续在创建时拒绝**，保留现有的错误与校验顺序。3.x 会持久化它们、直到 load 时才报错；0002 约束 1 要求本实现不写出 3.x 无法 load 的定义。稠密向量字段上的 FAISS 类型（AUTOINDEX、HNSW、HNSW_SQ、IVF_FLAT、IVF_SQ8）只接受 COSINE / L2 / IP；BM25 / NONE 在原有 metric 枚举校验之后返回 UNIMPLEMENTED。FLAT / BRUTE_FORCE 保留原行为。AUTOINDEX 的这一限制修正了已发布 rc2 可创建、却无法由 3.x load 的组合；已发布发行物不变。
3. **算术运算不纳入切片 B，继续明确拒绝**，后续单独评估。Arrow 路线的整数溢出与 Python 精确运算的语义不同，复刻成本高；统计中涉及的用例很少。
4. **索引文件删除的持久化确认采用持久化标记（方案乙）。** 删除前持久化意图，删除未确认时跨服务重开保持封锁；可信的内核重启允许自动恢复，启动来源或缓存边界不可信时保守保留、人工恢复。删除已确认后的标记清理另作可重试状态，不改动 manifest 提交语义。这是新增、不可从目录重建的私有安全状态，登记在 0002。
