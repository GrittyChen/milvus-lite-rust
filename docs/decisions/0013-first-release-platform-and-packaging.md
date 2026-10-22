# 0013 首发平台与发行物验收

- 状态：已确认并通过方案复审（2026-10-10）；准备切片实施评审已通过；候选阶段决策已确认（2026-10-10，见下节），2026-10-12 修订发布渠道与公开地址；版本、tag、仓库公开与 Release 发布分别另行授权；`0.1.0rc1` 已于 2026-10-13 作为预发布发布（[发布记录](../testing/first-release-acceptance.md#010rc1-发布)），`0.1.0rc2` 已于 2026-10-14 作为预发布发布（[发布记录](../testing/first-release-acceptance.md#010rc2-发布)），`0.1.0rc3` 已于 2026-10-15 作为预发布发布（[发布记录](../testing/first-release-acceptance.md#010rc3-发布)）
- 日期：2026-10-10
- 功能基线：`2151dcc`；参照 milvus-lite v3.2.1 / `43d1257`
- 权威合同：[0008 首发范围](0008-first-release-scope.md)、[0002 数据互通](0002-data-dir-compat.md)、[0003 生命周期](0003-in-process-server-lifecycle.md)、[0006 持久化](0006-m2-scope.md#四持久化)、[0012 CLI](0012-cli-server.md)

## 目标与已确认取舍

交付可安装、可恢复、可监督停止的候选发行物，不增加功能。数据与提交不变量沿用上述决策；测试、错误处理和落库卫生沿用 [AGENTS 全局规则](../../AGENTS.md#全局强制规则每次改动都要应用)，本决策只定义平台动作、期望和证据。

负责人于 2026-10-10 确认：

1. **首发暂不提供 Windows wheel**，相应收窄 0008 的首发平台范围。Windows 保留后续目标；首发 Python 包在构建时明确拒绝 Windows 目标，避免 pip 从 sdist 安装出已知不可用的包，现有缺陷明确披露。此次不创建公开镜像或租用机器，Windows 不再作为本次发行的运行门槛。
2. **继续提供 sdist**：矩阵内纳入验收；其它架构、musl、较低 glibc 等只供自行构建，无支持承诺。Windows、free-threaded 与非 CPython 明确拒绝。
3. **Intel Mac 分层**：构建、安装与读写恢复冒烟，不要求完整套件；发行声明中单独说明证据弱于其它目标。
4. Linux x86_64 目前也没有原生机器/runner，先准备构建与模拟验证。原生证据缺口单列，不把模拟标为原生通过。

范围外仍为 dump、新索引/过滤、性能重测、真实断电与新的生命周期架构。版本切换、tag、仓库转公开与发布另行授权（见候选阶段决策 4、6）。

## 候选阶段决策

负责人于 2026-10-10 在准备切片及原生 x86_64 验收收口后确认，提前回答 2026-10-17 检查点；2026-10-12 修订第 3、4、6 项（发布渠道改为 GitHub Release、确认公开地址），修订后的内容直接写在各项中：

1. **Linux x86_64 接受为标准层**：以已记录的原生结果为准，暂不另找新内核主机。宿主内核 3.10 只代表已测环境，不推导所有新内核的行为。原生运行已暴露与架构相关的 NaN 口径问题，因此不把语义与格式验证视为与架构无关：**最终候选在 Linux x86_64 与 arm64 上各补跑差分与存储互通**，复用现有原生主机与本机 Docker。
2. **SHUTDOWN-TIMING-01 作为候选门槛**：候选前删除跨请求耗时推断；门槛必须能明确判定通过或失败，不采用“失败后单跑通过即算过”的常设办法。等待在途引擎任务的语义以 grpc 的受控 RPC/LOCK 用例为主证据；正式 wheel 的 Python 用例只断言客户端取消、停服后 LOCK 释放与重开结果，正式产物不增加测试接口。
3. **首个版本为 `0.1.0rc1`**（Cargo `0.1.0-rc.1`）：首发范围冻结后以 RC 公开试用，候选验收通过后才发布；后续修复发 rc2 等，最终再发 0.1.0。已发布的发行文件不替换、不删除后重传，修复须换版本重建，与 PyPI 的文件名不可复用规则一致，以免同名文件对应不同字节，也不妨碍以后转 PyPI。GitHub Release 默认允许替换附件，因此发布前开启仓库的发布不可变（见第 6 项），由平台锁定附件与 tag。试用安装说明显式指定版本，避免预发布解析规则造成困惑。
4. **公开源码地址在最终候选构建前确定**：源码、问题反馈入口与非官方身份说明一起交付。2026-10-12 确认地址为 `https://github.com/GrittyChen/milvus-lite-rust`（负责人个人账号），仓库内容已确认全部可以开源，问题反馈使用该仓库的 GitHub Issues。仓库目前为私有，须在发布 Release 前转为公开，公开动作由负责人执行。地址随 rc1 版本切换进入元数据、README 与归档检查器的允许项，并补反证测试。
5. **维持已确认的支持矩阵与已登记差异**：Intel Mac 的证据层级、Windows 延后、GIL CPython 3.10–3.15 窗口与 sdist 边界直接写入发行说明；RC 阶段保留不建议生产使用的说明。
6. **发布方式（2026-10-12 修订）**：RC 阶段只发布 GitHub Release，不上传 TestPyPI 或 PyPI；试用验证没有问题后，再另行决定是否以及何时发布到 PyPI。原定的 TestPyPI 演练、手工上传正式 PyPI、后续转 Trusted Publishing 以及按组织治理与维护者账号权限随之撤回，届时与 PyPI 的版本一并另定。决定前不上传空壳占名；`milvus-lite-rust` 在 PyPI 上可能先被他人注册，负责人确认接受这一风险，安装说明因此不按项目名从 PyPI 解析本包（见下）。以下执行细节由负责人确认采纳：
   - Release 标记为预发布，tag 为 `v0.1.0rc1`，指向构建候选的冻结提交；附件是候选验收通过的同一组文件：矩阵内各平台 wheel、sdist，以及覆盖其余全部附件的 `SHA256SUMS`，不附带测试用 FI wheel。
   - 发布前在仓库设置的 Releases 一节开启 Enable release immutability（只对开启后发布的 Release 生效；2026-10-12 查询时尚未开启）。开启后附件与 tag 由平台锁定，删除 Release 后同名 tag 也不能再用，因此先建草稿、上传并核对全部附件，再正式发布。该设置由负责人开启；发布前查询仓库设置确认已开启，发布后确认该 Release 在 GitHub 上标为不可变，两项写入验收记录，后者不成立时按发布后冒烟失败处置。
   - RC 版本号一经打 tag 即不复用：草稿阶段发现候选问题时不发布，删除草稿，保留该 tag、不移动，修复后发下一个 RC。
   - Release 说明包含非官方声明、不建议生产使用、支持矩阵与证据层级、已接受的差异与限制、安装命令与校验方法，与 README 不矛盾。
   - 安装说明从 Release 安装预构建 wheel，显式指定版本，不引导 `pip install git+…` 等需要本机 Rust 的源码构建。本包只从下载并核对过的本地文件或 Release 地址安装，不让 pip 按项目名查询 PyPI（使用 `--find-links` 时同时使用 `--no-index`），pymilvus 另行从 PyPI 安装。具体命令在 rc1 版本切换时确定：README 随候选冻结，写入前只能用其他项目已公开的 GitHub Release 演练 pip 的取用方式；本项目附件上的实际可用性由发布后的下载冒烟证明。
   - 发布后在仓库外的干净环境中，按 README 与 Release 说明从公开地址实际下载，确认每个 wheel 与 sdist 的哈希与候选验收记录逐一相同、`SHA256SUMS` 的内容与之一致，再安装并做导入、CLI 与真实 `.db` 读写冒烟。失败时保留 tag 与附件、不替换，在 Release 说明中标明该版本不可用并指向后续 RC，修复后发下一个 RC。
   - 版本切换、tag、仓库转公开与 Release 发布分别明确授权。
7. **镜像和工具链保留到候选验收、首发完成**：x86_64 原生主机上的 manylinux_2_28_x86_64（`sha256:4131ddd7…`，镜像 ID `2dd9abc5…`）、PID 1 基础镜像 `python:3.10-slim-bookworm` amd64（镜像 ID `fbff8a8e…`）与 Rust 1.98.1 工具链；本机的 manylinux_2_28_aarch64（`sha256:abd1230a…`）与 arm64 PID 1 基础镜像（镜像 ID `8bfa0d97…`，本地无仓库摘要，仅按镜像 ID 固定）。保留的镜像使用含摘要或镜像 ID 的固定本地标签，不依赖 `latest`；arm64 的 Rust 工具链按官方 SHA256 校验后重新安装，不单独保留。镜像只在本地 Docker 存储中，不推送、不入库。首发完成后清理可再生缓存，长期保留产物、日志、校验和与复现信息。

## 首发候选矩阵

持续集成（[0020](0020-continuous-integration.md)）只回答当前提交在托管机器上能否通过可自动执行的套件，不替代本节矩阵；CI 通过不等于候选通过。

2026-10-16 后续范围决定：0.1.0 的功能范围固定为 rc3 已交付能力；[0015](0015-nullable-defaults-and-array.md) 的 schema 新功能进入 0.2.0 开发线。0.1.0 转正式版须独立判断稳定性与验收门槛，不由该范围决定自动授权；本次不切换版本或发布。

2026-10-17，负责人确认启动 `0.2.0rc1` 候选准备：先推送已评审的 0015／0016 及跨平台收口提交，再切换版本、更新 README 与发行说明，从最终冻结提交重建并执行本节完整候选门槛。范围为标量 nullable／默认值、ARRAY、Query iterator、range search／Search iterator V1；分区实施、BM25、真实 ANN 不进入本次候选。开发验收不能替代最终版本的产物验收；tag 与 Release 发布仍分别明确授权，候选通过后只交付可评审产物和证据，不自动发布。0.1.0 的正式化继续沿用上述独立判断。

同日候选复审发现 README 将服务端 Search iterator V2 误列为 UNIMPLEMENTED；实际忽略探测参数、缺少 V2 结果字段后由 SDK 回退 V1，与 3.x 一致。旧冻结提交 `911c47b` 的发行候选因此作废，原始日志与附件保留；修订只涉及文档，产品和测试不改。重新冻结并重建全部正式附件及隔离 FI wheel；逐文件核对产品／测试／构建输入，并比较新旧 wheel 的全部 Python 文件和原生库。只有这些执行载荷逐字节相同时，才沿用旧候选已完成的完整语义、故障注入及平台结果，明确标为沿用；新候选仍重做归档／二进制检查、各平台解释器安装与生命周期／新增功能冒烟、V2 原始 gRPC 与真实 SDK 回退核验及 sdist 源构建安装。任一执行载荷不同即补跑该平台完整门槛。新旧身份、具体实跑与沿用范围分别写入候选验收记录，不将旧结果写成本轮实测。

2026-10-20，负责人确认采用 `0.3.0rc1`（Cargo `0.3.0-rc.1`），范围为已评审的显式分区 A、partition key B 及非 key 同批重复主键 Upsert 修复；继承 0.2.0rc1 已有能力。0.2 RC 承接修复是范围边界，不承诺持续维护 0.1、0.2、0.3 多条线，旧线是否发修复版按实际需求另定。顺序为版本与发行文档修改 → 审阅 → 获准本地提交并冻结 → 从冻结提交启动完整候选矩阵，冻结前不投入长时间矩阵。新版本须生成新的产品 / 构建输入清单与指纹，旧 127 项输入哈希只描述开发基线。开发验收仅开启 core FI 的 Clippy 不替代最终候选的 py → grpc → core 完整 FI 编译检查。详细准备顺序见[候选计划](../releases/next-candidate-plan.md)，统一发行正文源见 [0.3.0rc1 说明](../releases/0.3.0rc1.md)。平台层级沿用本决策，不新增原生 Intel 全套、macOS 11 真机或真实断电门槛；提交、推送、tag 与 Release 发布分别授权。


2026-10-20，负责人通过 [0.3.0rc2 候选方案](../releases/0.3.0rc2-candidate-plan.md)，确认仅纳入已审 [0018 JSON WAL 编码复用](0018-json-write-normalization.md)，继承 0.3.0rc1 的功能与已登记差异；峰值资源问题继续单独跟进。已获准进行版本与发行文档准备，准备变更仍须审阅并获准本地提交后才冻结、重建全部附件及执行本节完整矩阵。产品原生代码已变化，不适用仅文档变更、执行载荷相同的旧矩阵沿用例外。原实施性能记录保持历史身份，不回填为新候选成绩；新输入清单、源码归档与产物身份重新生成。推送、tag 和发布继续分别授权，统一正文源见 [0.3.0rc2 说明](../releases/0.3.0rc2.md)。

| 目标 | 下限 / 产物 | 验收层级与资源现状 |
| --- | --- | --- |
| Linux x86_64 | glibc 2.28；manylinux_2_28_x86_64 | 标准层（负责人已接受）；2026-10-10 已在原生 x86_64 主机的 glibc 2.28 容器中实跑，见[首发准备验收](../testing/first-release-acceptance.md#linux-x86_64-原生标准层)；最终候选补跑差分与存储互通 |
| Linux arm64 | glibc 2.28；manylinux_2_28_aarch64 | 标准层；本机 Docker 可实跑，须另建兼容基线镜像；最终候选补跑差分与存储互通 |
| macOS arm64 | macOS 11；单架构 wheel | 标准层；本机可实跑 |
| macOS x86_64 | macOS 11；单架构 wheel | 分层：构建 + 冒烟；原生或 Rosetta 运行均标明实际方式 |

标准层运行默认/FI Rust、适用端到端、平台锁/提交/恢复及安装门槛。模拟测试所得计数可记录，不作为原生通过；Linux x86_64 的原生标准层结果已单独记录。

差分与互通可按 [0019](0019-test-sharding.md) 并行编排并以确定性分片执行，各阶段通过判据不变；证据为各分片执行记录、JUnit 与 `summary.json`，平台默认并发度见[试点验收](../testing/0019-pilot-acceptance.md)。

**资源检查点（原定 2026-10-17）已提前决定**：负责人于 2026-10-10 接受 Linux x86_64 的标准层，并追加两种 Linux 架构的差分与互通补跑，见「候选阶段决策」。脚本准备完成可以独立收口，不自动等同于该目标已获发布许可。

Linux 在实际 glibc 基线环境构建并检查动态库版本，已有 manylinux_2_34 产物不是 2.28 的证明。两个 Mac 构建均显式设置 MACOSX_DEPLOYMENT_TARGET=11.0，并核对 Mach-O 的最低系统字段，不只查看 wheel 文件名。

Rust 1.90 起 Intel Mac 被降为 [Tier 2 with host tools](https://blog.rust-lang.org/2025/08/19/demoting-x86-64-apple-darwin-to-tier-2-with-host-tools/)；本项目的分层来自负责人的取舍，不将 Rust 的等级直接当作本项目的通过结论。不生成 universal2，Windows arm64、32 位和 musl wheel 不在首发候选内。

## Python 与源码回退

2026-10-10 核实：[Python 3.14](https://www.python.org/downloads/release/python-3140/) 于 2025-10-07 发布，[3.15](https://www.python.org/downloads/release/python-3150/) 已于 2026-10-09 正式发布。因此首发验收窗口改为 **GIL CPython 3.10–3.15**，3.10 保留为兼容下限，不以其维护状态宣称安全支持。

- 标准层：同一个 cp310/abi3 wheel 在 3.10、3.15 跑两条 SDK 的端到端全套；3.11–3.14 做导入、CLI、真实 `.db` 读写、关闭与重开冒烟。
- Intel Mac：同一 wheel 在两个端点做上述冒烟，并记录架构运行方式。
- 发行元数据保持 requires-python >=3.10，不加预设上限。已验证的 3.10–3.15 窗口通过 classifiers 与 README 声明；以后更新验证记录，不用元数据先拒绝未来解释器。见 [PyPA 建议](https://packaging.python.org/en/latest/guides/dropping-older-python-versions/)。
- SDK 固定测试 2.6.17、3.0.2；依赖版本记录实际安装值。参考差分/互通环境仍可独立固定为 Python 3.12、Arrow 15/25。

已核对 [pymilvus 3.0.2 元数据](https://pypi.org/pypi/pymilvus/3.0.2/json)：3.14 以上使用 grpcio>=1.75.1；[grpcio 1.84.0](https://pypi.org/pypi/grpcio/1.84.0/json) 提供 cp314/cp315 的相关目标 wheel。元数据与存在 wheel 只证明可准备安装，不替代两条 SDK 的实际测试。

方案准备时已用仓库外副本的 sdist 构建验证 Python 3.14/3.15 冒烟可行；当前源码的正式 wheel、两条 SDK 端点全套及平台缺口分别记录在 [首发准备验收](../testing/first-release-acceptance.md)，不将副本实验替代实施结果。

sdist 同样保持 >=3.10 且仅支持 GIL CPython；已验证窗口不是构建时的最高版本限制。源码回退不自动扩大支持平台：Windows 目标、free-threaded 与非 CPython 在本项目的构建检查中明确失败；其它矩阵外环境可自行尝试，README/长描述说明其无支持承诺。

PyO3 0.29.2 在某些 free-threaded 配置下会改为版本专用构建，不能依赖它自然报错。在 crates/py/build.rs 中用已锁定的 pyo3-build-config 读取**目标**解释器配置，拒绝 free-threaded/非 CPython，并读取 CARGO_CFG_TARGET_OS 拒绝 Windows 目标，给出本项目的明确错误。它同时覆盖直接 maturin 和 pip/PEP 517，避免只在一个入口检查；不新增 abi3t 或另一套构建后端。Windows 判断使用构建目标而非 build.rs 所在的宿主平台，因此 macOS→Windows 交叉构建也必须拒绝。负例在进入产品引擎前失败且没有数据目录，受支持目标的正常 GIL 构建仍通过。

采用构建拒绝，而不在 NativeServer.start 才报错，是为了让 pip 源码安装本身明确失败。其代价是本期不对 Windows 的 py crate 要求成功交叉编译；它改为拒绝消息/退出码的反证。Windows 后续切片解开构建门槛后，恢复绑定层的编译和真实运行验收。

## 固定工具链与前置检查

交付 rust-toolchain.toml，固定 channel=1.98.1、minimal profile、rustfmt/clippy；MSRV 通过显式 cargo +1.89.0 检查。rust-toolchain.toml 不进入 sdist，源码安装使用用户自己的工具链，因此 MSRV 的两套 workspace/all-targets 检查是源码发行门槛。构建使用 locked 模式和目标默认 CPU，不设置 target-cpu=native。工具与依赖边界沿用 [AGENTS 技术栈](../../AGENTS.md#技术栈)。

受支持目标的前置门槛包含 --workspace --all-targets，默认与 fault-injection 两套。Windows 只对 core/grpc 执行 check/clippy：-p milvus-lite-core -p milvus-lite-grpc --target x86_64-pc-windows-msvc --all-targets，默认与 fault-injection 各一套；py crate 对 Windows 目标的失败必须匹配本项目的明确拒绝消息，不能把其它编译错误当作成功拒绝。这些不是 Windows 运行证据。

方案准备阶段的前置检查记录（实施后的结果见最终验收记录）：

| 检查 | 结果 | 边界 |
| --- | --- | --- |
| macOS Rust 1.89.0，默认与 all-features 的 workspace/all-targets check | 通过 | 不等于其它目标的运行或最低系统证据 |
| Windows target，默认/FI 的 workspace/all-targets check | 通过 | 本机交叉编译检查，无 Windows 运行 |
| Windows target，两套 Clippy -D warnings | 失败：db_persistence.rs 的 QueryVectors/SearchRequest 仅 Unix 用例使用，导入未加 cfg | 登记到 Windows 准备交付物；不放宽 lint |

## 已知 Windows 缺陷与定下的修法

Windows 不在首发 wheel 矩阵，但现有缺陷不能写成普通“待核实”：[sync_dir](../../crates/core/src/storage/fs_commit.rs) 直接对目录 File::open/sync_all，按 std 和 [Microsoft 目录句柄](https://learn.microsoft.com/en-us/windows/win32/fileio/obtaining-a-handle-to-a-directory) / [FlushFileBuffers](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers) 的要求，常规 Windows 路径会失败。错误原文与整条运行路径尚未真机复现。

后续 Windows 实施采用已有 0006 合同：

- sync_dir 的 Windows 分支仅验证路径存在且为目录，正常返回；不 File::open，不调用目录同步，也不产生 SyncDir 事件。缺失、类型或 metadata 错误照常返回；Linux/macOS 分支保持实际同步与错误传播。
- 同类扫描发现只读 File::open 后 sync_file：flush 的继承文件/schema、WAL 清理/保留及 runtime 保留 WAL。同步句柄在 Windows 必须有写权限；**仅 Windows 分支**的路径重开使用不创建、不截断的读写 OpenOptions，Unix 保持原来的只读打开，原 writer/frozen File 继续复用。每处需验证同步失败留存 WAL、原文件字节不变及只读输入的错误/兼容边界，不能只改目录后宣称 Windows 已修好。
- 临时 schema/manifest、Parquet 与 WAL writer 的原写句柄继续同步。没有 Windows 断电保证，也不伪造文件或目录事件。

FI trace 的两种期望明确分开：

| 项 | Linux/macOS | Windows |
| --- | --- | --- |
| replace 前文件同步 | 引用/schema、新输出、manifest tmp、原 WAL 先 SyncFile | 同样检查文件 SyncFile |
| 目录项同步 | 按原链检查 SyncDir | 全 trace 没有 SyncDir，不标为已同步 |
| WAL 删除 | 每个 unlink 前再次同步对应 WAL，data→delta，unlink 后同步 wal/ | 文件同步、提交点与 data→delta 次序相同，无 unlink 后目录同步 |
| 提交点、panic、保留 WAL 与恢复 | 原有故障矩阵 | 保留同样的数据/提交边界断言；目录同步特有情形按平台能力转为目录校验错误或显式不适用，不制造假同步 |

需要调整的落点包括 concurrency_faults.rs 的文件顺序用例及 flush.rs 的继承目录 trace 用例；共享的数据、文件同步、提交点和恢复断言仍执行。当前 Windows 延后，修法与用例清单先冻结，实施/运行结果单独登记。

测试基础设施按 [Python select 文档](https://docs.python.org/3.13/library/select.html) 修正 stdout 管道读取：test_cli.py、test_cli_server.py、test_signals.py 沿用 test_cli_signals.py 的读线程 + 队列方式，并验证 EOF/超时/回收。linux_lock_probe.py 只用于 Linux。fork、POSIX 信号和权限用例另列不适用清单，Windows 则验证字节范围 LOCK、文件替换、CTRL_BREAK_EVENT 与 TerminateProcess；准备完成不是平台支持。

## Linux PID 1：动作、断言与证据

数据卷和控制卷均使用本次测试创建的 **Docker 命名卷**；服务容器与 LOCK 探测容器在同一个 Docker VM 中挂载同一数据卷，以 flock 非阻塞争抢确认仍持锁。记录该 VM 的 Linux 内核版本，不使用 macOS bind mount/virtiofs 作为 Linux 锁或 fsync 的证据。

正式场景用 exec 形式的 ENTRYPOINT 直接启动 console script；由容器 /proc/1/cmdline 与 Docker 启动配置核对 CLI 身份，不将 docker exec 内探针自身的 PID 当作 CLI PID。FI 场景由包装脚本作为 PID 1，显式 assert getpid()==1，调用真实 cmdline.main，使用现有维护暂停接口。

| 场景 | 动作 | 判失败的断言 |
| --- | --- | --- |
| 正常关闭（正式 wheel） | 已确认 Insert/Upsert/Delete 后 docker stop | 退出 0，关闭行一次；重开行集、值与 seq 符合模型，LOCK 释放 |
| 关闭排空（FI wheel） | 等 docker logs 出现 PAUSED，再 SIGTERM；看到 DEFAULT 后写控制卷的 resume 文件 | 放行前服务未结束、LOCK 未释放；放行后退出 0、恢复完整 |
| 后续 SIGTERM 与监督器强杀（FI wheel） | DEFAULT 确认 SIG_DFL 后再 SIGTERM；保持暂停，随后 docker stop 使用明确短超时 | 第二次 SIGTERM 后仍运行、检查点未前进；最终 ExitCode=137、OOMKilled=false，结合 kill/signal=9 事件确认监督器强杀；重开全部已确认行 |

[pid_namespaces(7)](https://man7.org/linux/man-pages/man7/pid_namespaces.7.html) 给出 PID 1 无 handler 时的确定语义，第三行不再只是“记录实际反应”。通过 docker logs 观察检查点，通过独立控制卷的文件显式放行，不靠延时猜状态。正常与强杀结果分别记录。

部署说明明确：PID 1 在进入 SIG_DFL 后没有“二次 SIGTERM 强制退出”的退路；建议使用 --init，或为 flush/维护配置足够的 stop timeout，最后的 SIGKILL 按崩溃恢复处理。外部期限不是产品的退出时间保证，完整信号合同仍见 0012。

## 已复现的打包缺陷与修法

本轮独立构建原 sdist：根 README/LICENSE 缺失，PKG-INFO 没有长描述。缺根 README 的问题也影响非官方说明的可发现性；根 LICENSE 的随附和元数据应有明确检查。

最小可行方案是将**唯一** pyproject.toml 从 crates/py 迁移到仓库根，使发行项目与权威 README/LICENSE 同根：

- project.readme=README.md；project.license-files 包括 LICENSE、crates/grpc/proto/LICENSE。
- tool.maturin.manifest-path=crates/py/Cargo.toml；python-source=crates/py/python；原模块/console script/动态版本不变。
- 删除旧位置配置，复核现有 -m 命令与根 PEP 517 入口；同步构建身份脚本覆盖根 pyproject.toml。
- 使用根文件本身，不复制维护第二份，不使用 checkout 在 Windows 上可能失效的符号链接，也不用含 .. 的 license/include glob。

根配置迁移与 README 重写已实施。实际 wheel/sdist 的许可、License-File 与长描述，以及独立解包/PEP 517 安装的结果，见首发准备验收；面向安装用户的内容门槛如下。

homepage/repository 只填真实公开且获授权的项目地址，不填官方 Milvus 仓库或虚构镜像；地址已于 2026-10-12 确认（候选阶段决策 4），rc1 版本切换时写入 `[project.urls]` 的 Repository 与 Issues，元数据检查要求恰为这两项。Cargo 清单不重复填写，其缺少 repository 的提示保留，不以掩盖 Cargo 警告代替许可证/长描述检查。

## 面向安装用户的 README 与长描述

根 README 继续是唯一的发行长描述源，开发进度和机制细节留在已有决策/验收记录，不另维护一份 PyPI README。

实施时重写面向安装用户的部分，至少包含：非官方与无隶属关系说明；先执行 python -m pip uninstall milvus-lite；安装 pymilvus 与 milvus-lite-rust、不使用 pymilvus[milvus-lite]；官方包与本包共用模块/命令的冲突和后装覆盖边界；支持矩阵与 Intel Mac 分层；sdist 的实验性回退、Windows 构建期拒绝与 free-threaded 限制；简短的 `.db` 读写/关闭示例、CLI 与数据恢复/信号限制。开发构建方式与安装方式分别说明；rc1 起的安装方式按候选阶段决策 6，只从 GitHub Release 安装本包，不按项目名查询 PyPI。

用户 README 只使用已确认仓库地址下的绝对链接（源码、Issues 与 Release）、其它真实外部绝对链接及页内锚点，不设开发文档导航。源码内的详细入口继续由 AGENTS 索引；不填占位地址，也不把 docs/… 相对链接放进长描述。

打包检查同时验证 README 源内容与 wheel/sdist 的长描述：安装/冲突/平台/源码边界均在，所有文件型相对链接拒绝，绝对项目地址必须是已确认地址：按已确认仓库的路径段前缀匹配，路径含 `.`/`..` 段、百分号编码或反斜杠以及带用户信息的地址拒绝，不放行整个主机。安装命令不靠解析 shell 或 Markdown 判定，而是白名单（见候选阶段决策 6）：反斜杠续行合并后逐行检查原文与 HTML 解码后的文本，移除已确认仓库地址后同一行内 milvus、lite、rust 依次出现（不区分大小写）即视为引用本包名；除标题外，这样的行去除首尾空白后必须逐字等于检查器内置、按版本参数化的两条 Release 安装命令，且两条都须出现；`$` 与花括号在任何位置都拒绝。检查器防的是按常规写法误写的按名安装：名称以任意大小写与 `-_.` 分隔写出，或在三段之间插入引号、转义、字符引用、Markdown 与 HTML 标记、链接，都会被识别；同一行依次提到官方 milvus-lite 与 Rust 的句子也会被拒，需要调整语序或分行。不在检查范围、由 README 评审把关：在单词字母中间插入内容的刻意混淆；运行时计算或变换（命令替换、变量、管道中的文本变换、printf 或解释器的转义序列等），无论单行或跨行；长描述之外的内容（如 requirements 文件）；依赖当前目录已有文件的通配符。这样不只验证“有 Description”，也验证用户实际看到的说明。该检查不联网探测尚未公开的服务。主机检查覆盖包括代码示例及无协议 www. 地址在内的完整文本：已确认仓库之外仅允许 Apache 许可站点 `apache.org` / `www.apache.org` 与本地 RPC 示例的 `127.0.0.1` / `localhost`；内联及引用式文件相对链接拒绝，HTML `href` / `src` 属性拒绝；元数据的 Project-URL 须恰为 Repository 与 Issues 两项，不得有 Home-page；源码包中不得包含 `rust-toolchain.toml`。地址已确认，允许项、按名安装的拒绝规则及其反例已随 rc1 版本切换落地（候选阶段决策 4、6）。

## 迁移后的命令与同步范围

-m crates/py/Cargo.toml 指定的是 Rust crate，不是 pyproject 的位置；迁移后可保留该写法，但从仓库根目录、sdist 解包目录、已有脚本调用三个入口分别实跑。增加根 PEP 517 安装入口，不机械改为指向虚拟 workspace 的根 Cargo.toml。

实施时搜索 AGENTS 常用命令、tests/platform/README.md、构建/基准脚本及当前使用指南中的 -m 和 pyproject 引用，更新失效命令或补充仍有效的理由。历史测量记录保留原命令与产物身份，只追加迁移说明及当前入口链接。AGENTS 技术栈/架构地图补根 pyproject、crates/py/build.rs、已锁定的 pyo3-build-config 构建依赖与 rust-toolchain.toml，不提前把拟新增项标成已实现。

验收脚本命令进入 AGENTS 常用命令，并由实际执行验证参数：

```bash
<python> tests/packaging/check_artifacts.py --wheel <wheel> --sdist <sdist> --repo <repo>
<python> tests/packaging/run_acceptance.py --work-dir <outside-repo> --constraints <constraints>
<python> tests/packaging/run_acceptance.py --work-dir <another-outside-repo> --python <test-python> --constraints <constraints> --wheel <same-wheel> --sdist <same-sdist>
<python> tests/platform/linux_pid1_probe.py --formal-wheel <linux-wheel> --fi-wheel <linux-fi-wheel> --image <pinned-image> --constraints <constraints> --output <outside-repo>
```

## 具体交付物

下列列出准备切片的必交项；单列的 Windows 后续项只冻结方案，不要求在本切片实现。文件列表不表示已经存在或测试已经通过。

| 文件 | 动作 → 期望 → 自验证 |
| --- | --- |
| rust-toolchain.toml | 固定基准工具链 → 构建可追溯 → 四类 check/clippy 命令记录版本 |
| 根 pyproject.toml / 原 crates/py/pyproject.toml | 迁移唯一配置，保留 >=3.10 → 许可/长描述/入口正确 → 实际 wheel/sdist 解包检查 |
| README.md | 重写安装用户说明，按上节处理尚无公开地址的链接 → 内容和 PyPI 展示边界一致 → 长描述内容与链接反例 |
| crates/py/build.rs 与构建依赖声明 | 目标解释器与目标 OS 检查 → Windows/free-threaded/非 CPython 明确拒绝 → 拒绝消息反证及受支持 GIL 包验证 |
| tests/packaging/check_artifacts.py | 检查版本、标签、入口、LICENSE/README、长描述内容与链接、禁入文件 → 缺任一项失败 → 真实归档 + 删除/错内容/相对链接反例 |
| tests/packaging/install_smoke.py | 仓库外安装和真实 `.db`/CLI/重开 → 输出模型一致 → 无 SDK CLI 环境与两条 SDK 环境分开执行 |
| tests/packaging/run_acceptance.py | 调用构建、按完整约束安装、经分片工具执行适用套件并收集身份 → 子步骤失败、收集数不符或原生模块不符均不产出通过记录 → 人为失败传播自检（约束与分片执行由 [0021](0021-release-build-in-ci.md) 实施时补入） |
| tests/packaging/test_artifact_checks.py / test_acceptance.py | 反证检查器与运行器 → 错版本/标签/许可/输出/清理失败被捕获 → pytest |
| tests/platform/Dockerfile.pid1 / pid1_entry.py | 正式 ENTRYPOINT 与 FI 包装入口分开 → 实际 PID 1 与检查点可见 → 不增加产品接口 |
| tests/platform/linux_pid1_probe.py / test_pid1_probe.py | 执行上表与恢复 → 错退出码/少行/未放行不通过 → 模型反证 + 真实 Docker |
| tests/platform/windows_run.ps1 / README.md | 准备编译/LOCK/替换/控制台/恢复流程 → 无机器时明确未执行 → 未来真实 runner 逐项验收 |
| 三份管道读取测试与 db_persistence.rs | 准备可移植读取、修复 cfg 导入 → 共用断言与两套 core/grpc 静态门槛通过 → 本机回归 + Windows 交叉检查 |
| Windows 同步实现/trace 用例（后续 Windows 切片） | 按冻结的 Windows 修法落地 → 真实平台提交/恢复结果正确 → 本切片仅保留方案，后续实施和真机验证 |
| benchmarks/baseline/setup_rs.sh | 构建身份覆盖根打包输入 → 同版本配置改动可区分 → 保持历史测量记录，不改历史数字 |
| AGENTS.md 技术栈/架构地图/常用命令与 tests/platform/README.md | 同步根 pyproject、build.rs、pyo3-build-config、工具链以及 packaging/PID 1 命令 → 文档能直接执行 → 全域路径搜索和命令验证 |
| docs/testing/compaction-correctness.md 等历史记录 | 保留测量时的原 -m 命令与产物身份，追加迁移说明/当前入口链接 → 不改历史证据 → 检查历史与现行命令分开 |
| docs/testing/first-release-acceptance.md | 汇总源/产物身份、矩阵、命令、计数、缺口和决定 → 准备与候选结论分开 → 每项可追溯 |

## 收口条件与顺序

**0013 准备切片完成**：方案通过评审，表中准备切片的必交项及自检落地；Windows 两套 Clippy 中已确认的 cfg 导入问题须修复并使规定的静态门槛通过，不是登记后放行；本机支持目标的打包/安装与 Linux PID 1 实跑；Linux x86_64 构建/模拟材料齐全；Windows 构建拒绝的反证、core/grpc 两套静态门槛与准备项齐全，正式/FI 与未执行状态分开。此时可以提交准备切片，不要求一个不存在的 Windows runner 无限期阻塞。

**首发候选验收完成**：版本 `0.1.0rc1` 与公开源码地址已冻结并进入元数据与 README；全部候选从同一冻结提交重新构建并保存哈希；核准发行矩阵各行达到各自层级，Linux 两种架构的差分与存储互通补跑通过；SHUTDOWN-TIMING-01 已以确定性判据关闭；许可、元数据、源码回退、Python 窗口与正式产物安装通过；已接受的差异/限制集中记录，可直接用于发行说明。现有提交/恢复门槛沿用 AGENTS、0002、0006、0012，避免重复规则。

**发布完成**：候选验收之后依次分别授权 tag、仓库转公开与 GitHub Release 发布，发布前已开启发布不可变，附件是同一组已验收文件及其 `SHA256SUMS`；发布后从公开地址实际下载安装的冒烟通过（候选阶段决策 6）。是否转 PyPI 另行决定，不属于本发布。版本切换提交之前维持 0.1.0-dev.0，不把准备阶段或临时副本的结果写成已发布。

实施顺序：先落许可证/README 和解释器拒绝检查，固定工具链；再建立检查器/安装运行器与 PID 1 探针；完成本机与模拟材料、更新验收；最后进入候选与发布决定。Windows 后续切片另取实际执行环境，再修复并验证上述代码缺陷。
