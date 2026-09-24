"""验证 2.5.1 删除慢是缺索引（实现问题）而非架构问题：在 2.5.1 库的副本上对比有无索引时的按 id 删除。

用法: python sqlite_delete.py <2.5.1 的 .db 文件> [集合名，默认 bench]
.db 须是 bench.py 在 2.5.1 下 build 产生的 SQLite 文件。脚本会在同目录建临时副本并在结束时删除，不改动原文件。
SQL 与 2.5.1 的 CollectionData::Delete 相同（src/storage/collection_data.cpp）：DELETE ... WHERE milvus_id IN (...)。
先顺序扫一遍表预热页缓存，再各删 3 批、每批 1000 条。
"""
import os
import shutil
import sqlite3
import sys
import time

src = sys.argv[1]
table = sys.argv[2] if len(sys.argv) > 2 else "bench"
copy = src + ".sqlite_delete_tmp"
shutil.copyfile(src, copy)
try:
    conn = sqlite3.connect(copy)
    cols = [r[1] for r in conn.execute(f"pragma table_info('{table}')")]
    if cols != ["id", "milvus_id", "data"]:
        sys.exit(f"表 {table} 的列为 {cols}，不是 2.5.1 的数据表结构")

    def delete_batch(offset):
        ids = [r[0] for r in conn.execute(f"select milvus_id from '{table}' limit 1000 offset {offset}")]
        placeholders = ",".join("?" * len(ids))
        s = time.perf_counter()
        conn.execute(f"delete from '{table}' where milvus_id in ({placeholders})", ids)
        conn.commit()
        return round((time.perf_counter() - s) * 1000, 1)

    rows, _ = conn.execute(f"select count(*), sum(length(data)) from '{table}'").fetchone()
    print(f"行数 {rows}（已顺序扫描预热页缓存）")
    print("无索引，删除 1000 条 ms:", [delete_batch(o) for o in (5000, 20000, 40000)])
    s = time.perf_counter()
    conn.execute(f"create index milvus_id_ix on '{table}'(milvus_id)")
    conn.commit()
    print("建 milvus_id 索引 s:", round(time.perf_counter() - s, 2))
    print("有索引，删除 1000 条 ms:", [delete_batch(o) for o in (60000, 80000, 100000)])
    conn.close()
finally:
    os.remove(copy)
