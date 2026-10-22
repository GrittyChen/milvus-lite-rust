"""版本台账 docs/releases/consumed-versions.json 的严格读取（0021 第 3 条）。

凡正式签发步骤开始过的运行都登记在台账中；读取或校验失败一律拒绝，不当作空台账。
结局：candidate 为已签发、待发布决定；released、abandoned、signing-failed 为最终结局。
"""

import argparse
import json
from pathlib import Path
import sys

from candidate import VERSION, CandidateError, is_int, require

FORMAT = 1
OUTCOMES = ("candidate", "released", "abandoned", "signing-failed")
ENTRY_KEYS = {"version", "run_id", "run_attempt", "outcome"}


def validate(data):
    """返回 {版本: 条目}。"""
    require(isinstance(data, dict) and set(data) == {"format", "versions"}, "ledger keys differ")
    require(type(data["format"]) is int and data["format"] == FORMAT, f"unknown ledger format {data['format']!r}")
    require(isinstance(data["versions"], list), "ledger versions must be a list")
    entries = {}
    for entry in data["versions"]:
        require(isinstance(entry, dict) and set(entry) == ENTRY_KEYS, f"ledger entry keys differ: {entry!r}")
        require(isinstance(entry["version"], str) and VERSION.fullmatch(entry["version"]), f"bad ledger version {entry['version']!r}")
        require(is_int(entry["run_id"], 1) and is_int(entry["run_attempt"], 1), f"bad ledger run for {entry['version']}")
        require(entry["outcome"] in OUTCOMES, f"unknown ledger outcome {entry['outcome']!r}")
        require(entry["version"] not in entries, f"version {entry['version']} appears twice in ledger")
        entries[entry["version"]] = entry
    return entries


def loads(text):
    try:
        data = json.loads(text)
    except json.JSONDecodeError as error:
        raise CandidateError(f"ledger is not valid JSON: {error}") from error
    return validate(data)


def check_preparable(entries, version, run):
    """发布准备：无条目可以；有条目时必须正是这次运行，且仍处于待发布决定。"""
    entry = entries.get(version)
    if entry is None:
        return
    require(entry["run_id"] == run["id"] and entry["run_attempt"] == run["attempt"],
            f"ledger records run {entry['run_id']}.{entry['run_attempt']} for {version}, not {run['id']}.{run['attempt']}")
    require(entry["outcome"] == "candidate", f"ledger outcome for {version} is {entry['outcome']}")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("path", type=Path)
    parser.add_argument("--unconsumed", help="additionally require that this version is not in the ledger")
    args = parser.parse_args(argv)
    try:
        entries = loads(args.path.read_text())
        require(args.unconsumed is None or args.unconsumed not in entries, f"version {args.unconsumed} already consumed")
    except (OSError, CandidateError) as error:
        print(f"ledger rejected: {error}", file=sys.stderr)
        return 1
    print(f"ledger valid: {len(entries)} consumed versions")
    return 0


if __name__ == "__main__":
    sys.exit(main())
