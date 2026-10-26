# 门闩探针响应

0022 门闩平台探针（运行 40281637463，临时环境 `gate-probe`）的接口原始响应，只删去键名以 `url` 结尾、`node_id` 与 `gravatar_id` 字段，其余原样：

- `environment.json`：配置完成后的环境（含必需审批人与只允许探针分支的部署分支规则，`can_admins_bypass` 为默认的 `true`）；
- `jobs-waiting.json`、`jobs-rejected.json`：attempt 1 中引用环境的 `gate` job 在等待审批与被拒绝后的 jobs 响应；
- `approvals-rejected.json`、`approvals-approved.json`：attempt 1 的拒绝与 attempt 2 以回执格式备注的批准。

测试以它们为基础构造正确配置与各项偏差，见 [test_runs_and_release_set.py](../../test_runs_and_release_set.py) 与 [test_preserve.py](../../test_preserve.py)。
