"""核对 Linux strace 的真实成功系统调用，不把内存事件记录当系统调用证据。

输入为 `strace -ff -ttt -yy` 的输出前缀；目标是 concurrency_faults 中
inherited_and_new_files_are_synced_before_replace_and_wal_unlinks_stay_ordered。
"""

import json
from pathlib import Path
import re
import sys


def check(prefix):
    events = []
    for path in Path(prefix).parent.glob(Path(prefix).name + ".*"):
        for line in path.read_text().splitlines():
            match = re.match(r"(\d+\.\d+) (.*)", line)
            if match and re.search(r"= 0$", match[2]):
                events.append((float(match[1]), match[2]))
    events.sort()
    calls = [call for _, call in events]
    data = next((i for i, call in enumerate(calls) if call.startswith(("unlink(", "unlinkat("))
                 and "wal_data_000002.arrow" in call), None)
    delta = next((i for i, call in enumerate(calls) if call.startswith(("unlink(", "unlinkat("))
                  and "wal_delta_000002.arrow" in call), None)
    if data is None or delta is None or data >= delta:
        raise AssertionError("同代 data/delta unlink 缺失或顺序错误")
    wal = re.search(r'"([^\"]*/wal)/wal_data_000002.arrow"', calls[data])[1]
    base = wal.removesuffix("/wal")
    replaces = [i for i, call in enumerate(calls[:data]) if call.startswith(("rename(", "renameat(", "renameat2("))
                and f'"{base}/manifest.json"' in call]
    if not replaces:
        raise AssertionError("WAL unlink 之前没有成功提交 manifest")
    replace = replaces[-1]

    def synced(path, begin=0, end=replace):
        return any(call.startswith(("fsync(", "fdatasync(")) and f"<{path}>" in call
                   for call in calls[begin:end])

    for suffix in ("schema.json", "manifest.json.tmp",
                   "partitions/_default/data/data_000001_000001.parquet",
                   "partitions/_default/data/data_000002_000002.parquet",
                   "partitions/_default/delta/delta_000003_000003.parquet",
                   "wal/wal_data_000002.arrow", "wal/wal_delta_000002.arrow"):
        if not synced(base + "/" + suffix):
            raise AssertionError(f"replace 前未同步 {suffix}")
    for directory in (base, str(Path(base).parent), str(Path(base).parents[1]), str(Path(base).parents[2]),
                      base + "/partitions", base + "/partitions/_default",
                      base + "/partitions/_default/data", base + "/partitions/_default/delta", wal):
        if not synced(directory):
            raise AssertionError(f"replace 前未同步目录 {directory}")
    if not synced(base, replace + 1, data):
        raise AssertionError("manifest replace 后、WAL unlink 前未同步 collection 目录")
    if not synced(wal + "/wal_data_000002.arrow", replace + 1, data) or not synced(
            wal + "/wal_delta_000002.arrow", replace + 1, delta):
        raise AssertionError("已提交 WAL 在各自 unlink 前未再次同步")
    next_commit = next((i for i in range(delta + 1, len(calls))
                        if f"<{base}/manifest.json.tmp>" in calls[i] or f'"{base}/manifest.json"' in calls[i]), len(calls))
    if not synced(wal, data + 1, delta) or not synced(wal, delta + 1, next_commit):
        raise AssertionError("每次 WAL unlink 后缺少 wal/ 同步")
    return {"successful_calls": len(calls), "pre_replace_file_sync": True,
            "pre_replace_directory_sync": True, "post_replace_directory_sync": True,
            "data_then_delta_unlink": True, "sync_after_each_unlink": True}


if __name__ == "__main__":
    print(json.dumps(check(sys.argv[1])))
