"""独立进程中的真实 fork 探针；子进程不 exec，保留继承的 LOCK 引用。

参照端直接调用 MilvusLite.close，Rust 端调用 NativeServer.stop，均为实际收尾入口。
"""

import argparse
import fcntl
import gc
import json
import os
from pathlib import Path
import select
import signal


def lock_is_free(path):
    with open(path / "LOCK", "r+b") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return False
        fcntl.flock(lock, fcntl.LOCK_UN)
        return True


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("backend", choices=["rust", "reference"])
    parser.add_argument("path", type=Path)
    parser.add_argument("--child-stop", action="store_true")
    args = parser.parse_args()
    assert not args.child_stop or args.backend == "rust"

    if args.backend == "rust":
        from milvus_lite._native import NativeServer

        def start():
            return NativeServer.start(str(args.path), "127.0.0.1", 0, "test")

        def stop(server):
            server.stop(0)
    else:
        import milvus_lite
        from milvus_lite.db import MilvusLite

        assert milvus_lite.__version__ == "3.2.1"

        def start():
            return MilvusLite(str(args.path))

        def stop(server):
            server.close()

    server = start()
    ready_read, ready_write = os.pipe()
    exit_read, exit_write = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(ready_read)
        os.close(exit_write)
        signal.alarm(30)
        try:
            if args.child_stop:
                try:
                    stop(server)
                except RuntimeError as error:
                    assert "not the current process" in str(error), str(error)
                else:
                    raise AssertionError("inherited stop was accepted")
                del server
                gc.collect()
            os.write(ready_write, b"R")
            assert os.read(exit_read, 1) == b""
        except BaseException as error:
            os.write(2, f"child failed: {error!r}\n".encode())
            os._exit(1)
        os._exit(0)

    os.close(ready_write)
    os.close(exit_read)
    try:
        assert select.select([ready_read], [], [], 20)[0], "child did not become ready"
        assert os.read(ready_read, 1) == b"R", "child rejected the probe"
        assert os.waitpid(pid, os.WNOHANG) == (0, 0)
        assert not lock_is_free(args.path)
        stop(server)
        assert lock_is_free(args.path)
        reopened = start()
        stop(reopened)
        assert os.waitpid(pid, os.WNOHANG) == (0, 0)
    finally:
        os.close(exit_write)
        os.close(ready_read)
        _, status = os.waitpid(pid, 0)
    assert os.waitstatus_to_exitcode(status) == 0
    print(json.dumps({
        "held_before_close": True,
        "free_after_close": True,
        "reopen_while_child_alive": True,
        "child_stop_rejected": args.child_stop,
        "child_exit": 0,
    }))


if __name__ == "__main__":
    main()
