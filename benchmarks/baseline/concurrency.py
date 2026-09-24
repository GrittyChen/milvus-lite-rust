"""多客户端进程并发检索吞吐。

用法: python concurrency.py <db_path> <dim> <客户端数列表，如 1,2,4,8> [filter]
db_path 须是 bench.py build 阶段产生的库。服务端独立起进程，客户端各自一个进程，
避免客户端与服务端、客户端之间互相争用 GIL，测到的是服务端本身的并发能力。
"""
import json
import multiprocessing as mp
import os
import signal
import socket
import subprocess
import sys
import time
import warnings

warnings.filterwarnings("ignore")
import numpy as np

DUR = 8.0


# 与 bench.py 相同的检索参数，按服务端登记的索引类型选取
SEARCH_PARAMS = {"HNSW": {"ef": 64}, "IVF_FLAT": {"nprobe": 16}}


def worker(uri, dim, flt, seed, q, sp):
    from pymilvus import MilvusClient
    c = MilvusClient(uri=uri)
    qs = np.random.default_rng(seed).random((64, dim), dtype=np.float32).tolist()
    n, end = 0, time.time() + DUR
    while time.time() < end:
        c.search("bench", [qs[n % 64]], limit=10, filter=flt, search_params=sp)
        n += 1
    q.put(n)


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def main():
    db, dim, ks = sys.argv[1], int(sys.argv[2]), [int(x) for x in sys.argv[3].split(",")]
    flt = sys.argv[4] if len(sys.argv) > 4 else ""
    port = free_port()
    import milvus_lite
    is_v2 = not hasattr(milvus_lite, "MilvusLite")
    srv = None
    try:
        if is_v2:
            # 2.5.1 没有 server 子命令，用其 Server 类以 TCP 地址拉起 C++ 子进程
            from milvus_lite.server import Server
            srv = Server(db, address=f"127.0.0.1:{port}")
            if not (srv.init() and srv.start()):
                raise RuntimeError("2.5.1 Server 启动失败（数据目录是否被其他进程占用？）")
            alive = lambda: srv._p is not None and srv._p.poll() is None
        else:
            srv = subprocess.Popen([os.path.join(os.path.dirname(sys.executable), "milvus-lite"), "server", "--data-dir", db, "--port", str(port)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            alive = lambda: srv.poll() is None
        deadline = time.time() + 120
        while True:
            if not alive():
                raise RuntimeError("服务端进程已退出")
            try:
                socket.create_connection(("127.0.0.1", port), 0.2).close(); break
            except OSError:
                if time.time() > deadline:
                    raise RuntimeError("服务端启动超时")
                time.sleep(0.05)
        uri = f"http://127.0.0.1:{port}"
        from pymilvus import MilvusClient
        admin = MilvusClient(uri=uri)
        admin.load_collection("bench")
        index_type = admin.describe_index("bench", admin.list_indexes("bench")[0])["index_type"]
        sp = {"metric_type": "L2", "params": SEARCH_PARAMS.get(index_type, {})}
        res = {"ver": "2.5" if is_v2 else "3.x", "db": os.path.basename(db), "index_described": index_type, "filter": flt, "duration_s": DUR}
        for k in ks:
            q = mp.Queue()
            ps = [mp.Process(target=worker, args=(uri, dim, flt, i, q, sp)) for i in range(k)]
            for p in ps:
                p.start()
            for p in ps:
                p.join()
            if any(p.exitcode != 0 for p in ps):
                raise RuntimeError(f"{k} 个客户端中有进程异常退出")
            res[f"qps_k{k}"] = round(sum(q.get() for _ in ps) / DUR, 1)
        print(json.dumps(res, ensure_ascii=False))
    finally:
        if srv is None:
            pass
        elif is_v2:
            srv.stop()
        else:
            # 与 bench.py 一致：SIGINT 让 3.x 正常 close，不破坏复用的库
            if srv.poll() is None:
                srv.send_signal(signal.SIGINT)
                try:
                    srv.wait(timeout=300)
                except subprocess.TimeoutExpired:
                    srv.kill(); srv.wait()


if __name__ == "__main__":
    main()
