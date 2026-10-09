"""在实例所属进程控制维护等待；只在独立的测试 wheel 或真实 3.x 环境运行。"""

import argparse
import json
import sys
import threading


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("implementation", choices=["rust", "reference"])
    parser.add_argument("database")
    args = parser.parse_args()
    if args.implementation == "rust":
        from milvus_lite._native import NativeServer
        from milvus_lite import __version__
        from pymilvus import MilvusClient, DataType
        if not hasattr(NativeServer, "_wait_for_maintenance"):
            raise RuntimeError("isolated fault-injection wheel required")
        server = NativeServer.start(args.database, "127.0.0.1", 0, __version__)
        client = MilvusClient(uri=f"http://127.0.0.1:{server.port}", dedicated=True)
    else:
        from milvus_lite.db import MilvusLite
        from milvus_lite.schema.types import CollectionSchema, FieldSchema, DataType
        db = MilvusLite(args.database)
    entered = threading.Event()
    resume = threading.Event()
    paused_rust = False
    print(json.dumps({"ready": True}), flush=True)
    try:
        for line in sys.stdin:
            command = json.loads(line)
            op = command["operation"]
            if args.implementation == "rust":
                if op == "create":
                    schema = client.create_schema(auto_id=False, enable_dynamic_field=False)
                    schema.add_field("id", DataType.INT64, is_primary=True)
                    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=command.get("dim", 128))
                    client.create_collection("c", schema=schema)
                    result = None
                elif op == "insert":
                    result = client.insert("c", command["rows"])
                    result = {"insert_count": result["insert_count"], "ids": list(result["ids"])}
                elif op == "delete":
                    result = client.delete("c", ids=command["ids"])
                elif op == "flush":
                    client.flush("c")
                    result = None
                elif op == "wait":
                    result = server._wait_for_maintenance("c", 30.0)
                    if result["failed"]:
                        raise RuntimeError(result)
                elif op == "pause":
                    server._pause_compaction("before_prepare")
                    paused_rust = True
                    result = None
                elif op == "arrived":
                    server._wait_compaction_pause(30.0)
                    result = None
                elif op == "resume":
                    server._resume_compaction()
                    paused_rust = False
                    result = None
                elif op == "count":
                    result = client.query("c", filter="", output_fields=["count(*)"])[0]["count(*)"]
                else:
                    raise ValueError(op)
            else:
                if op == "create":
                    collection = db.create_collection("c", CollectionSchema(fields=[FieldSchema("id", DataType.INT64, is_primary=True), FieldSchema("vector", DataType.FLOAT_VECTOR, dim=command.get("dim", 128))]))
                    result = None
                else:
                    collection = db.get_collection("c")
                    if op == "insert":
                        result = collection.insert(command["rows"])
                    elif op == "delete":
                        result = collection.delete(command["ids"])
                    elif op == "flush":
                        collection.flush()
                        result = None
                    elif op == "wait":
                        collection._wait_for_bg(timeout=30)
                        result = None
                    elif op == "pause":
                        original = collection._compaction_mgr._compact_files
                        def paused(*arguments):
                            collection._compaction_mgr._compact_files = original
                            entered.set()
                            if not resume.wait(30):
                                raise TimeoutError("reference compaction pause timed out")
                            return original(*arguments)
                        collection._compaction_mgr._compact_files = paused
                        result = None
                    elif op == "arrived":
                        if not entered.wait(30):
                            raise TimeoutError("reference compaction did not enter pause")
                        result = None
                    elif op == "resume":
                        resume.set()
                        result = None
                    elif op == "count":
                        result = collection.num_entities
                    else:
                        raise ValueError(op)
            print(json.dumps({"result": result}), flush=True)
    finally:
        resume.set()
        if args.implementation == "rust":
            if paused_rust:
                server._resume_compaction()
            client.close()
            server.stop(2.0)
        else:
            db.close()


if __name__ == "__main__":
    main()
