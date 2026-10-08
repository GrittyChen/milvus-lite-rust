"""在独立进程中使用进程内服务；客户端数据不进入服务端 RSS。

stdout 的首行是实际 URI；stdin 关闭后正常停止。与 CLI 无关，避免
先挑端口再绑定的竞态。停止错误以非零退出暴露，不使用吞错的 atexit。
"""

import importlib.metadata
import json
import sys

import milvus_lite


def main():
    path = sys.argv[1]
    try:
        importlib.metadata.version("milvus-lite-rust")
    except importlib.metadata.PackageNotFoundError:
        rust = False
    else:
        rust = True
    if rust:
        from milvus_lite._native import NativeServer

        server = NativeServer.start(path, "127.0.0.1", 0, milvus_lite.__version__)
        try:
            print(json.dumps({"uri": f"http://127.0.0.1:{server.port}"}), flush=True)
            sys.stdin.read()
        finally:
            server.stop(2.0)
    elif hasattr(milvus_lite, "MilvusLite"):
        from milvus_lite.adapter.grpc.server import start_server_in_thread

        server, db, port = start_server_in_thread(path, host="127.0.0.1", port=0)
        try:
            print(json.dumps({"uri": f"http://127.0.0.1:{port}"}), flush=True)
            sys.stdin.read()
        finally:
            server.stop(grace=2).wait()
            db.close()
    else:
        from milvus_lite.server import Server

        server = Server(path)
        try:
            if not (server.init() and server.start()):
                raise RuntimeError("2.5.1 服务启动失败")
            print(json.dumps({"uri": server.uds_path}), flush=True)
            sys.stdin.read()
        finally:
            server.stop()


if __name__ == "__main__":
    main()
