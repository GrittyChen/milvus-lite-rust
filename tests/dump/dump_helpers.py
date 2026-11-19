"""tests/dump 的公共辅助：在子进程中建数据与运行 dump，读出 BulkWriter 产物。"""

from __future__ import annotations

import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))


def build(case, db, python=sys.executable):
    # 在独立进程中建数据：进程退出即释放 LOCK，dump 子进程才能打开同一目录。
    result = subprocess.run([python, os.path.join(HERE, "dump_cases.py"), case, str(db)],
                            capture_output=True, text=True, timeout=300)
    assert result.returncode == 0, result.stderr[-3000:]


def dump(args, python=sys.executable, **kwargs):
    kwargs.setdefault("timeout", 300)
    return subprocess.run([python, "-m", "milvus_lite", "dump", *map(str, args)],
                          capture_output=True, text=True, **kwargs)


def last_line(text):
    lines = [line for line in text.splitlines() if line.strip()]
    return lines[-1] if lines else ""


def outputs(out):
    """返回 {uuid 目录: [按序号排列的文件名]}；out 不存在时为空。"""
    found = {}
    if not os.path.isdir(out):
        return found
    for name in sorted(os.listdir(out)):
        path = os.path.join(out, name)
        if os.path.isdir(path):
            found[name] = sorted(os.listdir(path), key=lambda f: int(f.split(".")[0]))
    return found


def rows(out):
    """唯一一次导出的全部行（按文件序号拼接）与原始文本。"""
    found = outputs(out)
    assert len(found) <= 1, found
    collected, texts = [], []
    for uuid, files in found.items():
        for name in files:
            with open(os.path.join(out, uuid, name), encoding="utf-8") as f:
                text = f.read()
            texts.append(text)
            collected.extend(json.loads(text)["rows"])
    return collected, texts
