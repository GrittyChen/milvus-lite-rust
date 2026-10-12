# 首发平台与发行物准备验收

日期：2026-10-10。功能基线 `2151dcc`，本记录对应其后的 0013 准备切片；参照 milvus-lite v3.2.1 / `43d1257`。合同与负责人取舍见 [0013](../decisions/0013-first-release-platform-and-packaging.md)。记录时为 `0.1.0-dev.0`（Python 元数据 `0.1.0.dev0`），未发布；其后的版本切换、公开地址与发布渠道见 0013 候选阶段决策。

## 变更范围

唯一 `pyproject.toml` 移至根目录，根 README/Apache-2.0 许可及 proto 许可进入 wheel/sdist。README 改为安装用户说明，长描述与源文件相同；公开地址尚未确定，因此不添加文件型相对链接或虚构 repository/homepage。

py crate 的构建脚本按目标 OS 和 PyO3 目标解释器配置拒绝 Windows、free-threaded 与非 CPython。Windows 的目录/文件同步缺陷留在后续平台切片，Unix 存储行为未改。修复 Windows 静态检查的 Unix 专用测试导入；三份进程测试改用读线程与队列，不在本切片增加产品入口。

工具链固定为 Rust 1.98.1，MSRV 1.89.0。sdist 不包含 `rust-toolchain.toml`，使用安装用户自己的编译器；实际解包源码的两套 workspace/all-targets MSRV 检查列为发行门槛。Windows 没有 Rust 时，后端可能先报告 cargo 缺失，无法抵达本项目的拒绝消息；README/长描述明确告知其不支持。

## 产物身份

构建均为目标默认 CPU，无 `target-cpu=native`，使用锁定依赖。macOS 构建显式设置 `MACOSX_DEPLOYMENT_TARGET=11.0`，检查 Mach-O 架构与最低版本字段。Linux 在 manylinux_2_28 环境构建，逐项检查 ELF 的 GLIBC 版本依赖。下列摘要是实际二进制身份，不将相同源码不同 ZIP 时间戳产生的 wheel 哈希混用。

| 产物 | 原生库 SHA256 | 验证 |
| --- | --- | --- |
| macOS arm64 正式 | `e1d3c9d72ab36bacd384ddd6e7b85d8969f73742ffe7d0ee90088cafcc322975` | 单架构 arm64，Mach-O 最低 macOS 11.0 |
| macOS x86_64 正式 | `0d2d2f45a2e7afbd8ec8a9460f3203e83b8bf52b6b4f0a17a224e4f950fb8b80` | 单架构 x86_64，Mach-O 最低 macOS 11.0；Rosetta 冒烟 |
| Linux x86_64 正式 | `68a17c9c9a5c684aa44115e4bf06530af88530a3c49d34a40c0066dbd70eca52` | manylinux_2_28_x86_64，最高 GLIBC 2.28；ARM 宿主模拟构建与冒烟，构建于提交前；原生构建身份以下文「Linux x86_64 原生标准层」为准（构建路径不同，原生库字节不同） |
| Linux arm64 正式 | `0c3ec2f433b7404f2c745f57d3110656ab20ace4f070f3bedc353b27458a279c` | manylinux_2_28_aarch64，最高 GLIBC 2.28 |

sdist SHA256：`2cfce71a091339d5b7bd5a7e26ce3eceddf543a600a97d08d0268249aeb7d1bf`。wheel/sdist 的版本、标签、console script、两份许可内容及 License-File、markdown 长描述、禁入路径和正式/FI 标记均由 [归档检查器](../../tests/packaging/check_artifacts.py) 核对。原始文件与日志保留在仓库外，本记录不保存个人绝对路径。

Linux arm64 基础镜像：`quay.io/pypa/manylinux_2_28_aarch64@sha256:abd1230aa9e4058a74b89d27cd14ba18c0db036d7bfe6e6f219a29ea63c24eb6`。Linux x86_64 模拟镜像：`quay.io/pypa/manylinux_2_28_x86_64@sha256:4131ddd7acb9c6c966af02fe7e71edc51e078c2d41f475966697676430bfa2ae`。容器内使用官方 Rust 1.98.1 完整发行包，下载字节经官方 SHA256 核对后安装；网络超时的尝试不算构建结果。

## 当前实跑证据

| 检查 | 结果 | 边界 |
| --- | --- | --- |
| macOS 默认 workspace / core FI Rust | 251 / 323 passed | 当前源码，无忽略 |
| macOS 默认/FI Clippy 与 fmt | 通过 | `-D warnings` |
| Windows core/grpc 默认/FI all-targets check 与 Clippy | 四项通过 | macOS 上交叉静态检查，没有 Windows 运行 |
| Windows py target 拒绝 | 本项目预期消息，非零退出 | 首发不提供 Windows 包 |
| 实际 free-threaded CPython 3.14.8 构建拒绝 | 本项目预期消息，非零退出 | 无数据目录副作用 |
| 实际 PyPy 3.11 构建拒绝 | 本项目预期消息，非零退出 | Linux 容器内执行 |
| 仓库与 sdist 解包的 MSRV | 默认与 all-features workspace/all-targets 均通过 | Rust 1.89.0，不代替其它目标运行 |
| sdist 解包入口 | 根 wheel 构建及 `-m crates/py/Cargo.toml` sdist 构建通过 | 原打包命令仍有效 |
| sdist PEP 517 安装 | `uv pip install <sdist>` 后真实 SDK 生命周期通过 | 构建隔离，独立安装环境，原生库身份独立记录 |
| macOS 洁净无 SDK / SDK 安装 | 真实 CLI RPC 与 `.db` 关闭/重开通过 | 两环境隔离；无 SDK 环境不装 numpy/grpcio/pymilvus |
| macOS Python 3.10 两条 SDK | 各 272 passed | 同一正式 wheel |
| macOS Python 3.15，两条 SDK | 各 272 passed | 同一正式 wheel；SDK 3.0.2 首轮一条耗时下限失败，单跑与完整复跑均通过，判据未改 |
| macOS 3.11–3.14 | `.db` 生命周期冒烟通过 | SDK 3.0.2；非端到端全套 |
| Intel Mac 3.10 / 3.15 | `.db` 生命周期冒烟通过 | Apple Silicon 上经 Rosetta，不是原生 Intel 证据 |
| 两条 SDK 的差分 / 存储互通 | 各 122 / 68 passed | 当前正式 wheel，对照 3.x 与 Arrow 15/25 |
| macOS 两条 SDK 的隔离 FI wheel | 各 5 passed | compaction 进程退出与信号套件，不混作正式 wheel |
| Linux arm64 隔离 FI wheel | 5 passed | 独立测试环境，正式 wheel 不含入口 |
| Linux arm64 默认 / core FI Rust | 251 / 323 passed | 容器内部文件系统，非 root 用户 |
| Linux arm64 workspace/all-features Clippy | 通过 | `-D warnings` |
| Linux arm64 Python 3.10 / 3.15，两条 SDK | 四组各 272 passed | 同一正式 wheel，非 root 用户 |
| Linux arm64 Python 3.11–3.14 | CLI 与 `.db` 生命周期冒烟通过 | 同一正式 wheel；SDK 3.0.2 |
| Linux arm64 洁净无 SDK | CLI 接受独立 SDK 的真实 RPC 并正常停止 | 不装 numpy/grpcio/pymilvus |
| 脚本自检（benchmark + packaging + platform） | 99 passed | 包含拒绝、失败传播、路径与二进制下限反证 |

Python 端点 SDK 固定 2.6.17 / 3.0.2；实际依赖版本与原生库哈希随各安装日志记录。Linux arm64 的 CPython 3.15 安装中，SDK 依赖 orjson 3.13.0 从源码回退并用已安装的 Rust 编译；本包自身使用同一个 abi3 wheel，不将 SDK 依赖的这次安装写成全套无编译器安装证据。测试环境都在仓库外，未安装官方发行包到 Rust 环境。

## 复跑方式与证据检查

- 构建/归档/洁净安装：使用 [packaging 流程](../../tests/packaging/README.md)，实际执行过根入口、sdist 解包入口、PEP 517 与从仓库外调用 `setup_rs.sh`。`-m crates/py/Cargo.toml` 仍找到根发行配置。
- 运行回归：`cargo test --workspace`、core `--features fault-injection`；`pytest tests/python`、`tests/differential`、`tests/storage_interop` 的正式环境与隔离 `tests/compaction` 分开。
- 反证自检：缺/错许可、版本、标签、长描述、相对链接、FI 混入、路径穿越、错误架构/最低系统、流水线子步骤失败、根配置未跟踪改动及错误退出/他容器 kill 事件均不能产出通过记录。
- 完整路径扫描：`pyproject.toml`/`Cargo.toml`/构建入口引用，目标构建检查与两个许可证消费点；三个管道读线程的 EOF、超时、wait/join/close；PID 1 的暂停、SIG_DFL、LOCK、事件身份与恢复模型。引擎提交、锁序和 RPC 实现均未改。

当前准备项已落地并自验证，实施评审与修复复审已通过；候选发行和上传另行授权。

## PID 1、模拟与剩余门槛

PID 1 探针按 [平台流程](../../tests/platform/README.md) 使用正式 console ENTRYPOINT 和 FI 包装入口，数据/控制命名卷、同 Docker VM 的 peer flock、日志检查点及监督器 kill 事件分别验证。Docker Engine 29.8.2、VM 内核 7.0.14-linuxkit，基础镜像 ID `sha256:8bfa0d9787b8209665706873c70643c1e897c0b70bab81eb69f8f97dfac207c4`。三项均通过：正式关闭退出 0、重开 3 行/current_seq=6；FI 放行排空退出 0、重开 4 行/current_seq=4；FI 后续 SIGTERM 不结束服务，监督器超时留下 signal=9 事件，退出 137 且非 OOM、重开 4 行/current_seq=4。放行前 LOCK 始终被占用，停止后释放；关闭原文各一次。两个 FI 场景的检查点与正式 console 场景分别保存。测试通过不等于真实断电或任意文件系统的持久化证明。

准备阶段的 Linux x86_64 仅有 ARM 宿主上的模拟执行，不写成原生通过；原生标准层结果见下文「Linux x86_64 原生标准层」。Windows 没有运行环境；PowerShell 静态准备流程已写但未在 Windows 执行；对应 cargo 参数已在 macOS 交叉验证，不宣称 Windows 支持。macOS 11 的 Mach-O 下限检查不替代 macOS 11 真机运行。

准备切片和最终候选/上传是三个阶段。公开地址、正式版本、tag 和上传均待另行授权。保留已有存储、停服与信号合同：进程崩溃不等于断电；退出总耗时无硬上限；PID 1 切到 SIG_DFL 后后续 SIGTERM 被忽略，监督器最终 SIGKILL 走崩溃恢复。

## 评审收尾：归档约束、产物复用与停服判据

四项建议已落实：外链主机仅接受 Apache 许可站点，含引用式/自动链接，元数据拒绝未确认的 Project-URL/Home-page；sdist 拒绝开发工具链文件；Windows 脚本声明 PowerShell 7.2 下限，文档使用 `pwsh -NoProfile`；架构地图补根配置与 py build.rs。PowerShell 流程仍没有 Windows 执行证据。

运行器新增成对的 `--wheel` / `--sdist`，保留原构建模式。复用模式先拒绝缺失/不完整输入，再复制到本次独立目录；检查与安装使用相同副本，测试 Python 无需安装 maturin。反证覆盖不完整参数、缺失文件、不重建、测试解释器选择及外部输入在检查之后改写。原有子步骤失败不输出通过记录的约束保持不变。

实际用同一份 macOS arm64 正式 wheel / sdist，运行 Python 3.10 / 3.15 × SDK 2.6.17 / 3.0.2 四组复用安装。每组均通过无 SDK CLI 的真实 RPC 与 SDK 的 `.db` 生命周期冒烟；输出 wheel SHA256 均为 `23c6d448e1ec7c9f5cf43871a23b79b14d18bd6983eab52f8b63c2f66cf8d5c2`，原生库仍为表内 `e1d3c9…`。这四次没有重建或重跑 272 条全套；前面的全套结果保留为对应产物的历史证据。

### 单独记录：SHUTDOWN-TIMING-01

历史用例 `test_stop_waits_for_engine_task_before_releasing_lock`（已删除，替代方式见下文“关闭”段）在首次 Python 3.15 / SDK 3.0.2 全套中失败：`elapsed=6.02562862499326s`、`seconds=7.46040974999778s`，要求下限 `seconds−0.3−1=6.16040974999778s`，差约 0.1348s。前面的 LOCK 释放、客户端取消及取消时间断言均通过。

该断言用一个请求的耗时推断另一个请求的结束时间；校准已有预热，原现场没有 CPU 负载或引擎结束时刻记录，不能确认“冷启动”是原因，也不能仅凭复跑通过排除提前返回。

受控诊断使用同一正式原生库和同量检索：三次空闲校准为 4.0745 / 3.9111 / 4.0901s；8 个 CPU 竞争进程下的校准为 14.4319s；回到空闲后为 3.9512s。随后 stop 耗时 3.8752s，客户端在 stop 后 2.0023s 取消，LOCK 已释放。若套用负载期的校准，下限是 13.1319s，仍会误报。这证明跨请求耗时校准可产生失败，不能追溯证明原始事件的具体负载原因。

新增 grpc 的 [受控 RPC/LOCK 用例](../../crates/grpc/src/server.rs)：复用已有 cfg(test) 钩子，在实际 RPC 已入场且 runtime 已关闭之后继续持住任务，断言 stop 未返回、LOCK 未释放；显式放行后才能结束并重新取锁。测试不依赖搜索速度，不增加产品或 wheel 接口。使用独立 Cargo 输出目录的仓库外副本删除 Db.close 的在途等待后，该用例在 `assertion failed: waiting` 处失败；正常实现通过。原 Python 耗时断言本轮未改宽。

历史测试的校准脆弱性单独登记，后续以确定性的完成边界替代跨请求时间推断。当前停服等待的证据由受控 RPC/LOCK 反证补齐；原始事件的具体调度原因保持未确认，不写成已证明的冷启动问题。

反证实验首次误用共享 Cargo 输出目录，后续默认测试曾链接到变异缓存；这次失败结果已废弃。随后清理 core/grpc 构建缓存并重建正常工作区，将副本改为独立输出目录，再分别验证正常通过和变异失败。工作区的 Db.close 源码与正式 wheel 均未被变异改写。

**关闭（2026-10-10，候选门槛）**：按负责人决策删除跨请求耗时推断，原用例改为 [test_stop_cancels_long_request_then_releases_lock_and_reopens](../../tests/python/test_server_manager.py)，作为正式 wheel 的冒烟：宽限期满后客户端收到 `UNAVAILABLE`，停服后 LOCK 已释放，重开后 5 万行完整；同一用例对官方 3.2.1（`43d1257`）也通过，宽限期满的状态码一致。它仍有一个时序前提——检索须长于 2 秒宽限期：按多次测量中最快的一次近似换算到约 10 秒，按线性估计留约 4 倍余量（诊断中负载前后的校准差约 3.6 倍），不是保证；若因此拿到正常结果，失败信息会给出请求在停服开始后多久完成，用于区分前提不成立与未取消。取消早于停服完成、停服等待已开始的引擎任务，只由 grpc 的受控 RPC/LOCK 用例判定，正式 wheel 不增加测试接口。

同类清扫发现 `test_stop_lets_short_in_flight_request_finish` 也依赖时间校准（约 0.5 秒的检索须在 2 秒宽限期内完成），且是“宽限期内已入场请求照常返回”的唯一证据。按负责人同意，在 grpc 新增 `rpc_admitted_before_stop_completes_normally_within_grace`：请求入场后由钩子挡住，宽限 60 秒的 stop 发出停服信号后（仅测试构建的通知点），先观察 200 毫秒内 runtime 是否已关闭，再放行；断言 runtime 未提前关闭、客户端得到业务成功的正常应答（status 与 error_code 为 0、`LoadStateNotExist`）、stop 成功、LOCK 释放。正确实现在放行前不会关闭 runtime（tonic 收到停服信号后会等全部连接结束），因此该观察不会让正确实现失败；有限超时只在极端停顿下导致失败或漏检。该用例只能识别在信号后 200 毫秒内就取消的实现，宽限期被截短由下一段的下限断言识别。Python 短请求用例改为冒烟：按多次测量中最慢的一次近似换算到约 0.5 秒，按线性估计与宽限期留约 4 倍余量；请求也可能在停服前已完成，此时不覆盖在途情形。

**评审修正（2026-10-10）**：负责人评审确认，长请求用例中“客户端错误时刻早于停服完成”的比较会因线程调度误报——`done_at` 是线程记录时间的时刻而非收到错误的时刻，受控暂停记录线程后，已在停服完成前 2.99 秒捕获 `UNAVAILABLE` 的请求被记为晚约 49 微秒。该比较已删除。“取消早于停服完成”改由受控 RPC/LOCK 用例以事件顺序判定：runtime 关闭、stop 停在暂停点时，客户端须已收到错误（5 秒有限等待，短于钩子的放行期限）。独立复审指出该用例原先不能识别截短的宽限：现以 300 毫秒宽限加单向下限，从调用 stop 到进入暂停点须不少于宽限（宽限计时不会提前到期，不会误报）。变异矩阵（仓库外副本、各自独立输出目录）：M1 宽限截到 100 毫秒、M3 宽限改 0，均在下限断言失败，后者也被宽限用例拦下；M2 把 runtime 关闭移到 `Db::close` 之后，在取消顺序断言失败（`Elapsed`）；M4 删除 `Db::close` 在途等待，在 `waiting` 失败。另以 M4 构建的 wheel 运行 Python 长请求用例，三轮通过，确认 Python 用例不承担等待语义的判别。

### 收尾后的实跑结果

| 验证 | 结果 | 证据边界 |
| --- | --- | --- |
| 清理缓存后 `cargo test --workspace --locked` | 252 passed | 比初次准备记录多 1 条 cfg(test) RPC/LOCK 用例 |
| grpc `--all-features` | 21 passed | 含新用例与原隔离维护用例 |
| workspace 默认 / grpc all-features Clippy | 两套通过 | `--all-targets --locked -D warnings` |
| Windows core/grpc 默认 / FI Clippy | 两套通过 | 新 Rust 测试也可交叉编译，无运行结论 |
| benchmark + packaging + platform 自检 | 120 passed | 初次 99 的历史记录保留；新增地址、工具链、复用模式和失败传播反证 |
| 同一 wheel/sdist 的四组复用安装 | 全部通过 | Python 3.10 / 3.15 × SDK 2.6.17 / 3.0.2；无 SDK CLI 与 SDK 生命周期各执行 |
| 原耗时停服用例，Python 3.15 / SDK 3.0.2 | 连续三轮各 1 passed | 判据未改，不能单独作为原始事件原因的证明 |
| 独立副本删除在途等待 | 新 RPC/LOCK 用例明确失败 | 原始工作区和输出目录隔离 |

生产实现与发行 README 未改，继续使用已冻结的正式 wheel。Docker PID 1、Linux/Rosetta 完整矩阵、差分/互通与 core FI 全套本次没有重跑，沿用上节注明身份的初次记录，不写成这次增量的新实测。PowerShell 脚本未在 Windows 执行，原生 x86_64、Windows 运行和 macOS 11 真机缺口保留。修复复审已通过。

## 链接写法补齐

本次仅调整 check_description、相关反例与文档。主机检查覆盖全文，不再剔除行内/围栏代码；允许的本地 RPC 示例主机明确为 127.0.0.1、localhost，其它外链仍仅允许 Apache 许可站点。内联与引用式定义使用同一目标规则，HTML href/src 属性直接拒绝。也核对了角括号目标、大小写、引用式缩进与代码中的 git+https/git+ssh；没有引入 Markdown 渲染器或新依赖。

复审所列四种漏项均有反例；现有 README、回环代码示例、Apache 引用式链接和页内锚点仍通过。benchmark/packaging/platform 自检为 133 passed，冻结正式 wheel/sdist 的归档与二进制检查通过。Python 3.12 / SDK 3.0.2 复用安装完成无 SDK CLI 与 SDK 生命周期冒烟；原生库仍为 e1d3c9…，wheel 与 sdist 身份均未变。

许可证、标签、元数据及 sdist 检查未改；生产源码与发行 README 未改。本次不重跑 Rust、平台矩阵、差分和互通，沿用前述明确身份的结果；复审已通过。

## 准备切片收口

2026-10-10：实施评审及修复复审均通过，负责人授权将 0013 准备切片作为一个提交推送；不包含候选发行、公开源码地址或上传授权。最后补入无协议 www. 地址的精确主机检查，反例与 Apache 正常例均通过，提交前脚本自检为 135 passed，冻结产物检查仍通过；原有字段/许可/标签规则不变。平台缺口、SHUTDOWN-TIMING-01 和 2026-10-17 的资源检查点继续按上述边界保留。

## Linux x86_64 原生标准层

2026-10-10 在一台原生 x86_64 物理机（AMD EPYC 7702，无虚拟化）上执行，源码为 `b5393de` 的 `git archive`，下文测试修复以补丁形式叠加。宿主为 CentOS 7、内核 3.10.0-1160、glibc 2.17，不能直接加载 manylinux_2_28 wheel，因此构建、测试与安装均在 `quay.io/pypa/manylinux_2_28_x86_64@sha256:4131ddd7…`（镜像 ID `sha256:2dd9abc5…`，AlmaLinux 8.10、glibc 2.28）容器内执行：使用宿主原生 CPU 与内核，无模拟，并同时覆盖 glibc 下限。Docker 20.10.5，overlay2 / xfs；工作目录为宿主 ext4 绑定挂载；测试容器绑定 16 核并以非 root 用户运行。Rust 1.98.1 官方完整发行包与 crane 0.20.3 均经官方 SHA256 核对后使用；该主机 Docker 守护进程配置的代理无法访问镜像仓库，基础镜像经客户端按摘要获取后载入并核对镜像 ID。主机名与个人路径不入库。

| 检查 | 结果 | 边界 |
| --- | --- | --- |
| fmt；workspace 默认 / all-features Clippy | 通过 | `--all-targets --locked -D warnings` |
| 默认 workspace / core FI / grpc all-features | 252 / 323 / 21 passed | 含下文全部修复，`--no-fail-fast`；core lib release 145 passed，FI lib 整体并行 30 轮 0 失败 |
| 正式与 FI wheel、sdist 构建及归档/二进制检查 | 通过 | `cp310-abi3-manylinux_2_28_x86_64`，GLIBC 依赖最高 2.28；正式/FI 标记正确 |
| 构建拒绝：free-threaded 3.14t / 3.15t、PyPy 3.11 | 均为本项目拒绝原因，非零退出 | 镜像内真实解释器 |
| 同一正式 wheel：CPython 3.10.22 / 3.15.0 × SDK 2.6.17 / 3.0.2 | 四组各 272 passed | `run_acceptance.py --wheel --sdist --suite`，含无 SDK CLI 与 SDK 生命周期冒烟 |
| CPython 3.11.17 / 3.12.15 / 3.13.16 / 3.14.8，SDK 3.0.2 | 安装冒烟通过 | 同一正式 wheel |
| 隔离 FI wheel 的 compaction 套件 | 5 passed | CPython 3.12、SDK 3.0.2 |
| Rust 与 3.2.1 的 LOCK 双向互斥 | 通过 | flock 互斥、lockf 独立、释放后可打开 |
| strace 同步/提交/删除顺序 | 通过，83 次成功调用 | strace 5.18，FI 测试二进制 |
| PID 1 正常 / 排空 / 监督器强杀 | 退出 0 / 0 / 137（非 OOM，signal=9 事件）；重开 3 / 4 / 4 行 | 宿主内核 3.10；基础镜像为官方 `python:3.10-slim-bookworm` amd64（`sha256:fbff8a8e…`）；探针容器未绑核 |

产物身份（`b5393de`，修复前构建）：正式原生库 `69231b3bb81fd6a82cab04c5a146ecb767779fc6a70260023a07938e200669a6`，FI 原生库 `0c47ae7228a322254e117cad3a667281191d802440ba0a2c751b7eab43e3cff3`，sdist `4d60bcc47063c3efcffb794edaf504a1d0a770e99ac871418528617f2b304f6e`。同一提交在 macOS 上构建的 sdist 字节相同。前文“产物身份”节的 sdist `2cfce71…` 构建于提交之前，与 `b5393de` 的 sdist 只差 server.rs 中的 RPC/LOCK 测试，提交对应的 sdist 以 `4d60bcc…` 为准。修复后重建的正式原生库与修复前大小相同、`.text` 一致，差异只在符号字符串表、build-id 与 9 个 panic 位置行号（flat.rs 前移 2 行）；候选发行物仍须从最终提交重新构建。

### 原生运行发现并修复的测试缺陷

**批量距离核的 NaN 位模式**：`flat::tests::expanded_batch_kernel_matches_original_loops_including_special_values_and_tails` 在 x86_64 上失败（macOS 与 Linux arm64 通过，模拟环境未跑 Rust 测试）。两条路径都得到 NaN，但符号/payload 不同（传播输入 `0x7fc00123` 或产生 x86 默认 `0xffc00000`），且 debug 与 release 的选择不同。Rust 不保证 NaN 位模式；排序把所有 NaN 视为相同（`collection.rs` 与 `flat.rs` 已有 ±NaN 排序用例），对外顺序不受影响。隔离诊断显示，所有非 NaN 结果在 debug 与 release 下仍逐位相同。修复只改测试：NaN 结果要求两边都是 NaN，非 NaN 保持逐位比较；flat.rs 模块与 `score_batch` 注释及 [0004](../decisions/0004-m1-scope.md) 的跨平台逐位相同表述补 NaN 例外。变异反证：批量核一路乘 1.000001 后，修复后的测试在 dim=16 的非 NaN 用例失败。

**目录变更等待无关写入者的前置条件**：FI 用例 `db::tests::directory_changes_wait_for_an_unrelated_writer_before_taking_namespace` 在整个 lib 并行时偶发失败（单跑 0/300、db 模块并行 0/20、整个 lib 并行 2/10）。建表会排入后台维护，`compact_transaction` 也短暂持有写入许可的读锁；用例把“许可不可得”当作插入线程已入场，维护恰好持锁时前置条件不成立。诊断现场：插入在主线程仍持 collection 写锁时已返回 “database writes are blocked after an unconfirmed directory change”，目录变更先行、其后的写入被正确拒绝，产品行为符合合同。修复只改测试：建表后先等两个 collection 的已接受维护轮次完成，使观察到的占用只能来自插入线程；观察后以零超时核对没有维护轮次在途，不满足时以明确信息失败。同负载交替 30 轮：修复版 0/30；去掉前置等待的变异版 10/30，全部由新核对拦下；原版 1/30。修复版另跑整个 lib 并行 60 轮 0 失败。该用例来自 `2ed4a34`，与本切片的其它改动无关。

**并行等价用例的参照口径**（复审发现，属同一 NaN 口径）：`collection::tests::parallel_search_matches_serial_reference` 把每个并行变体与逐条打分的独立参照逐位比较，而产品在查询组满 4 条时走批量核，NaN 位模式可能不同；复审探针中同一查询单独检索与 4 条一组检索的 NaN 分数在 x86_64（Rosetta）release 上有 73/713 个位模式不同。用例改为两层：各行块划分与线程数对串行结果逐位比较（含 NaN 位模式，同一扫描代码），串行结果对独立参照只按“同为 NaN”比较；数据与查询加入 NaN、±inf、±0（每个分量仍只取一次随机数，共用该数据辅助函数的其它用例布局不变），维度加入 33。0005 验证表曾称覆盖 ±0、±inf，原数据实际只有 NaN 与有限值，现已补齐。变异反证（各变异使用独立的 Cargo 输出目录）：参照改回原始位比较后 aarch64 release 在 case 10 失败，说明 NaN 归一确被数据触发；参照对有限非零分数偏 1 ulp 时在 case 0 失败。修复后 aarch64 与 x86_64（Rosetta）的 debug/release 均通过。对外 NaN 分数的符号与 payload 因而不作保证，已写入 flat.rs 与 [0004](../decisions/0004-m1-scope.md)，排序与结果集合不受影响。

macOS 修复后默认 252、core FI 323 passed；Windows core/grpc 默认与 FI 交叉 Clippy 通过。差分/存储互通未在 x86_64 运行（与 arm64 层级一致）。负责人可据此在 2026-10-17 检查点确认 Linux x86_64 的标准层证据；首发候选的整体结论仍按 0013「收口条件」另行决定。
