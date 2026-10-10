"""仅 FI 镜像使用；PID 1 中等待真实维护边界，不修改产品信号处理。"""

import os
from pathlib import Path
import signal
import threading
import time

from milvus_lite import _native, cmdline

assert os.getpid() == 1
Original = _native.NativeServer
assert hasattr(Original, "_pause_compaction"), "isolated FI wheel required"


class Wrapper:
    def __init__(self, native):
        self.native = native

    @property
    def bound_address(self):
        return self.native.bound_address

    def stop(self, grace):
        assert signal.getsignal(signal.SIGTERM) == signal.SIG_DFL
        assert signal.getsignal(signal.SIGINT) == signal.SIG_DFL
        print("DEFAULT", flush=True)
        self.native.stop(grace)
        print("FINISHED", flush=True)


class Factory:
    @staticmethod
    def start(*args):
        native = Original.start(*args)
        native._pause_compaction("before_commit")

        def control():
            native._wait_compaction_pause(60)
            print("PAUSED", flush=True)
            deadline = time.monotonic() + 90
            while not Path("/control/resume").exists():
                if time.monotonic() > deadline:
                    # 不把控制端失联当作正常放行。
                    print("CONTROL_TIMEOUT", flush=True)
                    return
                threading.Event().wait(0.05)
            native._resume_compaction()
            print("RESUMED", flush=True)

        threading.Thread(target=control, daemon=True).start()
        return Wrapper(native)


_native.NativeServer = Factory
raise SystemExit(cmdline.main(["server", "--data-dir", "/data/service.db", "--host", "0.0.0.0", "--port", "19530", "--max-workers", "1"]))
