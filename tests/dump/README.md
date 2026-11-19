# tests/dump

`milvus-lite dump` 的端到端与差分（[0037](../../docs/decisions/0037-dump.md)「测试」）。不依赖 BulkWriter 的单测在 [tests/python/test_cli_dump.py](../python/test_cli_dump.py)。

- [e2e/](e2e/)：子进程运行已安装的 CLI，读回 BulkWriter 产物；覆盖各类 schema、3.x 五处缺陷的修正、写文件失败与线程竞态、失败清理、信号、`--uri` 与 LOCK 冲突。
- [diff/](diff/)：两侧分别建相同数据、运行各自的 dump，逐字节比较产物；登记的差异逐项断言。
- [dump_cases.py](dump_cases.py)：两者共用的建数据场景，只依赖 pymilvus，本实现与 3.x 的 python 都直接运行它。

## 环境

两个目录都要求 `pymilvus[bulk_writer]`，缺少时 [conftest.py](conftest.py) 直接报错，不跳过。它依赖的 pyarrow 没有 CPython 3.15 的 wheel，因此这里不并入 `tests/python`。

在已有的开发环境上叠装同版本的超集锁（不改变已有包的版本）：

```bash
uv pip install -p .venv/bin/python --no-build -r .github/ci/requirements/dump30.txt
# pymilvus 2.6 线：在装好 sdk26.txt 与本实现 wheel 的环境上
uv pip install -p <v26>/bin/python --no-build -r .github/ci/requirements/dump26.txt
```

差分另需 3.x 参照环境：按 [tests/differential/README.md](../differential/README.md) 准备 Arrow 25 环境，其锁 `ref25.txt` 已含 BulkWriter；`MILVUS_LITE_3X_PYTHON` 指向它的 python。conftest 核对参照版本（`REF_VERSION`，升级参照实现时与 tests/differential 同步修改）。

## 运行

```bash
.venv/bin/python -m pytest tests/dump/e2e
MILVUS_LITE_3X_PYTHON=<3.x 的 venv>/bin/python .venv/bin/python -m pytest tests/dump/diff
```

pymilvus 2.6 线按 [AGENTS.md](../../AGENTS.md)「常用命令」的方式在仓库外的目录运行同一路径。CI 中 e2e 在 python job、diff 在 Compare 的差分 job 运行；发行工作流记为 `dump-3.10-<sdk>` 与 `dump-diff-<sdk>` 阶段，计数见 [scorecard.py](../release/scorecard.py)。

用例会起子进程、在 `tmp_path` 下建数据目录；信号用例依赖“chunk 很小时每个文件约间隔 1 秒”来拉长导出窗口，D 的写失败用 `RLIMIT_FSIZE` 触发（以 root 运行时目录权限不起作用）。
