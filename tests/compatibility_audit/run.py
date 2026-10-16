"""离线运行固定兼容样本；源代码、环境和结果保存在仓库外。"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import time

from audit_plugin import MODES
from results import summarize

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
REF = "43d1257774e629bc9f66873977ab7c320d5bf5a7"
UPSTREAM = "dcda428d54d11e6084156e2342c513f08c9927bf"


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def manifest(name):
    path = HERE / "manifests" / name
    expected = json.loads((HERE / "manifests/identity.json").read_text())[name]
    if digest(path) != expected:
        raise ValueError(f"清单哈希不符：{name}")
    lines = path.read_text().splitlines()
    if not lines or len(lines) != len(set(lines)) or any(not line or line.startswith("/") or ".." in line.split("/") for line in lines):
        raise ValueError(f"清单无效：{name}")
    return lines


def outside(path):
    path = Path(path).resolve()
    if path == REPO or REPO in path.parents:
        raise ValueError("工作目录必须位于仓库外")
    return path


def archive(source, commit, target):
    actual = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
    if actual != commit:
        raise ValueError(f"源版本不符：{actual} != {commit}")
    output = target.with_suffix(".tar")
    with output.open("wb") as out:
        subprocess.run(["git", "-C", str(source), "archive", commit], stdout=out, check=True)
    target.mkdir()
    with tarfile.open(output) as saved:
        saved.extractall(target, filter="data")
    return digest(output)


def node_from_key(identity, suite):
    classname, test = identity.split("::", 1)
    parts = classname.split(".")
    for count in range(len(parts), 0, -1):
        path = "/".join(parts[:count]) + ".py"
        if (suite / path).is_file():
            return "::".join([path, *parts[count:], test])
    raise ValueError(f"找不到上游用例：{identity}")


def prepare(args):
    work = outside(args.work_dir)
    work.mkdir(parents=True, exist_ok=False)
    identity = {"reference": REF, "upstream": UPSTREAM}
    identity["reference_archive_sha256"] = archive(Path(args.reference), REF, work / "reference")
    if args.upstream:
        identity["upstream_archive_sha256"] = archive(Path(args.upstream), UPSTREAM, work / "upstream")
    suite = work / "rust-suite"
    shutil.copytree(work / "reference/tests", suite / "tests")
    fixture = suite / "tests/adapter/conftest.py"
    text = fixture.read_text()
    before = "        _cleanup_all_databases(client)\n"
    if text.count(before) != 1:
        raise ValueError("参照清理 fixture 结构改变")
    fixture.write_text(text.replace(before, "        for name in client.list_collections():\n            client.drop_collection(name)\n"))
    identity["rust_fixture_sha256"] = digest(fixture)
    (work / "source.json").write_text(json.dumps(identity, indent=2) + "\n")
    print(json.dumps(identity, indent=2))


IDENTITY = '''import hashlib,importlib.metadata as m,json,sys
from pathlib import Path
import milvus_lite
value={"python":sys.version,"module":milvus_lite.__file__,"packages":{d.metadata["Name"]:d.version for d in m.distributions()}}
try:
 import milvus_lite._native as n
 value["native_sha256"]=hashlib.sha256(Path(n.__file__).read_bytes()).hexdigest()
except ModuleNotFoundError:
 pass
print(json.dumps(value))'''


def run(args):
    work = outside(args.work_dir)
    if args.suite != "reference" and (args.selection != "all" or args.mode != "original"):
        raise ValueError("schema 子集与反事实模式只用于参照 adapter/compatibility 套件")
    source = json.loads((work / "source.json").read_text())
    if source["reference"] != REF or source["upstream"] != UPSTREAM:
        raise ValueError("工作区来源不符")
    run_dir = work / "runs" / args.name
    if Path(args.name).name != args.name or args.name in (".", ".."):
        raise ValueError("name 必须是单一目录名")
    run_dir.mkdir(parents=True, exist_ok=False)
    tool_source = run_dir / "tool-source"
    tool_source.mkdir()
    for path in HERE.glob("*.py"):
        shutil.copyfile(path, tool_source / path.name)
    shutil.copytree(HERE / "manifests", tool_source / "manifests")
    suite = work / ("reference" if args.backend == "reference" else "rust-suite")
    if args.suite == "upstream":
        suite = work / "upstream/tests/python_client"
        nodes = [node_from_key(identity, suite) for identity in manifest("upstream-identities.txt")]
    elif args.suite == "probe":
        nodes = manifest("probes.txt")
        suite = work
    else:
        nodes = manifest({"all": "reference.txt", "schema": "schema.txt", "partitions": "partitions.txt"}[args.selection])
    env = {k: v for k, v in os.environ.items() if k not in ("PYTHONPATH", "GAP_ANN_AS_FLAT", "PYTEST_ADDOPTS", "PYTEST_PLUGINS")}
    env.update(PYTHONDONTWRITEBYTECODE="1", PYTHONPATH=os.pathsep.join([str(tool_source), str(work / "reference")]) if args.backend == "reference" else str(tool_source),
               MLR_AUDIT_BACKEND=args.backend, MLR_AUDIT_MODE=args.mode, MLR_AUDIT_EVENTS=str(run_dir / "events.jsonl"),
               MLR_AUDIT_SELECTION=str(run_dir / "selection.json"), OMP_NUM_THREADS="1", OPENBLAS_NUM_THREADS="1")
    if args.suite == "upstream":
        env["PYTHONPATH"] += os.pathsep + str(work / "reference/tools/pytest_compat")
        env["CI_LOG_PATH"] = str(run_dir / "ci-logs")
        (run_dir / "ci-logs").mkdir()
    identity = json.loads(subprocess.check_output([args.python, "-c", IDENTITY], cwd=work, env=env, text=True))
    is_rust = "native_sha256" in identity
    if is_rust != (args.backend == "rust"):
        raise ValueError(f"后端与解释器不符：{identity}")
    if args.backend == "reference" and not Path(identity["module"]).is_relative_to(work / "reference"):
        raise ValueError("参照模块没有来自冻结归档")
    (run_dir / "selection.json").write_text(json.dumps(nodes, indent=2) + "\n")
    (run_dir / "events.jsonl").touch()
    cmd = [args.python, "-m", "pytest", *nodes, "-p", "audit_plugin", "-p", "no:cacheprovider", "-p", "no:rerunfailures",
           "--rootdir", str(suite), "--confcutdir", str(suite), "-q", "--timeout", "300" if args.suite == "upstream" else "180", "-o", "addopts=", f"--junitxml={run_dir / 'result.xml'}"]
    if args.suite == "upstream":
        cmd.extend(["--uri", str(run_dir / "suite.db"), "--show-capture=no"])
    if args.suite == "probe":
        code = "import audit_plugin,os,runpy; audit_plugin.install_observation(os.environ['MLR_AUDIT_MODE']); runpy.run_path(" + repr(str(tool_source / "client_probe.py")) + ",run_name='__main__')"
        cmd = [args.python, "-c", code, str(run_dir / "probe-db"), str(run_dir / "probe.json"), *nodes]
    metadata = {"backend": args.backend, "mode": args.mode, "suite": args.suite, "identity": identity, "source": source, "args": cmd,
                "tool_sha256": {str(p.relative_to(tool_source)): digest(p) for p in sorted(tool_source.rglob("*")) if p.is_file()}, "started": time.time()}
    (run_dir / "run.json").write_text(json.dumps(metadata, indent=2) + "\n")
    with (run_dir / "run.log").open("w") as log:
        result = subprocess.run(cmd, cwd=suite, env=env, stdout=log, stderr=subprocess.STDOUT)
    metadata.update(pytest_exit=result.returncode, finished=time.time())
    (run_dir / "run.json").write_text(json.dumps(metadata, indent=2) + "\n")
    if result.returncode not in (0, 1):
        raise RuntimeError(f"套件未完成：pytest exit {result.returncode}，见 {run_dir / 'run.log'}")
    if args.suite == "probe":
        cases = json.loads((run_dir / "probe.json").read_text())
        if set(cases) != set(nodes):
            raise ValueError("调用探针集合改变")
        report = {"backend": args.backend, "mode": args.mode, "formal": args.mode == "original", "count": len(cases),
                  "outcomes": {"success": sum(c["ok"] for c in cases.values()), "failure": sum(not c["ok"] for c in cases.values())},
                  "reasons": {}, "cases": cases}
    else:
        report = summarize(run_dir)
    (run_dir / "summary.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({k: report[k] for k in ("mode", "backend", "formal", "count", "outcomes", "reasons")}, ensure_ascii=False), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prep = commands.add_parser("prepare")
    prep.add_argument("--reference", required=True)
    prep.add_argument("--upstream")
    prep.add_argument("--work-dir", required=True)
    prep.set_defaults(func=prepare)
    execution = commands.add_parser("run")
    execution.add_argument("--work-dir", required=True)
    execution.add_argument("--python", required=True)
    execution.add_argument("--backend", choices=("reference", "rust"), required=True)
    execution.add_argument("--suite", choices=("reference", "upstream", "probe"), default="reference")
    execution.add_argument("--mode", choices=MODES, default="original")
    execution.add_argument("--selection", choices=("all", "schema", "partitions"), default="all")
    execution.add_argument("--name", required=True)
    execution.set_defaults(func=run)
    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
