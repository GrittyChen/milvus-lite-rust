"""汇总 job 的判定（0020「判定与流程」第 3 条）：读取环境变量 NEEDS（`toJSON(needs)`）。

只有两种合法形态：分类为全量时全部上游 success；分类为纯文档时分类 job success、其余上游全部 skipped。
分类 job 未成功、输出缺失、部分跳过、failure、cancelled 或混合状态都失败。
"""

import json
import os
import sys

CLASSIFIER = "changes"


def verdict(needs):
    changes = needs.get(CLASSIFIER)
    if not changes or changes.get("result") != "success":
        return False, f"{CLASSIFIER} did not succeed: {changes}"
    mode = (changes.get("outputs") or {}).get("docs_only")
    others = {name: need.get("result") for name, need in needs.items() if name != CLASSIFIER}
    if not others:
        return False, "no jobs besides the classifier"
    expected = {"false": "success", "true": "skipped"}.get(mode)
    if expected is None:
        return False, f"unexpected docs_only output {mode!r}"
    if any(result != expected for result in others.values()):
        return False, f"docs_only={mode} requires every job to be {expected}: {others}"
    return True, f"docs_only={mode}: {others}"


def main():
    ok, message = verdict(json.loads(os.environ["NEEDS"]))
    print(message)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
