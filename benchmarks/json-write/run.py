"""JSON 写入回归专项：预序列化 raw gRPC 与 pymilvus 端到端分别计时。"""

import argparse
import hashlib
import importlib.metadata as metadata
import json
import os
from pathlib import Path
import platform
import statistics
import sys
import time


def payload(row):
    return {"record": row % 97, "body": "x" * 2048,
            "scores": [i / 13 for i in range(16)],
            "tags": [f"tag-{i:02}" for i in range(24)],
            "meta": {"active": True, "title": "JSON write", "empty": None}}


def check_insert(response, rows):
    status = response.status
    if status.code or status.error_code or status.reason:
        raise RuntimeError(f"Insert 失败: {status}")
    if (response.insert_cnt != rows or list(response.IDs.int_id.data) != list(range(rows))
            or list(response.succ_index) != list(range(rows))):
        raise RuntimeError("Insert 数量、主键或成功位次错误")


def check_rows(actual, expected):
    def canonical(value):
        return json.dumps(value, ensure_ascii=False, sort_keys=True, allow_nan=False)

    if (len(actual) != len(expected)
            or {r["id"]: canonical(r["j"]) for r in actual}
            != {pk: canonical(value) for pk, value in expected.items()}):
        raise RuntimeError("写入行集或 JSON 内容错误")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--database", type=Path, required=True)
    parser.add_argument("--rows", type=int, default=5000)
    parser.add_argument("--samples", type=int, default=5)
    parser.add_argument("--mode", choices=("raw", "sdk"), required=True)
    parser.add_argument("--round", type=int, required=True)
    parser.add_argument("--build", required=True)
    args = parser.parse_args()
    if args.database.exists() or not args.database.parent.is_dir():
        parser.error("database 必须不存在，且父目录已存在；不得覆盖已有目录")
    if not 1 <= args.rows <= 10000 or args.samples < 3:
        parser.error("rows 必须在 1..10000，samples 至少为 3")
    repo = Path(__file__).resolve().parents[2]
    if args.database.resolve().is_relative_to(repo):
        parser.error("database 必须位于仓库外")

    import grpc
    from pymilvus import DataType, MilvusClient
    from pymilvus.grpc_gen import milvus_pb2, schema_pb2
    from milvus_lite.server_manager import ServerManager

    # JSON 编码、protobuf 序列化及 SDK 的输入对象构造均在计时外。
    objects = [payload(i) for i in range(args.rows)]
    encoded = [json.dumps(value, ensure_ascii=False).encode() for value in objects]
    records = [{"id": i, "v": [1.0, 0.0], "j": json.loads(value)}
               for i, value in enumerate(encoded)]
    prepared = []
    for sample in range(args.samples + 1):
        name = f"sample_{sample}"
        pk = schema_pb2.FieldData(field_name="id", type=DataType.INT64)
        pk.scalars.long_data.data.extend(range(args.rows))
        vector = schema_pb2.FieldData(field_name="v", type=DataType.FLOAT_VECTOR)
        vector.vectors.dim = 2
        vector.vectors.float_vector.data.extend([1.0, 0.0] * args.rows)
        column = schema_pb2.FieldData(field_name="j", type=DataType.JSON)
        column.scalars.json_data.data.extend(encoded)
        request = milvus_pb2.InsertRequest(collection_name=name, num_rows=args.rows,
                                          fields_data=[pk, vector, column]).SerializeToString()
        prepared.append((name, request))

    manager = ServerManager()
    channel = client = None
    elapsed = []
    try:
        uri = manager.start_and_get_uri(str(args.database))
        if uri is None:
            raise RuntimeError("启动服务失败")
        client = MilvusClient(uri, dedicated=True)
        channel = grpc.insecure_channel(uri.removeprefix("http://"), options=[
            ("grpc.max_send_message_length", -1), ("grpc.max_receive_message_length", -1)])
        raw_insert = channel.unary_unary(
            "/milvus.proto.milvus.MilvusService/Insert",
            request_serializer=None, response_deserializer=milvus_pb2.MutationResult.FromString)
        for sample, (name, request) in enumerate(prepared):
            schema = client.create_schema(auto_id=False, enable_dynamic_field=False)
            schema.add_field("id", DataType.INT64, is_primary=True)
            schema.add_field("v", DataType.FLOAT_VECTOR, dim=2)
            schema.add_field("j", DataType.JSON)
            indexes = client.prepare_index_params()
            indexes.add_index("v", index_type="FLAT", metric_type="L2")
            client.create_collection(name, schema=schema, index_params=indexes)
            started = time.perf_counter()
            response = (raw_insert(request) if args.mode == "raw"
                        else client.insert(name, records))
            duration = (time.perf_counter() - started) * 1000
            if args.mode == "raw":
                check_insert(response, args.rows)
            elif response.get("insert_count") != args.rows or response.get("ids") != list(range(args.rows)):
                raise RuntimeError(f"SDK Insert 数量或主键错误: {response}")
            if client.query(name, output_fields=["count(*)"]) != [{"count(*)": args.rows}]:
                raise RuntimeError("写入后行数错误")
            for start in range(0, args.rows, 500):
                ids = list(range(start, min(start + 500, args.rows)))
                check_rows(client.get(name, ids, output_fields=["j"]), {i: objects[i] for i in ids})
            # 在计时外保持每批的生命周期相同，不让后续采样积累数据或等待前批维护。
            client.flush(name)
            client.drop_collection(name)
            if sample:
                elapsed.append(duration)
    finally:
        try:
            if client is not None:
                client.close()
            if channel is not None:
                channel.close()
        finally:
            manager.release_all()

    try:
        version = metadata.version("milvus-lite-rust")
    except metadata.PackageNotFoundError:
        version = metadata.version("milvus-lite")
    native = None
    try:
        from milvus_lite import _native
    except ImportError:
        pass
    else:
        native = hashlib.sha256(Path(_native.__file__).read_bytes()).hexdigest()
    print(json.dumps({"build": args.build, "round": args.round, "mode": args.mode,
                      "rows": args.rows, "samples": args.samples, "elapsed_ms": elapsed,
                      "min_ms": min(elapsed), "median_ms": statistics.median(elapsed),
                      "json_bytes_per_row": [min(map(len, encoded)), max(map(len, encoded))],
                      "payload_sha256": hashlib.sha256(b"".join(encoded)).hexdigest(),
                      "raw_request_sha256": [hashlib.sha256(request).hexdigest() for _, request in prepared],
                      "benchmark_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                      "native_sha256": native, "implementation_version": version,
                      "python": sys.version.split()[0], "pymilvus": metadata.version("pymilvus"),
                      "machine": platform.platform(), "cpu_count": os.cpu_count(),
                      "load_average": os.getloadavg()}))


if __name__ == "__main__":
    main()
