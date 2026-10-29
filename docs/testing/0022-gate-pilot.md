# 0022 签发前保全门闩试点

- 状态：实施与试点完成。门闩平台探针（C）、正式环境与演练（D）、四次正式运行（E）均已执行；测试版本 `0.0.1rc904` 已签发并登记为 `candidate`，没有 tag、Release 或公开产物。0022 已于 2026-10-27 随 0021 最终采纳；冻结分支 `test/rc904` 已删除，两个环境长期保留。
- 依据：[0022](../decisions/0022-pre-signing-preservation-gate.md) 第 8 节「验证方式」中的试点计划；[0021 试点记录](0021-release-pilot.md) C 阶段「观察」。
- 范围：合入分支 `ci/0022-preserve-gate`（已删除）与受保护的 `main`；诊断分支 `diag/gate-probe-2` 与临时环境 `gate-probe-2`（均已删除）；冻结分支 `test/rc904`（试点后删除），环境 `release-signing` 与 `dry-run-signing`；发行工作流 [release.yml](../../.github/workflows/release.yml)，保全工具 [preserve.py](../../tests/release/preserve.py)。2026-10-26 至 2026-10-27（UTC）。
- 证据：按[证据的存放与复核范围](README.md)，GitHub Actions 运行日志公开、有保留期限，下文引用运行 ID 与提交号；各阶段的接口原始响应、全部 job 日志、三个已删分支（`ci/0022-preserve-gate`、`diag/gate-probe-2`、`test/rc904`）的 git bundle（删除前均已在全新克隆中恢复验证）与保全目录由负责人本地留存。正式运行的工作流产物自创建起保留 30 天；签发运行的来源证明在 GitHub 证明接口与公开透明日志中可查。每次运行与审批都经负责人逐次授权。

## 结论

- **门闩的三种结局都在正式环境上实测**：门闩处拒绝 → attest 从未分配执行机、版本未消费；格式正确但回执错误的批准 → attest 获执行机，「保全回执」失败、`sign` 跳过、版本未消费；先保存、后以保存工具输出的审批行批准 → 签发成功，本地留存与签发的字节一致。
- **先保存后签发由结构保证**：签发运行 40465396815 中，attest 等待审批期间完成保存与复核（08:26:26），批准在 08:27:31，`sign` 在 08:27:53 开始。0021 C 阶段观察到的“人工下载晚于签发”的缺口在门闩下不再出现。
- **环境预检与平台行为**：错误的保护规则（允许管理员绕过）在前置 job 即被拒绝；跳过的 job 引用不存在的环境不会自动创建该环境。
- **两次计划外失败都与门闩无关**：C 阶段第 1 次运行是探针驱动脚本的缺陷，门闩未执行；E 阶段第 1 次正式运行是 GitHub 接口的暂时性错误，在第一个构建之前失败。两者都没有重跑。
- 结局只是“已签发候选”：`0.0.1rc904` 由运行 40465396815 attempt 1 消费并登记台账，结局 `candidate`；不创建 tag、Release，不发布任何附件。

## 实施与合入（A、B）

- **A**：保全工具 `save` / `verify` / `digests` / `gate`、环境预检 `runs.check_signing_environment`、发行工作流改动（`preserve-digests` job、attest 按模式选择环境、「保全回执」位于 `sign` 之前）与自检，经三轮代码评审。本机以 EACCES 与 ENOSPC 对保存过程做文件系统故障注入，原始输出由负责人本地留存。
- **B**：单个提交 `effc8a6`（15 个文件）。工作分支 `ci/0022-preserve-gate` 上快速层 40323118408 与对照层 40323118610 均 attempt 1、全量、全部成功，三个平台 `tests/release` 445 项通过；`main` 未变时快进，`main` 推送触发的快速层 40324868088 成功。分支删除前生成 git bundle 并在全新克隆中验证可恢复。

## 门闩平台探针（C）

### 条件

- 诊断分支 `diag/gate-probe-2`（`effc8a6` 之上 4 个提交，只加一个探针工作流与驱动脚本，不合入）与临时环境 `gate-probe-2`：必需审批人只有仓库所有者，允许自审（规则内 `prevent_self_review=false`），关闭管理员绕过，无等待计时器，部署分支不限，无 secret 与变量，与 `release-signing` 的目标配置相同。
- 探针工作流三个 job：`env-read` 不引用环境、只有 `contents: read` 与 `actions: read`，以 `GITHUB_TOKEN` 执行真实的 `check_signing_environment`；`fixture` 由固定的测试夹具生成候选与 FI 摘要；`gated` 引用 `gate-probe-2`，在审批后执行真实的 `preserve.py gate` 筛选。全程不签发，没有 `id-token` 与 `attestations` 权限。
- 预算 4 次运行；不创建 `release-signing` 与 `dry-run-signing`。

### 结果

| 运行 | 提交 | 场景 | 结果 |
| --- | --- | --- | --- |
| 40327046531 | `2af76e7` | 正确回执（首次） | 计划外失败：`env-read` 未执行到门闩，见下文；`gated` 跳过、无执行机 |
| 40327697824 | `7c84e4f` | 正确回执 | `env-read`、`fixture`、`gated` 均成功；审批前 `gated` 处于等待、没有执行机；以 `preserved <候选 ID> <回执>` 批准后筛选到唯一匹配 |
| 40327943884 | `b30de54` | 错误回执（回执末位改动） | `gated` 的「保全回执」以 `expected exactly one approval … found 0` 失败 |
| 40328186070 | `31f794c` | 环境改为允许管理员绕过；另一跳过的 job 引用不存在的环境 `gate-probe-2-skipped` | `env-read` 以 `lets administrators bypass` 失败；`gated` 与引用不存在环境的 job 跳过、无执行机；`gate-probe-2-skipped` 查询为 404，没有部署记录，即未被自动创建 |

- **第 1 次运行的缺陷**：探针驱动脚本在模块顶部导入了测试模块，测试模块导入 pytest，而执行机上的系统 `python3` 没有 pytest，`env-read` 在调用环境预检之前即失败。这是探针驱动脚本的缺陷，不是门闩失败：产品工具 `preserve.py` 与 `runs.py` 只用标准库，未被执行到。修复（`7c84e4f`）只把该导入移到 `fixture` 子命令内部，经负责人授权，总预算仍为 4 次。教训：驱动脚本在推送前应以屏蔽 pytest 的解释器预演。
- 结束后关闭管理员绕过，删除 `gate-probe-2` 与诊断分支，环境列表为空；分支的 git bundle 已在全新克隆中验证可恢复。

## 正式环境与演练（D）

- **环境**：`release-signing`：必需审批人只有仓库所有者，允许自审，`can_admins_bypass=false`，无等待计时器，`deployment_branch_policy` 为空，无 secret 与变量；`dry-run-signing`：无保护规则。两者的创建请求与读回响应由负责人本地留存。
- **冻结提交**：`test/rc904` 上的 `ebb1266`（`ebb12668a1f0d186f26727e80c933d614e56d846`，基于 `main` 的 `effc8a6`），与 rc903 冻结提交的做法相同，只把版本、安装命令与打包检查改为 `0.0.1rc904`，README 标明发行流程测试、请勿安装；`.github/` 与 `tests/release/` 与 `effc8a6` 相同。推送触发的快速层 40334413222（12/12）与对照层 40334413221（11/11）均 attempt 1、全量、全部成功。
- **演练** 40337089326（`dry-run v0.0.1rc904`，`pid1_dry_run` 为空）：attempt 1，34 个 job 全部成功，两个 PID 1 job 实际执行；attest 经 `dry-run-signing`，无审批、无等待；一次签发覆盖两个演练文件（Rekor logIndex 3489234318），五个正式附件在 GitHub 证明接口均为 404；34 份 job 日志中没有 SIGSEGV。
- **演练保存**：在冻结提交的干净 worktree 中执行 `preserve.py save --mode dry-run`，末行为 `dry-run-receipt 0.0.1rc904@ebb1266…#40337089326.1 ebcd494d…`；规范文本 12 行，其 SHA256 等于回执；`verify` 通过；重复执行 `save` 只复核、不改文件。演练回执不能用于审批。

## 正式运行（E）

四次运行参数相同：分支 `test/rc904`，`commit=ebb12668a1f0d186f26727e80c933d614e56d846`，`version=0.0.1rc904`，`dry_run=false`，`pid1_dry_run=40337089326.1`。每次触发前在两个运行列表接口上间隔 60 秒两次确认没有未完成的运行，后三次另查继承运行的 jobs 接口确认接口稳定；每次只触发一次、均为 attempt 1，没有任何重跑。

| 运行 | 日期（UTC） | 场景 | 结局 | 版本 |
| --- | --- | --- | --- | --- |
| 40352352354 | 2026-10-26 | — | 前置检查因接口暂时性错误失败 | 不消费 |
| 40443119179 | 2026-10-27 | 门闩处拒绝 | attest 无执行机即失败 | 不消费 |
| 40454574315 | 2026-10-27 | 格式正确、回执错误的批准 | 「保全回执」失败，`sign` 跳过 | 不消费 |
| 40465396815 | 2026-10-27 | 保存后以正确回执批准 | 签发成功 | 消费 |

### 接口暂时性错误（40352352354）

前置检查读取继承的快速层 40334413222 attempt 1 的 jobs 时，接口返回 `stream error: stream ID 1; CANCEL; received from peer`，工具对接口错误不重试，前置检查失败；候选结果 job 的“前序 job 全部成功”守卫随之失败，其余 11 个 job（含 attest）跳过。attest 无执行机、无步骤；没有审批、待审批部署或 `release-signing` 部署记录；`runs.signing_started` 为 false；没有产物、证明、tag、Release 或台账记录。这是构建之前的安全失败，不提供门闩证据；未重跑，改以新运行继续。只读接口的有限重试记为 0021 最终采纳前的非阻断改进项，若实施须使用新的冻结提交并重做相应的快速层、对照层与演练。

### 门闩处拒绝（40443119179）

- 33 个前置 job 全部成功（含前置检查、候选结果与 `preserve-digests`），04:47:47 attest 在 `release-signing` 等待：无执行机、无步骤，审批记录为空，待审批部署只有 `release-signing`，`runs.signing_started` 为 false。
- 04:49:53 所有者经 `pending_deployments` 以 `state=rejected` 拒绝。04:49:54 attest 以 failure 结束，执行机仍为空、没有步骤、日志不存在；GitHub 对该 job 的唯一注解为 `The deployment was rejected or didn't satisfy other protection rules.`；审批记录为一条所有者对 `release-signing` 的 `rejected`；部署 7350592687 由 waiting 变为 failure。
- 候选产物的七个文件与其中的 `SHA256SUMS` 一致，七个摘要在 GitHub 证明接口均为 404；没有 tag、Release 或台账记录。

### 错误回执（40454574315）

- 等待态与上一次相同（33 个 job 成功，attest 无执行机、无审批，`runs.signing_started` 为 false）。06:32:44 所有者批准一次，备注为 `preserved 0.0.1rc904@ebb1266…#40454574315.1` 加 64 个 `0`：格式正确，回执错误。
- attest 获执行机，签发前复核成功，「保全回执」以 `expected exactly one approval with comment 'preserved <候选 ID> 9d7de54c…', found 0` 失败，`sign` 与「验证签发结果」跳过；`runs.signing_started` 仍为 false；部署 7351982234 最终为 failure。七个摘要的证明均为 404，没有 tag、Release 或台账记录。

### 正确回执并签发（40465396815）

时间线（UTC，2026-10-27）：

| 时间 | 事件 |
| --- | --- |
| 08:25:08 | attest 在 `release-signing` 等待（部署 7353693675 waiting）：无执行机、无步骤，审批记录为空，`runs.signing_started` 为 false；此前 33 个 job 全部成功 |
| 08:26:26 | 在 `ebb1266` 的干净 worktree 中执行 `preserve.py save --mode release` 完成；随后 `verify` 通过，末行 `preserved 0.0.1rc904@ebb1266…#40465396815.1 fd5c8dff…`；规范文本 12 行，其 SHA256 等于回执 |
| 08:27:31 | 所有者以该末行原文批准 |
| 08:27:34 | attest 获执行机；签发前复核与「保全回执」成功（08:27:52–08:27:53） |
| 08:27:53–08:27:56 | `sign` 一次调用签发七个主体（`Attestation created for 7 subjects`） |
| 08:27:55 | Rekor 记录写入 |
| 08:27:56–08:28:27 | 「验证签发结果」成功；运行 completed/success，34 个 job 全部成功 |

签发结果与核对：

- **一次证明覆盖七个主体**：GitHub 证明接口上七个主体各恰一份证明，按七个主体分别下载的证明包内容相同；Rekor 中一条 `dsse` 记录覆盖七个主体，UUID `108e9186e8c5677a2fa0c6539e111ee09b25f7072979c6c5bb4e3511855e525f0fa4791636956a6e`，logIndex 3510709796；签发证书身份为 `release.yml@refs/heads/test/rc904`、运行 `runs/40465396815/attempts/1`；证明的源提交为 `ebb1266`。
- **五个正式附件验签**：对本地保存的五个正式附件逐个执行 `gh attestation verify --signer-workflow …/release.yml --source-digest ebb1266…`，均通过，各返回同一个证明，主体为七个文件。
- **本地十一个文件**：`release/` 七个文件的 SHA256 与证明主体完全相同；`fi/` 四个 FI wheel 与候选结果的 `fi_wheels` 相同；规范文本中的十一行与这十一个文件逐一相同。发布准备工具 [release_set.py](../../tests/release/release_set.py) `--prepare` 对保存的七个文件输出 `release set verified: 0.0.1rc904@ebb12668a1f0d186f26727e80c933d614e56d846#40465396815.1`。
- **`post-sign/`**：签发后的第一项操作是在保全目录的 `post-sign/` 下补存证明包、Rekor 记录原文与签发证书、按七个主体摘要的 GitHub 证明计数与 Rekor 检索结果、attest 完整日志、五个正式附件的验签输出与 closeout；补存后重新执行 `verify`，输出与回执不变。
- **版本消费**：`runs.consumed_versions` 返回 `run 40465396815 attempt 1 started signing 0.0.1rc904`；台账登记运行 40465396815、attempt 1、结局 `candidate`。按 0021 第 20 条，该运行的任何 attempt 都不得重跑。
- 签发后没有 `v0.0.1rc904` 的 tag 或 Release；四次正式运行中分配过执行机的 job 日志都没有 SIGSEGV。保全目录、运行记录与全部日志由负责人本地留存。

## 耗时

| 运行 | 触发到 attest 等待审批 |
| --- | --- |
| 40443119179 | 92 分钟 |
| 40454574315 | 73 分钟 |
| 40465396815 | 76 分钟 |

审批之后到运行结束约 1 分钟。关键路径仍是 macos-x86_64 的差分与互通：40443119179 中该 job 排队 5.7 分钟、运行 75.8 分钟（差分 3.0.2 27.3、差分 2.6.17 22.1、互通 20.7），其余 job 在触发后 57 分钟内全部结束；macOS 并发上限使 8 个 macOS 测试 job 中有 4 个排队 2.8–7.5 分钟。

## 观察

- **门闩失败信息带出期望审批行**：错误回执运行的失败信息打印了本 attempt 的完整期望审批行（含正确回执）。回执由公开摘要与候选 ID 决定，不是秘密；该 attempt 已经批准过一次，不能再审批；新 attempt 或新运行的候选 ID 与回执都不同，无法挪用。是否省略留作 0021 最终采纳时的文案与可观测性改进。
- **wheel 不逐位可复现**：演练与三次走到候选结果的正式运行各自构建的四个正式 wheel 字节两两不同，sdist 四次相同，与 0021 C 阶段的观察一致；验收与签发针对的始终是本次运行的字节。

## 未采纳的提速提案

试点期间分析了发行工作流的耗时，两项提案本轮均不采纳：

- **缩减 SDK 线与 Python 端点矩阵**：现行合同要求每个标准平台验证 CPython 3.10 与 3.15 两个端点、pymilvus 2.6.17 与 3.0.2 两条 SDK、四组端点组合的完整 Python 套件、3.11–3.14 的安装与联合冒烟，以及两条 SDK 的差分、互通与 FI 恢复。删去组合属于支持声明变更，须另立决策，写明移除的组合、README 与支持矩阵的变化及补偿证据，并修订 0013、0021 与验收记录；至少等一个真实 CI 候选完整通过后，再依据实际耗时与失败分布提出。rc904 与首个真实候选继续使用完整矩阵。
- **正式运行复用演练产物**：演练只为两个演练文件签发证明，五个正式附件没有跨运行的来源绑定；正式候选结果绑定正式运行自身的构建、测试与 attempt；由正式运行为演练构建的字节签发，会把“本次正式运行签发了该字节”与“该字节由哪次运行构建”混在一起；还需要新的数据格式绑定演练运行、产物摘要、源码提交、验收结果与正式候选 ID。这会改变 0021 的候选身份与来源证明合同，若以后研究须另立设计，至少证明产物来源、预建产物未过期、正式与演练模式不能混用、证明的 invocation 语义准确，并重新完成全平台试点。正式运行继续从同一冻结提交独立重建全部附件。

发行工作流的编排调整（按 SDK 线与互通拆分 job、减少占用 macOS 并发的小 job）不改变证据范围；与只读接口的有限重试一样，0021 最终采纳时未实施，首个真实候选之后若仍需要，两者分别单独评审。编排调整其后已实施，见 [0021 试点记录「编排拆分」](0021-release-pilot.md#编排拆分)。

## 尚未执行

- **0022 最终采纳**（2026-10-27 已完成）：随 0021 最终采纳并入第 3、13、19、20 条，见 0022「对 0021 的影响」。
- **冻结分支**（已完成）：`test/rc904` 已删除，删除前生成的 git bundle 已在全新克隆中恢复验证，由负责人本地留存。
- **两个环境长期保留**：`release-signing` 与 `dry-run-signing` 是发行工作流引用的必需仓库设置，不属于试点清理（删除 `release-signing` 后正式运行在前置检查即失败；删除 `dry-run-signing` 后演练会隐式重建无保护的同名环境，掩盖配置缺失），已写入 [0021 第 19 条](../decisions/0021-release-build-in-ci.md#发布与权限)。
- **非阻断改进**：只读接口的有限重试；门闩失败信息是否省略期望审批行。
