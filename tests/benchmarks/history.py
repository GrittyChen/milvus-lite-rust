"""历史测量输入来自固定 Git 树，不能随当前脚本的新增、删除或修改漂移。"""
from pathlib import Path
import subprocess


BASELINE_COMMIT = "e7632ad8e38e226e3efcf69cc24baac2271e0ba4"


def copy_baseline_snapshot(repo, destination):
    tree = f"{BASELINE_COMMIT}:benchmarks/baseline"
    result = subprocess.run(["git", "-C", str(repo), "ls-tree", "--name-only", tree],
                            capture_output=True, text=True, check=True)
    destination.mkdir(parents=True, exist_ok=True)
    for name in result.stdout.splitlines():
        if Path(name).suffix in (".py", ".sh"):
            content = subprocess.run(["git", "-C", str(repo), "show", f"{tree}/{name}"],
                                     capture_output=True, check=True).stdout
            (destination / name).write_bytes(content)
