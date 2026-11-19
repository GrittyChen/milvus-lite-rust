"""独立进程 CLI 胶水；server 的线程、信号与错误边界见决策 0012，dump 见决策 0037。

dump 改写自 milvus-lite v3.2.2 之后的 1320d26 milvus_lite/cmdline.py（Apache-2.0）。
"""

from __future__ import annotations

import argparse
import copy
import ipaddress
import math
import os
import shutil
import signal
import struct
import sys
import threading
from pathlib import Path
from types import SimpleNamespace

_MAX_USIZE = (1 << (struct.calcsize("P") * 8)) - 1

DEFAULT_CHUNK_SIZE = 512 * 1024 * 1024
DEFAULT_BATCH_SIZE = 1000
# pymilvus 的 query iterator 拒绝超过 16384 的批大小，而且是在 BulkWriter 建好之后才报。
MAX_BATCH_SIZE = 16384
# DescribeCollection 与 3.x 一样不回显 ARRAY 元素的 max_length（0015），BulkWriter 校验时又要读它；
# 补上 Milvus 的上限只放宽校验用的副本，不改变服务端行为（0037 缺陷 B）。
ARRAY_VARCHAR_MAX_LENGTH = 65535


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


def _bounded_int(low, high=None):
    def parse(text):
        try:
            value = int(text)
        except ValueError:
            raise argparse.ArgumentTypeError(f"invalid int value: {text!r}") from None
        if value < low or (high is not None and value > high):
            bound = f"between {low} and {high}" if high is not None else f"at least {low}"
            raise argparse.ArgumentTypeError(f"must be {bound}")
        return value

    return parse


def build_parser(prog="milvus-lite"):
    parser = argparse.ArgumentParser(prog=prog)
    commands = parser.add_subparsers(dest="command", description="milvus-lite command line tool.")
    server = commands.add_parser("server", help="start the Milvus Lite gRPC server")
    server.add_argument("--data-dir", required=True, help="Milvus Lite data directory (created if it doesn't exist).")
    server.add_argument("--host", default="0.0.0.0", help="bind host (default: 0.0.0.0)")
    server.add_argument("--port", type=int, default=19530, help="bind port (default: 19530)")
    server.add_argument("--max-workers", type=int, default=10, help="thread pool size (default: 10)")
    server.set_defaults(func=server_func)

    dump = commands.add_parser("dump", help="dump a Milvus Lite collection to Milvus bulk JSON files")
    source = dump.add_mutually_exclusive_group(required=True)
    source.add_argument("-d", "--db-file", type=str, help="local Milvus Lite .db path")
    source.add_argument("--uri", type=str, help="Milvus server URI, for example http://127.0.0.1:19530")
    dump.add_argument("-c", "--collection", type=str, required=True, help="collection to dump")
    dump.add_argument("-p", "--path", type=str, required=True, help="dump file storage path")
    dump.add_argument(
        "--batch-size",
        type=_bounded_int(1, MAX_BATCH_SIZE),
        default=DEFAULT_BATCH_SIZE,
        help=f"query iterator batch size, 1-{MAX_BATCH_SIZE} (default: {DEFAULT_BATCH_SIZE})",
    )
    dump.add_argument(
        "--chunk-size",
        type=_bounded_int(1),
        default=DEFAULT_CHUNK_SIZE,
        help=f"BulkWriter chunk size in bytes (default: {DEFAULT_CHUNK_SIZE})",
    )
    dump.set_defaults(func=dump_func)
    return parser


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    if not hasattr(args, "func"):
        parser.print_help()
        return 0
    if args.command == "dump":
        return _run_dump(args)
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


# ---- dump（0037） ----


class _DumpInterrupted(BaseException):
    """信号 handler 抛出它来打断阻塞中的 RPC；不是 Exception，不会被途中的 except Exception 吞掉。"""

    def __init__(self, signum):
        super().__init__(signum)
        self.signum = signum


class _NonFiniteValue(Exception):
    def __init__(self, value):
        super().__init__(value)
        self.value = value


def bfloat16_to_float32(byte_data, np):
    """把 pymilvus 返回的 bfloat16 字节转成浮点数。"""
    bfloat16_array = np.frombuffer(byte_data, dtype=np.uint16)
    float32_as_uint32 = bfloat16_array.astype(np.uint32) << 16
    return float32_as_uint32.view(np.float32).tolist()


def binary_to_int_list(packed_bytes, np):
    """把打包的二进制向量字节转成 0/1 整数列表。"""
    byte_array = np.frombuffer(packed_bytes, dtype=np.uint8)
    return np.unpackbits(byte_array).astype(int).tolist()


def dump_func(args):
    return dump_collection(
        args.db_file,
        args.collection,
        args.path,
        uri=args.uri,
        batch_size=args.batch_size,
        chunk_size=args.chunk_size,
    )


def _run_dump(args):
    if threading.current_thread() is not threading.main_thread():
        print("MilvusLite dump failed: dump must run in the main thread", file=sys.stderr, flush=True)
        return 1
    signals = [signal.SIGINT, signal.SIGTERM]
    previous = {}

    def interrupt(signum, _frame):
        # 第一次信号就恢复默认处置：清理途中的第二次信号直接终止进程，不在清理中途再抛异常。
        for item in signals:
            signal.signal(item, signal.SIG_DFL)
        raise _DumpInterrupted(signum)

    interrupted = False
    try:
        # 显式接管：继承 SIG_IGN 时 Python 不安装 KeyboardInterrupt（0012 差异表）。
        for item in signals:
            previous[item] = signal.signal(item, interrupt)
        result = args.func(args)
    except _DumpInterrupted as error:
        interrupted = True
        name = signal.Signals(error.signum).name
        print(f"MilvusLite dump interrupted by {name}{_residue_note(error)}", file=sys.stderr, flush=True)
        return 128 + error.signum
    except Exception as error:
        print(f"MilvusLite dump failed: {_describe(error)}{_residue_note(error)}", file=sys.stderr, flush=True)
        return 1
    finally:
        # 被信号打断后保持 SIG_DFL，覆盖随后 atexit 中进程内服务的收尾。
        if not interrupted:
            for item, handler in previous.items():
                signal.signal(item, handler)
    print(
        f"Dump collection {result['collection']} success: {result['row_count']} rows -> {result['path']}",
        flush=True,
    )
    return 0


def _describe(error):
    if type(error) is RuntimeError:
        return str(error)
    message = getattr(error, "message", None)
    if type(error).__module__.startswith("pymilvus") and isinstance(message, str):
        return message
    return f"{type(error).__name__}: {error}"


def _residue_note(error):
    residue = getattr(error, "dump_residue", None)
    return f" (incomplete output left at {residue})" if residue else ""


def _is_local_db(uri):
    # 与 pymilvus 的判定一致：它先看 .db 后缀，再看协议。
    return uri.endswith(".db")


def dump_collection(db_file, collection_name, path, *, uri=None, batch_size=DEFAULT_BATCH_SIZE, chunk_size=DEFAULT_CHUNK_SIZE):
    """把一个集合导出为本地 Milvus BulkWriter JSON 文件，返回集合名、路径与行数。"""
    if not collection_name:
        raise RuntimeError("collection name is required")
    if not path:
        raise RuntimeError("dump path is required")
    if uri is None:
        if not db_file:
            raise RuntimeError("db_file or uri is required")
        if not Path(db_file).exists():
            raise RuntimeError(f"db_file: {db_file} not exists")
        uri = db_file
    elif _is_local_db(uri) and not Path(uri).exists():
        # 否则 pymilvus 会在这个路径新建数据目录。
        raise RuntimeError(f"uri: {uri} not exists")
    local_db = uri if _is_local_db(uri) else None
    if not 1 <= batch_size <= MAX_BATCH_SIZE:
        raise RuntimeError(f"batch size must be between 1 and {MAX_BATCH_SIZE}")
    if chunk_size < 1:
        raise RuntimeError("chunk size must be at least 1")

    out_path = Path(path)
    parent = out_path.parent
    if not parent.is_dir():
        raise RuntimeError(f"dump path({path})'s parent dir not exists: {parent} not exists")
    if out_path.exists() and not out_path.is_dir():
        raise RuntimeError(f"dump path({path}) exists and is not a directory")

    # 两类依赖都在连接之前检查，缺依赖时不先起进程内服务。
    api = _load_pymilvus_api()
    local_bulk_writer, bulk_file_type = _load_bulk_writer()
    np = api.numpy

    client = None
    writer = None
    try:
        # 构造时 pymilvus 已为本地目录起服务；放进 try，构造失败或被信号打断时同样按序收尾。
        client = api.MilvusClient(uri=uri)
        if not client.has_collection(collection_name):
            raise RuntimeError(f"Collection: {collection_name} not exists")
        schema = api.CollectionSchema.construct_from_dict(client.describe_collection(collection_name))
        client.load_collection(collection_name)
        total_rows = _collection_row_count(client, collection_name)

        data_type = api.DataType
        primary = next((field for field in schema.fields if getattr(field, "is_primary", False)), None)
        auto_pk_name = primary.name if primary is not None and (getattr(primary, "auto_id", False) or getattr(schema, "auto_id", False)) else None
        pk_name = primary.name if primary is not None else None
        drop_fields = [field.name for field in schema.fields if getattr(field, "is_function_output", False)]
        if auto_pk_name:
            drop_fields.append(auto_pk_name)
        bfloat16_fields = _fields_of(schema, getattr(data_type, "BFLOAT16_VECTOR", object()))
        binary_fields = _fields_of(schema, getattr(data_type, "BINARY_VECTOR", object()))
        float16_fields = _fields_of(schema, getattr(data_type, "FLOAT16_VECTOR", object()))

        writer = _checked_writer_class(local_bulk_writer)(
            schema=_writer_schema(schema, data_type),
            local_path=path,
            chunk_size=chunk_size,
            file_type=bulk_file_type.JSON,
        )
        dumped = 0
        iterator = client.query_iterator(collection_name, batch_size=batch_size, output_fields=["*"])
        progress = _make_progress(total_rows, f"Dump collection {collection_name}'s data")
        try:
            while True:
                rows = iterator.next()
                if not rows:
                    break
                for row in rows:
                    key = None if auto_pk_name or pk_name is None else row.get(pk_name)
                    for name in drop_fields:
                        row.pop(name, None)
                    _convert_vectors(row, bfloat16_fields, binary_fields, np)
                    writer.append_row(_plain_row(row, key, dumped + 1, float16_fields, np))
                    dumped += 1
                progress.update(len(rows))
                # 只读记录、不等待写线程：已失败时尽早停止；完整的检查在 commit 之后。
                writer.dump_raise_if_failed()
        finally:
            iterator.close()
            progress.close()
        writer.commit()
        writer.dump_wait()
        writer.dump_raise_if_failed()
        return {"collection": collection_name, "path": path, "row_count": dumped}
    except BaseException as error:
        if writer is None:
            raise
        residue, interrupt = _discard_output(writer, out_path)
        if residue:
            error.dump_residue = residue
        if interrupt is None:
            raise
        # 清理途中收到的信号：清理已完成，按中断报告，原错误作为原因保留。
        if residue:
            interrupt.dump_residue = residue
        raise interrupt from error
    finally:
        if client is not None:
            try:
                client.close()
            except Exception:
                pass
        if local_db is not None:
            # 在报告结果前停掉本进程为该目录起的服务：LOCK 随即释放，停服日志也排在结论行之前。
            from milvus_lite.server_manager import server_manager_instance

            server_manager_instance.release_server(local_db)


def _load_pymilvus_api():
    try:
        import numpy
        from pymilvus import CollectionSchema, DataType, MilvusClient
    except ImportError as error:
        raise RuntimeError("milvus-lite dump requires pymilvus to already be installed.") from error
    return SimpleNamespace(MilvusClient=MilvusClient, CollectionSchema=CollectionSchema, DataType=DataType, numpy=numpy)


def _load_bulk_writer():
    try:
        from pymilvus.bulk_writer import BulkFileType, LocalBulkWriter
    except ModuleNotFoundError as error:
        raise RuntimeError("milvus-lite dump requires pymilvus bulk writer dependencies to already be installed.") from error
    return LocalBulkWriter, BulkFileType


def _collection_row_count(client, collection_name):
    rows = client.query(collection_name, filter="", output_fields=["count(*)"])
    if not rows:
        return 0
    return int(rows[0].get("count(*)", 0))


def _fields_of(schema, dtype):
    return [field.name for field in schema.fields if getattr(field, "dtype", None) == dtype]


def _writer_schema(schema, data_type):
    copied = None
    for index, field in enumerate(schema.fields):
        if getattr(field, "dtype", None) != getattr(data_type, "ARRAY", None):
            continue
        if getattr(field, "element_type", None) != getattr(data_type, "VARCHAR", None):
            continue
        if "max_length" in (getattr(field, "params", None) or {}):
            continue
        if copied is None:
            copied = copy.deepcopy(schema)
        copied.fields[index].params["max_length"] = ARRAY_VARCHAR_MAX_LENGTH
    return schema if copied is None else copied


def _checked_writer_class(base):
    """BulkWriter 在子线程写文件，异常只打印、commit 照常返回；子类记下异常并可等待全部写线程（0037 缺陷 D）。

    等待不用 Thread.join：CPython 中被信号异常打断的 join 会把仍在运行的线程标记为已停止，
    之后再 join 立即返回。改用写线程自己置位的 Event，被打断后可以重新等待。
    """

    class CheckedWriter(base):
        def __init__(self, *args, **kwargs):
            self._dump_cond = threading.Condition()
            self._dump_done = {}
            self._dump_errors = []
            super().__init__(*args, **kwargs)

        def _flush(self, *args, **kwargs):
            # 先登记自己：基类在 finally 里把线程从 _working_thread 删掉之后，异常才传到这里。
            done = threading.Event()
            with self._dump_cond:
                self._dump_done[threading.current_thread()] = done
                self._dump_cond.notify_all()
            try:
                super()._flush(*args, **kwargs)
            except BaseException as error:
                with self._dump_cond:
                    self._dump_errors.append(error)
            finally:
                done.set()

        def dump_wait(self):
            current = threading.current_thread()
            # 线程 start() 之后、执行到 _flush 第一行之前，只出现在 _working_thread 中。
            pending = [t for t in list(getattr(self, "_working_thread", {}).values()) if t is not current]
            with self._dump_cond:
                # 没能启动的线程永远不会登记，以 is_alive 排除；这里从不 join，is_alive 可信。
                while not all(t in self._dump_done or not t.is_alive() for t in pending):
                    self._dump_cond.wait(0.1)
                events = [done for thread, done in self._dump_done.items() if thread is not current]
            for done in events:
                done.wait()

        def dump_raise_if_failed(self):
            with self._dump_cond:
                error = self._dump_errors[0] if self._dump_errors else None
            if error is not None:
                raise RuntimeError(f"failed to write dump files: {_describe(error)}") from error

    return CheckedWriter


def _discard_output(writer, out_path):
    """删除本次创建的 uuid 子目录，返回 (残留路径, 清理途中收到的中断)。

    必须先等写线程全部结束：否则线程在删除后仍会写文件并打印错误。等待中收到信号时
    （handler 已把处置恢复为 SIG_DFL，不会再有第二个）记下它、继续等待，清理完再交给调用方。
    """
    interrupt = None
    while True:
        try:
            writer.dump_wait()
            break
        except _DumpInterrupted as error:
            interrupt = interrupt or error
        except Exception:
            # 等不到写线程结束就不能删除，只报告残留。
            return _residue_path(writer), interrupt
    if not getattr(writer, "data_path", None):
        return None, interrupt
    data_path = Path(str(writer.data_path))
    try:
        if not data_path.exists():
            return None, interrupt
        if data_path.name != getattr(writer, "uuid", None) or not os.path.samefile(data_path.parent, out_path):
            return str(data_path), interrupt
        shutil.rmtree(data_path)
        return None, interrupt
    except OSError:
        return str(data_path), interrupt


def _residue_path(writer):
    data_path = getattr(writer, "data_path", None)
    return str(data_path) if data_path else None


def _convert_vectors(row, bfloat16_fields, binary_fields, np):
    for name in bfloat16_fields:
        if name in row and row[name] is not None:
            row[name] = bfloat16_to_float32(_first_bytes(row[name], np), np)
    for name in binary_fields:
        if name in row and row[name] is not None:
            row[name] = binary_to_int_list(_first_bytes(row[name], np), np)


def _first_bytes(value, np):
    if isinstance(value, bytes):
        return value
    if isinstance(value, bytearray):
        return bytes(value)
    if isinstance(value, (list, tuple)) and value:
        return _first_bytes(value[0], np)
    if isinstance(value, np.ndarray):
        return value.tobytes()
    raise TypeError(f"expected bytes-like vector payload, got {type(value).__name__}")


def _plain_row(row, key, ordinal, float16_fields, np):
    plain = {}
    for name, value in row.items():
        try:
            if name in float16_fields and isinstance(value, (bytes, bytearray)):
                # 只解码用于检查，原值照旧交给 BulkWriter。
                _to_plain(np.frombuffer(bytes(value), dtype=np.float16), np)
            plain[_to_plain(name, np)] = _to_plain(value, np)
        except _NonFiniteValue as error:
            label = f"row #{ordinal}" if key is None else f"row with primary key {key!r}"
            raise RuntimeError(
                f"field {name!r} of {label} contains a non-finite float ({error.value!r}), which JSON cannot represent"
            ) from None
    return plain


def _to_plain(value, np):
    """递归转成 JSON 可表示的 Python 值，并拒绝 NaN 与 ±Inf（0037 缺陷 E）。"""
    if isinstance(value, dict):
        return {_to_plain(key, np): _to_plain(item, np) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_to_plain(item, np) for item in value]
    if isinstance(value, np.ndarray):
        return _to_plain(value.tolist(), np)
    if isinstance(value, np.generic):
        return _to_plain(value.item(), np)
    if isinstance(value, float) and not math.isfinite(value):
        raise _NonFiniteValue(value)
    return value


def _make_progress(total, desc):
    try:
        from tqdm import tqdm
    except ImportError:
        return _NoopProgress()
    return tqdm(total=total, desc=desc)


class _NoopProgress:
    def update(self, _n):
        return None

    def close(self):
        return None
