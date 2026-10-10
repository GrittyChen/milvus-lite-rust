"""独立进程 CLI 胶水；线程、信号与错误边界见决策 0012。"""

from __future__ import annotations

import argparse
import ipaddress
import signal
import struct
import sys
import threading

_MAX_USIZE = (1 << (struct.calcsize("P") * 8)) - 1


def _validate_numbers(port: int, max_workers: int) -> None:
    if not 0 <= port <= 65535:
        raise ValueError("port must be between 0 and 65535")
    if not 1 <= max_workers <= _MAX_USIZE:
        raise ValueError("max_workers must be a positive usize")


def _native_host(host: str) -> str:
    # std 的 (host, port) 需要裸 IPv6，grpcio 的字符串地址则使用括号。
    if host.startswith("[") and host.endswith("]"):
        value = host[1:-1]
        ipaddress.IPv6Address(value)
        return value
    return host


def run_server(data_dir, host="0.0.0.0", port=19530, max_workers=10):
    if threading.current_thread() is not threading.main_thread():
        raise RuntimeError("server must run in the main thread")
    _validate_numbers(port, max_workers)
    host = _native_host(host)
    from milvus_lite import __version__
    from milvus_lite._native import NativeServer

    signals = [signal.SIGINT, signal.SIGTERM]
    if hasattr(signal, "SIGBREAK"):
        signals.append(signal.SIGBREAK)
    previous = {}
    stopped = threading.Event()
    server = None
    primary_error = None

    def request_stop(_signum, _frame):
        stopped.set()

    try:
        for item in signals:
            previous[item] = signal.signal(item, request_stop)
        server = NativeServer.start(data_dir, host, port, __version__, max_workers)
        if not stopped.is_set():
            print(f"MilvusLite gRPC server listening on {server.bound_address} (data_dir={data_dir!r})", flush=True)
        while not stopped.wait(0.2):
            pass
    except BaseException as error:
        primary_error = error
        raise
    finally:
        try:
            if server is not None:
                if stopped.is_set():
                    print("\nShutting down...", flush=True)
                # 原生 stop 不运行 Python handler；后续 OS 信号采用默认动作，不重入收尾。
                for item in previous:
                    signal.signal(item, signal.SIG_DFL)
                try:
                    server.stop(5.0)
                except BaseException as error:
                    if primary_error is None:
                        raise
                    print(f"MilvusLite server shutdown also failed: {error}", file=sys.stderr, flush=True)
        finally:
            for item, handler in previous.items():
                signal.signal(item, handler)


def server_func(args):
    return run_server(data_dir=args.data_dir, host=args.host, port=args.port, max_workers=args.max_workers)


def build_parser(prog="milvus-lite"):
    parser = argparse.ArgumentParser(prog=prog)
    commands = parser.add_subparsers(dest="command", description="milvus-lite command line tool.")
    server = commands.add_parser("server", help="start the Milvus Lite gRPC server")
    server.add_argument("--data-dir", required=True, help="Milvus Lite data directory (created if it doesn't exist).")
    server.add_argument("--host", default="0.0.0.0", help="bind host (default: 0.0.0.0)")
    server.add_argument("--port", type=int, default=19530, help="bind port (default: 19530)")
    server.add_argument("--max-workers", type=int, default=10, help="thread pool size (default: 10)")
    server.set_defaults(func=server_func)
    return parser


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    if not hasattr(args, "func"):
        parser.print_help()
        return 0
    try:
        _validate_numbers(args.port, args.max_workers)
    except ValueError as error:
        parser.error(str(error))
    try:
        args.func(args)
    except Exception as error:
        print(f"MilvusLite server failed: {error}", file=sys.stderr, flush=True)
        return 1
    return 0
