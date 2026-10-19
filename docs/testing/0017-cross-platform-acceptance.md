# 0017 整体跨平台开发验收

- 状态：跨平台开发验收已独立评审通过（2026-10-20）；发布候选验收另行执行。
- 冻结提交：`ac12cb2b89f237fe0050a918b72fe6d74d77aa1e`，包含显式分区 A、partition key B 和已独立评审通过的非 key Upsert 修复。
- 归档 SHA256：`4dcc8c62a1aae78f820d8d3bf76058c2d22bef65ded9e6649ca91f77f74e1bba`；364 个普通文件，另有 2 个软链；127 项 crate/构建输入与已审修复清单逐项匹配。
- 版本字段仍为开发用 `0.2.0rc1`，不代表已发布附件；本轮不切版本、不推送、不打 tag 或发布。

## 范围与证据分层

目标是补齐已审分区及修复在当前提交上的平台开发证据。macOS arm64 沿用已核验、同一 127 项 crate/构建输入的实施及独立评审结果，另跑本轮两类联合冒烟；Linux x86_64 原生与 arm64 容器从冻结归档构建，并执行workspace/core FI Rust、默认及仅开启 core FI 的 workspace Clippy、两条 SDK Python/差分全套、Arrow 15/25 互通、LOCK、strace 及安装后联合功能。Mac x86_64 按 0013 分层，从冻结归档交叉构建、二进制检查后，以 Rosetta 在 CPython 3.10/3.15 × 两条 SDK 做安装及两类生命周期冒烟。

本轮不将 3.12 的开发全套替代最终版本 3.10/3.15 两端点全套，不把 Rosetta 写成原生 Intel；[0013](../decisions/0013-first-release-platform-and-packaging.md) 的 sdist/MSRV、FI wheel/PID 1、解释器窗口、平台矩阵及公开发布门槛仍在版本/范围确认后从最终候选执行。

## 构建与环境

原生 x86_64 为 AMD EPYC 7702 / CentOS 7 内核 3.10.0-1160，容器 glibc 2.28；arm64 为 Docker Desktop 的 7.0.14-linuxkit 内核，容器 glibc 2.28。实际执行 UID 分别为 1145、501；root 仅安装跟踪工具，产品测试与 strace 被测程序均为非 root。

两套 Linux 为 manylinux_2_28 / glibc 2.28，使用各自原生 CPU 执行；arm64 位于本机 Docker 命名卷，x86_64 位于原生主机容器内部 Linux 文件系统，均非 root。Rust 1.98.1、maturin 1.15.0、locked/offline Cargo 依赖；新增 md5 从已验证缓存传输，Cargo.lock 校验不变。开发验证关闭增量和 debug 符号，未关闭 debug assertions。各日志记录实际 SDK、依赖、Python/native 与工具指纹；不把两个同名版本 wheel 当成同一构建。

## 验收矩阵

| 检查 | macOS arm64 | Linux x86_64 原生 | Linux arm64 | macOS x86_64 / Rosetta |
| --- | --- | --- | --- | --- |
| Rust workspace / core FI | 沿用 357/455 | 358/456 passed | 358/456 passed | 分层不执行 |
| 两条 SDK Python 全套 | 沿用各 473 | 各 473 passed | 各 473 passed | 分层不执行 |
| 两条 SDK 完整差分 | 沿用各 408；2.6 独立评审为专项 68 | 各 408 passed | 各 408 passed | 分层不执行 |
| Arrow 15/25 互通 | 沿用 211 | 211 passed | 211 passed | 分层不执行 |
| LOCK / strace 提交顺序 | 非 Linux 门槛 | 通过；85 条成功调用的顺序核验 | 通过；85 条成功调用的顺序核验 | 非 Linux 门槛 |
| 本轮分区联合生命周期 | 两条 SDK 各 5 阶段通过 | 两条 SDK 各 5 阶段通过 | 两条 SDK 各 5 阶段通过 | 四组合各 5 阶段通过 |
| 0015/0016 联合功能 | 两条 SDK 各 4 阶段通过 | 两条 SDK 各 4 阶段通过 | 两条 SDK 各 4 阶段通过 | 四组合各 4 阶段通过 |

Rosetta 二进制检查通过，最低部署版本为 11.0；实际 Python 架构均为 x86_64。产物身份统一列于下表。

## 开发产物身份

| 平台 | wheel SHA256 | native SHA256 |
| --- | --- | --- |
| macOS arm64（沿用已审 wheel） | `6ca974ec3c54cba0c50e03c542e0544d2f6cca740ef51942cfd830bad622bf4c` | `2b3fddf9b341d895c3fea627d4ba8ad16b31d6f07673d4e9007448b5b392a8ce` |
| macOS x86_64（本轮） | `0e770a32f2519e1c9cf79b75ad291c363aef343b8c118a075eab045866f39655` | `b1fd2550481c24dcbb0ccf61b3138cf714d1b5e86a2adaae69f08c7ea9c872bd` |
| Linux arm64（本轮） | `a6cb0412195e43f11b9a743ff732f548160abd6c28130fc9af367759de48b7b9` | `0038437717dd98d3829128439886a5cd938762edc3ab167949c1c3b25397cefb` |
| Linux x86_64（本轮） | `bdfaafca92f9fe09de229e09e64e2b051d6c04b8d483d0dc2575dc234b684d48` | `1c73f34201c30f6503f36219c97f45c5ffbe2a06725b0d6eef8a624ab9cfaf30` |

两份 Linux sdist 逐字节相同，SHA256 为 `ca6537d408db086ac8441e427260302a4810540862de21d66295151d1d84fb69`。包内容及二进制检查已通过；它们是本轮开发构建，不能与已发布的同名版本附件混用。分区联合探针独立于冻结产品归档，完整脚本随证据保存，SHA256 为 `f326052f35e2a76dcf2c2370251ddb8c21f77dbc695e0a9cc5c631e86abc8e2f`；原 0015/0016 探针保持 `de73c2cd2c07a65d990a407a3a0913f08ecbf8a9f9c7daec975d3fe9bb8d5486`。

## 本轮准备诊断

- 首次归档准备误用系统旧 Python 的 tarfile API，随后改用项目 CPython 3.12，完整源码校验通过；失败日志保留。
- x86_64 最初 TCP 连接超时；负责人修复网络后恢复，保留失败和重试日志，不将其计为测试失败或成功。
- 新联合探针首次直接比较 SDK 惰性列表与普通列表，未触发动态字段/ARRAY 物化。改为显式 `list(result)` 迭代，与既有差分 `plain()` 的逻辑一致，未改变行值或期望；原失败日志保留。SDK 3.0.2 的 `HybridExtraList.__iter__/__getitem__` 为直接代码依据。
- Linux arm64 SDK 2.6 缓存未命中，另建隔离环境准备依赖；参照环境仍保持固定 checkout。
- 互通的本地参照来源检查要求实体 checkout；初轮复用了指向旧参照的软链，解析后的导入路径不位于新快照的词法路径下，全部在 setup 被拒绝，未执行产品互通断言。为两 Linux 建立独立的同 commit 实体副本及专用解释器入口，验证原/新 Git 归档逐字节相同、原参照未变后，从互通阶段继续；保留初轮失败与原始阶段记录，不修改来源断言。之前通过的语义差分使用原固定参照，不因路径准备重跑或冒充新执行。
- arm64 strace 的通用包管理镜像元数据下载过慢，停止本次准备进程；按已解析包名从 AlmaLinux 下载 RPM，经 `rpm -K` 核对签名/摘要后安装。运行工具不改变产品依赖或数据格式。
- x86_64 原容器未配置 SYS_PTRACE，strace 在被测程序启动前报 PTRACE_TRACEME / Operation not permitted。原失败及跟踪文件另存；使用同一镜像、同一挂载和 UID/GID 的独立容器，仅增加 SYS_PTRACE 后继续跟踪和剩余冒烟，不修改测试或重跑已通过的套件。

## 证据身份

以下均为仓库外归档，不包含数据库、虚拟环境或构建缓存；wheel 与 sdist 另存。整包中的门槛脚本在打包目录再次执行通过，244 项内容（不含 SHA256SUMS 自身）均逐文件验哈希。

| 记录 | SHA256 |
| --- | --- |
| Linux arm64 平台证据（52 项，不含校验清单） | `5ef3e6d5bf2df1ad52e988ab2e2902bc34f30320690598dc3adb89788fc1fa5f` |
| Linux x86_64 平台证据（58 项，不含校验清单） | `36c29cd82f37583a566ff9468fb93c7c02a423d13b3155d192bdcd6a59fd506d` |
| 总证据包 | `408a7bafe5c741e4cc55d995fad6db94f489e978ca76ce37b21b554e1371a98e` |
| 总 SHA256SUMS | `ac47305a7c4b899c47357eb05995651d1566d604bc6a345700228c8cdfbcd35a` |
| 总门槛清单 | `be3293a9912cf2a097246dbcdba518b375d5f14ab0a830b3001a611150685dcc` |

Mac arm64 的原修复证据和独立评审另置于整包 carry-forward 项，沿用与本轮实跑明确分开。参照目录准备和 ptrace 权限修正前后的原始阶段/日志均保留，成功清单不代表所有准备命令第一次即成功。

## 后续边界

不重写 0017 A/B 的历史统计、性能和回退证据。JSON 写入与峰值资源差距继续独立处理；真实重启/断电、macOS 11 真机和原生 Intel 不新增证明。最终候选必须在版本范围另行确认后按 0013 重建验收；本次独立评审不替代最终候选验收。

## 收口与交付

总门槛已逐项核对阶段退出码、实际 SDK/native、完整套件计数、两个参照解释器的来源与版本、原始 strace 及探针结果。Rust 数量取每个顶层 harness 的最后汇总，剔除故障子进程重复输出；两 Linux 的默认及仅开启 `milvus-lite-core/fault-injection` 的 workspace/all-targets Clippy 均通过，无 skipped/xfail 替代通过。macOS arm64 的沿用另有 366 项逐文件证明，差异仅为 AGENTS 和修复验收记录的收口文档，产品、测试及构建字节均相同。

专用构建/验收容器已停止，工具链、镜像、环境及产物保留。ARM 已完成的 Cargo debug 缓存回收约 29.3 GiB，保留 release；回收前后 364 项冻结源文件、平台证据包及校验清单的哈希不变。它与此前本仓 debug/变异缓存清理分开记录，不把逻辑缓存大小当作磁盘物理增量。

当前新增内容仅为本记录和知识索引，产品冻结提交仍为 `ac12cb2`。显式分区、key、Upsert 修复分别位于 `c914ee0`、`d2729f8`、`ac12cb2`；未推送，未切版本或发布。本记录及新证据已独立评审通过；下一步确认版本范围和版本号。

## 独立评审收口（2026-10-20）

独立评审通过，限跨平台开发验收；未发现阻断。评审核验了冻结源码、127 项产品输入、244 项证据及产物身份，重放两 Linux 的 strace 检查，并实际复跑 macOS arm64 两条 SDK、Rosetta 四组合的两类联合冒烟。Linux 全套与 LOCK 本轮核查原始归档，未重新执行。独立评审报告及日志已在仓库外持久归档；报告 `review.md` 的 SHA256 为 `1b73edf541988bce56b025e423e1e6e4c256ca6919ef2521ef69c79d4e12a103`，原 244 项证据包及其哈希保持不变。

采纳非阻断建议，明确 FI Clippy 的实际命令为 `cargo clippy --workspace --all-targets --features milvus-lite-core/fault-injection --locked -- -D warnings`：工作区均参与检查，但只有 core 开启 FI；grpc／py 的 FI 专用代码未被该命令覆盖。最终候选须覆盖完整 FI 编译面，可用 `cargo clippy --workspace --all-targets --features milvus-lite-py/fault-injection --locked -- -D warnings`，通过 py → grpc → core 的 feature 传播开启三层 FI，并核验实际启用结果。本轮仅修正文档边界，不新增 Clippy 实跑或功能测试结果，不改既有计数。
