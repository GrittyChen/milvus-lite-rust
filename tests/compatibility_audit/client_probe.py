"""常见 MilvusClient 写法探针：每项独立建库，记录成功与否及错误首行。

用法：<python> probe.py <work_dir> <out.json>
"""
import json
import os
import random
import shutil
import sys
import traceback

from pymilvus import AnnSearchRequest, DataType, Function, FunctionType, MilvusClient, RRFRanker

DIM = 8
random.seed(7)


def vec():
    return [random.random() for _ in range(DIM)]


def rows(n=50):
    cats = ["tech", "ai", "art", "food"]
    return [
        dict(id=i, vector=vec(), category=cats[i % 4], score=i * 1.5, tags=["a", "b"][: i % 3],
             meta={"k": i % 5, "name": f"n{i}", "nested": {"x": i}}, title=f"title {i} apple" if i % 2 else f"doc {i}")
        for i in range(n)
    ]


def quick(c, n=50):
    c.create_collection("c", dimension=DIM)
    c.insert("c", rows(n))


def schema_coll(c, index_type="HNSW", params=None, metric="COSINE", extra=None, array=False, json_field=False):
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=True)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("vector", DataType.FLOAT_VECTOR, dim=DIM)
    s.add_field("category", DataType.VARCHAR, max_length=64)
    s.add_field("score", DataType.DOUBLE)
    if array:
        s.add_field("tags", DataType.ARRAY, element_type=DataType.VARCHAR, max_capacity=8, max_length=16)
    if json_field:
        s.add_field("meta", DataType.JSON)
    if extra:
        extra(s)
    ip = c.prepare_index_params()
    ip.add_index("vector", index_type=index_type, metric_type=metric, params=params or {})
    c.create_collection("c", schema=s, index_params=ip)
    return s


SCEN = {}


def scen(group):
    def deco(f):
        SCEN[f.__name__] = (group, f)
        return f
    return deco


# --- 基础（预期已支持，作为对照） ---
@scen("基础")
def quickstart_flow(c):
    quick(c)
    c.search("c", [vec()], limit=3, output_fields=["category"])
    c.query("c", filter="id in [1,2]", output_fields=["*"])
    c.delete("c", ids=[1])
    c.get("c", ids=[2])


@scen("基础")
def filter_eq_and(c):
    quick(c)
    c.query("c", filter="category == 'tech' and id > 3", output_fields=["id"])


@scen("基础")
def delete_by_filter(c):
    quick(c)
    c.delete("c", filter="id < 5")


@scen("基础")
def get_collection_stats(c):
    quick(c)
    c.get_collection_stats("c")


@scen("基础")
def search_with_offset(c):
    quick(c)
    c.search("c", [vec()], limit=3, offset=2)


# --- 过滤表达式 ---
@scen("过滤")
def filter_or(c):
    quick(c)
    c.query("c", filter="category == 'tech' or category == 'ai'", output_fields=["id"])


@scen("过滤")
def filter_not(c):
    quick(c)
    c.query("c", filter="not (id < 10)", output_fields=["id"])


@scen("过滤")
def filter_not_in(c):
    quick(c)
    c.query("c", filter="id not in [1,2,3]", output_fields=["id"])


@scen("过滤")
def filter_like(c):
    quick(c)
    c.query("c", filter="category like 'te%'", output_fields=["id"])


@scen("过滤")
def filter_range_chain(c):
    quick(c)
    c.query("c", filter="2 < id < 10", output_fields=["id"])


@scen("过滤")
def filter_arith(c):
    quick(c)
    c.query("c", filter="id % 2 == 0", output_fields=["id"])


@scen("过滤")
def filter_json_path(c):
    quick(c)
    c.query("c", filter="meta['k'] == 1", output_fields=["id"])


@scen("过滤")
def filter_json_nested(c):
    quick(c)
    c.query("c", filter="meta['nested']['x'] > 3", output_fields=["id"])


@scen("过滤")
def filter_json_contains(c):
    quick(c)
    c.query("c", filter="json_contains(tags, 'a')", output_fields=["id"])


@scen("过滤")
def filter_array_contains(c):
    schema_coll(c, "FLAT", metric="L2", array=True)
    c.insert("c", rows())
    c.query("c", filter="array_contains(tags, 'a')", output_fields=["id"])


@scen("过滤")
def filter_array_length(c):
    schema_coll(c, "FLAT", metric="L2", array=True)
    c.insert("c", rows())
    c.query("c", filter="array_length(tags) == 1", output_fields=["id"])


@scen("过滤")
def filter_array_index(c):
    schema_coll(c, "FLAT", metric="L2", array=True)
    c.insert("c", rows())
    c.query("c", filter="tags[0] == 'a'", output_fields=["id"])


@scen("过滤")
def filter_is_null(c):
    quick(c)
    c.query("c", filter="category is null", output_fields=["id"])


@scen("过滤")
def filter_template(c):
    quick(c)
    c.query("c", filter="id in {ids}", filter_params={"ids": [1, 2]}, output_fields=["id"])


@scen("过滤")
def filter_search_with_or(c):
    quick(c)
    c.search("c", [vec()], limit=3, filter="category == 'tech' or id < 3")


# --- 索引 ---
@scen("索引")
def create_coll_with_hnsw(c):
    schema_coll(c, "HNSW", {"M": 16, "efConstruction": 200})
    c.insert("c", rows())
    c.search("c", [vec()], limit=3, search_params={"params": {"ef": 64}})


@scen("索引")
def create_coll_with_ivf_flat(c):
    schema_coll(c, "IVF_FLAT", {"nlist": 16}, metric="L2")
    c.insert("c", rows())
    c.search("c", [vec()], limit=3, search_params={"params": {"nprobe": 4}})


@scen("索引")
def create_coll_with_autoindex(c):
    schema_coll(c, "AUTOINDEX")
    c.insert("c", rows())
    c.search("c", [vec()], limit=3)


@scen("索引")
def create_index_after_insert_hnsw(c):
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=True)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("vector", DataType.FLOAT_VECTOR, dim=DIM)
    c.create_collection("c", schema=s)
    c.insert("c", [dict(id=i, vector=vec()) for i in range(20)])
    ip = c.prepare_index_params()
    ip.add_index("vector", index_type="HNSW", metric_type="L2", params={"M": 8, "efConstruction": 64})
    c.create_index("c", ip)
    c.load_collection("c")
    c.search("c", [vec()], limit=3)


@scen("索引")
def scalar_inverted_index(c):
    schema_coll(c, "FLAT", metric="L2")
    c.release_collection("c")
    ip = c.prepare_index_params()
    ip.add_index("category", index_type="INVERTED")
    c.create_index("c", ip)


@scen("索引")
def list_describe_indexes(c):
    quick(c)
    names = c.list_indexes("c")
    c.describe_index("c", names[0])


@scen("索引")
def drop_index(c):
    quick(c)
    c.release_collection("c")
    c.drop_index("c", c.list_indexes("c")[0])


# --- 集合管理 ---
@scen("集合管理")
def has_list_describe(c):
    quick(c)
    c.has_collection("c")
    c.list_collections()
    c.describe_collection("c")


@scen("集合管理")
def rename_collection(c):
    quick(c)
    c.rename_collection("c", "d")


@scen("集合管理")
def alias_ops(c):
    quick(c)
    c.create_alias("c", "al")
    c.query("al", filter="id in [1]")
    c.list_aliases("c")
    c.drop_alias("al")


@scen("集合管理")
def partitions(c):
    quick(c)
    c.create_partition("c", "p1")
    c.has_partition("c", "p1")
    c.list_partitions("c")
    c.insert("c", [dict(id=1000, vector=vec(), category="x")], partition_name="p1")
    c.search("c", [vec()], limit=3, partition_names=["p1"])
    c.query("c", filter="id in [1000]", partition_names=["p1"])


@scen("集合管理")
def partition_key(c):
    def extra(s):
        s.add_field("tenant", DataType.VARCHAR, max_length=32, is_partition_key=True)
    schema_coll(c, "FLAT", metric="L2", extra=extra)
    c.insert("c", [dict(r, tenant=f"t{r['id'] % 3}") for r in rows()])
    c.search("c", [vec()], limit=3, filter="tenant == 't1'")


@scen("集合管理")
def databases(c):
    c.create_database("db1")
    c.list_databases()
    c.using_database("db1")
    quick(c)


@scen("集合管理")
def collection_properties(c):
    quick(c)
    c.alter_collection_properties("c", properties={"collection.ttl.seconds": 3600})


@scen("集合管理")
def add_collection_field(c):
    quick(c)
    c.add_collection_field("c", field_name="extra", data_type=DataType.INT64, nullable=True)


@scen("集合管理")
def load_release_state(c):
    quick(c)
    c.release_collection("c")
    c.get_load_state("c")
    c.load_collection("c")


# --- 检索形态 ---
@scen("检索形态")
def query_iterator(c):
    quick(c, 120)
    it = c.query_iterator("c", batch_size=20, filter="id >= 0", output_fields=["id"])
    while it.next():
        pass
    it.close()


@scen("检索形态")
def search_iterator(c):
    quick(c, 120)
    it = c.search_iterator("c", data=[vec()], batch_size=20, limit=60)
    while it.next():
        pass
    it.close()


@scen("检索形态")
def range_search(c):
    schema_coll(c, "FLAT", metric="L2")
    c.insert("c", rows())
    c.search("c", [vec()], limit=5, search_params={"params": {"radius": 2.0, "range_filter": 0.0}})


@scen("检索形态")
def group_by_search(c):
    schema_coll(c, "FLAT", metric="L2")
    c.insert("c", rows())
    c.search("c", [vec()], limit=3, group_by_field="category")


@scen("检索形态")
def query_limit_offset_count(c):
    quick(c)
    c.query("c", filter="", output_fields=["count(*)"])
    c.query("c", filter="id >= 0", limit=5, offset=3)


@scen("检索形态")
def upsert_partial(c):
    quick(c)
    c.upsert("c", [dict(id=1, category="new")], partial_update=True)


# --- 多向量 / 稀疏 / 全文 ---
@scen("多向量与全文")
def multi_vector_hybrid(c):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("v1", DataType.FLOAT_VECTOR, dim=DIM)
    s.add_field("v2", DataType.FLOAT_VECTOR, dim=DIM)
    ip = c.prepare_index_params()
    ip.add_index("v1", index_type="FLAT", metric_type="L2")
    ip.add_index("v2", index_type="FLAT", metric_type="L2")
    c.create_collection("c", schema=s, index_params=ip)
    c.insert("c", [dict(id=i, v1=vec(), v2=vec()) for i in range(30)])
    reqs = [AnnSearchRequest([vec()], "v1", {"metric_type": "L2"}, 5),
            AnnSearchRequest([vec()], "v2", {"metric_type": "L2"}, 5)]
    c.hybrid_search("c", reqs, RRFRanker(), limit=3)


@scen("多向量与全文")
def sparse_vector(c):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("sv", DataType.SPARSE_FLOAT_VECTOR)
    ip = c.prepare_index_params()
    ip.add_index("sv", index_type="SPARSE_INVERTED_INDEX", metric_type="IP")
    c.create_collection("c", schema=s, index_params=ip)
    c.insert("c", [dict(id=i, sv={i: 1.0, i + 1: 0.5}) for i in range(20)])
    c.search("c", [{3: 1.0}], anns_field="sv", limit=3)


@scen("多向量与全文")
def bm25_full_text(c):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("text", DataType.VARCHAR, max_length=256, enable_analyzer=True)
    s.add_field("sparse", DataType.SPARSE_FLOAT_VECTOR)
    s.add_function(Function(name="bm25", function_type=FunctionType.BM25, input_field_names=["text"], output_field_names=["sparse"]))
    ip = c.prepare_index_params()
    ip.add_index("sparse", index_type="SPARSE_INVERTED_INDEX", metric_type="BM25")
    c.create_collection("c", schema=s, index_params=ip)
    c.insert("c", [dict(id=i, text=f"apple banana {i}") for i in range(20)])
    c.search("c", ["apple"], anns_field="sparse", limit=3)


@scen("多向量与全文")
def text_match(c):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("vector", DataType.FLOAT_VECTOR, dim=DIM)
    s.add_field("text", DataType.VARCHAR, max_length=256, enable_analyzer=True, enable_match=True)
    ip = c.prepare_index_params()
    ip.add_index("vector", index_type="FLAT", metric_type="L2")
    c.create_collection("c", schema=s, index_params=ip)
    c.insert("c", [dict(id=i, vector=vec(), text=f"apple {i}") for i in range(20)])
    c.query("c", filter="text_match(text, 'apple')", output_fields=["id"])


@scen("多向量与全文")
def binary_vector(c):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("bv", DataType.BINARY_VECTOR, dim=16)
    ip = c.prepare_index_params()
    ip.add_index("bv", index_type="BIN_FLAT", metric_type="HAMMING")
    c.create_collection("c", schema=s, index_params=ip)
    c.insert("c", [dict(id=i, bv=bytes([i, 255 - i])) for i in range(20)])
    c.search("c", [bytes([1, 2])], anns_field="bv", limit=3)


@scen("多向量与全文")
def float16_vector(c):
    import numpy as np
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("v", DataType.FLOAT16_VECTOR, dim=DIM)
    ip = c.prepare_index_params()
    ip.add_index("v", index_type="FLAT", metric_type="L2")
    c.create_collection("c", schema=s, index_params=ip)
    c.insert("c", [dict(id=i, v=np.random.rand(DIM).astype(np.float16)) for i in range(20)])
    c.search("c", [np.random.rand(DIM).astype(np.float16)], anns_field="v", limit=3)


@scen("多向量与全文")
def nullable_default(c):
    s = MilvusClient.create_schema(auto_id=True)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("vector", DataType.FLOAT_VECTOR, dim=DIM)
    s.add_field("age", DataType.INT64, nullable=True)
    s.add_field("city", DataType.VARCHAR, max_length=16, default_value="sh")
    ip = c.prepare_index_params()
    ip.add_index("vector", index_type="FLAT", metric_type="L2")
    c.create_collection("c", schema=s, index_params=ip)
    c.insert("c", [dict(vector=vec(), age=None), dict(vector=vec(), age=3, city="bj")])
    c.query("c", filter="age is null", output_fields=["*"])


def main():
    work, out = sys.argv[1], sys.argv[2]
    only = sys.argv[3:] or list(SCEN)
    res = {}
    for name in only:
        group, fn = SCEN[name]
        d = os.path.join(work, name)
        shutil.rmtree(d, ignore_errors=True)
        os.makedirs(d)
        c = MilvusClient(os.path.join(d, "p.db"))
        try:
            fn(c)
            res[name] = dict(group=group, ok=True)
        except Exception as e:  # noqa: BLE001
            if hasattr(e, "details") and callable(e.details):
                msg = f"{e.code().name}: {e.details()}"[:300]
            else:
                msg = str(e).strip().splitlines()[0][:300] if str(e).strip() else type(e).__name__
            tb = traceback.extract_tb(e.__traceback__)
            api = next((f.line for f in reversed(tb) if f.filename == __file__), "")
            res[name] = dict(group=group, ok=False, err=f"{type(e).__name__}: {msg}", at=api)
        finally:
            try:
                c.close()
            except Exception:  # noqa: BLE001
                pass
        print(name, "OK" if res[name]["ok"] else "FAIL " + res[name]["err"][:160], flush=True)
    json.dump(res, open(out, "w"), ensure_ascii=False, indent=1)


if __name__ == "__main__":
    main()
