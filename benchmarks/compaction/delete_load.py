"""删除/重插/再删除的受控负载；对照已提交 Parquet 的失效行比例。"""

import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import selectors
import subprocess
import time

import numpy as np


class Engine:
    def __init__(self, python, implementation, database, log):
        self.log = log.open("w")
        runner = Path(__file__).resolve().parents[2] / "tests/compaction/engine_runner.py"
        self.process = subprocess.Popen([python, "-u", str(runner), implementation, str(database)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log, text=True)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self._read()

    def _read(self):
        if not self.selector.select(45):
            raise TimeoutError("engine command timed out")
        line = self.process.stdout.readline()
        if not line:
            raise RuntimeError(f"engine exited: {self.process.poll()}")
        return json.loads(line)

    def command(self, operation, **values):
        self.process.stdin.write(json.dumps({"operation": operation, **values})+"\n")
        self.process.stdin.flush()
        return self._read().get("result")

    def close(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=45)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
            raise
        finally:
            self.selector.close()
            self.log.close()
        if self.process.returncode:
            raise RuntimeError(f"engine exited {self.process.returncode}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--rust-python", required=True)
    parser.add_argument("--reference-python", required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--cycles", type=int, default=5)
    args = parser.parse_args()
    if args.cycles < 1:
        raise ValueError("positive cycles required")
    args.work_dir.mkdir(parents=True, exist_ok=True)
    spec = importlib.util.spec_from_file_location("compaction_run", Path(__file__).with_name("run.py"))
    base = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(base)
    rng = np.random.default_rng(42)
    vectors = rng.random((4000, 128), dtype=np.float32)
    rows = [{"id": i, "vector": vector.tolist()} for i, vector in enumerate(vectors)]
    removed = list(range(800))
    output = {}
    for implementation, python in (("rust", args.rust_python), ("reference", args.reference_python)):
        database = args.work_dir / f"{implementation}.db"
        if database.exists():
            raise ValueError("load database already exists")
        engine = Engine(python, implementation, database, args.work_dir / f"{implementation}.log")
        snapshots = []
        try:
            engine.command("create", dim=128)
            engine.command("wait")
            engine.command("insert", rows=rows[:1000])
            engine.command("flush")
            engine.command("wait")
            engine.command("delete", ids=removed)
            engine.command("flush")
            engine.command("wait")
            for start in (1000, 2000):
                engine.command("insert", rows=rows[start:start+1000])
                engine.command("flush")
                engine.command("wait")
            engine.command("pause")
            engine.command("insert", rows=rows[3000:])
            engine.command("flush")
            engine.command("arrived")
            engine.command("insert", rows=rows[:800])
            engine.command("delete", ids=removed)
            engine.command("resume")
            engine.command("wait")
            for cycle in range(args.cycles):
                if cycle:
                    engine.command("insert", rows=rows[:800])
                    engine.command("delete", ids=removed)
                engine.command("flush")
                engine.command("wait")
                count = engine.command("count")
                if count != 3200:
                    raise ValueError(f"logical row count changed: {count}")
                snapshot = base.physical(database)
                if snapshot["live_rows"] != count:
                    raise ValueError("physical model disagrees with engine")
                snapshot["cycle"] = cycle+1
                snapshots.append(snapshot)
        finally:
            engine.close()
        output[implementation] = snapshots
    output["script_sha256"] = hashlib.sha256(Path(__file__).read_bytes()+Path(__file__).with_name("run.py").read_bytes()).hexdigest()
    output["scope"] = "受控单 collection、4 千行、dim128、首轮准备时以活跃删除遮住已提交删除；仅计 manifest 引用的 Parquet"
    print(json.dumps(output, ensure_ascii=False))


if __name__ == "__main__":
    main()
