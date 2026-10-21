"""在 3.x 的 venv 中运行：起 3.x 的 gRPC 服务，把实际端口写到 stdout，stdin 关闭后停止。

用法：<3.x 的 python> server3x.py <data_dir>；输出一行「端口 版本号」。

不用 `milvus-lite server`：它的 `--port 0` 不回报实际端口，gRPC 又设了 SO_REUSEPORT，先挑空闲端口再启动时
端口被占也察觉不到；它还只在 KeyboardInterrupt 时关闭数据库。这里与 CLI 经同一个 `_build_server`
构造服务（servicer、256MB 消息上限、线程数都相同），见 0004「验收」第 1 项。

唯一的差别是关闭 SO_REUSEPORT：Linux 3.10 一类内核在端口 0 自动分配时，会把同一用户下另一个
SO_REUSEPORT 监听者正在用的端口再分给新服务，两个参照服务共用端口后连接被分流到错误的数据目录，
差分就在与错误的对象比较（见 0019「试点发现」）。差分进程本身就同时开着会话级与用例级参照服务，并行运行更会放大。
"""

import sys

import grpc
import milvus_lite
from milvus_lite.adapter.grpc.server import start_server_in_thread

_server = grpc.server


def _server_without_reuseport(*args, options=(), **kwargs):
    return _server(*args, options=[*options, ("grpc.so_reuseport", 0)], **kwargs)


grpc.server = _server_without_reuseport
server, db, port = start_server_in_thread(sys.argv[1], host="127.0.0.1", port=0)
print(port, milvus_lite.__version__, flush=True)
sys.stdin.read()
server.stop(grace=2).wait()
db.close()
