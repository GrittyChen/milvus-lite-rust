"""Linux 上两实现的 LOCK 双向互斥，以及 flock 与 POSIX record lock 的区别。

用法：python linux_lock_probe.py <Rust venv python> <3.x venv python> <工作目录>
工作目录必须位于容器自己的文件系统，不使用 macOS 的共享挂载。
"""

import fcntl
import json
from pathlib import Path
import selectors
import subprocess
import sys


def worker(engine, path, hold):
    if engine == "rust":
        import milvus_lite
        from milvus_lite._native import NativeServer

        server = NativeServer.start(path, "127.0.0.1", 0, milvus_lite.__version__)
        close = lambda: server.stop(2.0)
    else:
        from milvus_lite import MilvusLite

        db = MilvusLite(path)
        close = db.close
    try:
        print("READY", flush=True)
        if hold:
            sys.stdin.read()
    finally:
        close()


def main():
    if sys.platform != "linux":
        raise RuntimeError("此项验证必须在 Linux 上执行")
    rust_python, ref_python, work = sys.argv[1:]
    root = Path(work)
    root.mkdir(parents=True, exist_ok=True)
    evidence = []
    for engine, python, other, peer in (("rust", rust_python, "reference", ref_python),
                                         ("reference", ref_python, "rust", rust_python)):
        path = root / f"{engine}.db"
        with (root / f"{engine}.stderr").open("w") as log:
            proc = subprocess.Popen([python, __file__, "--worker", engine, str(path), "hold"],
                                    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True)
            try:
                with selectors.DefaultSelector() as selector:
                    selector.register(proc.stdout, selectors.EVENT_READ)
                    if not selector.select(30) or proc.stdout.readline().strip() != "READY":
                        raise RuntimeError(f"{engine} 持锁者未启动，见 {log.name}")
                got = subprocess.run([peer, __file__, "--worker", other, str(path), "probe"],
                                     capture_output=True, text=True, timeout=30)
                if got.returncode == 0 or "lock" not in got.stderr.lower():
                    raise RuntimeError(f"{other} 未明确拒绝 LOCK: {got.stdout}\n{got.stderr}")
                with (path / "LOCK").open("r+") as file:
                    try:
                        fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except BlockingIOError:
                        pass
                    else:
                        fcntl.flock(file, fcntl.LOCK_UN)
                        raise RuntimeError("flock 未与持锁者互斥")
                    fcntl.lockf(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    fcntl.lockf(file, fcntl.LOCK_UN)
                proc.stdin.close()
                if proc.wait(timeout=30) != 0:
                    raise RuntimeError(f"{engine} 正常停止失败")
                got = subprocess.run([peer, __file__, "--worker", other, str(path), "probe"],
                                     capture_output=True, text=True, timeout=30)
                if got.returncode != 0 or got.stdout.strip() != "READY":
                    raise RuntimeError(f"释放后仍不能打开: {got.stderr}")
                evidence.append({"holder": engine, "peer": other, "peer_blocked": True,
                                 "flock_blocked": True, "lockf_independent": True, "open_after_release": True})
            finally:
                if proc.poll() is None:
                    proc.kill()
                    proc.wait()
    print(json.dumps(evidence))


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--worker":
        worker(sys.argv[2], sys.argv[3], sys.argv[4] == "hold")
    else:
        main()
