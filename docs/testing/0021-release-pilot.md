# 0021 发行工作流试点

- 状态：合入前计数与第一次演练完成；0021「实施与试点」第 3 条中不签发正式证明的失败注入（A 阶段）已执行，发现的两个问题之一（容器 job 失败记录从未上传）已修复并补齐容器 job 的崩溃现场采集；涉及正式签发的注入（B 阶段）以测试版本 `0.0.1rc901` 完成，并发现整体重跑会删除已签发 attempt 的产物，据此在 0021 第 20 条补充签发后产物保全的操作禁令；七主体正式签发与公开下载核验（C 阶段）以测试版本 `0.0.1rc903` 与拒绝测试版本 `0.0.1rc902` 完成，新建的公开下载核验工作流正向、负向各运行一次；最终采纳与首个真实候选前的事项见「尚未执行」。
- 依据：[0021](../decisions/0021-release-build-in-ci.md)「实施与试点」第 2–4 条；合入计数按当时的 [0020](../decisions/0020-continuous-integration.md)「判定与流程」第 2 条。
- 范围：工作分支 `ci/pilot`、`test/lib-fork-lock-guard`、`ci/release` 与受保护的 `main`，发行工作流 [release.yml](../../.github/workflows/release.yml)，2026-10-22 至 2026-10-23；A 阶段失败注入的临时分支 `test/fi-a`、`test/fi-a-sha2`，容器诊断的诊断分支 `diag/container-core`、工作分支 `ci/container-diagnostics` 与验证分支 `verify/container-diag`，2026-10-24 至 2026-10-25（除工作分支外均在使用后删除）；B 阶段的临时分支 `test/fi-b1` 与台账分支 `ledger/rc901`，2026-10-25；C 阶段的临时分支 `test/fi-b3`（记录合入后已删除，删除前生成的 git bundle 已完成恢复验证）、台账分支 `ledger/rc903` 与工作分支 `ci/public-check`，公开检查工作流 [public-check.yml](../../.github/workflows/public-check.yml)，两个公开测试 Release，2026-10-25 至 2026-10-26。
- 证据：按[证据的存放与复核范围](README.md)，GitHub Actions 运行日志公开、有保留期限，下文引用运行 ID 与提交号；工作流产物需要仓库访问权限，各运行的产物自创建起保留 30 天（例如第一次演练 39907086903 的产物保留至 2026-11-22），rc901 正式运行 40189116947 attempt 1 的产物（原显示保留至 2026-11-24）已因整体重跑删除；rc903 正式运行 40215032490 的产物保留至 2026-11-24；演练的来源证明与 rc901、rc903 的透明日志记录都在公开透明日志中；两个公开测试 Release 是公开且不可变的产物。

## 结论

- **合入**：0021 的两个提交加入快速层工具自检，按 0020 第 2 条从零计数。首轮提交 `59d3a5f` 第 4 次运行出现测试自身的偶发失败，计数停止、不重跑；修复为 `3308b13`。0021 的两个提交按原内容移到修复之上为 `78d1f6f`，快速层 10/10 通过后从 `3308b13` 快进到 `main`。
- **演练**：运行 39907086903 在 `main` 的 `78d1f6f` 上以 `dry_run=true`、版本 `0.3.0rc2` 完整执行，33 个 job 全部成功，用时 80 分钟。候选结果的阶段与计数与 0.3.0rc2 候选逐项一致或差别有已知来源；Intel Mac 首次在原生机器上执行完整门槛。
- **签发**：演练只签发两个演练主体，一次调用、一个证明，绑定运行、工作流与冻结提交；五个正式文件没有任何证明。本地 `gh attestation verify`、`SHA256SUMS` 与发布准备工具的拒绝均已核实。
- 0021 中标注“待试点确认”的各项均已确认（见「待试点项」）。
- **C 阶段**：测试版本 `0.0.1rc903` 的正式运行 40215032490 一次签发七个主体，候选证据集按第 20 条完整留存；两个公开测试 Release（七主体有效的 `v0.0.1rc903` 与只有哨兵附件的 `v0.0.1rc902`）发布为不可变预发布；公开检查对前者全部通过、对后者在发行集合核对处按预期拒绝，六项本地反证均被拒绝。

## 合入前的快速层计数

| 提交 | 内容 | 快速层 | 对照层 |
| --- | --- | --- | --- |
| `59d3a5f` | 0021 方案与实施（基于 `6c56da3`） | 3 成功（39811235093、39814232133、39817074995），第 4 次失败（39819137221） | 1 成功（39811235149） |
| `3308b13` | 库内派生子进程的测试与裸句柄重开互斥 | 1 成功（39893217073）；快进后 `main` 推送触发 1 成功（39894538265） | 1 成功（39893217033） |
| `78d1f6f` | `59d3a5f` 的两个提交移到 `3308b13` 之上 | 10 成功 | 1 成功（39894565459） |

- **失败**：39819137221 的 rust-fi（ubuntu-24.04）中，core 库内一个 partition key 用例关闭后重开数据目录得到 `Locked`，同一运行其余 job 成功。根因是库内另一用例派生子进程时，子进程在 exec 前继承了裸 `DataDir` 的描述符；测试自身问题，产品代码未改。确定性复现、修复边界与验证记录在[进程内测试隔离 · 后续：库内单元测试](process-test-isolation.md#后续库内单元测试2026-10-23)，本文不重复。修复经负责人评审后单独提交，在自己的工作分支上两项检查通过后快进到 `main`。
- **`78d1f6f` 计数**：与 `59d3a5f` 的源码差别只有 `3308b13` 的四个文件。快速层 10 次：39894565413（推送触发）、39895864687、39897013298、39898018187、39898927805、39899834007、39900933799、39902040685、39903284175、39904791062，每次全部 job 成功；`compare-pass` 取自 39894565459。快进后 `main` 推送触发的快速层 39905884081 成功，即演练继承的运行。
- **被取消的对照层运行**：`59d3a5f` 上另有两次手动触发的对照层运行，是计数脚本误触发，不是测试失败。推送后按工作流列运行的接口有数分钟延迟，脚本没看到推送触发的运行，于 16:08 UTC 触发 39811406852；16:11 再次触发的 39811892416 按同分支并发规则取消了前者，自身成功。被取消的运行在该提交上留下一条 `compare-pass` failure，不影响后续提交。计数脚本随后改为合并按提交与按分支两个接口的结果，并在触发前间隔 60 秒两次确认没有未完成的运行。

## 演练

运行 39907086903：`dry-run v0.3.0rc2`，从 `main` 触发，`github.sha` 为 `78d1f6f58cc28b02e0a4fe2fbe49e61734abb2b6`，attempt 1；继承快速层 39905884081 与对照层 39894565459（各 attempt 1）。

### 冻结身份与附件

- 归档 SHA256 `aa599a8983d2afa14404054934fbedf9e90c8313c05ccba8a2f68b32915c6ced`；源树 437 项（435 个普通文件、2 个软链），清单 SHA256 `39d37c90dfcd92d2261849977a9b2c60cc96883743374f9072e3a764cf202a12`；产品 / 构建输入 127 项，清单 SHA256 `79da36fcff9b4e91896abfcbe86c9ab3a12d07b7b3fe8264c29257b24271407b`；`tool_versions` 为 git 2.55.0 / CPython 3.12.3。candidate job 核对了 identity job 上传的归档与 `archive_sha256` 一致。
- 五个正式附件与四个 FI wheel 的 SHA256 见候选结果 `dry-run-candidate-0.3.0rc2-39907086903-1.json`；它们与已发布的 0.3.0rc2 不是同一份源码与构建环境，字节不同，不作为该 RC 的复现证据。

### 阶段与计数

与 [0.3.0rc2 候选验收](0.3.0rc2-acceptance.md#三标准层实跑)对照（“Mac”为两个 macOS 平台，“Linux”为两个 Linux 平台）：

| 阶段 | 演练 | 0.3.0rc2 候选 | 说明 |
| --- | --- | --- | --- |
| Rust 默认 | Mac 361 / Linux 362 | 同 | |
| Rust 完整 FI（顶层） | Mac 484 / Linux 485 | core 459 / 460 + grpc 25 | 相同；演练按顶层 harness 合计 |
| CPython 3.10 / 3.15 × 两 SDK 全套 | 各 473 | 各 473 | |
| 两 SDK 完整差分（分片） | 各 421，118 组 | 各 420 | rc2 之后 `b788a83` 新增参照端口复用的回归用例 |
| Arrow 15 / 25 互通 | 211 | 211 | |
| FI wheel 恢复专项，两 SDK | 各 5 | 各 5 | |
| 3.11–3.14 安装与联合冒烟 | 四平台均通过 | 三平台通过 | |
| 正式 / FI 归档与二进制检查 | 四平台均通过 | 通过 | |
| LOCK / strace / PID 1（Linux） | 均通过；strace 85 条成功调用 | 同 | |
| sdist | 根与 `-m` 构建、PEP 517 安装、三类拒绝、解包后 MSRV 默认与完整 FI；随包产品输入 125 项 | macOS 上同类检查，125 项 | 演练在 Linux x86_64 manylinux 容器内执行 |
| 上一版本读取 | 6 例通过 | — | 读取方为已发布 0.3.0rc2 的 Linux x86_64 wheel |

平台与证据层级的变化：

- **Intel Mac**：0.3.0rc2 只有 Rosetta 上的构建与冒烟；演练在 `macos-15-intel` 上原生执行与 macOS arm64 相同的全部阶段，包括 CPython 3.15 全套、差分、互通、完整 FI Rust 与 FI wheel 恢复专项。
- **Linux**：两架构的标准层在托管机器上的固定 manylinux_2_28 容器内以 UID 1001（容器内无 passwd 条目）执行，内核 6.17.0-1022-azure。0.3.0rc2 的 x86_64 在原生远端主机、arm64 在本机 Docker VM 上执行。
- **工具自检**：0.3.0rc2 在候选中执行；演练按 0021 第 2 条从继承的快速层运行取得，不在发行工作流中重复。

### 环境

- 构建：四个平台均为 rustc / cargo 1.98.1、maturin 1.15.0、uv 0.12.23；macOS 为 15.7.9 / Xcode 16.4（arm64 镜像 `macos15 20260907.0337.1`，Intel `macos15 20260824.0482.1`）；Linux 为 glibc 2.28 容器，记录托管机器标签与容器镜像摘要。
- PID 1：两个 job 在托管机器上（`ubuntu24 20261012.334.1` / `ubuntu24-arm64 20261012.149.1`，Docker 28.0.4）拉取 0021 第 10 条固定的镜像。x86_64 为 `python@sha256:db6ee517…`，本地镜像 ID `sha256:fbff8a8ef46816a3d6872b3f98c4548892422e6686c0b38703575149dfa0b38a`；arm64 为 `python@sha256:f3902572…`，镜像 ID `sha256:23b459407c3482801a9ca6adb18ca6123559a0b518efd34f67fc946064e99f91`。其余阶段的 `pid1_image` 均为 `not-used`。
- Linux CPython 3.15 的 orjson：两个 orjson job 从 SHA256 为 `d1de5eb0…186c4f` 的 `orjson==3.13.0` sdist 构建（rustc 1.98.1、maturin 1.15.0），x86_64 wheel SHA256 `7c311a92c4aa96b536faff1f5cb57c74af8a508bacb19e61f2028e82291584ff`，arm64 `6e16d888bb74c38720c181bb10adb8525c2aa3d81407cd9b24ec08f4cd749eaa`，最高 GLIBC 依赖均为 2.28；两条 SDK 安装同一份 wheel，并在 3.15 全套之后核对环境中实际加载的就是这份。

### 耗时

| 阶段 | 运行（分钟） |
| --- | --- |
| 前置检查、冻结身份 | 0.2、0.1 |
| 构建：linux-aarch64 / linux-x86_64 / macos-arm64 / macos-x86_64 | 2.9 / 4.0 / 5.3 / 6.5 |
| orjson × 2 | 1.1–1.3 |
| 装配 | 0.2 |
| sdist、上一版本读取、Linux 系统检查（LOCK / strace）、PID 1 | 5.6、1.8、1.9、0.9–1.1 |
| 静态检查 / FI / Python 全套（四平台） | 3.0–8.7 / 1.2–3.4 / 15.0–25.4 |
| 差分与互通：linux-aarch64 / macos-arm64 / linux-x86_64 / macos-x86_64 | 31.8 / 39.4 / 49.8 / 63.6 |
| 候选结果、演练签发 | 0.1、0.4 |

总时长 80 分钟，关键路径是 macos-x86_64 的差分与互通：装配后排队 8.8 分钟，运行 63.6 分钟。macOS 的 8 个测试 job 中有 3 个装配后排队 3.4–8.8 分钟；Linux job 没有排队。

### 签发与核验

- attest job 以一次调用签发两个演练主体：`dry-run-candidate-0.3.0rc2-39907086903-1.json` 与 `dry-run-SHA256SUMS-39907086903-1`，job 内的“验证签发结果”步骤通过。
- 本地下载产物后，用 `gh attestation verify --repo … --signer-workflow …/release.yml --format json` 分别验证两个主体：各返回同一个证明，主体恰为这两个文件，`invocationId` 为 `runs/39907086903/attempts/1`，工作流 `.github/workflows/release.yml`（`refs/heads/main`），`resolvedDependencies[0].digest.gitCommit` 为 `78d1f6f`。
- 五个正式文件逐个执行 `gh attestation verify` 都返回 HTTP 404，没有任何证明。
- `shasum -a 256 -c dry-run-SHA256SUMS-39907086903-1` 六项全部 OK；`candidate.py check` 接受该候选结果（`mode=dry-run`）。
- 发布准备工具 [release_set.py](../../tests/release/release_set.py) 拒绝演练产物：原样交给它时找不到正式候选结果；把两个演练主体改成正式文件名后，以“文件名与内容不符”拒绝。

### 待试点项

0021 中写明须在托管机器上确认的各项都已确认：两个 PID 1 镜像可拉取并成功执行；容器 job 内 `runner.temp` 可用；Intel Mac 上 CPython 3.15、两条 SDK、参照依赖与资源均可行，全部阶段通过；orjson 现场构建并被实际加载；macOS job 的空 `container` 配置不影响执行；签发后立即验签成功；Linux 容器以 UID 1001、无 passwd 条目运行，各阶段不受影响。

## 观察

- 容器 job 的环境只能记录托管机器标签与容器镜像摘要，看不到宿主镜像版本；PID 1 job 不在容器内，记录了完整的宿主镜像版本。与 0021 第 12 条的预期一致。
- 演练总时长主要取决于 macOS 差分与互通及 macOS 并发排队。

## 失败注入 A 阶段（不签发正式证明）

按 0021「实施与试点」第 3 条，在临时分支上执行不涉及正式签发的注入。每项都须使工作流失败，且不产生可被误认为候选通过的候选结果。

### 条件

- **测试版本**：`0.0.1rc909`（Cargo `0.0.1-rc.909`），属于负责人保留的发行流程测试号段 `0.0.1rc901`–`0.0.1rc909`，只用于 GitHub 发行流程测试，不上传任何包索引。A 阶段全部运行使用该版本，从不签发，结束后仍未消费，作为 A 阶段的哨兵不再复用。
- **提交**：`test/fi-a` 上的 `e624323`（下称 B0，基于 `main` 的 `72fd39f`）把版本、安装命令与打包检查改为测试版本，README 标明发行流程测试、请勿安装；`test/fi-a-sha2` 上的 `c55c22e`（下称 B2）在 B0 上只加一行注释，只用于“引用的演练不是同一提交”。两者都不合入。
- **分支内的签发闩**：attest job 去掉 `id-token: write` 与 `attestations: write`；sign 与“验证签发结果”两个步骤的条件恒为假，从不执行，因此不会被前置检查算作签发已开始；正式运行在签发前复核之后由一个闩步骤直接失败；演练运行的 attest 成功结束但不签发任何文件。
- **注入入口**：发行工作流增加一个只在本分支存在的 `inject` 输入，每个取值只在 `linux-x86_64` 上触发一种注入；CI 增加只在本分支存在的 `inject_failure` 输入，用于在同一提交上制造一次失败的新运行。
- **每次运行后的断言**：`main` 仍为 `72fd39f`，台账未变；没有 `v0.0.1rc909` 的 tag 或 Release；每个 attempt 的签发步骤都没有开始；没有正式候选结果产物；运行产出的全部 wheel、sdist、候选结果与 `SHA256SUMS` 在证明接口上都返回 404。断言先在第一次演练 39907086903 上做阳性对照，正确检出其签发步骤与两个演练主体的证明。
- B0 与 B2 推送触发的快速层（40107397122、40107397329）与对照层（40107397114、40107397322）全部成功。

### 结果

全部注入都使工作流失败；候选结果 job 的“前序 job 全部成功”守卫失败，attest 跳过（复核一项除外，见表），断言全部通过。

| 层 | 场景 | 运行 | 拦截位置与信息 |
| --- | --- | --- | --- |
| 前置 | 从非冻结提交触发（B2 分支、输入 B0） | 40109447943 | “提交身份”步骤 |
| 前置 | 版本已存在（`0.3.0rc2`） | 40109461852 | `version already consumed: tag v0.3.0rc2 exists` |
| 前置 | 正式运行未引用演练 | 40109604512 | `a release run must reference a dry run of the same commit that included PID 1` |
| 前置 | 引用的不是演练（引用 40109604512） | 40109686137 | `run 40109604512 is not a dry run of 0.0.1rc909` |
| 前置 | 同一提交的新快速层运行进行中（40109795804，随后成功） | 40109809204 | `latest … run 40109795804 attempt 1 is in_progress/None` |
| 前置 | 同一提交的新快速层运行失败（40110481614，`inject_failure`） | 40110547726 | `latest … run 40110481614 attempt 1 is completed/failure`；之后手动全量快速层 40110647027 成功，恢复可继承状态 |
| 签发前复核 | 演练前置检查之后对同一提交新开快速层（40117662412） | 40117626588 | 其余 33 个 job 全部成功；attest 的签发前复核拒绝：同一提交的快速层运行集合多出 40117662412；sign 跳过 |
| 前置 | 引用失败的演练（40117626588） | 40122919119 | `dry run 40117626588 attempt 1 is completed/failure` |
| 前置 | 引用其他提交的演练（B2 引用 B0 上的 40111344765） | 40122978131 | `dry run 40111344765 is for e624323…, not c55c22e…` |
| 构建 | 构建失败 | 40123088263 | build (linux-x86_64) 的“构建”步骤 |
| 构建 | 未跟踪的构建输入 | 40123831091 | `untracked build input crates/core/src/injected_untracked.rs` |
| 装配 | 正式 wheel 缺失 | 40124927840 | `formal set differs`，列表缺 x86_64 Linux wheel |
| 装配 | 正式集合多出文件 | 40125885515 | `formal set differs`，列表多出 cp311 wheel |
| 装配 | 构建记录之后修改正式 wheel | 40126580385 | `build-linux-x86_64/formal/…x86_64.whl does not match its build record` |
| 验收 | FI wheel 混入正式集合（覆盖同名正式 wheel） | 40127752551 | 装配按文件名核对无法区分（FI wheel 与正式 wheel 同名）；tests (linux-x86_64, static) 与 (python) 的发行物检查均报 `formal/FI artifact mismatch` |
| 验收 | 差分环境装入同名的其他构建（FI wheel） | 40133677393 | tests (linux-x86_64, compare)：`environment loads native module 0c21…, wheel contains 3d6e…` |
| 验收 | 正式模式下测试失败（期望计数加 1） | 40138739844 | tests (linux-x86_64, python) 在分片计划的收集数核对处失败（`--expect 474`）；失败记录因下文第 2 项缺陷未上传，本地以同一命令复现为 `收集数量 473 与预期 474 不符` |

- **作为引用的演练**：B0 上的演练 40111344765 全部 33 个 job 成功（81.5 分钟），两个 PID 1 job 成功；签发前复核通过，sign 跳过，25 个产物摘要均无证明。构建、装配与验收层的正式运行都引用它。
- **未消费核实**：以 [runs.py](../../tests/release/runs.py) 的 `consumed_versions` 检查全部 15 次标题为 `release v0.0.1rc909` 的运行的每个 attempt，结论为未消费。
- **只由自检覆盖**：台账格式错误或同一版本重复（台账按 `main` 读取，临时分支无法注入）；结论为成功却缺少成功 PID 1 job 的演练（现有工作流中成功的演练必然包含成功的 PID 1 job，只能来自改过的工作流），自检覆盖只有一个、没有与其中一个失败三种情形。纯文档运行被拒已由 40102059658 实测（见 [0020 试点记录](0020-ci-pilot.md#纯文档跳过)）。

### 发现

1. **jobs 接口的步骤数据滞后**：演练 40111344765 的 attest 于 17:45:07 完成，17:50 查询时步骤列表为空，17:52 只有两个步骤且其中一个仍显示进行中，约 17:57 才完整（sign 为 `completed/skipped`）；演练 40117626588 的 attest 完成后 16 秒内步骤数据即完整，运行中未执行的步骤状态为 `pending`。前置检查的签发判据遇到已分配执行机而步骤为空或不全，以及状态为 `pending` 的 sign，都按已开始处理，误判只会偏向拒绝：紧接在一次正式运行之后的同版本前置检查可能把未签发的版本判为已消费，数据补全后恢复；未观察到放行方向的误判。B 阶段在步骤数据补全后再触发依赖该判据的运行。
2. **Linux 容器 job 的失败记录从未上传**：tests 与 linux-system 的“保存运行记录”使用多行 `path`，在容器 job 中只有第一行被换算成容器内路径（`/__w/_temp/…`），其余行仍是宿主机路径，结果为 `No files were found … No artifacts will be uploaded`；`if-no-files-found: ignore` 使该步骤显示成功。0021 第 21 条“只在失败时上传的诊断记录保留 14 天”在 Linux 容器 job 上因此不成立。修复见下一节。

## 容器诊断修复

### 提交

- `f66bd15`：失败时由 [collect_records.sh](../../.github/ci/collect_records.sh) 把运行记录按相对路径复制到 `$RUNNER_TEMP/failure-records`，上传只给这一个路径；全部工作流中容器 job 的 artifact 步骤只有上述两处多行路径。[test_workflow_paths.py](../../tests/ci/test_workflow_paths.py) 检查容器 job 的 artifact 步骤只用单行路径，在改动前的 `release.yml` 上检出这两处。
- `b880fdd`：发行工作流中运行产品或测试代码的容器 job（tests、linux-system、sdist、上一版本读取）接入崩溃现场采集，做法与边界见 [0020](../decisions/0020-continuous-integration.md) 公共约定 10；容器分支的自检 3 项。两个提交使工具自检由 43 项变为 48 项，按负责人确定的范围在 `b880fdd` 上计数（见 [0020 试点记录](0020-ci-pilot.md#发行工作流容器诊断修复的计数)）。

### 托管机器的事实

只用于诊断的工作流（运行 40166358454、40166490001）在 `ubuntu-24.04` 与 `ubuntu-24.04-arm` 上，分别以宿主机、固定 manylinux_2_28 镜像中的 UID 1001 与 root 执行：

- 宿主机 `kernel.core_pattern` 为 `|/usr/lib/systemd/systemd-coredump …`，`systemd-coredump.socket` 为 active；容器内该文件只读，core 大小限制已为 unlimited；镜像自带 gdb 8.2（`8.2-20.1.el8_10`），镜像中没有 `zstd` 命令，CPython 3.14 的 `compression.zstd` 可用。
- 把宿主机 `/var/lib/systemd/coredump` 只读挂入容器后，四种组合中 `ctypes.string_at(0)` 的 SIGSEGV 与 `os.abort()` 的 SIGABRT 各在约 1 秒内留下一个 `.zst` core；UID 1001 的 core 带 `user:1001:r--` ACL，可直接读取；core 中记录的是容器内路径，解压后 gdb 打出 `strlen`（或 `__strlen_evex`）与 `raise` → `abort` 的原生栈。

### 定向验证

验证分支 `verify/container-diag` 的 `a7a6e60` 在 `b880fdd` 上增加不合入的脚手架：A 阶段相同的签发闩（演练不签发任何文件），以及 `inject=crash` 输入：在 4 个平台 tests 的 python 部分完成第一组验收后、linux-system 完成 LOCK 检查后、sdist 与上一版本读取的测试步骤中，让进程先 SIGSEGV 再 SIGABRT（tests、linux-system 与上一版本读取在已加载 `milvus_lite._native` 的 SDK 解释器中执行），随后使该步骤失败。版本保持 `0.3.0rc2`，演练模式。该提交推送触发的快速层 40167058332 与对照层 40167058348 全部成功。

| 项 | 运行 | 结果 |
| --- | --- | --- |
| 干净演练 | 40168269949 | 33 个 job 全部成功（81.6 分钟），新增的采集与报告步骤不改变结论；没有失败记录产物；25 个产物摘要均无证明，sign 跳过，只有未签名的演练候选结果 |
| 注入演练 | 40173299185 | 失败的正是 8 个注入 job，失败步骤均为原测试步骤，“崩溃现场”步骤本身成功、不改变 job 结论；其余 23 个 job 成功；候选结果 job 失败，attest 跳过；18 个产物摘要均无证明，没有候选结果产物 |

注入演练逐项：

- **失败记录**：产生 6 个 artifact。tests python 的 4 个平台各含 `acc-3.10-2.6.17/` 下 8 个非空日志（每个 artifact 合计 2.2–2.7 KB）；linux-system 两个架构各含 `lock/` 下 4 个文件，均为 0 字节（LOCK 文件本身为空、锁检查成功时 stderr 为空，strace 目录在注入时尚未生成），证明上传链路有效，但不提供日志内容。
- **Linux 容器的原生栈**：tests python（x86_64、arm64，UID 1001）、linux-system（x86_64、arm64，root）、sdist（UID 1001）与上一版本读取（UID 1001）各取得 2 个 core，SIGSEGV 与 SIGABRT 都由 gdb 打出收到信号线程的原生栈（`__strlen_evex` / `strlen` → `_ctypes` → `ffi_call`；`raise` → `abort`）。tests 的 SDK 解释器是指向 `/opt/python` 的软链，gdb 提示 `core file may not match specified executable file`，栈仍可读。
- **macOS**：两个平台走系统崩溃报告路径，不受 `CRASH_CORE_DIR` 影响。macos-15 读到 SIGSEGV 与 SIGABRT 两份报告；macos-15-intel 只读到 SIGSEGV 一份：报告步骤在当时已有的报告都可解析时即结束等待，第二份报告之后才写出。这是 `dd86a57` 的 macOS 等待条件本来的局限，快速层与对照层相同，本次未修改；之后由 `c8349e0` 修复：报告全部可解析且 20 秒内没有新增或改写才读取，最长等 90 秒，没有报告时等满上限；`tests/ci` 新增 8 项自检，本机 macOS 上连续两次真实崩溃在 22 秒内收齐两份报告，两份诊断报告由负责人本地留存。托管机器上没有再做两次崩溃的注入。
- **未覆盖**：PID 1 job 的进程运行在探针另起的容器中，core 交给宿主机，但宿主机上没有对应镜像的文件；只编译、不运行产品的 build 与 orjson job 不接入。

## 失败注入 B 阶段（签发开始即消费）

按 0021 第 3 条与「实施与试点」第 3 条，验证正式签发步骤一旦开始即消费版本，以及前置检查对已消费版本的各项判据。每个正式步骤经负责人逐次授权。

### 条件

- **测试版本**：`0.0.1rc901`（Cargo `0.0.1-rc.901`），同一版本按顺序覆盖四种拒绝；`0.0.1rc902` 留空，A 阶段的 `0.0.1rc909` 不复用。
- **冻结提交**：`test/fi-b1` 上的 `86d0b56`（基于 `main` 的 `4412b5d`），把版本、安装命令与打包检查改为测试版本，README 标明发行流程测试、请勿安装；attest 去掉 `attestations: write`、保留 `id-token: write`，使正式运行的签发步骤真正开始；演练跳过 sign 与其验证步骤。分支已删除，其来源（git bundle 与 patch）由负责人本地留存。
- 推送触发的快速层 40181113707 与对照层 40181113746 全部成功。作为 PID 1 引用的演练 40182194888 全部 33 个 job 成功（约 81 分钟），两个 PID 1 job 成功，sign 与验证步骤跳过，25 个产物摘要均无证明。
- **每步后的核对**：attest 的步骤数据完整后再判定；按七个主体的摘要查询 GitHub 证明接口与 Rekor（`index/retrieve`）。Rekor 查询先以第一次演练 39907086903 的演练候选结果做阳性对照（检索到其条目），以未签发的 sdist 做阴性对照（返回空）。`main`、台账、tag 与 Release 逐步核对。

### 结果

| 步骤 | 运行 | 结果 |
| --- | --- | --- |
| 签发开始后失败 | 40189116947 attempt 1 | 前置检查与全部前序 job、候选结果成功；签发前复核成功；sign 于 12:52:16 开始，以 `Failed to persist attestation: Resource not accessible by integration` 失败，即签名完成后存入 GitHub 证明接口被拒；验证步骤跳过 |
| 整体重跑 | 40189116947 attempt 2 | 前置检查拒绝：`version already consumed: run 40189116947 attempt 1 started signing 0.0.1rc901`；attest 跳过 |
| 台账更新前的同版本新运行 | 40196067412 | 前置检查拒绝，原因同上；attest 跳过 |
| 台账登记后的同版本新运行 | 40199042185 | 前置检查拒绝：`version already consumed: version 0.0.1rc901 is in the ledger`；attest 跳过 |

- **签发的实际结果**：七个主体在 GitHub 证明接口上均返回 404；Rekor 中有一条覆盖七个主体的 `dsse` 记录，UUID `108e9186e8c5677ae13a8a773796a946264bda09d9c711e985ea46c7a5fb056268aac12e04c1fe1b`，logIndex 3461575068，写入时间 2026-10-25 12:52:17 UTC；签发证书的身份为 `release.yml@refs/heads/test/fi-b1`、提交 `86d0b56`、运行 `runs/40189116947/attempts/1`。即“透明日志已有记录、GitHub 侧没有可查询证明”的结果不明状态，与 0021 第 3 条在签发开始时即消费版本的理由一致。后三步之后，七个主体在 Rekor 中仍各只有这一条记录。
- **台账**：`871e07d` 登记 `0.0.1rc901`、运行 40189116947、attempt 1、结局 `signing-failed`，只改台账文件；在 `ledger/rc901` 上全量快速层 40196712304 与对照层 40196712280 成功后快进 `main`，`main` 推送触发的快速层 40198124669 成功。
- 全程没有 `v0.0.1rc901` 的 tag 或 Release；后三步都没有产物。
- **步骤数据**：本阶段 attest 的步骤数据都在 job 结束后十几秒内完整，例如 40189116947 attempt 1 于 12:52:32 已含 `sign=completed/failure`，没有出现 A 阶段的滞后。

### 重跑删除了已签发 attempt 的产物

- 40189116947 attempt 1 结束时有 `release-candidate-v0.0.1rc901` 等工作流产物（显示 2026-11-24 过期）。整体重跑为 attempt 2 之后，该运行的产物数为 0，按产物名检索整个仓库也为 0。
- 重跑前只下载了候选结果产物，七个来源证明主体由负责人本地留存；同一重跑也删除了 `fi` 产物，候选结果中记录的四个 FI wheel 已无法恢复，rc901 的本地留存因此只是七个来源证明主体，不是完整的候选证据集。七个主体的核对：`SHA256SUMS` 六项核对通过，候选结果记录的五个正式附件哈希与文件一致，七个摘要在 Rekor 中都检索到上面的记录；留存中另有 Rekor 记录原文、签发证书与 attest job 的签发日志。
- **后果**：对真实候选，如果对一次签发已开始（尤其已签发成功）的正式运行重跑，候选文件会在负责人下载之前被删除，而证明不可撤回、版本已消费，只能换版本重来；前置检查能拦住重跑后的再次签发，拦不住重跑开始时的删除。负责人将其定为 0021 正式采纳的阻断项，操作禁令已补充到 0021 第 20 条。只重跑失败 job 是否同样删除产物未实测。

## C 阶段：七主体正式签发与公开下载核验

按 0021 第 13、20 条与「实施与试点」第 2、3 条，以测试版本完成一次七主体正式签发，发布一个七主体有效、一个刻意无效的公开测试 Release，并以新建的公开下载核验工作流各核对一次。签发、台账、提交与快进、tag、草稿、发布与每次公开检查都经负责人逐次授权。

### 条件

- **测试版本**：`0.0.1rc903`（Cargo `0.0.1-rc.903`）用于七主体有效的 Release；B 阶段留空的 `0.0.1rc902` 用于拒绝测试，只发布 Release，不构建、不签发；`0.0.1rc904` 备用。
- **冻结提交**：`test/fi-b3` 上的 `36c9ade`（基于 `main` 的 `80d72d3`），只把版本、安装命令与打包检查改为测试版本，README 标明发行流程测试、请勿安装；`release.yml` 与 `80d72d3` 逐字相同，B 阶段的修改不带入，此后发行工作流冻结。分支的来源（git bundle 与 patch，已验证可恢复）由负责人本地留存。
- 推送触发的快速层 40202243779（12/12）与对照层 40202243804（11/11）全部成功。作为 PID 1 引用的演练 40203730325 全部 33 个 job 成功，两个 PID 1 job、sign 与验证步骤成功；其证明（Rekor logIndex 3462916646）主体恰为两个演练文件，五个正式附件在 GitHub 证明接口均为 404，在 Rekor 中检索不到。

### 正式签发

- 运行 40215032490（`release v0.0.1rc903`，`pid1_dry_run=40203730325.1`）只触发一次，attempt 1，33 个 job 全部成功。sign 于 2026-10-25 18:13:44–18:14:00 UTC 以一次调用签发七个主体（`Attestation created for 7 subjects`），验证步骤成功。
- GitHub 证明接口上七个主体各有一份证明，为同一个证明；Rekor 中一条 `dsse` 记录覆盖七个主体，UUID `108e9186e8c5677a9845cd3429e964f0341681662b96a43736081727fcb9afeb773af6cfb780e8dd`，logIndex 3464728511，签发证书身份为 `release.yml@refs/heads/test/fi-b3`、运行 `runs/40215032490/attempts/1`；证明的主体恰为七个文件，源提交 `36c9ade`。
- **产物保全**按 0021 第 20 条执行：发现候选结果 job 完成后立即下载候选结果与 `fi` 两个产物，四个 FI wheel 与候选结果的 `fi_wheels` 逐项一致；签发后的第一项操作是发布准备工具，结果为 `release set verified: 0.0.1rc903@36c9ade…#40215032490.1`；完整候选证据集（七个主体、四个 FI wheel、证明包、Rekor 记录与签发证书、attest 日志）由负责人本地留存，从留存副本以证明包离线执行 `gh attestation verify`（限定签发工作流与源提交）七项通过，发布准备工具再次通过。全程没有重跑。
- **台账**：`f8343dd` 登记 `0.0.1rc903`、运行 40215032490、attempt 1、结局 `candidate`，只改台账文件；在 `ledger/rc903` 上全量快速层 40261403924 与对照层 40261403913 成功后快进 `main`，`main` 推送触发的快速层 40262462083 成功。

### 公开下载核验工作流

- `f39e2c5` 新增 [public-check.yml](../../.github/workflows/public-check.yml) 与 [public_release.py](../../.github/ci/public_release.py)：手动触发，verify 只在 `main` 上运行，默认分支的检查器检出绑定运行提交，与冻结 tag 分目录检出；工作流没有任何写权限；除 checkout 的只读仓库访问外，显式的 `GH_TOKEN` 只用于读取 Release 与查询证明，附件不带认证从公开地址下载，并与 API digest 核对；发行集合交给冻结 tag 中的 [release_set.py](../../tests/release/release_set.py)，通过后 tag 须指向候选冻结提交；四个平台在只装了固定 SDK 的干净 Python 3.12 环境中执行冻结 tag README 的安装命令（拆成参数、不经 shell），以 pip report 绑定本平台的公开 wheel 与核验过的哈希，再核对冒烟输出的版本与 SDK；任何 job 失败或跳过都使 public-pass 失败。负责人复审发现默认分支检查器起初没有绑定运行提交、也没有 `main` 守卫，从其他 ref 触发时可能用被改过的检查器自证通过；修复后加入静态自检，删除 `ref` 或守卫的变异均被检出。
- 在 `ci/public-check` 上快速层 40266236206（12/12，工具自检 112 项）与对照层 40266236193（11/11）成功后快进 `main`，`main` 推送触发的快速层 40267793877 成功。
- 守卫写在工作流文件内，从修改过工作流的其他分支触发不受其约束；因此引用公开检查结果时核对运行元数据：`head_branch=main`、`head_sha` 为当时的 `main`、工作流为已合入的 `public-check.yml`。下面两次运行均满足。

### 两个公开测试 Release

| Release | tag（对象 → 提交） | 附件 | 发布时间（UTC） |
| --- | --- | --- | --- |
| `v0.0.1rc903`（ID 423863200） | `2272a05` → `36c9ade` | 七个来源证明主体；四个 FI wheel 不公开 | 2026-10-26 04:44:03 |
| `v0.0.1rc902`（ID 423872968） | `7b92c93` → `f39e2c5` | 只有哨兵 `REJECTION-TEST.txt`（237 字节，不含可安装内容） | 2026-10-26 05:01:13 |

- 两个 Release 都由负责人以本人认证在本地创建 annotated tag、草稿并发布，工作流不获得 `contents: write`。标题与正文标明发行流程测试（rc902 为拒绝测试）、不是产品版本、请勿安装。草稿阶段核对附件名称、大小与 API digest 以及标题、正文，并经认证下载到新目录，与本地留存或源文件逐字节比对，负责人审核后以 `--prerelease --latest=false` 发布。创建 rc903 草稿前，发布准备工具在台账登记后再次通过（台账条目正是这次运行，结局 `candidate`）。
- 发布后两个 Release 均为预发布且不可变，附件、标题、正文与草稿一致，tag 未变；`/releases/latest` 发布前后都返回 404（全部 Release 均为预发布）；Release 页面、`expanded_assets` 与 API 可匿名访问。
- rc903 的正文保持冻结。其中 0021 链接指向 `main` 而不是冻结提交，示例证明命令没有 `--source-digest`；两项记为下一个发行流程测试或自动化模板的非阻断改进项，不为修改正文新建 Release 或替换 tag。

### 公开检查

| 运行 | 版本 | 结果 |
| --- | --- | --- |
| 40271363620 | `0.0.1rc903` | success：verify、四个平台 install 与 public-pass 全部成功，用时约 2 分钟 |
| 40272184378 | `0.0.1rc902` | failure（预期）：verify 在发行集合核对处失败，install 未执行，public-pass 失败 |

- 两次运行都在 `main` 的 `f39e2c5` 上由 `workflow_dispatch` 触发，attempt 1，各只触发一次；工作流至今只有这两次运行。
- **正向**：七个附件匿名下载，与 API digest 一致；冻结 tag 的发布准备工具输出 `release set verified: 0.0.1rc903@36c9ade…#40215032490.1`，tag 即候选冻结提交。四个 install 取得同一份 `verified-sums` 产物，pip report 绑定各平台的公开 wheel 与哈希，冒烟输出 `version=0.0.1rc903`、`sdk=3.0.2`、四个阶段全部通过（两台 Mac 为 CPython 3.12.10，两台 Linux 为 3.12.15）。
- **负向**：verify 的版本格式、Release 属性（附件只有 `REJECTION-TEST.txt`）与匿名下载成功，发行集合以 `release set rejected: expected one candidate result for 0.0.1rc902, found []` 失败，其后步骤跳过。install 依赖的结果为 skipped，四个平台都没有执行；GitHub 对未展开的矩阵只列出 1 个名为 `install (${{ matrix.platform }})` 的 skipped 占位 job，这是展示方式，不作为判据。public-pass 实际运行，以 `VERIFY: failure`、`INSTALL: skipped` 失败。
- 两次运行后，两个 Release、两个 tag、`main` 与 `/releases/latest` 均未变化。

### 本地反证

匿名下载 rc903 的七个附件（与本地留存逐字节一致），用 tag `v0.0.1rc903` 中的 `release_set.py`（对原样附件通过），每项在独立副本上执行 `release_set.py --dir <副本> --version 0.0.1rc903 --repo GrittyChen/milvus-lite-rust`，全部以退出码 1 拒绝：

| 变异 | 拒绝原因 |
| --- | --- |
| 删除候选结果 | `expected one candidate result for 0.0.1rc903, found []` |
| 篡改候选结果（插入一个空格） | `attestation verify failed for candidate-…json`：该摘要在证明接口上为 404 |
| 候选结果的 `mode` 改为 `dry-run` | 同上，证明验证先拦截 |
| 混入演练候选结果 `dry-run-candidate-0.0.1rc903-40203730325-1.json` | `attachment set differs: missing [], extra ['dry-run-candidate-…']` |
| 缺少 macOS x86_64 wheel | `attachment set differs: missing ['…macosx_11_0_x86_64.whl'], extra []` |
| 增加 `NOTES.txt` | `attachment set differs: missing [], extra ['NOTES.txt']` |

改动候选结果内容都会改变其摘要，在解析内容之前即被证明验证拒绝。各项的命令、输入清单与完整输出由负责人本地留存；公开 Release 与本地留存原件未被改动。

### 观察

- 候选结果 job 于 18:13:33 结束，attest 于 18:13:35 开始，sign 18:13:44 开始；按 30 秒间隔发现候选结果 job 完成后下载，开始下载时 sign 已经完成。当前人工轮询流程无法保证下载先于签发，第 20 条的“先下载”在现有流程下只能缩小产物只存在于工作流中的时间窗；这是技术兜底需要消除的缺口，例如在 attest 前增加必须成功的外部保存或门闩步骤，使保存成为签发的前置条件。
- 正式运行重新构建的四个正式 wheel 与演练 40203730325 的同名 wheel 字节不同，sdist 相同：本次演练与正式运行的四个 wheel 未逐位一致，当前工作流尚未实现 wheel 的逐位可复现；验收针对的始终是本次运行签发的字节。

## 尚未执行

- **签发后产物保全的技术兜底**：[0022 签发前的候选保全门闩](../decisions/0022-pre-signing-preservation-gate.md)用于消除当前人工流程无法保证下载先于签发的缺口（见 C 阶段「观察」）；实施与试点已完成，见 [0022 试点记录](0022-gate-pilot.md)，最终采纳随 0021 修订。
- **测试 Release 正文模板**：rc903 正文的两项非阻断改进（0021 链接改用冻结提交的永久链接、示例证明命令加 `--source-digest <冻结提交>`）留给下一个发行流程测试或自动化模板。
- **停服路径段错误**：`09653c2` 对照层计数中出现一次停服路径 SIGSEGV，根因未查明。valgrind 与停服窗口定向压力均未发现问题，负责人决定对**当前产品源码与依赖谱系显式做发布级风险接受，任何再现即撤回**；适用范围、失效与继承、停止条件见[停服段错误调查「结果与出口」](shutdown-segfault-investigation.md#结果与出口)。首个真实候选须按其中条件判断能否继承，最终 Release 记录须写明「根因未查明、显式风险接受」。
- **最终采纳**：第 20 条补充的签发后产物保全整理为正式合同（重跑删除产物为采纳阻断项）；第 20 条的公开下载工作流写明实现与判定，判定按行为描述，不以 GitHub 对未展开矩阵的占位 job 数量为条件，引用结果时核对运行元数据：正向要求 verify、四个平台 install 与 public-pass 全部成功，负向要求 verify 在预定步骤失败、install 矩阵未展开且没有任何安装执行、public-pass 失败；0021「实施与试点」第 4 条要求修订的 0013、证据说明、AGENTS 索引与常用命令，以及 README 与发行说明的支持矩阵，都尚未修改；在此之前，0013 的现行流程仍然有效。下一个正式候选须使用新版本号，并引用同一冻结提交上一次包含 PID 1 的成功演练。
