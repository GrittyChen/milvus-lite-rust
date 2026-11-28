# 0042 TestPyPI 演练

[0042](../decisions/0042-pypi-prerelease.md) 实施步骤 2：在 TestPyPI 上用已发布的 0.9.0rc1 走通上传工作流与 `testpypi` 一路的公开下载核验。演练于 2026-11-28（UTC）进行，两次运行的工作流与工具都取自 `main` 的 `799856b57a74f488a4d8e4a19ae1c6372430ef30`。

## 结论

- 可信发布在真实索引上走通。上传 job 用 GitHub OIDC 换取临时凭据，全程没有 API token。上传的就是 v0.9.0rc1 Release 中的四个 wheel 与 sdist，逐字节相同。
- 上传前的核验链与守卫按设计执行：冻结 tag 的 `release_set.py` 核对通过，tag 指向候选提交；上传守卫在 TestPyPI 上看到 0 个已有文件，5 个待传。
- 上传后索引立即列出恰好五个文件，哈希与 Release 的 `SHA256SUMS` 一致，可见性等待没有发生。四个标准平台从 TestPyPI 安装 wheel、核对哈希并冒烟全部通过。
- 未覆盖：本次没有已存在的文件，上传后也立即可见，下列路径都没有在真实索引上执行：
  - 上传守卫的续传判定（索引上已有本组同名同哈希文件时只列出其余文件）：有仓库内反证用例。
  - uv 的 `--check-url` 跳过已存在文件：没有测试，只在工作流中使用。
  - 可见性等待的轮询与超时：有仓库内反证用例。
  - 正式 PyPI 一侧，包括 `pypi` 环境、冻结 tag 纳入 PyPI 的守卫正向路径与抢注窗口，留到首个 PyPI 候选。

## 运行

| 步骤 | 运行 | 结果 |
| --- | --- | --- |
| 上传 | [pypi-publish 46135397758](https://github.com/GrittyChen/milvus-lite-rust/actions/runs/46135397758)（`version=0.9.0rc1`、`target=testpypi`） | verify、upload、pypi-pass 均成功 |
| 公开下载核验 | [public-check 46136843433](https://github.com/GrittyChen/milvus-lite-rust/actions/runs/46136843433)（`source=testpypi`） | verify、四个平台 install、public-pass 均成功 |

- **公开产物**：[v0.9.0rc1 Release](https://github.com/GrittyChen/milvus-lite-rust/releases/tag/v0.9.0rc1)，tag 指向冻结提交 `a8f3b5112149d006ce8cf29e990c290a6dec24de`；`SHA256SUMS` 的 SHA256 为 `8f829a88a920006922d407fb4a2145af84f81bac76dd169d96bfc05398447cbf`。
- **环境**：`testpypi` 按 0042 决策 4 配置，建好后读回核对（本地留存）：
  - 必需审批人只有负责人，`prevent_self_review=false`、`can_admins_bypass=false`；
  - 不设等待计时器、环境 secret 或变量；
  - 部署分支只允许 `main`。
- **审批**：经负责人授权，以负责人账号经审批接口批准。
- **名字**：演练前只读查询，`milvus-lite-rust` 在 TestPyPI 与 PyPI 上均返回 404（本地留存）。

## 上传结果

TestPyPI JSON API（`/pypi/milvus-lite-rust/0.9.0rc1/json`）列出的五个文件，与 Release `SHA256SUMS` 的哈希逐一相同，文件地址都在 `test-files.pythonhosted.org`。上传顺序为四个 wheel 在前、sdist 最后，与 `files` 子命令给出的顺序一致。uv 的日志中每个文件的 “Uploading” 行出现两次，这是进度输出，不是重试：每个文件只有一行 “Uploaded”，索引上每个文件也只有一个上传时间。

sdist（`milvus_lite_rust-0.9.0rc1.tar.gz`）的 SHA256 为 `cd3cf0d78b8c2f735890c341885a34bcbb9792812b7e31cd7549018d0800c55b`，与 Release 相同。

四个平台的 install job 用工具生成的固定命令（`--index-url https://test.pypi.org/simple/ --no-deps --only-binary :all:`）安装，pip report 中的 wheel 哈希与 `SHA256SUMS` 一致：

| 平台 | wheel SHA256 |
| --- | --- |
| manylinux_2_28_x86_64 | `cb0ee2519b724fe67f664d17a5e3912548c07f8949c5213e25618d59ce026d71` |
| manylinux_2_28_aarch64 | `fd327f326211a575c9036365d87e873cb2c944b367b6ee4a4df730447fd7971a` |
| macosx_11_0_arm64 | `540d52751aa256abfe16acf4183abfe65943d5cc3a914d7e18abaed91dce0ce7` |
| macosx_11_0_x86_64 | `aec87dc18e3cd42cf868197f82d368c58edc61491da97a528a23ef19877b3930` |

冒烟输出的版本为 0.9.0rc1，SDK 为冻结 tag 锁定的 pymilvus 3.0.3，四个阶段全部通过。

## 计时

用于估计 0042 决策 3 的抢注窗口中上传与审批的部分，时间为 2026-11-28 UTC：

| 阶段 | 时长 |
| --- | --- |
| 触发到 verify 完成（10:29:16 → 10:30:02） | 46 秒 |
| 等待审批（10:30:02 → 10:43:51） | 约 14 分钟；其间在讨论审批方式，不代表正式上传时的耗时 |
| 批准到上传 job 完成（10:43:51 → 10:44:57） | 66 秒，其中上传步骤 16 秒、可见性核对 1 秒 |
| public-check（10:47:43 → 10:50:05） | 2 分 22 秒 |

审批人预先到位时，上传与审批约 2 分钟。连同 0042 中已测的约 27 分钟，首个 PyPI 版本的窗口约半小时，与决策 3 的估计一致。

## 证据

- 仓库内可重跑：`.venv/bin/python -m pytest tests/ci`，判据为全部通过（演练时 221 项）。与上文未覆盖路径对应的用例在 [test_pypi_release.py](../../tests/ci/test_pypi_release.py)：续传判定 `test_guard_resumes_with_identical_files`，异组文件与哈希不符 `test_guard_rejects_foreign_or_different_files`，可见性等待 `test_wait_visible_*`；`testpypi` 一路的固定命令在 [test_public_release.py](../../tests/ci/test_public_release.py) 的 `test_sources_pick_their_commands`。
- 公开产物：v0.9.0rc1 Release 附件与 `SHA256SUMS`；TestPyPI 上的五个文件及其 JSON API 记录（`https://test.pypi.org/pypi/milvus-lite-rust/0.9.0rc1/json`）。
- CI 运行记录：上表两次运行，`head_sha` 均为 `799856b`，有保留期限。
- 本地留存，由负责人本地留存：
  - 两次运行各 job 的日志，含 uv 的逐文件上传输出；
  - 运行时间记录与 TestPyPI JSON 响应；
  - `testpypi` 环境的读回输出：用 `gh api repos/<owner>/<repo>/environments/testpypi` 及其 `deployment-branch-policies`、`secrets`、`variables` 子接口读取，需要仓库管理权限；
  - 演练前查询名字的输出。
