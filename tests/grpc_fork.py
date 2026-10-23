"""测试与验收驱动进程在首次导入 grpc 之前调用，只对本进程关闭 grpcio 核心的 fork 处理。

grpcio 核心默认注册 pthread_atfork。驱动进程导入 pymilvus 后，每次 subprocess 派生子进程，子进程在 exec 之前
都会恢复 gRPC 的线程并轮询父进程的描述符，偶发致命断言使子进程在 exec 前无声退出，父进程读到空输出。驱动进程
只为 exec 而 fork，不需要 fork 处理。该变量只在 grpc_init 时读取一次，因此先设置、导入 grpc 完成初始化、再恢复
原值：exec 出的子进程（包括专门验证 fork 的探针）仍按默认配置初始化自己的 gRPC。
"""

import importlib.util
import os
import sys

VARIABLE = "GRPC_ENABLE_FORK_SUPPORT"
_state = None


def disable_fork_handlers(required=True):
    """返回本进程是否已关闭 fork 处理；grpcio 未安装且 required 为假时返回 False，为真时报错。可重复调用。"""
    global _state
    if _state is not None:
        return _state
    if "grpc" in sys.modules:
        raise RuntimeError(f"grpc was imported before {__name__}.disable_fork_handlers(); "
                           f"{VARIABLE} is read once at grpc_init and can no longer change this process")
    # 只按顶层包是否存在区分“未安装”：包内模块缺失或安装损坏时导入原样失败，不当作未安装。
    if importlib.util.find_spec("grpc") is None:
        if required:
            raise ModuleNotFoundError("grpcio is not installed; this driver imports pymilvus and needs it")
        _state = False
        return _state
    previous = os.environ.get(VARIABLE)
    os.environ[VARIABLE] = "0"
    try:
        import grpc  # noqa: F401
    finally:
        if previous is None:
            del os.environ[VARIABLE]
        else:
            os.environ[VARIABLE] = previous
    _state = True
    return _state
