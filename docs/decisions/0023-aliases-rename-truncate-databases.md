# 0023 别名、改名、清空、集合属性与多数据库

- 状态：合同终稿；2026-10-29 负责人确认五项取舍（见「已确认的取舍」），独立评审三轮后按意见修订并确认；未改代码
- 日期：2026-10-29
- 参照：milvus-lite v3.2.1 / `43d1257774e629bc9f66873977ab7c320d5bf5a7`；pymilvus 3.0.2 实测，2.6.17 经源码核对：`use_database` 同样先调 DescribeDatabase，改名请求只设 `newDBName`，数据库经 `dbname` 元数据传递，与 3.0.2 一致
- 基线：已发布 `0.3.0rc3`；仓库基线 `1026c0854886c70e303b725b4fdb00b98e95ff3a`
- 事实依据：本文「参照实测」；探针脚本（MilvusClient 与 raw gRPC 两类）、两端原始输出与落盘文件由负责人本地留存，实施前移入 [tests/compatibility_audit/](../../tests/compatibility_audit/) 作为可重跑工具
- 继承：[0002 互通约束](0002-data-dir-compat.md)、[0006 第 14–18 项](0006-m2-scope.md)、[0007 存储与锁序](0007-m2-storage-engine.md)、[0010 合并与 GC](0010-compaction-and-tombstone-gc.md)、[0011 独立删除状态](0011-committed-delete-state.md)、[0014 删除意图](0014-index-definitions-and-filter-expressions.md)、[0017 分区](0017-partitions.md)

## 推进边界

按 0.3.0rc3 重做的固定统计（方法同 [0014 收口](../testing/0014-compatibility-closure.md)，被测为公开 Release 的 macOS arm64 wheel）：

- 参照用例 760 条通过 608 条；其余 152 条为 151 条明确的 UNIMPLEMENTED 与 1 条原有跳过。按首个阻断，database 37 条（CreateDatabase 17、ListDatabases 14、DescribeDatabase 5、AlterDatabase 1），alias / rename / truncate / 集合属性 14 条（CreateAlias 4、TruncateCollection 4、RenameCollection 3、AlterCollection 3）。
- 上游子集 90 条通过 82 条；其余 8 条中 7 条属于本文范围（create_alias 5、drop_alias 1、rename_collection 1）。
- 49 项调用探针中 3.x 成功 43 项、本实现 34 项，相差的 9 项中 4 项属于本文范围（rename、alias、databases、collection_properties）。

统计只计首个阻断，解除后可能暴露其他阻断；数字是诊断样本，不是收益承诺。统计输出由负责人本地留存，实施验收按同一方法重跑。

| 切片 | 范围 | 进入下一步的条件 |
| --- | --- | --- |
| 合同 | 本文：参照真值、必须保留的行为、差异、提交协议方向与已确认的取舍 | 独立复审本文 |
| A | 对外只开放默认数据库：CreateAlias / AlterAlias / DropAlias / DescribeAlias / ListAliases，所有集合 RPC 的别名解析；RenameCollection；TruncateCollection；AlterCollection 的集合属性；`schema.json` 改为与 3.x 相同的键排序写出；对已存在的非默认数据库按 0006 第 15 项返回 UNIMPLEMENTED。内部接口从一开始按「数据库名 + 集合名」建模 | 实施设计（锁、提交点、两种意图、故障矩阵）单独评审后实施 |
| B | CreateDatabase / DropDatabase / ListDatabases / DescribeDatabase / AlterDatabase；请求 `db_name` 与 `dbname` 元数据路由；A 的全部能力按数据库隔离 | A 收口后单独设计评审；主要增加数据库生命周期与请求路由，不重拆 A 的数据结构 |

不在范围：TIMESTAMPTZ 及写入 `timezone` 属性（取舍 1）；AddCollectionField / AlterCollectionSchema / AlterCollectionField；snapshot RPC；LoadPartitions / ReleasePartitions；跨数据库改名（取舍 4）。现有 3.x snapshot 文件的钉住语义保持不变。

## 参照实测

实测环境为 macOS arm64（APFS，大小写不敏感）、CPython 3.12、pymilvus 3.0.2。「1/0」表示 status 的 `code` 为 1、旧字段 `error_code` 为 0：3.x 的 gRPC 层对不是 `MilvusLiteError` 的 Python 异常（如 `ValueError`、`OSError`）只设 `code=1`；`MilvusLiteError` 子类按映射表取值，表中没有的子类默认为 1/1。集合不存在为 100/4，数据库类错误为 6/6，集合或别名已存在为 1/1。

### 别名

| 操作 | 3.x 结果 |
| --- | --- |
| CreateAlias 判定顺序 | 解析目标（目标可以是别名）→ 校验别名名字 → 目标存在 → 别名与集合冲突 → 别名已存在；非法别名配不存在的目标时报非法名 |
| 成功 | `aliases.json` 记录解析后的真实集合名 |
| 别名名字非法（`/`、`\`、空串、`.`、`..`、NUL、超过 255 字节） | 1/0，与集合名同一套校验与文案，如 `invalid collection name: 'a/b'` |
| 目标不存在（含没有 `schema.json` 的残留目录） | 100/4，`collection 'nope' does not exist` |
| 别名与已有集合同名（按路径加 `schema.json` 判定：含大小写不敏感文件系统上只差大小写的集合；没有 `schema.json` 的残留目录不算冲突，别名照常创建） | 1/1，`alias 'c2' conflicts with an existing collection` |
| 别名已存在（同一或不同目标） | 1/1，`alias 'a1' already exists` |
| AlterAlias | 先判别名存在（100/4，`alias 'zz' does not exist`），再判目标存在（100/4）；目标可以是别名；不校验别名名字，`a/b` 报别名不存在 |
| DropAlias | 不校验名字；不存在时成功（no-op） |
| DescribeAlias / 不存在 | `alias`、`collection`；100/4，`alias 'zz' does not exist` |
| ListAliases 全部 / 按集合 | 排序后的别名列表；按集合时回填解析后的 `collection_name`，集合不存在时返回空列表、成功、回填原名 |
| 经别名 Has / Describe / Insert / Search / Query / 统计 / load 状态 | 按目标集合执行，错误文案中的集合名是目标名；Describe 的 `collection_name` 是目标名 |
| DescribeCollection 的 `aliases` | 不论直接还是经别名访问都是空列表 |
| ShowCollections | 只列真实集合；`collection_names` 中给出别名时被当作未知名过滤，返回空 |
| 以别名为名 CreateCollection | 1/1，`collection 'a1' already exists` |
| **经别名 DropCollection** | 删除目标集合，并删除所有指向它的别名 |
| 悬空别名 / 指向只差大小写的名字 | 读取时保留，不剔除目标已不存在的条目；大小写不敏感文件系统上指向只差大小写名字的别名照常可用 |
| `aliases.json` 不是对象 / 无法解析 | 前者按空表处理；后者使所有需要解析别名的 RPC 失败（1/0） |
| 落盘 | JSON 对象，`indent=2`、`sort_keys`、`ensure_ascii=False`；写 `aliases.json.tmp` 后 `os.replace`，不 fsync |

### 改名

| 操作 | 3.x 结果 |
| --- | --- |
| 判定顺序 | 解析旧名（可以是别名）→ 旧名存在（100/4）→ 校验新名（1/0）→ 新名冲突（1/1） |
| 新名已是集合或别名 / 与旧名相同 / 只差大小写（大小写不敏感文件系统） | 1/1，`collection '<new>' already exists` |
| 新名是没有 `schema.json` 的残留目录 | 空目录被 POSIX rename 静默覆盖并成功；非空目录报 1/0 的 `OSError` |
| 执行 | 关闭旧句柄 → 目录改名 → 重写 `schema.json` 的 `collection_name` → 把指向旧名的别名改指新名，并**总是**保存 `aliases.json`（没有别名时写出 `{}`） |
| 改名后状态 | 新名相当于重新打开：有索引定义时为 released，Query / Search 报 101，需重新 load；无索引定义时可直接读；数据、索引定义、分区保留 |
| 请求带 `newDBName`（pymilvus `target_db`） | **忽略**，即使该数据库不存在也在当前数据库内改名，成功 |
| 崩溃窗口 | 三步不原子；3.x 打开集合时只用目录名，`schema.json` 中的名字与目录不同也能打开；中途退出时别名可能仍指向旧名 |

### 清空

| 操作 | 3.x 结果 |
| --- | --- |
| 执行 | 记下分区列表、索引定义、`schema_version` 与是否 loaded；关闭句柄后 `rmtree` 整个集合目录，重建目录、`schema.json` 与新的 manifest（`version` 1、`current_seq` 0、`active_wal_number` 为空，分区与索引定义保留），重新打开；原来 loaded 则重新 load |
| 清空后 | count 为 0；分区、索引定义、别名保留；load 状态保持原样；目录内的 snapshot 随目录删除 |
| auto_id | 从 1 重新分配（清空前已分配到 5，清空后插入得到 1、2、3，重开后继续为 4、5） |
| 经别名 / 不存在 | 清空目标集合 / 100/4 |
| 崩溃窗口 | `rmtree` 不原子：中途退出可能留下带 `schema.json` 的残缺集合（可见但数据不全）；`rmtree` 之后、写回 `schema.json` 之前退出，留下没有 schema 的目录，集合消失且这个名字之后无法再创建 |

### 集合属性

| 操作 | 3.x 结果 |
| --- | --- |
| AlterCollection 设置 / 替换 | 值按字符串保存（整数 5 存为 `"5"`）；进程内新键追加在末尾、已有键原位替换 |
| 删除键（含不存在的键） | 成功，不存在的键忽略 |
| `timezone` | 用 `zoneinfo` 校验，未知时区为 6/6，`unknown timezone 'Mars/Base'` |
| `mmap.enabled`、`collection.ttl.seconds` 等 | 原样保存，不产生任何效果 |
| 集合不存在 / 经别名 | 100/4 / 修改目标集合 |
| 落盘 | 重写 `schema.json`：`indent=2`、`sort_keys`、`ensure_ascii=False`，写 tmp、fsync 文件后 `os.replace`，不 fsync 目录 |
| 顺序 | DescribeCollection 在同一进程内按插入顺序返回；改名、清空或重开之后从 `schema.json` 读回，按文件中的顺序返回。3.x 总是排序写出，因此读回自己写的文件即为键序（跨进程实测：进程内 `zeta, alpha, mid`，重开后 `alpha, mid, zeta`）；读取插入顺序写出的文件时返回插入顺序 |

### 数据库

| 操作 | 3.x 结果 |
| --- | --- |
| 路由 | 请求的 `db_name` 非空时用它，否则取 `dbname` 元数据，否则为 `default`。**例外**：CreateDatabase 与 DropDatabase 只读请求体的 `db_name`，忽略元数据，空串归一为 `default`。pymilvus 的 `using_database` 先调 DescribeDatabase，失败则不切换；`MilvusClient(db_name=...)` 每次请求带元数据；单次调用的 `db_name` 关键字参数不切换数据库 |
| 布局 | 默认数据库即数据根目录（`collections/`、`aliases.json`、`database_properties.json`）；其他数据库在 `databases/<db>/` 下各有同样三项，都按需创建：非默认数据库创建时即写出属性文件，默认数据库只在第一次有变化的 Alter 时写出 |
| Create | 校验名字 → 已存在 → 校验属性；在 `.database-staging/` 建好 `collections/` 与 `database_properties.json` 后改名移入 `databases/` |
| 已存在（含 `default`、空名、大小写不敏感文件系统上只差大小写的名字） | 6/6，`database 'd1' already exists` |
| 非法名 / 超过 255 字节 | 1/0，`invalid database name: 'a/b'` / `database name too long (256 bytes, max 255)` |
| List | 排序后的名字，含 `default` |
| Describe / 不存在 | `db_name` 与各属性；6/6，`database 'dx' does not exist`；大小写不敏感文件系统上 `D1` 能描述 `d1`，返回请求中的 `D1` |
| Alter / 删除属性 | 先判数据库存在，再合并或删除；没有变化时不写；不存在的键忽略 |
| Drop：`default` 或空名 / 不存在 / 非空 | 1/0，`cannot drop default database` / 6/6 / 6/6，`database 'd1' is not empty`（目录内有带 `schema.json` 的集合即非空）；大小写不敏感文件系统上 Drop `D1` 删除 `d1` |
| Drop 空数据库 | `shutil.rmtree` 删除整个目录，不经 staging |
| 不存在的数据库内的集合操作 | Describe、Query、Insert 等为 6/6；HasCollection 与 ShowCollections 为 1/0（HasCollection 的 `value` 为 false） |
| 隔离 | 同名集合、同名别名在不同数据库互不影响；集合名可以与数据库名相同 |
| 跨进程重开 | 数据库、属性、别名与其中的集合照常可见；有索引定义的集合为 NotLoad |
| 落盘 | `database_properties.json`：`indent=2`、`sort_keys`、`ensure_ascii=False`，写 tmp、fsync 文件后 `os.replace`，不 fsync 目录 |

### 本实现现状（0.3.0rc3）

- 上述 RPC 全部返回 UNIMPLEMENTED；`using_database` 因 DescribeDatabase 未实现而失败、不切换。
- [0006 第 15 项](0006-m2-scope.md)「请求名命中别名或数据库已存在时返回 UNIMPLEMENTED」**从未实现**：用本实现打开 3.x 写出的带别名目录，经别名的 Describe / Query 报 100/4「集合不存在」，HasCollection 为 false；带 `db_name` 访问已存在的 `databases/<db>` 报数据库不存在（[db.rs](../../crates/core/src/db.rs) 的 `require_database` 对非默认库一律如此）。0006 差异表与 README 的相应表述与实际不符。
- create / drop 已按 3.x 维护默认数据库的 `aliases.json`，且每次保存都剔除目标路径已不存在的别名（[0007](0007-m2-storage-engine.md#取锁顺序)）。
- `schema.json` 按结构体字段与属性插入顺序写出，与 3.x 的 `sort_keys` 不同：同样的属性，3.x 重开后按键序返回，本实现重开后按插入顺序返回；这是 0.3.0rc3 已有、未登记的差异，由 A 改为排序写出修正（读取仍按文件顺序，与 3.x 相同）。
- 在没有 `schema.json` 的残留目录上 CreateCollection 报 1/1「already exists」；3.x 报 1/0，文案为带绝对路径的 `[Errno 17] File exists`。
- 打开集合时要求 `schema.json` 的名字与目录相同，否则 fail-closed（[recovery.rs](../../crates/core/src/storage/recovery.rs)）。
- CreateCollection 非法名等 `ValueError` 类错误已是 1/0，与 3.x 一致。

## 必须保留的行为

1. **每个集合 RPC 先解析别名，再按名字查找。** 别名指向的总是真实集合名，解析只做一层；经别名的 Drop、Rename、Truncate、AlterCollection 作用于目标集合；错误文案使用目标名；DescribeCollection 的 `aliases` 恒为空列表；ShowCollections 不展开别名。
2. **错误码、文案与判定顺序沿用上表**，包括 1/0 与 1/1 的区别、HasCollection / ShowCollections 在数据库不存在时的 1/0、CreateDatabase / DropDatabase 的路由例外与空名归一。实施时逐行做 raw gRPC 差分。
3. **改名后的集合按重新打开的状态返回**（有索引定义即 released）。清空保持原 load 状态、分区、索引定义与别名，并使 auto_id 从 1 重新分配。
4. **集合属性**：值按字符串保存；除 `timezone`（取舍 1）外的键不产生任何效果，也不拒绝；同一进程内按插入顺序返回，从磁盘读回后按文件中的顺序返回。
5. **名字与文件系统**：沿用 [0006 第 18 项](0006-m2-scope.md) 的精确名字规则。新建集合、新建数据库、改名的新名遇到已存在的路径（包括只差大小写或只差 Unicode 规范化的名字）报「已存在」，与 3.x 一致；新建别名与带 `schema.json` 的已有路径冲突时报「与集合冲突」，与 3.x 一致，没有 `schema.json` 的残留目录不构成冲突。按只差大小写的名字访问或删除已有集合、数据库，以及别名的目标存在判定，都按精确名字且带 `schema.json`，只差大小写时报「不存在」，与 3.x 不同；残留目录上的新建集合与改名另见差异。
6. **悬空别名**：别名 RPC 的读写保留悬空别名，与 3.x 相同；只有 create / drop 沿用 0007 的剔除规则。
7. **互通**：本实现写出的 `aliases.json`、`database_properties.json`、`schema.json` 与清空后的 manifest，按 3.x 的 `json.dump` 参数输出（递归键排序，嵌套对象同样排序；`indent=2`、`ensure_ascii=False`），3.x 打开后结果相同；反之亦然。

## 建议登记的差异

### 持久化保证与失败语义（只比 3.x 更严格，成功路径不变）

| 边界 | 3.x | 本实现 |
| --- | --- | --- |
| `aliases.json`、`database_properties.json`、`schema.json` 的写入 | 都不 fsync 目录，`aliases.json` 也不 fsync 文件 | 写 tmp、fsync 文件、改名、fsync 目录后才返回成功；tmp 改名之前失败则状态不变，改名之后目录同步失败按 0007 进入写入封锁 |
| 清空的崩溃窗口 | 可能留下残缺集合，或集合消失且名字不可再用 | 原地清空，以一次 manifest 原子替换为提交点，配合清空意图；本实现重开后只有「未清空」或「已清空」两种结果 |
| 改名的崩溃窗口 | 名字可能与目录不一致，别名可能仍指向旧名 | 配合改名意图；除 3.x 介入或需人工处理的情形外，本实现重开后只有「改名前」或「改名后」两种结果，别名与 `schema.json` 随之一致 |
| Drop 空数据库 | `rmtree` 中途退出会留下半个目录 | 改名移入 `.database-staging/` 并 fsync 后再删除；打开时按 3.x 规则清空 staging |

### 有意的请求语义差异

| 输入 | 3.x | 本实现 | 理由与影响 |
| --- | --- | --- | --- |
| RenameCollection 的 `newDBName` 非空且不等于当前数据库（按归一化后的精确名比较；pymilvus `target_db`） | 忽略该字段（目标库不存在也忽略），在当前数据库内改名并返回成功 | 在「当前数据库存在」之后、「旧名存在」判定与任何目录变更之前返回 UNIMPLEMENTED，写明不支持跨数据库改名；为空或等于当前数据库时照常改名 | 用户显式请求了另一个目标库，静默忽略会得到成功响应，却改了当前库里的同名集合；跨库改名未实现，按不变量 15 显式失败。参照用例与上游子集不覆盖这一输入；验收用 raw gRPC 与 pymilvus `target_db` 的预期差异用例（取舍 4） |
| 按只差大小写或 Unicode 规范化的名字访问、描述或删除已有集合与数据库；3.x 写出的、指向只差大小写名字的别名 | 按路径命中，例如 DescribeDatabase `D1` 成功、DropDatabase `D1` 删除 `d1`；这类别名照常可用 | 报「不存在」 | 沿用并扩展 [0006 第 18 项](0006-m2-scope.md)（只按精确名字访问），避免另一个名字删掉真实目录；只影响大小写不敏感或做规范化的文件系统 |
| 改名的新名或新建集合的名字是没有 `schema.json` 的残留目录 | 改名：空目录被静默覆盖并成功，非空目录报 1/0；新建集合：1/0，文案带绝对路径 | 一律报 1/1「已存在」，不覆盖任何目录 | 静默覆盖会丢掉目录中可能存在的内容；文案不暴露绝对路径。残留目录来自 3.x 清空或 drop 中途退出 |
| 范围外 schema 的集合（0006 第 14 项 fail-closed）上的清空与改属性 | 能成功 | 返回 UNIMPLEMENTED，零写入；别名的创建、改指向、删除照常；改名照常（只改原始 JSON 的 `collection_name`），但 `schema.json` 不是对象或 `collection_name` 不是字符串时在任何变更之前报错 | 清空需要完整恢复集合，改属性要按本实现理解的内容判定 `timezone` 等属性，都可能误解本实现不认识的内容 |

### 范围延后

| 输入 | 3.x | 本实现 |
| --- | --- | --- |
| 写入 `timezone` 属性 | 校验后保存 | 返回 UNIMPLEMENTED，零写入；判定顺序见取舍 1；随 TIMESTAMPTZ 切片解除 |

### 已有差异的范围变化

- `aliases.json` 无法解析时，本实现所有需要解析别名的 RPC 失败、不写入任何文件，与 3.x 同为失败；错误沿用现有存储错误映射，实施时与 3.x 的 1/0 差分，不同则登记。
- create / drop 剔除悬空别名（0007 既有差异）不扩展到别名 RPC；本实现自身的改名不再留下悬空别名。

## 文件与提交协议（方向，细节随 A / B 的实施设计评审）

### 作用域与锁

- **作用域**：A 虽只对外开放默认数据库，别名表、集合路径、名字锁、意图与提交结果从一开始按「数据库名 + 集合名」建模，不写死数据根目录（取舍 5）。
- **入口**：所有新操作都经 Db 入口与关闭闸门。写请求（插入、删除、Upsert、Flush、索引与分区操作、别名写入、AlterCollection、清空）**先取得数据库写入闸门的共享许可，再解析别名**；读请求在别名叶子锁下取一次解析结果。改名、create、drop 持独占许可，因此别名写入不会与之交错：CreateAlias 与同名 CreateCollection、CreateAlias 与 drop 目标不会同时成功。
- **别名叶子锁**按数据库各一把，保护该库 `aliases.json` 的完整读改写；不在持有它时取其他锁。
- **改名**持独占许可与目录变更锁，并按精确名字的字节序依次取得新旧两个名字锁；[lock_order](../../crates/core/src/lock_order.rs) 目前禁止同层嵌套，须增加「同层按名字字节序」的规则与检查。
- **不等待维护**：合并任务全程持有数据库写入闸门的共享许可（[db.rs](../../crates/core/src/db.rs) 的合并入口），parking_lot 的读写锁有独占等待者时新的共享请求也会阻塞。因此**持数据库写入闸门期间不得等待需要该闸门的维护任务**。改名与清空都不排空维护队列，而是在 flush 锁与状态写锁下设置标记（改名为「已移除」，同 drop；清空为「清空代」递增），合并在提交边界（重新取 flush 锁之后）看到标记即放弃，不提交、不删除任何输入或已提交文件，自己的临时输出照常清理。
- **清空**持共享许可，取 flush 锁与状态写锁并设置标记后执行；进行中的合并在提交边界放弃。从第 1 步到第 3 步发布新状态之前，状态写锁一直持有，内部 flush 也不为锁外阶段放开它，因而期间没有任何写入进入新的 WAL；代价是同一集合的读请求在这段时间内等待，接受。
- **AlterCollection** 持共享许可、目标集合的 flush 锁（串行化 schema 与 manifest 写入）与状态写锁（发布新属性）。
- 锁序沿用 [0007](0007-m2-storage-engine.md#取锁顺序)，debug 锁序检查覆盖新增的锁与同层规则。

### 别名

- 读改写在该库的别名叶子锁内完成，按「持久化保证」一表写入；写入失败不改变内存中的映射。别名 RPC 保存时不剔除悬空别名。
- 目标存在按精确名字且带 `schema.json` 判定；「与集合冲突」按 3.x 的路径加 `schema.json` 判定，因此只差大小写的已有集合构成冲突、没有 `schema.json` 的残留目录不构成冲突，与 3.x 一致。0007 按路径判定悬空的规则只用于 create / drop 的剔除。

### 改名

1. 持独占许可、目录变更锁与两个名字锁。按「改名」一表的判定顺序检查；`newDBName` 按取舍 4 在旧名存在判定之前处理；新名路径存在（含残留目录）即报「已存在」。读取旧目录的 `schema.json`，它不是 JSON 对象或 `collection_name` 不是字符串时，在任何变更之前报错。
2. 像 drop 一样隔离旧句柄：在 flush 锁与状态写锁下标记已移除，fsync 全部 WAL 文件与 `wal/` 目录后关闭（同步失败则撤销标记并报错，不做任何变更，与 drop 相同）；不等待维护队列，之后的维护任务在提交边界看到标记即退出，因而不能再在旧路径重建目录。从未打开或 fail-closed 的集合没有句柄，直接按目录处理。
3. 在数据库根目录写入并 fsync 改名意图（私有文件，记录旧名与新名；tmp、文件同步、改名、目录同步）。
4. 在同一 `collections/` 下改名并 fsync 该目录，再按 [0006 第 17、18 项](0006-m2-scope.md) 核对精确名字；核对失败则改回旧名并 fsync，删除意图后报错。
5. 在新目录中只改原始 JSON 的 `collection_name` 为新名，其余键与值原样保留，按 3.x 的参数排序写出，经 tmp、文件同步、改名、目录同步替换 `schema.json`。
6. 把指向旧名的别名改指新名并写入 `aliases.json`。
7. 删除改名意图并 fsync 数据库根目录；从 collection 表与失败判定缓存中清除新旧两个名字的条目。新名在第一次访问时按目录重新打开。
- **第 5–7 步失败**：当次 RPC 报「已改名、元数据维护未完成」类错误，保留意图；该数据库进入「改名未完成」状态，拒绝 create、drop、改名与别名写入（这些操作会剔除或改写别名），直到当前实例重试成功或重开处理完毕；其他读写照常。第 4 步目录同步失败按 0007 进入写入封锁，意图保留。
- **重开时**（取得 LOCK、清空 staging 之后，处理任何请求之前）逐个处理各数据库根目录的改名意图。先 fsync 该库的 `collections/` 目录，再按精确名字列表并以带 `schema.json` 为准判定：
  - 新名存在、旧名不存在：若 `schema.json` 的名字仍是旧名则按第 5 步改为新名，再把仍指向旧名的别名改指新名，最后删除意图；
  - 旧名存在、新名不存在（目录改名之前退出）：删除意图；
  - 两者都存在：说明其后被其他写入者改动，删除意图、不改别名与 schema；
  - 精确名字都不存在，但新名或旧名的路径存在（例如在会规范化名字的文件系统上改名后、改回之前退出）：保留意图，该数据库进入「改名未完成」状态并给出诊断，由人工处理；
  - 路径也都不存在：其后被其他写入者删除或再次改名，删除意图、不改别名；
  - 处理中遇到 I/O 错误：保留意图并进入「改名未完成」状态，下一次受限操作先重试；
  - 意图损坏或版本未知：不自动处理，进入「改名未完成」状态并给出诊断，人工恢复方式随实施设计冻结，原则同 0014。

### 清空

清空要求集合已能完整打开（不是 fail-closed），且没有未处理的 0014 删除意图（否则按 0014 的封锁报错）。

1. 取 flush 锁与状态写锁，递增清空代，使进行中的合并在提交边界放弃。先 flush，并确认其 manifest 提交与目录同步均已完成。关闭当前 WAL 写入句柄，删除 `wal/` 下的**全部** WAL 文件（包括打开时继承、已落盘但尚未删除的旧代），同步 `wal/` 目录并核对其为空。原因：两边恢复都重放 `wal/` 下的全部文件、不看 `current_seq`（3.x [recovery.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/milvus_lite/engine/recovery.py)；本实现 `current_seq` 归零后可见性过滤为空），任何残留 WAL 都会在清空后作为新数据重放，旧 delete 的较大 `_seq` 还会删掉清空后从 1 开始的新行。这一步不改变「操作前」状态：全部行已在 manifest 引用的文件中。失败则报错、集合保持操作前状态。
2. 准备候选 manifest：`version` 为旧值加 1，`current_seq` 为 0，`active_wal_number` 为空，保留分区列表、索引定义与 `schema_version`，各分区文件列表为空。写入并 fsync 集合根目录的清空意图（私有文件；tmp、文件同步、改名、目录同步，做法同 [0014 的删除意图](0014-index-definitions-and-filter-expressions.md#实施设计)），记录旧 manifest 与候选 manifest 的内容摘要（集合从未 flush、没有 `manifest.json` 时，旧摘要记为「不存在」）、待删 snapshot 的名字与 id，以及待删的旧文件（旧 manifest 与这些 snapshot 引用的 data / delta 与索引侧车）。第 3 步替换之前在进程内失败时删除意图并报错，集合保持操作前状态；删除意图失败也无妨，重开时按旧摘要放弃。
3. 以一次 manifest 原子替换为提交点，沿用 [fs_commit.rs](../../crates/core/src/storage/fs_commit.rs) 的阶段语义：改名成功即已替换，此后的目录同步失败走现有的重新提交与写入封锁路径。替换成功后立即在内存中发布清空后的状态：按原 load 状态发布空快照，并重置一切带旧 `_seq` 的内存状态——独立已提交删除表（0011）、DropPartition 保留的全局 delta（0017）、合并清理记录、可见性位图，清空数据源表与待提交代。SourceId 计数与 WAL 代编号在进程内继续单调递增、不回到 1，`last_published_generation` 等已提交边界随之清除，避免按旧边界删除清空后新写入的 WAL；重开后由恢复按实际 WAL 重新计算编号。
4. **只有 manifest 替换与集合目录同步都已确认后才开始清理**；目录同步失败时保留意图、不删除任何文件。删除是幂等的，文件已不存在视为已删除（3.x 介入时可能已按孤儿清理删掉）；snapshot 必须名字与 id 都与意图一致才删除。清理顺序：逐个删除意图所列 snapshot 的元数据文件并同步其目录 → 删除这些 snapshot 的 manifest 副本 → 删除意图所列、且不被当前 manifest 与剩余 snapshot 引用的旧文件，每类删除后同步所在目录 → 用新 manifest 原子重写 `manifest.json.prev`（否则 3.x 在 manifest 解析失败时会回退到指向已删文件的旧版）→ 删除意图并同步集合根目录 → 恢复后台维护。清理阶段不触碰 `wal/`。
5. 清理失败：当次 RPC 报「已清空、清理未完成」类错误，内存保持清空后的状态；该集合标记为清理未完成，在清理完成前拒绝写入、Flush、索引与分区变更等会改写 manifest 的操作，并暂停其后台维护；下一次这类请求先重试清理，重开时按意图补做。
6. **重开时**，在读取 `schema.json` 与 manifest 之后、扫描与重放 WAL 和孤儿清理之前处理清空意图：
   - 当前 manifest 摘要等于候选：先 fsync `manifest.json` 与集合根目录（进程崩溃时改名可能仍只在页缓存中，断电会回滚），成功后才执行第 4 步的清理；同步失败则本次打开失败、不删除任何文件、不缓存判定。清理只删意图所列 snapshot，以及不被当前 manifest 与剩余 snapshot 引用的所列旧文件；完成后删除意图，并重新读取 snapshot 的钉住集合，再进入 WAL 重放与孤儿清理。此时 `wal/` 若有文件，说明提交后有其他写入者（例如 3.x 写入后被强杀），保留并照常重放，不得删除。清理遇到 I/O 错误时本次打开失败、不缓存判定，下次访问重试。
   - 等于旧 manifest（含旧摘要为「不存在」且当前也没有 manifest）：提交前退出或提交前失败，删除意图，不删除任何其他文件；集合处于操作前状态，`wal/` 中若有之后写入的 WAL 照常重放。
   - 两者都不等：manifest 已被其他写入者改写，无法判断提交是否发生，删除意图、不删除任何其他文件。
   - 意图损坏、版本未知或字段不合法：该集合 fail-closed，不删除任何文件；人工恢复方式随实施设计冻结，原则同 0014。
- **`_seq` 归零的依据**：提交后，新 manifest 与内存中都不再有任何带旧 `_seq` 的可达数据，旧 WAL 已在提交前删除，旧文件只被意图与 snapshot 引用、不参与可见性；因此不变量 1–5（`_seq` 全序、MemTable 互清、tombstone GC、文件不可变、manifest 唯一真值）对清空后的集合重新成立。新文件名中的序号区间可能与残留旧文件重复，flush 与合并的输出命名继续避开既有文件与钉住的侧车（[flush.rs](../../crates/core/src/storage/flush.rs)）。
- **与 0014 删除意图共存**：两种意图文件名不同。清空前要求没有删除意图，清空期间索引变更被独占锁排除，因此正常运行中两者不会同时存在；重开时若同时出现，该集合 fail-closed。
- **意图是新的私有持久化文件**（清空意图在集合根目录，改名意图在数据库根目录），按全局规则 8 冻结 `format_version` 并说明兼容性；3.x 不读取、也不删除这两个位置的陌生文件。
- **回退到旧版本**（0.3.0rc3 及更早）：旧版不认识两种意图。正常完成的改名与清空不留下意图，`schema.json` 名字与目录一致，旧版照常打开；只有意图未处理完时回退才有影响——改名意图未处理时，旧版的 create 可能剔除仍指向旧名的别名；清空意图未处理时，旧版的写入会改写 manifest，新版重开时放弃该意图，旧 snapshot 残留。回退前应以新版正常打开并关闭一次数据目录，使意图处理完毕；这一要求写入发行说明。
- **残余（只在 3.x 介入时出现）**：提交后、清理完成前退出，若随后先由 3.x 打开，3.x 仍能看到并恢复旧 snapshot；它的新写入可能覆盖钉住的旧文件（文件名按 `_seq` 区间命名，归零后会重用），或把钉住的旧索引侧车当作新 segment 的索引加载，直到本实现重开完成清理。3.x 在这期间改写过 manifest，本实现就放弃该意图，残留 snapshot 由 3.x 自行维护。

### AlterCollection

- 要求集合不是 fail-closed；先判集合存在，再判 `timezone`（取舍 1），再合并属性。
- 在原始 JSON 上只改 `properties` 与 `collection_name`（写目录名），其余键与值原样保留，不经内存 schema 重新生成整个文件；按 3.x 的参数递归排序写出，按「持久化保证」一表写入。tmp 改名之前失败则属性不变；改名之后目录同步失败按 0007 进入写入封锁。
- 内存中新键追加、已有键原位替换，与 3.x 进程内顺序相同；从磁盘读回后按文件中的顺序。

### 数据库（B）

- Create 在 staging 中建好后改名移入并 fsync `databases/`，改名后核对精确名字；Drop 在独占许可下再次确认非空，改名移入 staging 并 fsync 后删除。默认数据库不可删除，它的属性文件在数据根目录。
- 数据库生命周期与 0006 第 17 项的 staging 规则、各库的别名叶子锁与改名意图一起冻结在 B 的实施设计中。
- 在 B 之前，A 对请求的非默认数据库：`databases/<db>` 已存在时返回 UNIMPLEMENTED（落实 0006 第 15 项），不存在时保持现有的「数据库不存在」。

## 已确认的取舍

2026-10-29 负责人确认：

1. **`timezone` 延后到 TIMESTAMPTZ 切片。** 请求中只要出现 `timezone` 键（新增、修改或设为与现值相同，不论值是否合法），整个请求返回 UNIMPLEMENTED 且零写入；与普通属性混在一个请求里时也不先写普通属性。判定位置按 RPC 固定：
   - CreateCollection：保持现有位置，即 schema 校验中「schema has no fields」之后、字段校验与「已存在」之前（3.x 的 `validate_schema` 同样先于已存在判定；现有用例见 [crates/core/tests/db.rs](../../crates/core/tests/db.rs)）；
   - CreateDatabase：名字校验与「已存在」之后，对应 3.x 的属性校验位置；
   - AlterCollection、AlterDatabase：先判对象存在，再判 `timezone`。

   删除 `timezone` 键不受影响。数据库属性中 3.x 已写入的 `timezone` 可以读取、展示与删除；集合 schema 中已有 `timezone` 的集合仍是 0006 第 14 项的范围外 schema（Describe、Drop、别名与改名可用，清空与改属性按差异表返回 UNIMPLEMENTED）。受影响的 6 条数据库参照用例（3 条直接设置 `timezone`，2 条使用带 `timezone` 的哨兵数据库，1 条重复创建用例）标记为「由 TIMESTAMPTZ 切片解除」，不算 B 的遗漏。理由：编译期时区数据库只能得到近似 3.x 的名字集合，而 3.x 的集合取决于平台 `zoneinfo`；TIMESTAMPTZ 正式实现时还要决定时区与夏令时规则的来源，现在做半套会提前固定数据来源。
2. **清空时的 snapshot：提交后清理，加可恢复的清空意图。** 不在 manifest 提交前删除 snapshot：那会出现「旧数据完整、集合尚未清空，snapshot 却已部分或全部消失」的第三种状态。snapshot 的恢复不会自动发生，真正的风险是清空成功后旧 snapshot 仍可被恢复、继续钉住旧文件，由提交后清理与重开时按意图补做来消除。协议见「清空」。
3. **放宽 `schema.json` 名字与目录不同。** 只放宽「合法但不相等」这一种情况：仍要求 JSON 可解析、字段与类型合法、`collection_name` 存在且为字符串；打开后一律以目录名为准，DescribeCollection 返回目录名。读取时不修复文件；改名时只改原始 JSON 的 `collection_name`，AlterCollection 重写时写入目录名，重开处理改名意图时补写。其他损坏仍 fail-closed。同步修改 [recovery.rs](../../crates/core/src/storage/recovery.rs) 的打开校验与 [db.rs](../../crates/core/src/db.rs) 中未打开集合的 load 状态判定。
4. **拒绝跨数据库改名。** 见「有意的请求语义差异」。
5. **A 先于 B，A 的内部接口带数据库作用域。** A 直接收回 alias、rename 与上游子集的主要缺口，只改动默认数据库内的集合命名空间；B 让所有 RPC 经过数据库路由，把目录发现、存在性判定与隔离错误扩散到整个入口面，风险更高，单独评审。

## 验收方向

- **参照用例**：范围内的 51 条按同一 760 条清单重跑并逐条核对；其中 7 条预期仍不通过（6 条按取舍 1，1 条 AlterDatabase 用例属于 TIMESTAMPTZ）。上游子集与探针同样重跑。不以通过数代替逐项差分。
- **差分**：「参照实测」每一行在 pymilvus 2.6.17 与 3.0.2 两条线对 3.x 做 raw gRPC 与 MilvusClient 差分，包括 `code` / `error_code`、文案、判定顺序、load 状态、属性顺序（进程内与重开后）。
- **预期差异**：`newDBName`、只差大小写的访问与删除、残留目录上的新建集合与改名、fail-closed 集合上的清空与改属性、`timezone` 写入，各有 raw gRPC 与 pymilvus 用例，断言错误、零写入与判定位置。
- **互通**：3.x 与本实现交替执行别名、改名、清空、属性修改与数据库操作后互相打开，比较可见集合、别名、属性（含顺序）、数据、load 状态与 auto_id；包括 3.x 改名或清空中途退出留下的目录，以及本实现意图未完成时由 3.x 打开的情况。
- **故障**：每个提交点前后注入进程退出、I/O 错误与 panic，重开后只能处于「操作前」或「操作后」之一。清空覆盖：WAL 删除中途、意图写入前后、manifest 替换前后与目录同步失败、snapshot 删除中途、旧文件删除中途、`.prev` 重写前后、意图损坏、提交后由 3.x 写入（WAL 未落盘被强杀，或已 flush 改写 manifest）再由本实现重开、打开时继承旧 WAL 的集合再清空。改名覆盖：意图写入前后、目录改名前后与目录同步失败、精确名字核对失败与改回、`schema.json` 改名前后、别名写入前后、第 5–7 步失败后的「改名未完成」状态与以旧名 create 的拒绝、意图损坏、意图未处理时由 3.x 打开并改动。清空另覆盖重开补做清理时 manifest 与目录同步失败。
- **并发**：别名写入与同名 CreateCollection、与 drop 目标；经别名的读写与 AlterAlias；改名与旧名 / 新名上的请求、与旧句柄的 flush 和合并；清空与读写、Flush 及后台合并；AlterCollection 与 flush；数据库 Drop 与其中集合的打开。debug 锁序检查覆盖新锁与同层规则。
- **文档同步**（实施时）：
  - [0002](0002-data-dir-compat.md)：别名、数据库属性与 `schema.json` 的写入保证；共享文件的排序写出；两种私有意图文件加入约束 1 的例外清单。
  - [0006](0006-m2-scope.md)：第 14 项中「别名命中仍 UNIMPLEMENTED」一句、第 15 项、第 17 项关于残留目录的表述，以及差异表中涉及别名 RPC、已有别名与其他数据库、只差大小写的各行。
  - [0007](0007-m2-storage-engine.md)：目录变更锁的持有者（新增改名与数据库生命周期）、按数据库的别名叶子锁、同层名字锁规则、悬空别名剔除只在 create / drop。
  - [0011](0011-committed-delete-state.md)：清空重置独立已提交删除表；[0014](0014-index-definitions-and-filter-expressions.md)：清空与删除意图的互斥。
  - README 的支持范围、已知差异与回退前须正常打开一次的要求、AGENTS 的知识索引与架构地图（新增 module）、[tests/python/adapter/INVENTORY.md](../../tests/python/adapter/INVENTORY.md)。


## A 实施设计（终稿）

本节把合同「文件与提交协议」落到现有代码上，冻结 A 的模块划分、锁、两种意图的格式、清空与改名的阶段、失败语义和测试矩阵。首轮评审的六项阻断（清空后读视图未重建、改名提交前失败未恢复可写代、清空删除 WAL 中途失败、意图格式粒度不足、JSON 写出未全覆盖且数字规则有误、改名跨提交点的状态未闭合）第二轮的两项阻断（逐个删除 WAL 留下的已提交后缀会被 3.x 重复回放、snapshot 侧车校验拒绝合法的历史 snapshot）第三轮的两项阻断（没有 `wal/` 的集合清空被误判为失败、清空未重写旧版未排序的 `schema.json`）与第四轮的一项阻断（清空中 `schema.json` 目录同步失败未按合同进入整库写入封锁）已在下文对应位置处理。评审通过后才动代码；B 的设计在 A 收口后另写。

### 对合同方向的细化

| 合同中的方向 | 本设计 | 理由 |
| --- | --- | --- |
| 别名写入「先取共享许可再解析别名」 | 别名写入（CreateAlias / AlterAlias / DropAlias）与 AlterCollection 持**独占许可**；集合数据写入仍持共享许可 | 合同要求这两类元数据写入在目录同步失败时按 0007 进入整库写入封锁，现有封锁标志只能在独占许可下设置；独占也从结构上排除了与 create / drop / 改名的交错。两者都是低频操作，代价是等待在途写入与合并结束 |
| 清空意图记录两份 manifest 的「内容摘要」 | 意图保存旧 manifest 的完整内容与候选 manifest 的完整字节，按字节相等判定 | 不新增哈希依赖、不扩大 md5 的用途；字节相等是无碰撞的身份判定。候选只含空分区；旧 manifest 体积与文件数成正比，清空是低频操作 |
| 清空「持共享许可」，合并在提交边界按「清空代」放弃 | 清空全程持**独占许可**，不设清空代；清理失败后的重试走下述共享到独占的重新入闸 | 清空第 3 步（WAL 目录迁移）与第 6 步（`schema.json` 规范重写）在改名之后目录同步失败时，按持久化合同都要进入整库写入封锁，只能在独占许可下设置。合并全程持共享许可，因此清空期间不会有合并在运行，清空代没有作用，按「能减不加」去掉。代价是清空期间整个数据库的写入，以及其他集合未缓存时的首次懒打开都等待；已经打开集合的纯读不受影响。清空是低频操作，3.x 的清空同样完全串行 |
| 清空第 1 步「删除 `wal/` 下全部 WAL 文件」 | 在 flush 确认之后把**整个 `wal/` 目录原子改名移入 `.database-staging/`**，同步目录，需要时再建立新的空 `wal/` | 不能逐个删除：中途失败会留下已提交行的 WAL 后缀，3.x 打开时会重复回放（实测 3.2.1：已提交的 2 行在残留 WAL 重放后计数为 4、主键为 `1, 2, 1, 2`），本实现虽会过滤，但互通失败。目录级改名只有「仍在原处」与「已移入 staging」两种状态，两边打开时都会清空 staging |
| 改名第 5–7 步失败「其他读写照常」 | 只在目标文件替换**之前**失败时成立；替换之后的目录同步失败按持久化合同进入整库写入封锁 | 合同的持久化保证一表要求所有共享文件的改名后目录同步失败进入封锁，两处必须一致，以持久化保证为准 |

### 模块与数据结构

- **作用域键**：新增 `CollectionKey { database, name }`，`database` 在入口归一化（空串为 `default`）。`Db` 的 collection 表、名字锁注册表、失败判定缓存都改按 `CollectionKey` 索引；`collection_path(key)` 对默认库返回 `<root>/collections/<name>`，非默认库的分支在 B 接通。
- **数据库状态**：`Db` 新增 `databases: BTreeMap<String, Arc<DatabaseState>>`，A 中只有 `default`。`DatabaseState` 持有：
  - 别名表：叶子锁（层级 6）保护的 `Option<BTreeMap<String, String>>`，首次使用时从 `aliases.json` 加载；读或解析失败不缓存，下次重试。数据目录由 LOCK 独占，只有本进程写这个文件，缓存与磁盘一致；写入的文件替换成功后才更新缓存。
  - 改名未完成状态：`Option<RenameIntent>`，见「改名」。
- **新模块**：
  - `alias`：别名表的加载、解析、写入与判定（取代 db.rs 中现有的 `read_aliases*` / `save_aliases*`）；
  - `storage::rename_intent`、`storage::truncate_intent`：两种意图的读写、校验与故障点，结构仿照 [index_intent](../../crates/core/src/storage/index_intent.rs)；
  - `pyjson::dump_file`：与 Python `json.dump(obj, indent=2, sort_keys=True, ensure_ascii=False)` 逐字节相同的输出。键按码点递归排序；数字沿用 [pyjson.rs](../../crates/core/src/pyjson.rs) 现有的 Python 导入导出语义——整数保留精确十进制值，浮点按 Python `repr` 重新格式化（`1E2` 写为 `100.0`，整数 `-0` 写为 `0`），与 Python `json.load` 后再 `json.dump` 的结果相同。唯一例外：溢出为 inf 的数字保留严格 JSON 文本（如 `1e+999`），不写 `Infinity`，见 [0006 差异表](0006-m2-scope.md#与-3x-的差异)。
- **全部共享 JSON 文件改用 `dump_file`**：[schema_file.rs](../../crates/core/src/storage/schema_file.rs) 的 `to_bytes`（CreateCollection、AlterCollection、改名）、`aliases.json`、[manifest.rs](../../crates/core/src/storage/manifest.rs) 的 `to_bytes`（flush、合并、索引与分区变更、清空的当前 manifest，以及清空重写的 `manifest.json.prev`）。3.x 的 manifest 与 snapshot 同样用 `sort_keys=True` 写出。读取不变（保留文件顺序）。
- **名字检查放宽**（取舍 3）：[recovery.rs](../../crates/core/src/storage/recovery.rs) 的 `load_committed` 与 [db.rs](../../crates/core/src/db.rs) 的 `unopened_load_state` 删除名字相等检查；`collection_name` 缺失或不是字符串仍由 `SchemaFile::read` 拒绝。集合名一律取目录名。
- **已有非默认库**：`require_database` 改为 `Db` 的方法：非默认库且 `databases/<db>` 是目录时返回 Unsupported（UNIMPLEMENTED，写明数据库尚未支持），否则保持 DatabaseNotFound。
- **错误**：`EngineError` 新增 `AliasAlreadyExists`（1/1，`alias 'a1' already exists`）、`AliasConflictsWithCollection`（1/1，`alias 'c2' conflicts with an existing collection`）、`AliasNotFound`（100/4，`alias 'zz' does not exist`），名字按现有 `py_repr` 格式化。

### 锁

| 操作 | 取得的锁（按层级顺序） |
| --- | --- |
| 读请求（Describe、Query、Search、统计、load 状态等） | 别名叶子锁内取一次解析结果 → 名字锁（仅首次打开）→ 状态读锁 |
| 集合数据写入（插入、删除、Upsert、Flush、索引与分区操作） | 共享许可 → 解析别名 → 现有链路 |
| 别名写入 | 独占许可 → 别名叶子锁（读改写全程） |
| AlterCollection | 独占许可 → 名字锁 → flush 锁 → 状态写锁 |
| RenameCollection | 独占许可 → 目录变更锁 → 两个名字锁（按精确名字字节序）→ 旧句柄的 flush 锁 → 状态写锁（短暂）→ 别名叶子锁（短暂） |
| TruncateCollection | 独占许可（第 1–7 步全程）→ 解析别名 → 名字锁 → flush 锁（第 1–7 步全程）→ 状态写锁（第 1–5 步发布新读视图之前全程） |

- **同层名字锁**：新增 `NameLease::lock_pair(a, b)`，内部按字节序先后加锁；[lock_order](../../crates/core/src/lock_order.rs) 增加一种只供该函数使用的同层登记方式，debug 构建断言同层至多两把且后者名字更大。其他同层嵌套仍报错。
- **不等待维护**：合并全程持共享许可，独占许可的取得自然排在进行中的合并之后；任何操作都不在持有许可时等待维护队列。改名设置的「已移除」标记与现有 drop 相同，合并在提交边界的 `is_deleted` 检查处放弃。清空持独占许可，开始时没有进行中的合并，结束前也不会有新的合并开始；之后开始的合并读取的是清空后的 manifest。
- **清理重试重新入闸**：普通写请求只允许在共享许可下检查 `truncate_cleanup`，不得在共享许可下执行第 6 步。发现清理未完成时，必须在任何业务变更之前释放名字/状态守卫、flush 锁与共享许可；随后调用 `Db::retry_truncate_cleanup_exclusive(key)`，重新取得独占许可、名字锁与 flush 锁，重新确认意图后执行第 6 步。该函数确认清理成功后释放独占许可，原请求再从最初的写入入口完整重跑，不保留任何已经检查、计算或冻结但尚未提交的中间状态。清理中若出现 `schema.json` 的改名后目录同步失败，仍持独占许可设置整库写入封锁。禁止把共享许可升级为独占许可。

### 别名

- **解析**：`Db::resolve(database, name) -> CollectionKey` 在别名叶子锁内查表，命中返回目标名，否则返回原名。所有带集合名的入口在上表规定的位置调用一次；CreateCollection 不解析（以别名为名报已存在）。
- **CreateAlias**：按合同顺序判定：解析目标 → `validate_filesystem_name(alias)` → 目标按精确名字且带 `schema.json` 存在 → `collections/<alias>/schema.json` 路径存在即冲突 → 表中已有该别名。写入：新表 → `dump_file` → `aliases.json.tmp.<pid>.<n>`（`create_new`）→ fsync → 改名 → fsync 数据根目录 → 更新缓存。改名之前失败：不变。改名之后目录同步失败：更新缓存（与磁盘一致）、设置整库写入封锁、报「已修改、持久化未确认」，与 0007 相同。
- **AlterAlias**：别名存在 → 解析目标 → 目标存在；写入同上。**DropAlias**：不校验名字，不存在即成功，存在则写入同上。
- **DescribeAlias / ListAliases**：只读缓存；ListAliases 按集合时先解析，回填解析后的名字，结果按码点排序。
- **悬空别名**：别名 RPC 不剔除。现有 `save_aliases_inner` 中的剔除移到 create（改名之前，沿用 0007）与 drop 的调用点，作为显式步骤。

### AlterCollection

1. 独占许可下解析别名、取名字锁并打开集合；打开失败按原错误返回（范围外 schema 的 Unsupported 即合同要求的 UNIMPLEMENTED）。请求的 `properties` 中出现 `timezone` 键时返回 Unsupported，零写入。
2. 取 flush 锁与状态写锁。在内存副本上先删除 `delete_keys`、再设置 `properties`（新键追加、已有键原位替换），以新副本调用 `CollectionSchema::validate`。
3. 读取 `schema.json` 的原始 JSON，只替换 `properties` 对象与 `collection_name`（目录名），其余原样，以 `dump_file` 得到规范字节。**只有规范字节与现有文件字节完全相同时才跳过写入**；名字不同或只是键序不同（例如 0.3.0rc3 写出的未排序文件）也要写。写入经 tmp、fsync、改名、fsync 集合目录。
4. 成功后发布新属性。tmp 改名之前失败：不变，报错。改名之后目录同步失败：发布新属性、设置整库写入封锁并报错。

### 改名

`Db::rename_collection(database, old, new, new_db)`，分为五个阶段：

| 阶段 | 步骤 | 失败处理 | 失败后的状态 |
| --- | --- | --- | --- |
| P0 检查 | 独占许可；改名未完成时先重试补完（见 P4），仍失败则拒绝。归一化当前库并 `require_database`；`new_db` 非空且不等于当前库时返回 Unsupported。取目录变更锁；解析旧名，旧名按精确名字且带 `schema.json` 存在，否则 CollectionNotFound；`validate_filesystem_name(new)`；新名是别名或 `collections/<new>` 路径存在（含残留目录与只差大小写的名字）即 CollectionAlreadyExists；`lock_pair` 取两个名字锁；读取旧目录 `schema.json`，不是对象或 `collection_name` 不是字符串则报 Storage 错误 | 直接报错 | 操作前，无任何变更 |
| P1a 同步 WAL | 有句柄时在其 flush 锁与状态写锁下 `mark_deleted(true)`、`sync_retained_wal` | `sync_retained_wal` 失败（data、delta 文件或 `wal/` 目录同步）：只撤销已移除标记，WAL 写者保持原样，与 drop 相同 | 操作前，旧集合可继续读写 |
| P1b 意图与目录改名 | 写改名意图（tmp、fsync、改名、fsync 数据库根目录）；`rename(collections/old, collections/new)`；以目录列表核对 `new` 的精确名字 | 已写的意图删除；目录已改名而核对失败时先改回旧名并 fsync；在 `state_change` 内撤销已移除标记并 `freeze_active(true)`，使旧句柄进入编号严格更大的新代、恢复可写（WAL 写者已在 P1a 关闭，与 drop 目录改名失败后的处理相同）；代推进失败按 `state_change` 的现有处理使该集合不可用。改回失败则设置整库写入封锁、意图保留；只有删除意图失败时进入改名未完成状态 | 操作前，旧集合可继续读写；删除意图失败时该库受限、可重试，重开时按「旧名存在、新名不存在」删除意图 |
| P2 退役旧名 | 核对通过后**立即**从 collection 表移除旧句柄，清除新旧两个名字的失败判定缓存；然后 fsync `collections/` | 目录同步失败：设置整库写入封锁，报「已改名、目录持久化未确认」，意图保留 | 旧名报不存在；新名在封锁期间首次打开按现有规则报封锁错误；重开后按意图收尾 |
| P3 元数据 | 改写新目录 `schema.json` 的 `collection_name`（原始 JSON、`dump_file`、tmp、fsync、改名、fsync 集合目录）；别名叶子锁内把指向旧名的条目改指新名并写入 `aliases.json` | 文件替换**之前**失败：意图记入改名未完成状态，报「已改名、元数据维护未完成」。替换**之后**目录同步失败：设置整库写入封锁并报错，意图保留 | 前者：数据读写照常，create、drop、改名与别名写入先重试补完、仍失败则拒绝；后者：整库写入封锁，重开后按意图收尾 |
| P4 删除意图 | 删除意图并 fsync 数据库根目录 | 记入改名未完成状态并报错 | 数据读写照常；下次受限操作先重试删除；断电后意图若重现，重开时按「新名存在、旧名不存在」幂等收尾 |

- **补完的可重入性**：P3、P4 的每一步先判断是否已完成再执行：`schema.json` 的字节等于按新名得到的规范字节；`aliases.json` 存在、没有指向旧名的条目且字节等于当前表的规范输出（3.x 改名总会写出该文件，没有别名时为 `{}`）；意图已不存在。
- **启动处理**：`Db::new` 在 LOCK 与 staging 清理之后、开始服务之前，对默认库执行合同中的重开判定（先 fsync `collections/`，再按精确名字列表且带 `schema.json` 判定）。处理失败或需人工处理时不让启动失败，而是把该库置为改名未完成状态并在日志中给出诊断。
- **意图格式**（`format_version` 1，`deny_unknown_fields`）：`{"format_version": 1, "operation": "rename_collection", "old": <名字>, "new": <名字>}`。`operation` 必须为该字面值；`old`、`new` 都通过 `validate_filesystem_name` 且不相等；任何字段缺失、类型不符、取值不合法或有未知字段都视为损坏。
- **人工恢复**（意图损坏、版本未知，或精确名字都不存在而路径存在）：停止持有该数据目录的服务，核对 `collections/` 中的实际目录与 `aliases.json`，按需修正后删除数据库根目录的 `milvus-lite-rust.rename-intent.json`，再启动。只处理这个文件，不删除集合目录。

### 清空

`Db::truncate_collection(database, name)`：

1. 独占许可（写入封锁中则拒绝）；解析别名，取名字锁并打开集合（范围外 schema 返回 Unsupported）。存在未处理的 0014 删除意图时按 0014 的封锁报错。取 flush 锁与状态写锁。有上一次清空的清理未完成时先重试，仍失败则报错。
2. 在状态写锁内 flush：新增 `flush_locked`，复用现有冻结、写文件与提交的函数，只是磁盘阶段不放开状态锁；之后要求 `!needs_retry()`（manifest 提交与目录同步都已确认），否则报错、不做后续步骤。
3. 以 `symlink_metadata` 检查 `collections/<name>/wal`：
   - 不存在（从未写入、只建了 schema、上一次清空后尚未写入）：视为已经没有 WAL，跳过本步其余部分；
   - 是文件、符号链接或其他非目录类型：报错，零写入；
   - 是普通目录：要求当前活跃代没有 WAL 文件，且其编号严格大于 `wal/` 中所有规范 WAL 的编号（flush 之后全部 WAL 行都已在 manifest 中），否则报错；以 `create_dir_synced` 确认 `.database-staging/` 是普通目录，并确认目标 `.database-staging/truncate-wal-<pid>-<n>` 不存在（`symlink_metadata` 为 NotFound），否则报错、零写入。然后关闭 `data_wal` / `delta_wal`，把 `wal/` 整个改名为该目标，再同步集合目录与 staging 目录。空的 `wal/` 目录同样迁移。
   - 改名之前或改名失败：WAL 仍在原处（全部是已提交行，与操作前相同）；在 `state_change` 内 `freeze_active(true)`，使后续写入进入编号严格更大的新代，然后报错。
   - 改名成功而目录同步失败：设置整库写入封锁、`freeze_active(true)` 并报错，不继续提交。残余：断电可能使改名回退，已提交的 WAL 回到原处，3.x 打开时会重复回放这些行；这与本实现现有 flush「提交 manifest 之后、清理 WAL 之前」的崩溃窗口属于同一类，本实现自身恢复时会过滤。
   - 成功后 `wal/` 不存在；后续写入创建 WAL 时按需以 `create_dir_synced` 建立。两边打开时都会清空 staging，因此之后任何时刻退出都不会再回放这些 WAL；staging 中的目录在第 6 步清理时删除，删除失败只记日志。
   - 已迁移 WAL 而第 4、5 步在 manifest 替换之前失败（不再继续提交）时，尽力删除该 staging 目录并同步 staging 根目录，失败只记日志、留待下次打开清理，避免长时间运行的进程中多次失败累积磁盘占用。
4. 构造候选 manifest：复制当前 manifest，清空各分区的 data / delta 列表，`version` 加 1，`current_seq` 置 0，`active_wal_number` 置空，其余字段原样。写清空意图（见下文格式；tmp、fsync、改名、fsync 集合目录）。意图写入失败：删除残留 tmp，报错，状态为操作前。
5. `prepare_manifest` + `replace`。`BeforeReplace` 失败：删除意图、报错，状态为操作前。替换成功（含 `AfterReplace` 的目录同步失败）后**按重开的路径重建读视图**：
   - 调用 `recovery::load_committed(集合目录)`：此时 `schema.json` 仍是清空开始时的磁盘内容（规范重写在第 6 步），因此同一进程内的属性顺序与 3.x 相同——3.x 清空时从磁盘读回 schema 并以它重建集合，同一进程沿用读入的顺序；分区与索引定义的顺序来自候选 manifest，manifest 是候选，`wal/` 已移入 staging（缺失的 `wal/` 按无 WAL 处理），因此得到空的 `RecoveredCollection`（`next_seq` 为 1，auto_id 从 1 开始）；
   - 以 `Collection::from_recovered` 构造新的 `Collection`，再覆盖：load 状态取原值；`generation` 与 SourceId 计数取「原值递增」与恢复结果中的较大者；沿用原 `flush_lock` 与 `maintenance_status` 两个 `Arc`；
   - 在同一个状态写锁临界区内以 `*collection = rebuilt` 原地替换，句柄身份不变；读者此后看到空集合（count、get、query、search 均为空），进行中的读请求仍使用其已取得的不可变快照；
   - 重建失败（例如 `schema.json` 此刻无法读取）：使旧句柄不可用并从 collection 表移除，报 Storage 错误；下次访问按重开路径处理意图。
   - 目录同步失败时，新读视图仍发布，`retry_manifest = true`（沿用现有重试路径），意图保留，清理不开始。
   发布后释放状态写锁，继续持 flush 锁执行第 6 步。
6. 目录同步确认后清理（持独占许可与 flush 锁、不持状态写锁）：先以 `dump_file` 把 `schema.json` 重写为规范字节（`collection_name` 写目录名，其余字段与值不变；tmp、fsync、改名、fsync 集合目录；字节已相同时跳过写入，但仍 fsync 集合目录，保证上一次未确认的替换落盘），使磁盘与重开后的顺序与 3.x 清空后相同（3.x 清空总会重写 `schema.json`）→ 逐个删除名字与 id 都匹配的 snapshot 元数据文件并 fsync 其目录 → 删除这些 snapshot 的 manifest 副本目录 → 删除意图中待删且不被当前 manifest 与剩余 snapshot 引用的文件，每个目录删除后 fsync → 以候选字节原子重写 `manifest.json.prev` → 删除意图并 fsync 集合目录 → 按剩余 snapshot 重新读取 `pinned_files`（通常为空）→ 清除 `truncate_cleanup` 标记、恢复 `can_compact` 与后台维护 → 删除第 3 步移入 staging 的 WAL 目录（失败只记日志）。删除幂等（不存在视为已删除），不触碰新的 `wal/`。期间新读请求看到空集合；任何写入都在清空返回、释放独占许可之后才进行。
7. `schema.json` 规范重写在改名之后目录同步失败：按持久化合同设置整库写入封锁并报错，意图保留，重开时按意图补做（与 AlterCollection、改名一致）。其他清理失败（包括 `schema.json` 的 tmp 写入、文件同步与改名失败）：当次报「已清空、清理未完成」；`PersistentCollection::truncate_cleanup` 记录意图，`can_compact` 返回 false。之后的 Insert、Delete、Upsert、Flush、索引与分区操作在共享许可下只做“是否待清理”的检查；若待清理，立即释放共享许可并按上节“清理重试重新入闸”调用独占重试。独占重试成功后原请求从头重新执行；重试仍失败时原请求返回错误。`retry_manifest` 与清理重试仍保持提交顺序：先完成 manifest 重试，再执行第 6 步。
- **意图格式**（`format_version` 1，所有层级 `deny_unknown_fields`）：

  | 字段 | 内容与校验 |
  | --- | --- |
  | `format_version` | 整数 1 |
  | `operation` | 字面值 `truncate_collection` |
  | `old_manifest` | null（集合从未写出 `manifest.json`）或旧 `manifest.json` 的完整内容（字符串）；非 null 时必须能按 `Manifest::parse` 解析 |
  | `candidate_manifest` | 候选 manifest 的完整序列化字节（字符串）；必须能解析，且与由 `old_manifest`（null 时为默认 manifest）按第 4 步推导的结果在结构上完全相等：分区名集合相同且文件列表全空、`current_seq` 为 0、`active_wal_number` 为 null、`version` 为旧值加 1、索引定义与 `schema_version` 相同 |
  | `snapshots` | `[{name, id, files}]`：`name` 匹配 3.x 的 `^[A-Za-z0-9_.-]+$` 且不为 `.`、`..`；`id` 为 32 位小写十六进制（3.x `uuid4().hex`）；名字在意图内不重复；`files` 为该 snapshot 元数据引用的 data、delta 与索引侧车 |
  | `index_files` | 当前（旧）manifest 中 data 文件对应的索引侧车 |
  | 路径 | `snapshots[].files` 与 `index_files` 中每一项都必须是 `partitions/<合法分区名>/data/<data 文件名>`、`partitions/<合法分区名>/delta/<delta 文件名>` 或 `partitions/<合法分区名>/indexes/<侧车文件名>`：分区名用 [partition_paths](../../crates/core/src/storage/partition_paths.rs) 的 `valid_name`，文件名为单一路径段并分别符合 data、delta 的 `<kind>_<数字>_<数字>.parquet` 形态与 0014 的侧车解析（同 index_intent 的 `valid_sidecar_path`）；`index_files` 中侧车的 stem 必须对应旧 manifest 同一分区的某个 data 文件；`snapshots[].files` 中侧车的 stem 必须对应**同一 snapshot** 同一分区的某个 data 文件（snapshot 可以钉住已被合并移出 manifest 或所在分区已被 Drop 的历史文件，分区名只校验合法性，不要求仍在 manifest 中）；重复只在同一列表内禁止，不同 snapshot 之间、snapshot 与 `index_files` 之间可以引用同一路径 |

  待删文件集合不单独存储，由意图推导：旧 manifest 引用的 data / delta、`index_files` 与 `snapshots[].files` 的并集（去重），减去当前 manifest 与剩余 snapshot 引用的文件。任何校验失败即视为损坏：该集合 fail-closed，不删除任何文件。
- **重开处理**：`collection_inner` 在 `load_committed` 之前处理清空意图：读取并校验意图 → 读取 `schema.json`（必须可解析）与 `manifest.json` 的字节 → 等于 `candidate_manifest` 时先 fsync `manifest.json` 与集合目录，再执行第 6 步（含 `schema.json` 的规范重写，因此随后读回的顺序与 3.x 重开后相同），失败则本次打开报 Storage 错误、不缓存；否则删除意图、不做其他删除 → 继续现有的 `load_committed` 与孤儿清理（钉住集合按剩余 snapshot 重新读取）。同时存在 0014 删除意图与清空意图时 fail-closed。
- **人工恢复**（意图损坏或版本未知）：停止服务；若 `manifest.json` 已是空的候选状态，按需手工删除意图所列 snapshot；然后删除集合目录的 `milvus-lite-rust.truncate-intent.json` 并重启，孤儿文件由打开时的现有清理处理。

### gRPC

- [build.rs](../../crates/grpc/build.rs) 的 `IMPLEMENTED_METHODS` 加入 CreateAlias、AlterAlias、DropAlias、DescribeAlias、ListAliases、RenameCollection、TruncateCollection、AlterCollection；处理体只做解码、调用 core 与编码。
- 数据库沿用现有 `database()`（请求 `db_name` 优先，其次 `dbname` 元数据）。RenameCollection 的 `newDBName` 原样交给 core。AlterCollection 的属性按现有 `schema::properties` 的 Python dict 语义解码。
- 响应：DescribeAlias 填 `alias` 与 `collection`，`db_name` 留空（与 3.x 相同）；ListAliases 填 `collection_name` 与 `aliases`；TruncateCollection 只有 status。

### 测试矩阵

| 层 | 内容 | 位置 |
| --- | --- | --- |
| 单元 | 别名表加载与解析失败不缓存；`dump_file` 与真实 Python `json.dump` 逐字节对照（schema、alias、manifest、`.prev`；默认值、分区、`index_specs`、非 ASCII、`1E2`、`-0`、宽整数、默认浮点值）；两种意图逐字段篡改（路径穿越、snapshot 名穿越、同一列表内重复、未知字段、候选 manifest 与旧 manifest 关系不符、侧车 stem 不属于对应 data 集合）均 fail-closed 且 unlink 次数为零；合法意图的三类正例：snapshot 之后经合并移出 manifest 再清空、两个 snapshot 共享同一 data 与侧车、snapshot 钉住已 Drop 分区的数据，校验通过且清空后无遗留文件；`lock_pair` 的顺序与同层断言；放宽名字检查的边界 | core 模块内 |
| 故障与崩溃（fault-injection） | 合同「验收方向 / 故障」列出的每个点，加：别名写入与 AlterCollection 的 tmp 改名前后与目录同步失败；0.3.0rc3 写出的未排序 `schema.json` 上的无操作 Alter 会重写；改名 P1a 的 data、delta 与 `wal/` 目录同步失败，以及 P1b 每个出口（意图写失败、目录改名失败、核对失败并改回、删除意图失败）之后执行插入、Flush、重开，新旧数据都不丢；改名 P2、P3 各暂停点上旧名 / 新名的 Describe、统计、Query、Insert、Flush 与 create / drop 的确定结果及写入闸门状态；清空第 3 步对五种 `wal/` 状态（从未写入、只建 schema、上一次清空后未写入、存在空 `wal/`、`wal/` 不存在）均成功，`wal/` 为文件或符号链接以及 staging 目标已存在时零写入失败；WAL 目录改名之前、之后与目录同步之后退出，分别用本实现与 3.x 重开，计数与主键不重复、不丢失，后续写入与下一次 flush 成功；改名失败与目录同步失败之后的插入、Flush 与重开；清空第 4–7 步每步之后进程退出、I/O 错误与 panic；重开处理中的 fsync 失败；清理失败后由 Insert、Delete、Upsert、Flush、索引和分区操作分别触发“释放共享许可 → 独占重试 → 成功则从头重跑”的完整路径，并在独占重试的 schema 目录同步失败时断言整库写入封锁、无并发写入越过封锁 | [db_persistence.rs](../../crates/core/tests/db_persistence.rs)、[crash_recovery.rs](../../crates/core/tests/crash_recovery.rs) |
| 同一进程的读视图 | 清空后立即 count / get / query / search 为空，auto_id 从 1 开始，属性、分区与索引顺序与重开及 3.x 相同；清空后重开 auto_id 继续递增；以 0.3.0rc3 写出的未排序 `schema.json` 直接清空，比较清空前、清空后同一进程、清空后重开三种状态的属性顺序，并比较磁盘字节，均与 3.x 相同；第 4、5 步失败后 staging 中迁移的 WAL 目录被删除；第 6 步成功后 `pinned_files` 为空、`truncate_cleanup` 已清除、后台合并恢复；清空第 5 步发布之后、第 6 步完成之前用暂停钩子发起读写与 Flush：读看到空集合，写入与 Flush 在清空返回后才执行；清空中 `schema.json` 规范重写的 tmp 写入、文件同步、改名、集合目录同步分别注入失败，断言写入闸门状态（只有目录同步失败时整库封锁），成功重试或重开后磁盘为规范字节、重开属性顺序与 3.x 相同 | [db_persistence.rs](../../crates/core/tests/db_persistence.rs) |
| 并发 | 合同「并发」各组合；独占许可（别名写入、AlterCollection、改名、清空）与进行中合并的先后，清空期间整个数据库的写入等待，已打开集合的纯读不受影响，未打开集合的首次懒打开等待；清理重试完成共享到独占的重新入闸后，原请求从头执行且不能重复应用任何变更 | [concurrency_faults.rs](../../crates/core/tests/concurrency_faults.rs) |
| 差分 | 「参照实测」每一行的 raw gRPC 与 MilvusClient 对照，两条 SDK 线；预期差异单独断言 | `tests/differential/` 新增文件 |
| 互通 | 合同「互通」各项，含 3.x 中途退出的目录与本实现意图未完成时由 3.x 打开；Arrow 15 / 25 两个参照环境 | [tests/storage_interop/](../../tests/storage_interop/) |
| 端到端 | 参照 adapter 用例中范围内的条目复制或启用，更新 INVENTORY | [tests/python/adapter/](../../tests/python/adapter/) |
| 统计 | 760 / 90 / 49 三组按同一方法重跑，逐条核对合同预期 | 仓库外，记录写入验收文档 |

### 实施顺序与验收记录

1. `pyjson::dump_file`，schema、alias、manifest 全部改为排序写出；放宽名字检查；`require_database` 对已有非默认库返回 UNIMPLEMENTED；
2. `CollectionKey` 与 `DatabaseState` 重构（行为不变，全部现有测试通过）；
3. 别名表、全入口解析与五个别名 RPC；
4. AlterCollection；
5. 改名与改名意图；
6. 清空与清空意图；
7. 文档同步（合同「文档同步」清单）与三组统计。

每一步按「改了什么就跑什么」跑齐对应套件；第 2 步之后每步单独评审。实施验收另写 `docs/testing/0023-a-acceptance.md`，记录测试、差分、互通、统计与残余风险，不在本文追加证据。
