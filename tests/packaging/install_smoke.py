"""安装产物的导入、CLI 或 SDK 生命周期冒烟；每次使用独立工作目录。"""

import argparse
import hashlib
import importlib.metadata as metadata
import json
import queue
import threading
from pathlib import Path
import signal
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work-dir", required=True, type=Path)
    parser.add_argument("--mode", choices=["minimal", "sdk"], required=True)
    parser.add_argument("--peer-python", type=Path)
    args = parser.parse_args()
    # 存量库不属于探针；成功和失败都不覆盖它。
    args.work_dir.mkdir(parents=True, exist_ok=False)
    import milvus_lite
    from milvus_lite._native import NativeServer
    import milvus_lite._native as native
    if any(hasattr(NativeServer, n) for n in ["_pause_compaction", "_inject_compaction_fault"]):
        raise RuntimeError("formal installed wheel required")

    help_result = subprocess.run([sys.executable, "-m", "milvus_lite", "--help"], capture_output=True, text=True, timeout=15, check=True)
    assert "server" in help_result.stdout
    if args.mode == "minimal":
        for name in ["pymilvus", "numpy", "grpcio"]:
            try:
                metadata.version(name)
            except metadata.PackageNotFoundError:
                continue
            raise RuntimeError("minimal environment contains " + name)
        if args.peer_python is None:
            raise ValueError("minimal CLI probe requires an independent SDK interpreter")
        log = (args.work_dir / "cli.log").open("w+")
        process = subprocess.Popen([sys.executable, "-m", "milvus_lite", "server", "--data-dir", str(args.work_dir / "minimal.db"), "--host", "127.0.0.1", "--port", "0"], stdout=subprocess.PIPE, stderr=log, text=True)
        output = queue.Queue()
        def read_output():
            for line in process.stdout:
                output.put(line)
            output.put(None)
        reader = threading.Thread(target=read_output, daemon=True)
        reader.start()
        try:
            ready = output.get(timeout=20)
            assert ready is not None and "listening on " in ready
            address = ready.split("listening on ", 1)[1].split(" ", 1)[0]
            peer = """import sys
from pymilvus import MilvusClient
client=MilvusClient(uri='http://'+sys.argv[1],dedicated=True)
try:
    assert client.list_collections()==[]
    client.create_collection('c',dimension=2)
    client.insert('c',[{'id':1,'vector':[1.0,0.0]}])
    assert list(client.get('c',[1]))[0]['id']==1
finally:
    client.close()
"""
            subprocess.run([str(args.peer_python), "-c", peer, address], capture_output=True, text=True, timeout=30, check=True)
            process.send_signal(signal.SIGTERM)
            process.wait(timeout=30)
            reader.join(timeout=5)
            assert not reader.is_alive()
            assert process.returncode == 0
            lines = []
            while not output.empty():
                value = output.get_nowait()
                if value is not None:
                    lines.append(value)
            assert "".join(lines) == "\nShutting down...\n"
        finally:
            if process.poll() is None:
                process.kill(); process.wait()
            reader.join(timeout=5)
            assert not reader.is_alive()
            process.stdout.close(); log.close()
    else:
        from pymilvus import MilvusClient
        from milvus_lite.server_manager import server_manager_instance as manager
        path = args.work_dir / "sdk.db"
        client = MilvusClient(str(path), dedicated=True)
        try:
            client.create_collection("c", dimension=2, metric_type="L2")
            client.insert("c", [{"id": 0, "vector": [1.0, 0.0], "category": "tech"}, {"id": 1, "vector": [0.0, 1.0], "category": "games"}])
            assert list(client.query("c", filter="category == 'tech'", output_fields=["id"])) == [{"id": 0}]
            client.flush("c")
            client.upsert("c", [{"id": 0, "vector": [0.0, 1.0], "category": "ai"}])
            client.delete("c", ids=[1])
        finally:
            client.close()
            manager.release_server(str(path))
        client = MilvusClient(str(path), dedicated=True)
        try:
            client.load_collection("c")
            assert list(client.get("c", [0, 1], output_fields=["vector", "category"])) == [{"id": 0, "vector": [0.0, 1.0], "category": "ai"}]
            assert client.search("c", [[0.0, 1.0]], limit=1, search_params={"metric_type": "L2"})[0][0]["id"] == 0
        finally:
            client.close()
            manager.release_server(str(path))
    print(json.dumps({"python": sys.version, "mode": args.mode, "version": milvus_lite.__version__, "native_sha256": hashlib.sha256(Path(native.__file__).read_bytes()).hexdigest(), "passed": True}))


if __name__ == "__main__":
    main()
