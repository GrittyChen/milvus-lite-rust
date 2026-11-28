# 0042：发布到 PyPI（预发布）

- 状态：草案，负责人已复审通过文本（2026-11-28）；未实施。采纳后修订以下几处，修订清单见「实施步骤」第 1 条：
  - [0013](0013-first-release-platform-and-packaging.md) 候选阶段决策 6 中“RC 阶段只发布 GitHub Release”及由它派生的安装说明与白名单；
  - [0021](0021-release-build-in-ci.md) 第 20 条中的发布步骤，以及第 11 条中允许有限重试的操作清单（加入决策 5 的可见性等待）。
- 日期：2026-11-28。
- 依据：[0013](0013-first-release-platform-and-packaging.md) 候选阶段决策 3、6；[0020](0020-continuous-integration.md) 第 3 条；[0021](0021-release-build-in-ci.md) 第 3、20 条；[0039](0039-release-speedup.md)。

## 问题

已发布的十三个产品 RC（0.1.0rc1 至 0.9.0rc1）都只发在 GitHub Release 上。0013 决策 6 把“是否上 PyPI”留到试用验证之后再定。现在上 PyPI 有两条理由：

- 1.0 的前提是有外部真实使用，而用户默认从 PyPI 安装。从 Release 安装要写 `--no-index --find-links`，门槛高，下载也难统计。
- `milvus-lite-rust` 在 PyPI 与 TestPyPI 上仍未被注册（2026-11-28 只读查询 JSON API，均返回 404）。首次上传之前，名字随时可能被他人注册。

上 PyPI 与现行做法有四处冲突，需要先定下来：

1. **PyPI 文件不可替换**。文件名一经使用就不能复用，删除后也不能再上传同名文件；处理坏版本只能 yank 或删除，本项目选择 yank（决策 9）。
2. **README 就是 PyPI 页面**。README 是 wheel 与 sdist 的长描述，随候选冻结。现行 README 写着“没有发布到 PyPI”，只给 Release 安装命令；检查器只放行这类命令。
3. **公开核验从冻结 README 取安装命令**。[public_release.py](../../.github/ci/public_release.py) 的 `install` 从冻结 tag 的白名单取唯一一条 `pip install` 命令，并把下载地址固定为 Release 地址。
4. **上传凭据与触发**。凭据放在哪里、由谁触发，决定了“PyPI 上的文件就是已验收的文件”能否成立。

## 决策

1. **上传 Release 上已发布的四个 wheel 与 sdist**，与 Release 附件逐字节相同，不另行构建或打包。候选结果 JSON 与 `SHA256SUMS` 不上传。
2. **上传在 GitHub Release 发布且 public-check（GitHub 一路）通过之后进行**。PyPI 比 GitHub Release 更不可逆，放在最后：
   - GitHub 侧发现问题时，可以放弃该版本，PyPI 上什么都还没消费；
   - PyPI 上传失败时，GitHub Release 仍然有效，修好传输问题后用同一组文件重试。
3. **只有首个 PyPI 版本有抢注窗口**。项目首次上传成功之后，该名字归本项目所有，他人无法再向它上传。窗口是“首个 PyPI 版本的 Release 发布”到“首次上传成功”之间，处理如下：
   - **上传前查名**：冻结候选前、发布 Release 前、触发上传前，各只读查一次 PyPI 上的名字。
   - **连续操作**：依次进行发布 Release、台账登记 `released` 并按推送门槛合入 `main`（核验 job 读 `main` 上的台账）、public-check、触发上传、审批，负责人预先到位审批。已测部分约 27 分钟，加上上传与审批估计约半小时，主要花在台账提交的推送门槛上。按 0.9.0rc1 实测：
     - Release 发布到台账提交 47 秒；
     - 台账提交的 CI 12 分 22 秒（运行 46109525797）、Compare 24 分 35 秒（运行 46109525665），两者并行；
     - public-check 1 分 36 秒（运行 46111463393）；
     - 上传与审批的时长要在 TestPyPI 演练中计时，结果写进试点记录。
   - **窗口内被注册**：README 中的 PyPI 命令此时指向他人的包。立即停止上传，在 `main` 的 README 与置顶 Issue 中说明，并尽快发布不含 PyPI 命令的新版本，之后另议名字。
   - **不采用“先传 PyPI、后发 Release”**：那样上传会发生在公开下载核验之前，上传工作流也无法匿名取得草稿 Release 的附件。
   - **不采用“首个 PyPI 版本的 README 只写 Release 命令”**：按名从 PyPI 安装的用户同样会落到他人的包上，窗口并没有消失，反而拖长。
4. **用 PyPI 可信发布，不用长期 API token**。新增工作流 `.github/workflows/pypi-publish.yml`，只由 `workflow_dispatch` 触发，输入为版本号与目标索引（`pypi` 或 `testpypi`）。工作流分两个 job，都以 `github.ref == 'refs/heads/main'` 为条件。
   - **核验 job**：只读，没有 `id-token`。
     - 复用 public-check 的步骤：Release 属性、匿名下载、用冻结 tag 中的 `release_set.py` 核对发行集合与候选结果的来源证明、tag 提交与冻结提交一致。
     - 版本守卫：
       - 台账中该版本为 `released`；
       - 拒绝 `0.0.1rc9NN` 这类流程测试版本；
       - 目标为 `pypi` 时，冻结 tag 的白名单须含 PyPI 安装命令（见第 6 条）；旧 tag 的检查器没有导出 PyPI 命令时按“不含”处理，一律拒绝，这样已发布的旧版本永远传不上 PyPI；
       - 目标索引上该版本尚不存在；或已有的文件全部属于本组，且同名同哈希（断点续传）。出现本组以外的任何文件即失败。
     - `SHA256SUMS` 的摘要经 job output 传给上传 job，与 public-check 相同。
   - **上传 job**：
     - 运行在 GitHub 环境 `pypi` 或 `testpypi` 中。沿用 0021 第 19 条中 `release-signing` 的做法：只有一条 `required_reviewers` 规则，审批人为负责人，`prevent_self_review=false`；`can_admins_bypass=false`；不设等待计时器、环境 secret 或变量。另外只允许 `main` 部署。只有这个 job 获得 `id-token: write`。
     - 先用 job output 中的摘要核对下载到的 `SHA256SUMS`，再按它核对五个文件。
     - 用 [0020](0020-continuous-integration.md) 第 3 条按哈希固定的 uv 逐个上传，四个 wheel 在前、sdist 最后。这样即使上传中途失败，也不会出现只有 sdist、缺 wheel 的状态，避免用户落到本机构建。
     - 命令为 `uv publish --trusted-publishing always --check-url <索引的 simple 地址>`。`--check-url` 只跳过目标索引上已存在且完全相同的文件，用于断点续传。TestPyPI 另加 `--publish-url https://test.pypi.org/legacy/`。
     - 可信发布者在 PyPI 一侧绑定仓库、工作流文件名与环境名，三者改动须同步改 PyPI 配置。
   - **签发路径不变**：[runs.py](../../tests/release/runs.py) 的签发路径判定比较三样东西：release.yml 的头部、release.yml 的最后两个 job、`tests/release/` 下的六个签发工具（`candidate.py`、`ledger.py`、`preserve.py`、`release_set.py`、`runs.py`、`sums.py`）。本决策不改 release.yml，新增的工具函数放在 `.github/ci/`，不改这六个文件，因此 [0039](0039-release-speedup.md) 的基线判定不受影响。合入后用 `runs.py basis` 对新提交确认签发路径未变。
5. **上传后核验**：[public-check.yml](../../.github/workflows/public-check.yml) 增加输入 `source`，取值为 `github`、`pypi` 或 `testpypi`，默认 `github`。
   - `github` 一路保持现行，安装命令取自冻结 README 中的 Release 安装命令（见第 6 条）。
   - `pypi` 一路只对冻结白名单含 PyPI 命令的版本可用，其余版本直接失败并说明原因。核对两项：
     - PyPI JSON API 列出的该版本恰为这五个文件，`digests.sha256` 与 Release 的 `SHA256SUMS` 一致；
     - 在干净环境中按冻结 README 的 PyPI 命令安装，pip report 的 wheel 哈希与 `SHA256SUMS` 一致。下载地址不固定，按哈希判定。
   - `testpypi` 一路不对照 README，由工具生成固定命令 `--index-url https://test.pypi.org/simple/ --no-deps --only-binary :all: milvus-lite-rust==<版本>`，其余核对同 `pypi` 一路。
   - 刚上传后，JSON API 与 simple 索引可能因 CDN 缓存暂时看不到新文件。工具在固定上限内（10 分钟）轮询可见性，这只是等待，不是重试判定；到达上限即失败。判定本身只执行一次。这是 0021 第 11 条之外新增的一类等待，随本决策修订该条。
6. **README 与检查器**（随首个 PyPI 版本的候选冻结）：
   - README 的主安装命令改为 `python -m pip install --only-binary milvus-lite-rust milvus-lite-rust==<版本>`。`--only-binary` 只作用于本包。
   - 原 Release 安装命令保留，作为第二种安装方式，供 `github` 一路使用；下载核对 `SHA256SUMS` 与 `gh attestation verify` 的段落也保留，PyPI 上的文件与 Release 附件相同，同样适用。
   - 删去“没有发布到 PyPI”一句。“本包只从 GitHub Release 安装”及其对 `--no-index` 的解释改为分别说明两种安装方式。卸载官方包、不用 `pymilvus[milvus-lite]` 的说明不变。
   - [check_artifacts.py](../../tests/packaging/check_artifacts.py) 的白名单增加 PyPI 命令，并单独导出，供第 4、5 条判断版本是否“已纳入 PyPI”。[public_release.py](../../.github/ci/public_release.py) 按 `source` 从冻结白名单取对应命令，并按哈希而不是固定地址判定 pip report。
   - 复核旧版本时仍用该版本 tag 中的工具，旧版本只支持 `github` 一路。
7. **首个 PyPI 版本是下一个候选，不回填已发布版本**。已发布版本的 README 写着“没有发布到 PyPI”，长描述无法再改，回填会让 PyPI 页面自相矛盾。
   - 相对 0.9.0rc1 没有改变构建产物行为的代码变化时，取 `0.9.0rc2`；有这类变化时，取 `0.10.0rc1`。README、元数据、工作流、检查器、`tests/` 中的工具与 `crates/` 内的测试模块都不算。
   - 元数据加 `Development Status :: 4 - Beta` 分类。`Project-URL` 保持 Repository 与 Issues 两项不变，检查器要求这两项恰好如此（0013）。
8. **sdist 一并上传**。它在 0013 的发行集合内，也是其他平台自行构建的入口。没加 `--only-binary` 的用户在不受支持的平台上会下到 sdist，构建时被拒绝或因缺少 Rust 而失败。这与现行 sdist 的行为相同（[使用说明「从源码构建」](../usage.md#从源码构建)）。
9. **只 yank，不删除**。发现严重问题的版本在 PyPI 上 yank，并发新版本修复；不删除文件，不复用文件名，与 0013 决策 3 一致。yank 记入该版本的发布后记录；台账结局不变，因为 GitHub Release 仍为 `released`。
10. **台账格式不变**。PyPI 的上传运行 ID、五个文件在 PyPI 上的哈希与 public-check（`pypi` 一路）的结果，记入该版本验收记录的发布阶段。PyPI 只接收已 `released` 的版本，版本消费的判据仍按 0021 第 3 条。
11. **在 TestPyPI 上演练**：首次正式上传前，用 0.9.0rc1 的公开文件在 TestPyPI 上走一遍上传与 `testpypi` 一路核验。
    - 目标为 `testpypi` 时，第 4 条的 PyPI 命令守卫不适用，其余守卫照常。
    - 此后改动上传工作流，都要先在 TestPyPI 上演练。演练用一个尚未上传过 TestPyPI 的较早已发布版本，让上传与 OIDC 真正执行，而不是全部命中跳过。只有 0.3.0rc3 及之后的版本有候选结果、tag 中的 `release_set.py` 与台账条目，能通过核验 job；除 0.9.0rc1 外还有 0.3.0rc3、0.4.0rc1 至 0.8.0rc1 共 6 个。用完之后的演练方式届时另定。

## 不采用的方案

- **在 release.yml 的签发 job 中上传**：
  - 上传会发生在负责人的验收与发布决定之前；
  - 签发 job 要多一项发布用途的 `id-token`；
  - 签发路径因此变化，需要演练（0039）。
- **本机用 twine 与 API token 上传**：要保存长期机密；上传的字节经过个人机器，不能由工作流证明它们就是 Release 附件。
- **使用 `pypa/gh-action-pypi-publish`**：它能生成并上传 PEP 740 证明，但要新引入一个容器 action 及其镜像。uv 已按哈希固定。uv 只能上传已有的证明文件，自己不生成（[uv 文档](https://docs.astral.sh/uv/guides/package/)），因此 PyPI 上不会有 PEP 740 证明，来源证明只依靠发行工作流签发的 GitHub 构建证明，README 的核对命令即针对它。PEP 740 证明只能说明“由上传工作流上传”，不能代替构建证明。
- **先上传空壳占名**：0013 已否决，理由不变。

## 负责人要做的对外动作

1. 在 PyPI 与 TestPyPI 上开账号或使用已有账号，开启 2FA；考虑加第二位维护者，避免单点。
2. 在两边分别配置待定可信发布者：
   - 项目 `milvus-lite-rust`；
   - 仓库 `GrittyChen/milvus-lite-rust`；
   - 工作流 `pypi-publish.yml`；
   - 环境 `pypi` 或 `testpypi`。
   待定发布者不占名；首次上传时使用，成功后转为普通发布者（[PyPI 文档](https://docs.pypi.org/trusted-publishers/creating-a-project-through-oidc/)）。
3. 在 GitHub 仓库建 `pypi` 与 `testpypi` 两个环境，设置见决策 4 的上传 job。
4. 分别授权以下几步：工作流合入、TestPyPI 演练、首个 PyPI 候选的发行、正式上传。

## 实施步骤与验收

1. **工作流、工具与文档**：
   - 新增 `pypi-publish.yml`、public-check 的 `source` 输入及工具函数，工具放在 `.github/ci/`。
   - 自检覆盖以下情形：哈希不符、来源证明失败、台账未 released、流程测试版本、冻结白名单不含 PyPI 命令、索引上已有不同哈希的同名文件、文件数不是五、断点续传、可见性等待超时。
   - 同步修订以下位置：
     - 0013：附件说明、安装说明、白名单表述，以及“是否转 PyPI 另行决定”；
     - 0021 第 11、20 条；
     - [AGENTS.md](../../AGENTS.md) 中“未发布到 PyPI”；
     - [使用说明](../usage.md)；
     - [tests/packaging/test_artifact_checks.py](../../tests/packaging/test_artifact_checks.py) 中按名安装的负例；
     - [.github/ci/README.md](../../.github/ci/README.md) 与 [tests/packaging/README.md](../../tests/packaging/README.md)。
   - 经临时分支拿到 CI 与 Compare 后合入。
2. **TestPyPI 演练**：演练前只读查一次 TestPyPI 上的名字。若已被他人注册，演练无法进行，停下另议，不改用其他名字。用 0.9.0rc1 跑一次上传与 public-check（`testpypi` 一路），结果与各步计时记入 `docs/testing/0042-pypi-pilot.md`，创建后在 [docs/README.md](../README.md) 的 0042 行补链接。
3. **首个 PyPI 候选**：版本切换提交改 README、元数据与白名单，发行说明中的安装命令与 README 一致。按 0021 走完整候选，GitHub Release 发布后通过 public-check（`github` 一路）。
4. **正式上传**：按第 3 条连续操作，触发 `pypi-publish.yml`，负责人在 `pypi` 环境审批；再跑 public-check（`pypi` 一路），结果记入该版本验收记录。

## 残余风险

- **首个 PyPI 版本的抢注窗口**：见决策 3。窗口内被注册的概率低，但后果是 README 命令指向他人的包，只能靠发新版本与公告处理。
- **PyPI 自身故障或策略变化**：例如上传限额、元数据规则变化。上传失败不影响 GitHub Release；须改元数据时只能换版本。
- **按名安装**：PyPI 上只有预发布版本时，不带版本号的 `pip install milvus-lite-rust` 会装上最新的 rc，且可能取 sdist。README 始终写明版本号与 `--only-binary`。
