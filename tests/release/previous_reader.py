"""已发布的上一版本读取新产物生成的数据目录（0021 第 9 条的兼容探针）。

新版本解释器写出普通与 partition key 集合的 WAL、flush 与实际合并三种目录，上一版本解释器读取并核对值；
key 集合由上一版本追加后，再由新版本重开核对。写入与读取代码取自仓库的互通用例，与其断言保持同源。
上一版本环境先核对版本号与原生模块哈希，避免把未知产物当作已发布的上一版本。
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

STATES = ("wal", "flushed", "compacted")
IDENTITY = ("import hashlib, importlib.metadata as m, pathlib, milvus_lite._native as n;"
            "print(m.version('milvus-lite-rust'), hashlib.sha256(pathlib.Path(n.__file__).read_bytes()).hexdigest())")
REOPEN = """
import sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as m
c=MilvusClient(sys.argv[1],dedicated=True);c.load_collection('c')
assert list(c.get('c',ids=[1],output_fields=['extra']))==[{'id':1,'extra':100}]
assert list(c.get('c',ids=[100],output_fields=['k','j']))==[{'id':100,'k':1,'j':None}]
c.close();m.release_server(sys.argv[1]);print('ok')
"""


def snippet(repo, test, name):
    text = (Path(repo) / "tests/storage_interop" / test).read_text()
    marker = f"{name}=r'''" if f"{name}=r'''" in text else f"{name} = r'''"
    if text.count(marker) != 1:
        raise SystemExit(f"cannot locate {name} in {test}")
    return text.split(marker, 1)[1].split("'''", 1)[0]


def run(work, name, python, code, args):
    with (work / f"{name}.out").open("x") as out, (work / f"{name}.log").open("x") as log:
        process = subprocess.run([str(python), "-B", "-c", code, *map(str, args)], cwd=work, stdout=out, stderr=log,
                                 env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"}, timeout=300)
    if process.returncode:
        raise SystemExit(f"{name} failed with {process.returncode}: {(work / f'{name}.log').read_text()[-3000:]}")
    return (work / f"{name}.out").read_text()


def data_files(database, partition):
    manifest = json.loads((database / "collections/c/manifest.json").read_text())
    return manifest["partitions"][partition]["data_files"]


def probe(repo, work, new_python, old_python, old_version, old_native):
    work = Path(work).resolve()
    # 子进程在工作目录中运行；解释器只取绝对路径、不解析软链，venv 的 python 解析后会落到基础解释器。
    new_python, old_python = os.path.abspath(new_python), os.path.abspath(old_python)
    work.mkdir(parents=True)
    version, native = run(work, "old-identity", old_python, IDENTITY, []).split()
    if (version, native) != (old_version, old_native):
        raise SystemExit(f"previous reader is {version} with native {native}, expected {old_version} with {old_native}")
    code = {"write-default": snippet(repo, "test_nullable_scalars.py", "RUST_WRITE"),
            "read-default": snippet(repo, "test_nullable_scalars.py", "READ_CHANGE"),
            "write-key": snippet(repo, "test_partition_key.py", "WRITE"),
            "read-key": snippet(repo, "test_partition_key.py", "READ_APPEND")}
    cases = []
    for kind in ("default", "key"):
        for state in STATES:
            name = f"{kind}-{state}"
            database = work / f"{name}.db"
            run(work, f"write-{name}", new_python, code[f"write-{kind}"], [database, state])
            if state == "compacted":
                # 与互通用例相同的判据：普通集合合并为一个文件；key 集合 _pk_10 的九次 flush 发生了实际合并。
                files = data_files(database, "_default" if kind == "default" else "_pk_10")
                if (kind == "default" and len(files) != 1) or (kind == "key" and len(files) >= 9):
                    raise SystemExit(f"{name} did not compact: {files}")
            if kind == "default":
                run(work, f"read-{name}", old_python, code["read-default"], [database, 12 if state == "compacted" else 3, "read"])
            else:
                rows = json.loads(run(work, f"read-{name}", old_python, code["read-key"], [database]).splitlines()[-1])
                if len(rows) != (11 if state == "compacted" else 3):
                    raise SystemExit(f"{name} read {len(rows)} rows")
                if run(work, f"reopen-{name}", new_python, REOPEN, [database]).strip() != "ok":
                    raise SystemExit(f"{name} reopen by the new version failed")
            cases.append(name)
    return {"previous_version": old_version, "previous_native_sha256": old_native, "cases": len(cases), "names": cases,
            "reopen_code_sha256": hashlib.sha256(REOPEN.encode()).hexdigest()}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--new-python", type=Path, required=True)
    parser.add_argument("--old-python", type=Path, required=True)
    parser.add_argument("--old-version", required=True)
    parser.add_argument("--old-native-sha256", required=True)
    args = parser.parse_args(argv)
    print(json.dumps(probe(args.repo, args.work_dir, args.new_python, args.old_python, args.old_version, args.old_native_sha256)))


if __name__ == "__main__":
    sys.exit(main())
