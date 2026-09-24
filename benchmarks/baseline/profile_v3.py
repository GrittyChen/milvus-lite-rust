"""绕过 gRPC，直接用 3.x 嵌入式引擎 API 对检索与 count(*) 做 cProfile，拆分耗时归属。

用法: python profile_v3.py <db_path>（须是 3.x 下 bench.py build 阶段产生的库）
关注点：FAISS / numpy 计算本身的占比，对比 Python 逐行去重、墓碑判断与行物化的占比。
"""
import cProfile
import io
import pstats
import sys
import time

import numpy as np
from milvus_lite import MilvusLite

db = MilvusLite(sys.argv[1])
col = db.get_collection("bench")
col.load()
print("segments", len(col._segment_cache), "memtable_rows", col._memtable.size())
dim = next(f.dim for f in col._schema.fields if f.name == "vec")
q = [np.random.default_rng(1).random(dim, dtype=np.float32).tolist()]
col.search(q, top_k=10, metric_type="L2")


def run(name, fn, n):
    s = time.perf_counter()
    for _ in range(n):
        fn()
    print(f"\n== {name}: {round((time.perf_counter() - s) / n * 1000, 2)} ms/次")
    pr = cProfile.Profile(); pr.enable()
    for _ in range(n):
        fn()
    pr.disable()
    buf = io.StringIO()
    pstats.Stats(pr, stream=buf).sort_stats("tottime").print_stats(10)
    print(buf.getvalue())


run("search top10", lambda: col.search(q, top_k=10, metric_type="L2", search_params={"ef": 64}), 20)
run("query count(*)", lambda: col.query(expr="cat >= 0", output_fields=["count(*)"]), 2)
