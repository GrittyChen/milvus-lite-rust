# 固定兼容样本与行为调查

用途是重复 [0014 收口统计](../../docs/testing/0014-compatibility-closure.md)，并为 [0015](../../docs/decisions/0015-nullable-defaults-and-array.md) 收集诊断。此目录不进入默认 pytest 收集；这里的运行器不会下载代码、安装依赖或访问远程服务。测试服务仅绑定本机。

## 固定输入

- 参照：[milvus-lite v3.2.1 / 43d1257](https://github.com/milvus-io/milvus-lite/tree/43d1257774e629bc9f66873977ab7c320d5bf5a7)。固定 adapter / compatibility 的 760 条用例，另保存原先排除的 139 条内部依赖用例身份。
- 上游：[Milvus dcda428](https://github.com/milvus-io/milvus/tree/dcda428d54d11e6084156e2342c513f08c9927bf)。保存原选择器 71 项及展开后的 90 条身份，运行时逐项核对。
- 调用探针：沿用原 49 项脚本，SHA256 为 `ecf7af124b2a1a98f5ada1f7cc4f87852b0af14672d848dae1df9abb76002bba`。只检查调用成功，不比较返回值，也不作为持久化验收。

[manifests](manifests/) 中只有用例身份与校验和，不包含参照或上游测试代码。运行器从使用者事先准备的本地 checkout 执行 `git archive`，在仓库外展开。源 checkout 保持只读；Rust 后端仅在展开的副本中替换起服务入口及默认 database 清理 fixture，测试本体与断言保持原样。准备时要求 HEAD 与固定 commit 相同，归档不包含未提交修改。

与 [0014 收口记录](../../docs/testing/0014-compatibility-closure.md#证据保留与边界) 的旧清单对应：旧文件是 JUnit 身份的 JSON 数组，例如 `tests.adapter.module.Class::method`；本目录改为每行一个可执行的 pytest 节点，例如 `tests/adapter/module.py::Class::method`。格式和字节不同，因此哈希不同；转换后逐项核对 760/139 条集合相同，均无重复，没有更换样本。

| 清单 | 0014 的旧 JSON SHA256 | 当前逐行 pytest 节点 SHA256 |
| --- | --- | --- |
| 760 条参照样本 | `372f0443afb5d242dc39cc5a4baad82f9bdc7f9a0ac483aaa4ff6f507ef3a902` | `55481d4fc03ff0f4f53097ff61b51d56590dc8ef31201bd315f8213d6dd266f2` |
| 139 条内部依赖排除项 | `aa834087b15f16990ab6fd68f6625c2ebdebface98b9c3a567b84b7efc0ef371` | `d627e940c31ad92c92ed516cea316365b0bd4cef59eaf4ba717b18fae84923bb` |

运行器要求 Python 3.12。测试解释器分别由参数指定；参照环境需安装固定参照的依赖、pytest 和 pytest-timeout；Rust 环境安装待统计的正式 wheel、pymilvus、pytest 和 pytest-timeout，不能安装官方 milvus-lite。上游套件还需要它自己的测试依赖。运行器将完整包版本、Python、原生库哈希、命令、工具哈希、JUnit 与事件保存到独立输出目录，已有目录不会覆盖。

## 三组统计

以下变量由调用者设置为实际路径；`AUDIT_WORK` 必须在本仓之外。虚拟环境的 python 路径应保留 venv 中的路径，不解析其符号链接到基础解释器。

```bash
python tests/compatibility_audit/run.py prepare \
  --reference "$AUDIT_REF" --upstream "$AUDIT_UPSTREAM" --work-dir "$AUDIT_WORK"

python tests/compatibility_audit/run.py run --work-dir "$AUDIT_WORK" \
  --python "$AUDIT_REF_PYTHON" --backend reference --name reference-original
python tests/compatibility_audit/run.py run --work-dir "$AUDIT_WORK" \
  --python "$AUDIT_RUST_PYTHON" --backend rust --name rust-original

python tests/compatibility_audit/run.py run --work-dir "$AUDIT_WORK" \
  --python "$AUDIT_UPSTREAM_REF_PYTHON" --backend reference --suite upstream --name upstream-reference
python tests/compatibility_audit/run.py run --work-dir "$AUDIT_WORK" \
  --python "$AUDIT_UPSTREAM_RUST_PYTHON" --backend rust --suite upstream --name upstream-rust

python tests/compatibility_audit/run.py run --work-dir "$AUDIT_WORK" \
  --python "$AUDIT_REF_PYTHON" --backend reference --suite probe --name probes-reference
python tests/compatibility_audit/run.py run --work-dir "$AUDIT_WORK" \
  --python "$AUDIT_RUST_PYTHON" --backend rust --suite probe --name probes-rust
```

完整统计预期包含范围外能力失败，因此 pytest 退出 1 不使运行器立即失败；它仍原样记录所有 failure/error/skip。收集失败、异常退出、重复、缺少或新增用例均使运行器失败，不能用通过数替代集合核对。上游禁用 rerun 插件，不增加重试。Rust 的 faiss 可用性垫片沿用原统计，只用于让原测试尝试 ANN 定义；不会改写索引参数。

## 受控反事实

固定 [schema.txt](manifests/schema.txt) 为原 120 条 nullable 阻断、3 条默认值阻断及 1 条 nullable 向量跳过，共 124 条。模式为 `scalar-nullable`、`defaults`、`scalar-nullable-defaults`、`all-nullable`；`original` 是唯一不干预模式。scalar 只包括已有 BOOL、INT8/16/32/64、FLOAT/DOUBLE、VARCHAR、JSON，不移除 ARRAY、时间或几何字段的 nullable。

干预发生在客户端 FieldSchema 构造时；不修改记录中的 None、缺失键或断言。事件记录原属性、干预属性及写入字段是否缺失/为 null；共享 fixture 的 schema 使用会带创建用例身份。`defaults` 仅用于诊断，也会移除向量等范围外类型上的默认值声明，不能据此扩大实现范围。

```bash
python tests/compatibility_audit/run.py run --work-dir "$AUDIT_WORK" \
  --python "$AUDIT_REF_PYTHON" --backend reference --selection schema \
  --mode scalar-nullable --name reference-scalar
python tests/compatibility_audit/run.py run --work-dir "$AUDIT_WORK" \
  --python "$AUDIT_RUST_PYTHON" --backend rust --selection schema \
  --mode scalar-nullable --name rust-scalar
python tests/compatibility_audit/compare.py \
  --baseline "$AUDIT_WORK/runs/rust-original/summary.json" \
  --reference "$AUDIT_WORK/runs/reference-scalar/summary.json" \
  --current "$AUDIT_WORK/runs/rust-scalar/summary.json" \
  --output "$AUDIT_WORK/scalar-comparison.json"
```

结果分为没有干预、参照不再通过、干预后两端通过、残余 UNIMPLEMENTED、其他待检查失败。最后一类不能自动归因于结果不一致；“参照不再通过”也不能自动断言是唯一原因。**全部干预结果均为诊断，不计入兼容性通过数、收益承诺或发行验收。** 原始运行和干预运行分目录保存。

## schema 真值与回退预检

```bash
PYTHONDONTWRITEBYTECODE=1 "$AUDIT_REF_PYTHON" tests/compatibility_audit/schema_truth.py \
  --reference "$AUDIT_REF" --kind values --output "$AUDIT_WORK/values.json"
PYTHONDONTWRITEBYTECODE=1 "$AUDIT_REF_PYTHON" tests/compatibility_audit/schema_truth.py \
  --reference "$AUDIT_REF" --kind filters --output "$AUDIT_WORK/filters.json"
PYTHONDONTWRITEBYTECODE=1 "$AUDIT_REF_PYTHON" tests/compatibility_audit/schema_truth.py \
  --reference "$AUDIT_REF" --kind vectors --output "$AUDIT_WORK/vectors.json"
PYTHONDONTWRITEBYTECODE=1 "$AUDIT_REF_PYTHON" tests/compatibility_audit/schema_truth.py \
  --reference "$AUDIT_REF" --kind rpc --output "$AUDIT_WORK/rpc.json"
```

values/filters/vectors 分别以 Arrow 15.0.0 与 25.0.1 的参照解释器运行，输出使用不同文件名。rpc 以 pymilvus 2.6.17 与 3.0.2 的参照环境分别运行，保留 raw gRPC 状态、请求/响应以及 SDK 实际发送的请求。组件真值不等于端到端通过；捕获的异常也是预期数据的一部分。

vectors 的子进程在写入后用 `os._exit` 退出，父进程读取遗留 WAL，再执行 Flush 和实际 compaction；没有断电证明。正常服务入口与直接构造 Arrow null 的组件路径必须区分。

加上 `--verify-nullable-filter-fixture` 可逐项核验仓库内的 414 项标量空值过滤真值；Arrow 15/25 分别执行，不覆盖原 fixture。

```bash
PYTHONDONTWRITEBYTECODE=1 "$AUDIT_REF_PYTHON" tests/compatibility_audit/rollback_probe.py seed \
  --reference "$AUDIT_REF" --state wal --work-dir "$AUDIT_WORK/seed-wal.db"
PYTHONDONTWRITEBYTECODE=1 "$AUDIT_OLD_PYTHON" tests/compatibility_audit/rollback_probe.py check \
  --seed "$AUDIT_WORK/seed-wal.db" --work-dir "$AUDIT_WORK/old-reader-wal"
```

`--state segment` 生成已 Flush 的另一份种子。旧解释器分别安装已发布的 rc1、rc2、rc3 wheel；检查过程复制种子，不接触原件。断言旧能力 collection 可读取、新属性 collection 明确拒绝、受保护 collection 的文件名与内容不变，以及拒绝后健康 collection 仍可读。健康 WAL collection 在正常关闭时可合法 Flush，因此不要求其全部文件保持不变。以上预检的写入方是参照，不能代替新 Rust 实际写出的目录。

切片 A 用候选原生 wheel 生成自己的种子，并明确选择本切片的集合；对 `wal`、`segment`、`compacted` 三种状态及三个旧版本分别执行：

```bash
PYTHONDONTWRITEBYTECODE=1 "$AUDIT_RUST_PYTHON" tests/compatibility_audit/rollback_probe.py seed-native \
  --state compacted --collections legacy nullable defaults --work-dir "$AUDIT_WORK/native-compacted.db"
PYTHONDONTWRITEBYTECODE=1 "$AUDIT_OLD_PYTHON" tests/compatibility_audit/rollback_probe.py check \
  --seed "$AUDIT_WORK/native-compacted.db" --collections legacy nullable defaults \
  --work-dir "$AUDIT_WORK/old-reader-native-compacted"
```

`compacted` 写入多个已 Flush 文件后先关闭，再独立重开、加载并关闭排空，断言最终 manifest 仅含一个数据文件。这样避免后续建表按 0010 取消尚未提交的维护；不能以调用过合并或已 Flush 代替实际合并证据。B 的原生回退矩阵使用 `--collections legacy nullable defaults array`，其余步骤相同。

工具反例检查：`python tests/compatibility_audit/selfcheck.py`。大日志、生成目录、环境冻结与结果 JSON 保存在仓库外；版本选择、提交与发布按各自授权执行。


## 分区与 Query iterator 调查

[分区调查记录](../../docs/testing/0016-partition-investigation.md) 和 [0016 合同](../../docs/decisions/0016-query-iteration-and-range-search.md) 使用只读、固定 commit 的参照；直接引擎 Arrow 15/25、RPC 与 SDK 2.6/3.0 分环境执行。`AUDIT_REF` 和解释器变量必须为绝对路径；工作目录必须不存在且在仓库外。以下验证不覆盖 fixture，失败直接退出；paths 的大小写别名事实依赖文件系统，在大小写敏感卷上应单独解释差异，不能重生成覆盖本机事实。

```bash
PYTHONDONTWRITEBYTECODE=1 "$AUDIT_REF_PYTHON" tests/compatibility_audit/partition_truth.py \
  --reference "$AUDIT_REF" --work-dir "$AUDIT_WORK/partition-engine" --kind engine \
  --verify tests/compatibility_audit/fixtures/partitions_v321.json
PYTHONDONTWRITEBYTECODE=1 "$AUDIT_REF_PYTHON" tests/compatibility_audit/query_iteration_truth.py \
  --reference "$AUDIT_REF" --work-dir "$AUDIT_WORK/query-truth" \
  --verify tests/compatibility_audit/fixtures/query_iteration_v321.json
```

partition_truth 的 kind 另有 paths、key、rpc，分别使用不同的外部目录。paths 只在该可丢弃目录内部测试危险名称，包含越出 collection 的路径及大小写别名，不对用户已有目录执行；key 通过子进程受控退出验证分桶持久化，不代表断电实验。

固定 62 条分区样本用 `--selection partitions`；`--mode partition-key` 只移除客户端 is_partition_key 属性。可加 `MLR_AUDIT_PARTITION_TRACE=1` 记录 setup/call/teardown 的分区 API，不记录数据、不更改返回值或异常。反事实结果仍只用于诊断，不能计入兼容性通过数。

## 0017 分区边界补测

[设计草案](../../docs/decisions/0017-partitions.md) 与 [调查记录](../../docs/testing/0017-partition-design-investigation.md) 使用两个新工具。解释器须 CPython 3.12，并安装本地固定参照及依赖；Arrow 15/25 的固定环境准备见 [互通 README](../storage_interop/README.md)。SDK 分别固定 2.6.17 / 3.0.2；不同时安装 Rust 包。脚本和解释器实际导入的参照源码须与 `--reference` 指向同一 checkout，HEAD 为 `43d1257`。源代码只读；数据、日志与输出均在新的仓库外目录。

```bash
PYTHONDONTWRITEBYTECODE=1 "$AUDIT_REF_PYTHON" tests/compatibility_audit/partition_boundaries.py \
  --reference "$AUDIT_REF" --work-dir "$AUDIT_WORK/partition-boundaries" \
  --verify tests/compatibility_audit/fixtures/partition_boundaries_v321.json \
  --variant mac-alias

PYTHONDONTWRITEBYTECODE=1 "$AUDIT_REF_PYTHON" tests/compatibility_audit/partition_drop_anchor.py \
  --reference "$AUDIT_REF" --work-dir "$AUDIT_WORK/partition-anchor"
```

boundary 的 `mac-alias` 变体对应本轮 macOS 大小写及 NFC/NFD 均别名的卷，`linux-distinct` 对应本轮 Linux 两对均独立的卷。Linux 使用后者，数据必须在真实 Linux 文件系统/命名卷，不以宿主 macOS 绑定目录代替。变体还保留两平台超长名字的 errno 差异，因此不能只按操作系统名称推断其它卷也会通过；与两份事实均不同时应保留失败并调查，不自动选另一个或覆盖 fixture。Arrow 15/25、两条 SDK 分目录执行；--verify 只比较现有事实，不改 fixture。

每次 17 组，包含服务端路径穿越、物理别名、符号链接、建目录失败的登记残留及重开前后顺序。每组自己的可丢弃根目录是所有解析路径的硬边界；工具有意允许越出自己创建的 collection，才能观测参照缺陷，但不接受用户已有目录。根路径归一化与 `..` 删除枚举错误的唯一归一化见调查记录；raw 错误仍留在各项日志。异常退出、超时或 fixture 不同都直接失败，无重试。工具不会联网、pip 或进入默认 pytest。

anchor 创建自己的种子，再复制为 control/anchor 两支：一支真实 Drop，另一支仅转存数据副本中的既有 delta 并同次保存 manifest，实际移除旧分区目录，再由真实 3.x 读取和合并。固定断言控制组复活、转存组不复活，合并后数据文件数确为 1。这是编码原型，没有实现产品的碰撞、故障注入或并发协议，不替代未来 Rust 分区验收。

## 分区可见性组件真值

```bash
PYTHONDONTWRITEBYTECODE=1 "$AUDIT_REF_PYTHON" tests/compatibility_audit/partition_visibility_truth.py \
  --reference "$AUDIT_REF" --work-dir "$AUDIT_WORK/partition-visibility" \
  --verify tests/compatibility_audit/fixtures/partition_visibility_v321.json
```

环境及新目录要求与上节相同，Arrow 15/25、SDK 2.6.17/3.0.2 分目录执行；--verify 不覆盖 fixture。14 条 INT64/VARCHAR 直接引擎轨迹的 170 个状态包含完整 segment 候选、活跃主键位置、活跃/已提交删除及独立 Query 期望；另有两条原始 gRPC 删除后重插轨迹。特别固定 Delete 未 Flush 会被重插移除、已 Flush 的删除继续保留这两支，不扩展 key 路由。

直接引擎 [] 的混合源差异保留为 direct_empty_diagnostic；adapter 实际把空 repeated 参数转为 None，所以组件对照参数为 None 或非空列表，不把内部差异搬成公开行为。数据和诊断均由 --verify 严格核对，没有因模型失败而覆盖期望；该输入边界及初次失败见 [组件记录](../../docs/testing/0017-visibility-component.md)。Rust 模型的构建、1870 次读取与 1716 次旧快照对照、资源诊断见 [组件 README](../../benchmarks/partition-visibility/README.md)。组件证明不等于产品分区、WAL 或整库 RSS 验收。


## Range search 真值

```bash
PYTHONDONTWRITEBYTECODE=1 "$AUDIT_REF_PYTHON" tests/compatibility_audit/range_truth.py \
  --reference "$AUDIT_REF" --work-dir "$AUDIT_WORK/range-truth" \
  --verify tests/compatibility_audit/fixtures/range_v321.json
```

分别使用 2.6.17 / 3.0.2 的参照环境，解释器、参照 checkout 为绝对路径；不修改参照或覆盖已有目录。15 组数据集覆盖活跃、混合、segment、重开和实际 compaction，包含 raw RPC 与 V1 页序、字段及错误原文。数据、输入矩阵与成功/失败断言固定，`--verify` 不覆盖 fixture。范围窗及 V1 的既有终止限制见 0016；不能把双方共同的早停当作完整遍历证明。

## 0017 产品边界与回退

以下只在待验收的 Rust 环境运行，不调用参照的危险路径；路径均由调用者设置为实际的仓库外新目录。

```bash
"$AUDIT_RUST_PYTHON" tests/compatibility_audit/partition_product_boundaries.py \
  --work-dir "$AUDIT_WORK/product-boundaries"
"$AUDIT_RUST_PYTHON" tests/compatibility_audit/partition_rollback.py seed \
  --state wal --work-dir "$AUDIT_WORK/partition-wal.db"
"$AUDIT_OLD_PYTHON" tests/compatibility_audit/partition_rollback.py check \
  --seed "$AUDIT_WORK/partition-wal.db" --work-dir "$AUDIT_WORK/partition-old-reader"
```

产品边界分别在两条 SDK、macOS 别名卷和 Linux 命名卷执行：记录实际目录身份、危险名和链接拒绝、超长名失败不登记、拒绝前后哈希不变，以及损坏 collection 不影响同库健康 collection。不能只凭操作系统名推断大小写或 Unicode 别名行为。解释器必须加载 Rust；该工具没有参照真值替换功能。

回退的旧解释器安装已发布 0.2.0rc1；对 `wal`、`segment` 两种状态各生成原生种子并复制后检查。非默认分区被明确拒绝且文件不变；同库健康 collection 仍可读取；最后一个非默认分区 Drop 后仅剩默认分区的目录可读，转存的全局删除继续生效。它不替代 0015 的 rc1–rc3 类型回退矩阵。

分区与 V1 search iterator 的额外交叉对照在两条 SDK 分别执行：

```bash
MILVUS_LITE_3X_PYTHON="$AUDIT_REF_PYTHON" "$AUDIT_RUST_PYTHON" \
  tests/compatibility_audit/partition_search_iteration.py \
  --work-dir "$AUDIT_WORK/partition-search-iteration"
```

固定两种主键、混合及重开、全部/单/组合/未知选择及范围开关，40 项逐页对照；严格核对完整条数和主键，再沿用 0004 分数误差预算。记录实际客户端迭代器类型，未知分区必须为空页；SDK 的空初始页诊断保留在日志，不当作服务端失败。此工具同样不进入默认 pytest、不覆盖已有目录。
