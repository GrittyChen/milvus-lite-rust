# 0020：持续集成

- 状态：方案经负责人与独立复审两轮评审、负责人两轮复审通过，复审建议已采纳。工作流已实施（[ci.yml](../../.github/workflows/ci.yml)、[compare.yml](../../.github/workflows/compare.yml)），待实施评审与试点。
- 日期：2026-10-22。
- 依据：[0004 跨平台](0004-m1-scope.md#跨平台)、[0013 首发候选矩阵](0013-first-release-platform-and-packaging.md#首发候选矩阵)、[0019 分片](0019-test-sharding.md)、[证据的存放与复核范围](../testing/README.md)。

## 问题

仓库没有任何 CI。0004 曾要求 Linux 与 Windows 的 CI 与 wheel 构建最迟在首次发布前完成；Windows 后由 0013 延后，其余实际由 0013 的人工候选矩阵把关发布，日常改动靠提交前手动按「改了什么就跑什么」执行，漏跑无人发现。仓库已公开，外部 PR 与下一条开发线都需要一道自动、公开可见的门槛。

GitHub 为公开仓库免费提供标准托管机器：Linux x86_64 与 arm64 为 4 vCPU / 16 GB，macOS arm64 为 3 vCPU / 7 GB，存储均为 14 GB SSD（[GitHub 托管机器参考](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)，以试点实测为准）。0019 的分片工具已能在单机内并行差分与互通。

## 决策

CI 分快速层与对照层，在三个标准平台的托管机器上运行：Linux x86_64（`ubuntu-24.04`）、Linux arm64（`ubuntu-24.04-arm`）与 macOS arm64（`macos-15`）。CI 只回答「当前提交在托管机器上能否通过可自动执行的套件」，不替代候选矩阵。下文命令是本方案的范围合同；实施后具体命令以工作流文件为准，本文只约束范围与判据。

### 公共约定

1. **checkout**：本仓以 `fetch-depth: 0` 取完整历史（[tests/benchmarks/test_history.py](../../tests/benchmarks/test_history.py) 读取历史提交中的基准脚本），并设 `persist-credentials: false`。
2. **Cargo**：工作流直接执行的 cargo 与 maturin 命令一律加 `--locked`，包括 0019 的三条互通预构建命令；用例内部自行调用的 cargo 不改，job 结束时以 `git diff --exit-code -- Cargo.lock` 确认锁文件未变。不设 `CARGO_TARGET_DIR`，因为互通用例写死仓库内 `target/debug/examples` 的产物路径。`CARGO_INCREMENTAL=0`、`CARGO_PROFILE_DEV_DEBUG=0`、`CARGO_PROFILE_TEST_DEBUG=0` 在 job 级设置，使用例内部的构建与预构建一致；它们关闭增量缓存与调试符号，不改变测试选择或断言。
3. **Python 环境**：uv 经 setup-python 以 `pip install uv==<固定版本>` 安装。每个环境按仓库内提交的完整 constraints 安装（[.github/ci/requirements](../../.github/ci/README.md)），constraints 由 uv 生成，覆盖构建后端、pytest 与全部传递依赖，三平台共用，更新随提交评审；每个环境把 `uv pip freeze --python <解释器>` 写入日志。环境共四套：开发（CPython 3.12、pymilvus 3.0.2）、2.6 线（CPython 3.10、pymilvus 2.6.17）、参照 Arrow 25（CPython 3.12、可编辑安装参照 checkout、pyarrow 25.0.1、pymilvus 3.0.2）与参照 Arrow 15（pyarrow 15.0.0、NumPy 1.26.4）。
4. **零跳过**（规则 4）：所有 pytest 运行都经 0019 的分片工具执行，`summary.json` 的 `passed` 进入判定；该工具逐项记录结果，拒绝 skipped、xfailed 与 xpassed（非严格的 xpass 在普通 pytest 中退出码为 0，JUnit 也只记为普通用例），并按写在工作流中的收集数核对用例集合。差分与互通按对照层分片；`tests/python` 与各工具自检以 `--shards 1 --order round-robin` 单片执行，保持原收集顺序（`balanced` 在无耗时数据时会按 nodeid 重排）。例外有两个：分片工具自身的 `tests/sharding`，为避免用工具验证工具，以普通 pytest 运行，要求退出码为 0，且末行汇总中 passed 的数量等于工作流中写明的收集数、不出现 failed、error、skipped、xfailed 与 xpassed（pytest 的汇总行会分别列出它们）；warnings 与耗时不影响判定；以 subtests 组织的 `benchmarks/json-write/diagnostics/selfcheck.py`，分片工具按用例计数时与 JUnit 不符，改为核对 unittest 输出恰好运行预期数量且末行为不带附注的 `OK`。Rust 测试汇总的 ignored 须为 0。[index_changes.rs](../../crates/core/tests/index_changes.rs) 中凭据不同与外来所有权两个分支只在 root 下执行、非 root 时略过并显示为通过，因此两个 Linux 平台另以 root 运行已构建的 `index_changes` 测试二进制中的权限用例，并按用例数量核对，避免筛选落空。
5. **时序与信号**：同一 job 内 Python 全套串行执行，不与 cargo test 或其他套件同时运行；按 0019 P0 第 2 条，在 Python 全套与差分前记录 SIGINT / SIGQUIT 的实际处理函数。
6. **超时**：每个 job 设 `timeout-minutes`，取试点最长耗时的 2–3 倍；超时计为失败。分片工具没有单分片看门狗，用例挂死只能由此结束。
7. **缓存**：只缓存 Cargo registry 与 uv 下载缓存，不缓存 `target`；键包含 `runner.os`、`runner.arch`、工具链版本、`Cargo.lock` 与 constraints 的哈希。缓存只影响耗时，试点中以一次无缓存运行对照确认。
8. **shell**：所有步骤显式以 bash 运行（GitHub 此时使用 `bash -eo pipefail`）。未指定时为 `bash -e`，`cargo test | tee` 的结果只取决于 `tee`，测试进程崩溃或中途退出会被吞掉。
9. **不条件跳过 job**：任何 job 都不以 `if` 条件跳过，每个工作流的每种触发都运行其全部 job；平台差异以矩阵内的独立 job 表达。因此汇总判定中 skipped 一律视为失败，见「判定与流程」第 3 条；对照层按套件执行的步骤前有守卫，套件名不在已知清单中时失败。矩阵设 `fail-fast: false`，一个平台失败时仍收齐其余结果。

### 快速层

触发：推送到分支（不含 tag）、PR、手动。同仓分支一般直接推送、不开 PR；开 PR 时同一提交会运行两次，接受这一重复，不用 job 条件去重。

按磁盘与并行拆成独立 job：每个平台的默认 Rust（第 1 项默认 Clippy 与 fmt、第 2 项默认测试及 Linux 上的 root 权限用例）、完整 FI Rust（第 1 项 FI Clippy、第 2 项 FI 测试）、Python（第 3、4 项；`tests/benchmarks` 有用例导入已安装的扩展，因此工具自检与端到端同在一个 job）各为一个 job，第 5 项在 Linux x86_64 单独成 job。

1. `cargo fmt --all --check`；Clippy 覆盖默认与完整 FI 两个编译面（经 py → grpc → core 开启 FI，即 [0017 跨平台收口](../testing/0017-cross-platform-acceptance.md)要求最终候选覆盖的口径）：`cargo clippy --workspace --all-targets --locked -- -D warnings` 与 `cargo clippy --workspace --all-targets --features milvus-lite-py/fault-injection --locked -- -D warnings`。
2. `cargo test --workspace --locked` 与 `cargo test --workspace --features milvus-lite-py/fault-injection --locked`；后者同时覆盖 core、grpc 与 py 的 FI 测试，包括只在 grpc FI 构建下编译的服务宿主用例。
3. Python 端到端两条 SDK 线：开发环境以 `maturin develop --release --locked` 安装后运行 `tests/python`；同一提交 `maturin build --release --locked` 的 wheel 装入 2.6 线环境，在仓库外目录运行同一套用例。
4. 工具自检：`tests/packaging`、`tests/platform`、`tests/benchmarks`、`tests/sharding` 与 `tests/compatibility_audit/selfcheck.py`、`benchmarks/json-write/diagnostics/selfcheck.py`。
5. 仅 Linux x86_64：按声明的 `rust-version` 执行 `cargo +1.89 check --workspace --all-targets --locked` 及其完整 FI 版本（`--features milvus-lite-py/fault-injection`），与 [0013](0013-first-release-platform-and-packaging.md) 源码发行的默认与 FI 两套 workspace / all-targets 口径一致；按 0013 的口径对 core / grpc 做 `x86_64-pc-windows-msvc` 交叉 check 与 Clippy（默认与 FI）。

第 1–4 项在三个平台执行。Rust 单测在 x86_64 与 aarch64 上各自验证 [flat.rs](../../crates/core/src/flat.rs) 的求和顺序固定，只间接支持 0004 对跨架构距离逐位相同的要求，不直接比对两种架构的输出；本方案不新增跨架构黄金值。

### 对照层

触发：推送到非 main 分支（不含 tag）、PR、main 每晚一次、手动。

每个平台的两条 SDK 差分、互通与固定真值核验各为一个 job。

1. **参照**：以 depth 1 取公开仓库 `milvus-io/milvus-lite` 的固定 commit `43d1257774e629bc9f66873977ab7c320d5bf5a7`（即 tag `v3.2.1`），放到被忽略的 `references/milvus-lite/`，核对 HEAD 与工作树干净；参照只在运行时取得，不入库。差分的 `MILVUS_LITE_3X_PYTHON` 使用参照 Arrow 25 环境：[test_filter_s1.py](../../tests/differential/test_filter_s1.py) 在 arm64 上要求参照的 pyarrow 为 25.0.1，不固定 pyarrow 的环境会在两个 arm64 平台失败。
2. **差分与互通**：两条 SDK 线的完整差分与 Arrow 15 / 25 互通按 0019 用 tests/sharding 分片执行，`summary.json` 的 `passed` 与各项核对进入判定。`--expect` / `--expect-group` 写在工作流中（当前差分 421 / 118、互通 211 / 0），与增删用例的提交一同修改、一同评审，不从同一次收集取值。互通先按 0019 执行三条预构建命令并核对 `fresh`；同一 job 内 FI 的 cargo test 不与互通同时运行。并发度按托管机器的核数与内存在试点中确定。
3. **固定真值核验**：S1 语法、顶层动态过滤、完整布尔树与 ARRAY 的 `--verify`（后两项 Arrow 15 / 25 各执行），`schema_truth.py --verify-nullable-filter-fixture`（Arrow 15 / 25 各执行），Unicode 分类表核验（CPython 3.12，在仓库内执行以使用固定工具链的 rustfmt），以及 `partition_visibility_probe verify-core`。命令见 AGENTS「常用命令」。

### 不纳入 CI

以下继续由 0013 的候选矩阵与专项承担，CI 通过不等于候选通过：manylinux_2_28 正式 wheel / sdist 的构建与安装验收、原生 x86 老内核与 PID 1、Intel Mac（托管的 `macos-15-intel` 是否用于提升其证据，另行评估）、隔离 fault-injection wheel 的 `tests/compaction`、Linux 容器的 LOCK 与 strace、发布流程、全部性能基准（共享机器噪声大，规则 7 的数据不取自 CI）、Windows 运行（0013 已延后）。分区、Query iterator、range search 与 partition key 的调查真值需要另建 SDK 2.6 参照环境与专门流程，仍按各自 README 显式执行。

## 判定与流程

1. **失败处理**：按规则 5，任何失败都要定位根因；不加 retry，不以重新运行变绿作为放行依据。重新运行只用于收集诊断。某个提交出现过失败，即使重新运行后变绿，也须先在修复提交或试点记录中写明根因，才能快进到 main。
2. **试点阈值**：按平台、按 job 分别计数，快速层连续 10 次、对照层连续 5 次运行无失败；出现失败时修复根因后重新计数。达到后由负责人决定是否设为必须通过检查。
3. **必须通过检查**：每个工作流以一个依赖其全部 job 的汇总 job 作为分支保护的检查入口，矩阵调整时不留下过时的必须检查。上游失败时普通依赖会使汇总 job 变为 skipped，而分支保护把 skipped 视为可接受，因此汇总 job 以 `if: always()` 运行，逐个检查 `needs.*.result`，只有全部为 `success` 才通过，`failure`、`cancelled` 与 `skipped` 都主动以非零退出。分支保护把该检查的来源限定为 GitHub Actions，并开启「Do not allow bypassing the above settings」，否则仓库管理员不受分支保护限制（[About protected branches](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-protected-branches/about-protected-branches)）。分支保护属于仓库设置，由负责人操作。
4. **推送方式**：设必须通过后，先推送工作分支，CI 通过后将同一提交快进到 main（`git push origin <commit>:main`），保持线性历史，不产生 merge commit；必须通过检查按提交哈希判定，快进时无需重跑。外部 PR 不用 GitHub 的 rebase / squash 合并（会生成未经检查的新提交），由负责人在本地 rebase 到 main 后按同一方式推送。
5. **证据归类**：CI 日志在公开仓库中任何人可见，但有保留期限；记录中引用时写运行链接与提交号，不作为发布证据，候选验收仍按 0013 留证。

## 安全

- 工作流权限只给 `contents: read`，不使用任何 secret；触发器只用 `push`、`pull_request`、`schedule` 与 `workflow_dispatch`，不用 `pull_request_target`。fork PR 只拿到只读 token，其缓存写在 PR 的 merge ref 下，不会污染 main 的缓存；保留仓库「首次贡献者的工作流须批准」设置。
- 只使用 GitHub 官方 action（checkout、setup-python、cache、upload-artifact），按完整 commit 哈希固定。Rust 工具链由 [rust-toolchain.toml](../../rust-toolchain.toml) 经托管机器自带的 rustup 安装。
- 工作分支的新推送取消该分支仍在运行的旧运行；main 上的运行不取消，每个提交都得到结果。
- 外部 PR 的代码会在托管机器上执行，Linux 上还以 root 运行权限用例；这依赖托管机器每次一次性隔离、只读 token、不使用 secret 与首次贡献者审批。将来引入任何 secret 或改用自托管机器时，须重新评估。

## 资源与已知风险

- **磁盘**：本机同一 target 依次加入默认、完整 FI、MSRV 与 Windows 目标后约 13 GiB（调试信息开启），托管机器存储为 14 GB。快速层已按默认 / FI / MSRV 与 Windows 拆成独立 job，并关闭调试信息；试点记录每个 job 的峰值占用与剩余空间，不足时继续拆分。
- **时序敏感用例**：Python 全套含按 CPU 速度标定的停服用例，LOCK 重开与 compaction 等待也受调度影响。托管机器的性能波动可能暴露新的时序问题，按根因处理。
- **端口窗口**：0019 记录的 CLI 用例端口交接窗口在分片下仍存在，处理方式不变。参照服务关闭 SO_REUSEPORT 后，端口自动分配与内核版本无关。
- **定时运行**：schedule 只在默认分支运行，负载高时可能延迟；公开仓库 60 天无活动时定时工作流会被停用（[触发事件](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows)），需要时手动重新启用。

## 实施与试点

1. 本方案经负责人与独立复审评审通过后实施。
2. 新增 `.github/workflows/` 下的工作流与 constraints；参照准备若在工作流中难以阅读，放入一个仓库内脚本，并给出本地等价命令。
3. 在工作分支试点：记录各 job 耗时、峰值内存与磁盘、缓存命中、失败及其根因，包括一次无缓存运行、一次「工作分支通过后快进 main」的实测，以及失败注入：在工作分支临时让任一平台 job 失败、构造一个非严格 xpass，确认汇总检查为 failure、分支保护阻止快进，形成 `docs/testing/0020-ci-pilot.md`；满足「判定与流程」第 2 条后，由负责人决定必须通过检查与推送方式。
4. 实施时同步：AGENTS 的知识索引与常用命令（差分的 3.x 环境改为参照 Arrow 25 环境，setup.sh 建出的环境不固定 pyarrow，不再适用于 arm64）、0004「跨平台」注明由本决策接续、0013 写明 CI 与候选矩阵的边界并链接本文、[证据的存放与复核范围](../testing/README.md) 补充 CI 日志的归类。
