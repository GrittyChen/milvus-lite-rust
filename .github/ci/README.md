# CI 辅助文件

范围、判据与不纳入 CI 的内容见 [0020](../../docs/decisions/0020-continuous-integration.md)；工作流为 [ci.yml](../workflows/ci.yml)（快速层）与 [compare.yml](../workflows/compare.yml)（对照层）。

## 固定依赖

[requirements/](requirements/) 下每个 `.in` 是一个环境的直接依赖，同名 `.txt` 是由它解析出的完整版本集合，CI 以 `uv pip install --no-build -r <name>.txt` 安装，只用 wheel。

| 环境 | 解释器 | 用途 |
| --- | --- | --- |
| `dev` | CPython 3.12 | 构建并安装本实现、pymilvus 3.0.3 线、工具自检、驱动分片工具 |
| `sdk26` | CPython 3.10 | 安装 wheel 后的 pymilvus 2.6.18 线 |
| `sdk26-py315` | CPython 3.15 | 发行工作流中 3.15 的 pymilvus 2.6.18 线；`sdk26` 选中的 numpy 2.4.6 没有 cp315 wheel，因此单独解析，不复用 3.10 的集合 |
| `sdk30` | CPython 3.10–3.15 | 发行工作流中各解释器的 pymilvus 3.0.3 线（3.10 / 3.15 全套与 3.11–3.14 冒烟）；逐解释器、逐平台核对过 wheel |
| `dump30` / `dump26` | CPython 3.10、3.12 | `tests/dump` 需要的 `pymilvus[bulk_writer]`，分别叠装在 3.0.3 线（`dev`、发行的 3.10 环境）与 2.6.18 线（`sdk26`）已有的环境上：两份锁是同版本的超集，叠装不改变已有包的版本。BulkWriter 依赖的 pyarrow 没有 cp315 wheel，因此不进 3.15 的环境，tests/dump 也不进 `tests/python`（[0037](../../docs/decisions/0037-dump.md)「测试」） |
| `ref25` / `ref15` | CPython 3.12 | 参照实现（`compare.yml` 的 `REFERENCE_COMMIT`）的 Arrow 25 / Arrow 15 环境，`ref25` 另含 `pymilvus[bulk_writer]` 供 dump 差分使用；参照实现的依赖取自其 `pyproject.toml`，checkout 另以 `--no-deps -e` 安装 |
| `ref26` | CPython 3.12 | 只用于存储互通：与 `ref25` 相同，只把 pyarrow 换成 26.0.1。pyarrow 低于 26 读不了含空行的定长列表 Parquet，nullable 稠密向量的互通要在 Arrow 26 上验证 3.x 的读取（[0038](../../docs/decisions/0038-nullable-dense-vectors.md)）。生成时沿用 `ref25` 的 `--exclude-newer 2026-10-08T00:00:00Z`，另加 `--exclude-newer-package pyarrow=2026-11-05T00:00:00Z`，其余包与 `ref25` 逐行相同；整体放宽日期会改动 13 个共同包。[tests/ci](../../tests/ci/test_dump_requirements.py) 核对这一点 |
| `build` | — | 可编辑安装参照时的构建依赖（hatchling 及其可编辑安装额外请求的 editables），以及从 sdist 构建 jieba 的 setuptools，经 `--build-constraint` 使用 |

Linux 上的 CPython 3.15 例外：`orjson==3.13.0` 的 cp315 Linux wheel 只有 `manylinux_2_39`，无法在 glibc 2.28 的测试环境安装，按 [0021](../../docs/decisions/0021-release-build-in-ci.md) 第 11 条只允许它从 sdist（SHA256 `d1de5eb04485110c5da4c657e49168995d55e076b1ce60f1a042e254f4186c4f`）在独立 job 中构建一次，其余依赖仍只用 wheel。

参照环境的 jieba 例外：3.x 的 jieba 分词器依赖 `jieba==0.42.1`，它在 PyPI 上只有 sdist。按 [0021](../../docs/decisions/0021-release-build-in-ci.md) 第 11 条，对照层与发行工作流各有一个独立的 `jieba` job，用 [jieba_wheel.py](jieba_wheel.py) 从 sdist（SHA256 `055ca12f62674fafed09427f176506079bc135638a14e23e25be909131928db2`）构建一次纯 Python wheel，构建后端 setuptools 由 `build.txt` 固定，并取出 sdist 中的 `test/test.py` 供 jieba 固定真值核验使用；测试 job 把这份 wheel 以 `--no-deps --no-index` 装入两套参照环境，再以 `check-installed` 核对 wheel 的哈希、实际导入的版本与词典（[0036](../../docs/decisions/0036-jieba.md) 决策 G）。本地准备参照环境的等价命令见[互通 README](../../tests/storage_interop/README.md)。

`dump30`、`dump26`、`ref25`、`ref26` 的 `.in` 带两条上限 `argon2-cffi-bindings<26` 与 `cryptography<49`：两者由 BulkWriter 依赖的 minio / azure-storage-blob 引入，argon2-cffi-bindings 26.1.0 在 macOS 上只有 arm64 wheel，cryptography 自 49.0.0 起不再提供 macOS x86_64 wheel，而发行工作流在 macOS x86_64 上以 `--no-build` 安装。[tests/ci](../../tests/ci/test_dump_requirements.py) 同时核对 `.in` 与 `.txt` 中的上限。

更新任一 `.in` 或参照版本后，在仓库根目录按各 `.txt` 文件头记录的命令重新生成，并随同一提交评审。生成使用 CI 固定的 uv 版本，并以 `--exclude-newer` 截止到生成日前两周，避免刚发布的版本进入环境；只需跟进个别包时（各锁的 pymilvus、ref26 的 pyarrow），用 `--exclude-newer-package` 只放宽该包，其余包的截止日期不变。pymilvus 是被测对象，可以不受两周的限制：升级时须先核对接入点与 proto 的变化，并用新版跑完两条线的端到端与差分（pymilvus 3.0.3 发布 12 天后即按此纳入）；`--universal` 使同一份结果在三个平台通用，但不保证每个平台都有 wheel：例如参照环境的 faiss-cpu 1.15.1 在三个平台都有 wheel，但 macOS arm64 的最低目标是 macOS 14，更换托管机器或依赖版本时须重新核对。

### uv 的获取

uv 不经 PyPI 索引安装：索引响应可能被截断，而 pip 不重试响应体的解析错误（[0021 试点记录「偶发故障」](../../docs/testing/0021-release-pilot.md#偶发故障)）。[install_uv.sh](install_uv.sh) 由 [fetch.py](fetch.py) 按 [requirements/uv-wheels.txt](requirements/uv-wheels.txt) 的固定地址下载本平台 wheel、核对 sha256，再 `pip install --no-index --no-deps` 本地文件并核对版本；发行工作流（经 [release_env.sh](release_env.sh)）、快速层与对照层共用。清单的版本须与四个工作流（快速层、对照层、发行与 [PyPI 上传](../workflows/pypi-publish.yml)）的 `UV_VERSION` 一致，四个平台键须齐全，由 [tests/ci](../../tests/ci/test_fetch.py) 核对。

平台映射：Linux x86_64 → `manylinux_2_17_x86_64.manylinux2014_x86_64`；Linux aarch64 → `manylinux_2_28_aarch64`（manylinux_2_28 容器与 ubuntu-24.04-arm 的 glibc 均不低于 2.28，与 pip 按标签优先级的选择相同）；macOS arm64 → `macosx_11_0_arm64`；macOS x86_64 → `macosx_10_12_x86_64`。

更新 `UV_VERSION` 时同一提交重写清单，哈希按三个来源交叉核对后才写入：PyPI JSON（`https://pypi.org/pypi/uv/<版本>/json`）的 `digests.sha256`；本地下载后重算；wheel 中的 `uv` 二进制与 astral-sh/uv 同版本 GitHub Release 的对应压缩包（`uv-<target>.tar.gz`，先核对其 `.sha256` 附件）中的二进制逐字节相同，且该压缩包通过 `gh attestation verify --repo astral-sh/uv`。uv 0.12.23 在 PyPI 上没有 PEP 740 来源证明，因此以 GitHub Release 的证明为第三个来源。

## 传输层有限重试

按 0021 第 11 条，判定层不重试；传输层只对内容固定或只读的操作有限重试，间隔 5 / 15 / 45 秒，每次输出 `transport-retry` 警告注解，用尽即失败：

- [fetch.py](fetch.py) `file`：内容由 sha256 固定的下载（uv wheel、orjson sdist、jieba sdist、上一版本 wheel），任何下载失败都重试，哈希不符立即失败；
- [tests/release/runs.py](../../tests/release/runs.py) 的 `Gh(retry=True)`：只读 GET 只在暂时性传输错误（连接失败、超时、HTTP 500/502/503/504、HTTP/2 stream 错误如 `stream CANCEL`）或成功响应的 JSON 无法解析时重试，任何 HTTP 4xx 原样失败（429 也不重试：gh api 的错误输出不含 `Retry-After`）；前置检查（含未引用演练时读取已发布基线的运行记录，[0039](../../docs/decisions/0039-release-speedup.md)）、签发前复核、演练的证明 404 检查、`runs.py basis` 与 `release_set.py --prepare` 使用它，0022 的保全路径（门闩与 save；verify 不访问接口）使用默认的不重试实例；
- [Dockerfile.pid1](../../tests/platform/Dockerfile.pid1) 的 SDK 安装：完整 constraints 固定版本、只用 wheel，失败时整体重试；镜像构建输出不进入 job 日志，由 [linux_pid1_probe.py](../../tests/platform/linux_pid1_probe.py) 检索重试行后转发为注解，次数写入 `identity.json`。

另有一类等待，不是重试（[0042](../../docs/decisions/0042-pypi-prerelease.md) 决策 5）：[pypi_release.py](pypi_release.py) `wait-visible` 在上传后 10 分钟内每 15 秒查询一次索引，等它恰好列出这五个文件；还没看到与连接失败继续等，出现本组以外的文件或哈希不同立即失败，判定只在看到完整集合时执行一次。

uv、cargo、rustup 与 `gh attestation verify` 自带的网络重试保持默认，见各自官方文档；[release_env.sh](release_env.sh) 安装 rustup 的 `curl --retry 3` 维持原样。

## 检查脚本

- [rust_results.py](rust_results.py)：cargo test 的汇总须存在，且失败与 ignored 均为 0；可选的第二个参数要求 passed 总数恰为该值，用于按名称筛选的运行。
- [pytest_summary.py](pytest_summary.py)：仅用于分片工具自身的自检，核对普通 pytest 的末行汇总。
- [unittest_summary.py](unittest_summary.py)：用于以 subtests 组织的自检，核对运行数量与末行 `OK`。
- [cargo_fresh.py](cargo_fresh.py)：互通预构建后复跑的全部产物须为 fresh。
- [changes.py](changes.py)：两个工作流的分类 job 判定是否为纯文档，以待合入的全部差异为准，任何异常按全量执行（0020 公共约定 9）。
- [summary.py](summary.py)：`ci-pass` 与 `compare-pass` 的判定，只接受全量全部成功或纯文档时测试 job 全部跳过两种形态。
- [jieba_wheel.py](jieba_wheel.py)：参照环境 jieba 的受控构建与安装核对（见上文「固定依赖」），自检在 [tests/ci](../../tests/ci/test_jieba_wheel.py)。

changes.py 与 summary.py 的自检在 [tests/ci](../../tests/ci/test_ci_scripts.py)，随快速层工具自检运行。
- [crash_capture.sh](crash_capture.sh)：测试步骤开头以 source 执行，Linux 打开 core dump 并写到 `$RUNNER_TEMP/cores`、提供恢复 core 限制的 `CRASH_SUDO` 供 root 用例使用，macOS 与发行工作流的容器 job（设置 `CRASH_CORE_DIR`）只留时间标记。
- [crash_report.py](crash_report.py)：job 失败时打出崩溃进程的原生调用栈（Linux 用 gdb 读 core，先完整打印收到信号的线程；容器 job 从只读挂入的宿主机 systemd-coredump 目录取 core 并解压；macOS 读本 job 内的 ReportCrash 诊断报告，等到报告都可解析且 20 秒内没有新增或改写再读，先后写出的多份报告都收集，总等待仍不超过 90 秒），只用于诊断，不改变 job 结论；容器分支与 macOS 等待的自检在 [tests/ci](../../tests/ci/test_crash_report.py)。
- [collect_records.sh](collect_records.sh)：发行工作流的 job 失败时把运行记录复制到 `$RUNNER_TEMP/failure-records`，上传步骤只给这一个路径：容器 job 中 upload-artifact 的多行 path 只有第一行会换算成容器内路径，其余行找不到文件。upload-artifact 拒绝名字含 `" : < > | * ?` 与回车、换行的文件，只要有一个整份记录就上传失败，因此复制后剔除并列出这类条目。[tests/ci](../../tests/ci/test_workflow_paths.py) 检查容器 job 的 artifact 步骤只用单行路径，并在 Linux 与 macOS 上运行本脚本核对剔除结果。
- [public_release.py](public_release.py)：[公开下载核验工作流](../workflows/public-check.yml)（0021 第 20 条）的默认分支侧工具：输入版本先于任何 API 调用与 checkout 校验；用 token 读取 Release，要求已发布、预发布且不可变，附件名与公开地址合法；下载步骤不拿 token，附件须与 API digest 一致；发行集合由冻结 tag 中的 release_set.py 核对，通过后 tag 须指向候选冻结提交；安装 job 在只装了 `sdk30` 的干净 venv 中（以 pip 的 `--only-binary :all:` 安装，不另装 uv），把冻结 tag README 的安装命令拆成参数、只替换解释器后执行，pip report 须指向本平台的公开 wheel 与核验过的哈希，冒烟输出的版本与 SDK 须为预期值。输入 `source`（[0042](../../docs/decisions/0042-pypi-prerelease.md) 决策 5）选择安装来源：`github` 取冻结 README 中的 Release 安装命令，下载地址须为 Release 地址；`pypi` 取冻结 README 中的 PyPI 命令，冻结 tag 的检查器未导出 `pypi_install_command` 即拒绝；`testpypi` 不对照 README，用固定命令；后两者只核对文件主机与文件名，以哈希为准，verify job 另以 `pypi_release.py wait-visible` 核对索引上恰为这五个文件。自检在 [tests/ci](../../tests/ci/test_public_release.py)。
- [pypi_release.py](pypi_release.py)：[PyPI 上传工作流](../workflows/pypi-publish.yml)（0042）的默认分支侧工具：`guard` 要求台账为 released、拒绝 `0.0.*` 流程测试版本、目标为 pypi 时冻结 tag 已纳入 PyPI，索引上现有的文件都属于本组且同名同哈希（断点续传）；`files` 在上传 job 中按 verify job 输出的摘要核对 SHA256SUMS 与五个文件，按 wheel 在前、sdist 最后列出；`wait-visible` 见「传输层有限重试」末段。工具不放进 `tests/release/`，不改变 0039 的签发路径。上传用 `uv publish --trusted-publishing always` 逐个文件执行，`--check-url` 跳过已存在且相同的文件。汇总 job `pypi-pass` 要求 verify 与 upload 都成功，从非 `main` 触发时两者被跳过，整次运行因此失败。自检在 [tests/ci](../../tests/ci/test_pypi_release.py)。

## 工作流静态检查

修改工作流后用 [actionlint](https://github.com/rhysd/actionlint) 1.7.12 检查 `.github/workflows/` 下全部文件，不使用配置文件或忽略规则，结果须零告警。1.7.7 及更早版本不认识 `macos-15-intel` 标签，会对发行工作流误报。
