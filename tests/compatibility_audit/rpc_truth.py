"""同一参照服务的原始协议与客户端预填观察；保留原始状态和字段。"""

import json
from pathlib import Path


def run(directory):
    import grpc
    from google.protobuf.json_format import MessageToDict
    import pymilvus
    from pymilvus import DataType, MilvusClient
    from pymilvus.grpc_gen import common_pb2, milvus_pb2 as pb, milvus_pb2_grpc, schema_pb2 as sp
    from milvus_lite.adapter.grpc.server import start_server_in_thread

    directory = Path(directory)
    directory.mkdir(exist_ok=False)
    server, db, port = start_server_in_thread(str(directory))
    channel = grpc.insecure_channel(f"127.0.0.1:{port}")
    stub = milvus_pb2_grpc.MilvusServiceStub(channel)
    output = {"pymilvus": pymilvus.__version__, "raw": [], "sdk": [], "upsert": []}

    def call(method, request):
        try:
            return {"response": MessageToDict(method(request, timeout=10), preserving_proto_field_name=True)}
        except grpc.RpcError as error:
            return {"grpc_code": error.code().name, "details": error.details()}

    def schema(name, nullable, default):
        fields = [sp.FieldSchema(name="id", data_type=5, is_primary_key=True),
                  sp.FieldSchema(name="vector", data_type=101, type_params=[common_pb2.KeyValuePair(key="dim", value="2")]),
                  sp.FieldSchema(name="x", data_type=5, nullable=nullable)]
        if default is not None:
            fields[-1].default_value.int_data = default
        return sp.CollectionSchema(name=name, fields=fields)

    try:
        for nullable, default in ((False, None), (True, None), (False, 5), (True, 0)):
            for layout, numbers, mask in (("compact", [7, 0], [False, True, True]),
                                           ("full", [99, 7, 0], [False, True, True]),
                                           ("all_null", [], [False] * 3), ("all_valid", [1, 7, 0], [True] * 3),
                                           ("no_mask", [1, 7, 0], []), ("missing", None, []),
                                           ("bad_mask", [7, 0], [False, True]), ("bad_length", [7], [False, True, True])):
                name = f"raw_{len(output['raw'])}"
                entry = {"nullable": nullable, "default": default, "layout": layout}
                entry["create"] = call(stub.CreateCollection, pb.CreateCollectionRequest(collection_name=name, schema=schema(name, nullable, default).SerializeToString()))
                entry["index"] = call(stub.CreateIndex, pb.CreateIndexRequest(collection_name=name, field_name="vector", extra_params=[
                    common_pb2.KeyValuePair(key="index_type", value="FLAT"), common_pb2.KeyValuePair(key="metric_type", value="L2")]))
                entry["load"] = call(stub.LoadCollection, pb.LoadCollectionRequest(collection_name=name))
                fields = [sp.FieldData(field_name="id", type=5, scalars=sp.ScalarField(long_data=sp.LongArray(data=[1, 2, 3]))),
                          sp.FieldData(field_name="vector", type=101, vectors=sp.VectorField(dim=2, float_vector=sp.FloatArray(data=[1, 0] * 3)))]
                if numbers is not None:
                    fields.append(sp.FieldData(field_name="x", type=5, valid_data=mask, scalars=sp.ScalarField(long_data=sp.LongArray(data=numbers))))
                request = pb.InsertRequest(collection_name=name, num_rows=3, fields_data=fields)
                entry["request"] = MessageToDict(request, preserving_proto_field_name=True)
                entry["insert"] = call(stub.Insert, request)
                entry["query"] = call(stub.Query, pb.QueryRequest(collection_name=name, expr="id >= 0", output_fields=["id", "x"]))
                output["raw"].append(entry)
                call(stub.DropCollection, pb.DropCollectionRequest(collection_name=name))

        class Capture:
            def __init__(self, method, requests):
                self.method, self.requests = method, requests

            def record(self, request):
                self.requests.append(MessageToDict(request, preserving_proto_field_name=True))

            def __call__(self, request, *args, **kwargs):
                self.record(request)
                return self.method(request, *args, **kwargs)

            def future(self, request, *args, **kwargs):
                self.record(request)
                return self.method.future(request, *args, **kwargs)

        for nullable, default in ((True, None), (False, 5), (True, 0)):
            for input_name, row in (("missing", {}), ("null", {"x": None}), ("zero", {"x": 0})):
                client = MilvusClient(f"http://127.0.0.1:{port}", dedicated=True)
                name = f"sdk_{len(output['sdk'])}"
                entry = {"nullable": nullable, "default": default, "input": input_name, "requests": []}
                try:
                    s = client.create_schema(auto_id=False, enable_dynamic_field=False)
                    s.add_field("id", DataType.INT64, is_primary=True)
                    s.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
                    kwargs = {"nullable": nullable}
                    if default is not None:
                        kwargs["default_value"] = default
                    s.add_field("x", DataType.INT64, **kwargs)
                    indexes = client.prepare_index_params()
                    indexes.add_index("vector", index_type="FLAT", metric_type="L2")
                    client.create_collection(name, schema=s, index_params=indexes)
                    handler = client._get_connection()
                    original = handler._stub.Insert
                    handler._stub.Insert = Capture(original, entry["requests"])
                    try:
                        entry["insert"] = {"ok": True, "value": client.insert(name, [{"id": 1, "vector": [1.0, 0.0], **row}])}
                    except Exception as error:
                        entry["insert"] = {"ok": False, "type": type(error).__name__, "error": str(error)}
                    finally:
                        handler._stub.Insert = original
                    entry["query"] = client.query(name, filter="id >= 0", output_fields=["id", "x"])
                except Exception as error:
                    entry["error"] = {"type": type(error).__name__, "error": str(error)}
                finally:
                    try:
                        client.drop_collection(name)
                    finally:
                        client.close()
                output["sdk"].append(entry)
        for nullable, default in ((True, None), (False, 5), (True, 0)):
            for partial in (False, True):
                for input_name, row in (("missing", {}), ("null", {"x": None}), ("zero", {"x": 0})):
                    client = MilvusClient(f"http://127.0.0.1:{port}", dedicated=True)
                    name = f"upsert_{len(output['upsert'])}"
                    entry = {"nullable": nullable, "default": default, "partial": partial, "input": input_name, "requests": []}
                    try:
                        s = client.create_schema(auto_id=False, enable_dynamic_field=False)
                        s.add_field("id", DataType.INT64, is_primary=True)
                        s.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
                        kwargs = {"nullable": nullable}
                        if default is not None:
                            kwargs["default_value"] = default
                        s.add_field("x", DataType.INT64, **kwargs)
                        indexes = client.prepare_index_params()
                        indexes.add_index("vector", index_type="FLAT", metric_type="L2")
                        client.create_collection(name, schema=s, index_params=indexes)
                        client.insert(name, [{"id": 1, "vector": [1.0, 0.0], "x": 7}])
                        handler = client._get_connection()
                        original = handler._stub.Upsert
                        handler._stub.Upsert = Capture(original, entry["requests"])
                        try:
                            entry["upsert"] = {"ok": True, "value": client.upsert(name, [{"id": 1, "vector": [1.0, 0.0], **row}], partial_update=partial)}
                        except Exception as error:
                            entry["upsert"] = {"ok": False, "type": type(error).__name__, "error": str(error)}
                        finally:
                            handler._stub.Upsert = original
                        entry["query"] = client.query(name, filter="id >= 0", output_fields=["id", "x"])
                    finally:
                        try:
                            client.drop_collection(name)
                        finally:
                            client.close()
                    output["upsert"].append(entry)
    finally:
        channel.close()
        server.stop(grace=0).wait(timeout=10)
        db.close()
    # 两条 SDK 的 insert 返回值可能仍含 protobuf repeated 容器。
    return json.loads(json.dumps(output, default=list))
