"""0030 稀疏向量的差分场景：写入与原始 RPC 边界、索引与加载规则、检索的次序、错误先后与分区语义。

每个场景是 `fn(c, rec)`：在客户端 `c` 上按顺序执行，`rec(name, fn)` 记下一步的结果。测试在本实现与真实 3.x 上
各跑一遍，逐项比较；`REGISTERED` 是 0030 已登记的差异，两边的结果都逐项断言。
"""

import json
import struct

from pymilvus import AnnSearchRequest, DataType, Function, FunctionType, MilvusClient, RRFRanker, WeightedRanker
from pymilvus.grpc_gen import common_pb2, milvus_pb2, schema_pb2

SCENARIOS = {}


def scenario(fn):
    SCENARIOS[fn.__name__] = fn
    return fn


def packed(pairs):
    return b"".join(struct.pack("<If", i, v) for i, v in pairs)


def hits(res):
    return [[(h["id"], h["distance"], h.get("entity")) for h in q] for q in res]


def bits(res):
    """分数的 float32 位模式：区分 0.0 与下溢后仍为正的极小值。"""
    return [[(h["id"], struct.pack(">f", h["distance"]).hex()) for h in q] for q in res]


def stub(c):
    return c._get_connection()._stub


def schema(dense=True, nullable=False, dynamic=False):
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=dynamic)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("tag", DataType.VARCHAR, max_length=8)
    s.add_field("n", DataType.INT64)
    if dense:
        s.add_field("d", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("sp", DataType.SPARSE_FLOAT_VECTOR, nullable=nullable)
    return s


def sp_schema(nullable=False, dense=False):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    if dense:
        s.add_field("d", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("sp", DataType.SPARSE_FLOAT_VECTOR, nullable=nullable)
    return s


ROWS = [
    {"id": 1, "tag": "a", "n": 1, "d": [1.0, 0.0], "sp": {0: 1.0, 5: 2.0}},
    {"id": 2, "tag": "b", "n": 2, "d": [0.0, 1.0], "sp": {5: 1.0, 9: 3.0}},
    {"id": 3, "tag": "a", "n": 3, "d": [1.0, 1.0], "sp": {0: 0.5}},
    {"id": 4, "tag": "b", "n": 4, "d": [0.5, 0.5], "sp": {9: 1.0, 100: 4.0}},
    {"id": 5, "tag": "a", "n": 5, "d": [0.2, 0.9], "sp": {7: 1.0}},
]
Q = [{0: 1.0, 5: 1.0}]


def sparse_index(c, field="sp", **params):
    p = c.prepare_index_params()
    p.add_index(field, index_type="SPARSE_INVERTED_INDEX", metric_type="IP", params=params or None)
    return p


def both_index(c):
    p = c.prepare_index_params()
    p.add_index("d", index_type="FLAT", metric_type="L2")
    p.add_index("sp", index_type="SPARSE_INVERTED_INDEX", metric_type="IP")
    return p


def index(c, name, field="sp", index_type="SPARSE_INVERTED_INDEX", metric="IP", **params):
    p = c.prepare_index_params()
    p.add_index(field, index_type=index_type, metric_type=metric, params=params or None)
    c.create_index(name, p)


def raw_insert(c, name, contents, nulls=None, start=100):
    n = len(nulls) if nulls is not None else len(contents)
    fields = [schema_pb2.FieldData(
        field_name="id", type=DataType.INT64,
        scalars=schema_pb2.ScalarField(long_data=schema_pb2.LongArray(data=list(range(start, start + n)))))]
    fd = schema_pb2.FieldData(field_name="sp", type=DataType.SPARSE_FLOAT_VECTOR, vectors=schema_pb2.VectorField(
        sparse_float_vector=schema_pb2.SparseFloatArray(contents=contents)))
    if nulls is not None:
        fd.valid_data.extend(nulls)
    fields.append(fd)
    r = stub(c).Insert(milvus_pb2.InsertRequest(collection_name=name, fields_data=fields, num_rows=n))
    return (r.status.code, r.status.reason, r.insert_cnt)


def raw_search(c, name, query, topk=10, offset=None, expr="", extra=None, output=("n",)):
    ph = common_pb2.PlaceholderGroup(placeholders=[common_pb2.PlaceholderValue(tag="$0", type=104, values=[packed(query)])])
    params = {"topk": str(topk), "anns_field": "sp", "metric_type": "IP"}
    if offset is not None:
        params["offset"] = str(offset)
    params.update(extra or {})
    r = stub(c).Search(milvus_pb2.SearchRequest(
        collection_name=name, dsl=expr, dsl_type=1, placeholder_group=ph.SerializeToString(),
        search_params=[common_pb2.KeyValuePair(key=k, value=v) for k, v in params.items()],
        output_fields=list(output)))
    if r.status.code:
        return {"code": r.status.code, "reason": r.status.reason}
    return {"ids": list(r.results.ids.int_id.data), "scores": list(r.results.scores), "topks": list(r.results.topks)}


@scenario
def writes_reads_indexes_and_search(c, rec):
    """建表、写入校验、原始写入、读回、索引定义、加载与检索后半段（Search / Hybrid / 更新后）。"""
    rec("create/dense+sparse", lambda: c.create_collection("c", schema=schema(), index_params=both_index(c)))
    rec("describe/c", lambda: [(f["name"], str(f["type"]), f.get("params")) for f in c.describe_collection("c")["fields"]])
    rec("create/sparse_only_noindex", lambda: c.create_collection("so", schema=schema(dense=False)))
    rec("create/sparse_only_indexed", lambda: c.create_collection("si", schema=schema(dense=False), index_params=sparse_index(c)))
    rec("create/nullable", lambda: c.create_collection("nu", schema=schema(dense=False, nullable=True), index_params=sparse_index(c)))
    s2 = MilvusClient.create_schema(auto_id=False)
    s2.add_field("id", DataType.INT64, is_primary=True)
    s2.add_field("sp", DataType.SPARSE_FLOAT_VECTOR, dim=8)
    rec("create/with_dim", lambda: c.create_collection("wd", schema=s2))
    rec("insert/c", lambda: c.insert("c", ROWS)["insert_count"])
    rec("insert/si", lambda: c.insert("si", [{k: v for k, v in r.items() if k != "d"} for r in ROWS])["insert_count"])
    values = {
        "negative": {1: -1.0}, "inf": {1: float("inf")}, "zero_only": {1: 0.0}, "empty": {},
        "max_dim": {2**32 - 2: 1.0}, "too_big_float32": {1: 1e39}, "tiny_float32": {1: 1e-46},
        "list_pairs": [(3, 1.5), (1, 2.0)], "int_weight": {1: 2},
    }
    for i, (name, v) in enumerate(values.items()):
        rec(f"insert/value/{name}", lambda i=i, v=v: c.insert("si", [{"id": 50 + i, "tag": "x", "n": 0, "sp": v}])["insert_count"])
    rec("insert/missing_sp", lambda: c.insert("si", [{"id": 70, "tag": "x", "n": 0}]))
    rec("insert/null_sp_not_nullable", lambda: c.insert("si", [{"id": 71, "tag": "x", "n": 0, "sp": None}]))
    rec("insert/nullable_null", lambda: c.insert("nu", [{"id": 1, "tag": "x", "n": 0, "sp": None},
                                                          {"id": 2, "tag": "y", "n": 0, "sp": {3: 1.0}}])["insert_count"])
    rec("insert/nullable_missing", lambda: c.insert("nu", [{"id": 3, "tag": "x", "n": 0}])["insert_count"])
    rec("insert/nullable_value", lambda: c.insert("nu", [{"id": 4, "tag": "x", "n": 0, "sp": {3: 2.0}}])["insert_count"])
    for i, (name, contents) in enumerate({
        "dup_dims": [packed([(5, 1.0), (5, 2.0)])], "unsorted": [packed([(9, 1.0), (2, 2.0)])],
        "truncated": [packed([(1, 1.0)])[:6]], "nan": [packed([(1, float("nan"))])],
        "reserved_dim": [packed([(2**32 - 1, 1.0)])], "empty_bytes": [b""],
    }.items()):
        rec(f"raw_insert/{name}", lambda contents=contents, i=i: raw_insert(c, "so", contents, start=100 + 10 * i))
    rec("raw_insert/count_mismatch", lambda: raw_insert(c, "so", [packed([(1, 1.0)])] * 2, nulls=[True], start=200))
    rec("query/c", lambda: c.query("c", filter="id > 0", output_fields=["sp"]))
    rec("query/so", lambda: c.query("so", filter="id > 0", output_fields=["*"]))
    rec("query/nu", lambda: c.query("nu", filter="id > 0", output_fields=["sp"]))
    rec("get/c", lambda: c.get("c", ids=[2], output_fields=["*"]))
    rec("filter/on_sparse", lambda: c.query("c", filter="sp == 1"))
    rec("filter/is_null", lambda: c.query("nu", filter="sp is null"))
    for name, kw in {
        "bm25_metric": dict(metric_type="BM25"), "cosine": dict(metric_type="COSINE"),
        "flat_type": dict(index_type="FLAT", metric_type="IP"), "autoindex": dict(index_type="AUTOINDEX", metric_type="IP"),
        "k1": dict(metric_type="IP", params={"bm25_k1": 1.2}), "drop_ratio": dict(metric_type="IP", params={"drop_ratio_build": 0.2}),
        "drop_ratio_zero": dict(metric_type="IP", params={"drop_ratio_build": 0}),
        "algo": dict(metric_type="IP", params={"inverted_index_algo": "DAAT_MAXSCORE"}),
        "algo_ok": dict(metric_type="IP", params={"inverted_index_algo": "taat_naive"}), "no_metric": dict(),
    }.items():
        def make(kw=kw, name=name):
            c.create_collection(f"ix_{name}", schema=schema(dense=False))
            p = c.prepare_index_params()
            args = dict(index_type="SPARSE_INVERTED_INDEX")
            args.update(kw)
            p.add_index("sp", **args)
            c.create_index(f"ix_{name}", p)
            return c.describe_index(f"ix_{name}", "sp")
        rec(f"index/{name}", make)
    rec("index/sparse_metric_l2", lambda: (c.create_collection("ix_l2", schema=schema(dense=False)),
                                           index(c, "ix_l2", metric="L2")))
    rec("index/sparse_on_dense", lambda: (c.create_collection("ix_d", schema=schema()), index(c, "ix_d", field="d")))
    rec("load/so_noindex", lambda: c.load_collection("so"))
    rec("search/so_noindex", lambda: hits(c.search("so", [{0: 1.0}], anns_field="sp", limit=3)))
    rec("load/c", lambda: c.load_collection("c"))
    rec("search/basic", lambda: hits(c.search("c", Q, anns_field="sp", limit=5, output_fields=["tag"])))
    rec("search/output_sp", lambda: hits(c.search("c", Q, anns_field="sp", limit=5, output_fields=["sp", "d"])))
    rec("search/nq2", lambda: hits(c.search("c", [{9: 1.0}, {7: 2.0, 0: 1.0}], anns_field="sp", limit=2)))
    rec("search/empty_query", lambda: hits(c.search("c", [{}], anns_field="sp", limit=5)))
    rec("search/zero_query", lambda: hits(c.search("c", [{0: 0.0}], anns_field="sp", limit=5)))
    rec("search/no_overlap", lambda: hits(c.search("c", [{12345: 1.0}], anns_field="sp", limit=5)))
    rec("search/negative_query", lambda: hits(c.search("c", [{0: -1.0}], anns_field="sp", limit=5)))
    rec("search/inf_query", lambda: hits(c.search("c", [{0: float("inf")}], anns_field="sp", limit=5)))
    for metric in ["IP", "L2", "BM25", "ip", ""]:
        rec(f"search/metric_{metric}", lambda metric=metric: hits(c.search(
            "c", Q, anns_field="sp", limit=5, search_params={"metric_type": metric})))
    for name, params in {"drop_ratio_search": {"drop_ratio_search": 0.5}, "drop_ratio_search0": {"drop_ratio_search": 0},
                         "k1": {"bm25_k1": 1.0}, "algo": {"inverted_index_algo": "TAAT_NAIVE"}}.items():
        rec(f"search/params/{name}", lambda params=params: hits(c.search(
            "c", Q, anns_field="sp", limit=5, search_params={"params": params})))
    rec("search/filter", lambda: hits(c.search("c", Q, anns_field="sp", limit=5, filter="tag == 'a'")))
    rec("search/offset", lambda: hits(c.search("c", Q, anns_field="sp", limit=1, offset=1)))
    rec("search/range", lambda: hits(c.search("c", Q, anns_field="sp", limit=5,
                                              search_params={"params": {"radius": 0.6, "range_filter": 3.0}})))
    rec("search/group", lambda: hits(c.search("c", Q, anns_field="sp", limit=5, group_by_field="tag")))
    rec("search/group_size", lambda: hits(c.search("c", Q, anns_field="sp", limit=5, group_by_field="tag", group_size=2)))
    rec("search/round", lambda: hits(c.search("c", [{0: 0.123456, 5: 0.333333}], anns_field="sp", limit=5, round_decimal=2)))
    rec("search/dense_query_on_sparse", lambda: hits(c.search("c", [[1.0, 0.0]], anns_field="sp", limit=5)))
    rec("search/sparse_query_on_dense", lambda: hits(c.search("c", Q, anns_field="d", limit=5)))
    rec("search/text_query_on_sparse", lambda: hits(c.search("c", ["hello"], anns_field="sp", limit=5)))
    rec("search/default_field_dense", lambda: hits(c.search("c", [[1.0, 0.0]], limit=2)))
    rec("search/sparse_only_no_anns", lambda: (c.load_collection("si"), hits(c.search("si", Q, limit=3)))[1])
    rec("search/sparse_only_anns", lambda: hits(c.search("si", Q, anns_field="sp", limit=3)))
    rec("search/nullable", lambda: (c.load_collection("nu"), hits(c.search("nu", [{3: 1.0}], anns_field="sp", limit=3)))[1])
    rec("search/partition", lambda: hits(c.search("c", Q, anns_field="sp", limit=5, partition_names=["_default"])))
    boost = Function(name="b", input_field_names=[], function_type=FunctionType.RERANK,
                     params={"reranker": "boost", "filter": "tag == 'b'", "weight": 10.0})
    rec("search/boost", lambda: hits(c.search("c", Q, anns_field="sp", limit=5, ranker=boost)))
    reqs = [AnnSearchRequest([[1.0, 0.0]], "d", {"metric_type": "L2"}, 5), AnnSearchRequest(Q, "sp", {"metric_type": "IP"}, 5)]
    rec("hybrid/rrf", lambda: hits(c.hybrid_search("c", reqs, RRFRanker(), limit=5)))
    rec("hybrid/weighted", lambda: hits(c.hybrid_search("c", reqs, WeightedRanker(0.3, 0.7), limit=5)))
    rec("hybrid/weighted_norm", lambda: hits(c.hybrid_search("c", reqs, WeightedRanker(0.3, 0.7, norm_score=True), limit=5)))
    rec("hybrid/output_sp", lambda: hits(c.hybrid_search("c", reqs, RRFRanker(), limit=5, output_fields=["sp"])))
    rec("delete", lambda: c.delete("c", ids=[1]))
    rec("upsert", lambda: c.upsert("c", [{"id": 2, "tag": "b", "n": 2, "d": [0.0, 1.0], "sp": {0: 9.0}}]))
    rec("upsert_partial", lambda: c.upsert("c", [{"id": 3, "n": 33}], partial_update=True))
    rec("after_update", lambda: hits(c.search("c", Q, anns_field="sp", limit=5, output_fields=["n", "sp"])))
    rec("flush", lambda: c.flush("c"))
    rec("after_flush", lambda: hits(c.search("c", Q, anns_field="sp", limit=5, output_fields=["n"])))
    rec("query_after_flush", lambda: c.query("c", filter="id > 0", output_fields=["sp"]))
    rec("upsert_partial_flushed", lambda: c.upsert("c", [{"id": 4, "n": 44}], partial_update=True))
    rec("insert_after_flush", lambda: c.insert("c", [{"id": 6, "tag": "a", "n": 6, "d": [1.0, 0.0], "sp": {0: 1.0, 5: 1.0}}]))
    rec("mixed_sources", lambda: hits(c.search("c", Q, anns_field="sp", limit=5, output_fields=["sp"])))
    rec("mixed_query", lambda: c.query("c", filter="id > 0", output_fields=["sp", "n"]))


def reopened(c, rec):
    """`writes_reads_indexes_and_search` 之后两边各自关闭并重开服务。"""
    rec("r/describe_index", lambda: c.describe_index("c", "sp"))
    rec("r/load", lambda: c.load_collection("c"))
    rec("r/search", lambda: hits(c.search("c", Q, anns_field="sp", limit=5, output_fields=["sp"])))
    rec("r/query", lambda: c.query("c", filter="id > 0", output_fields=["sp"]))
    rec("r/nu", lambda: (c.load_collection("nu"), c.query("nu", filter="id > 0", output_fields=["sp"]))[1])
    rec("r/so", lambda: (c.load_collection("so"), c.query("so", filter="id > 0", output_fields=["sp"]))[1])


@scenario
def raw_rpc_tuning_and_load_rules(c, rec):
    """原始写入的边界、调参项在建索引 / 加载 / 检索时的校验、稠密字段上的稀疏索引、加载规则与 iterator。"""
    c.create_collection("r", schema=sp_schema())
    for i, (name, contents) in enumerate({
        "dup_dims": [packed([(5, 1.0), (5, 2.0)])], "unsorted": [packed([(9, 1.0), (2, 2.0)])],
        "zero_and_value": [packed([(1, 0.0), (2, 2.0)])], "truncated": [packed([(1, 1.0)])[:6]],
        "nan": [packed([(1, float("nan"))])], "neg_inf": [packed([(1, float("-inf"))])],
        "reserved_dim": [packed([(2**32 - 1, 1.0)])], "empty_bytes": [b""],
        "negative_zero": [packed([(1, -0.0), (3, 1.0)])],
    }.items()):
        rec(f"raw/{name}", lambda contents=contents, i=i: raw_insert(c, "r", contents, start=100 + 10 * i))
    rec("raw/query", lambda: c.query("r", filter="id > 0", output_fields=["sp"]))
    c.create_collection("nu", schema=sp_schema(nullable=True))
    rec("nullable/compact", lambda: raw_insert(c, "nu", [packed([(3, 1.0)])], nulls=[False, True], start=1))
    rec("nullable/full", lambda: raw_insert(c, "nu", [b"", packed([(4, 1.0)])], nulls=[False, True], start=11))
    rec("nullable/count", lambda: c.query("nu", filter="", output_fields=["count(*)"]))

    def with_default():
        s = MilvusClient.create_schema(auto_id=False)
        s.add_field("id", DataType.INT64, is_primary=True)
        s.add_field("sp", DataType.SPARSE_FLOAT_VECTOR, default_value={1: 1.0})
        c.create_collection("dv", schema=s)
        return c.describe_collection("dv")["fields"]
    rec("default/declare", with_default)
    for name, params in {"k1": {"bm25_k1": 1.2}, "drop": {"drop_ratio_build": 0.2},
                         "algo": {"inverted_index_algo": "DAAT_MAXSCORE"}}.items():
        c.create_collection(f"p_{name}", schema=sp_schema())
        c.insert(f"p_{name}", [{"id": 1, "sp": {1: 1.0}}])
        rec(f"tune/{name}/create", lambda name=name, params=params: index(c, f"p_{name}", **params))
        rec(f"tune/{name}/load", lambda name=name: c.load_collection(f"p_{name}"))
        rec(f"tune/{name}/search", lambda name=name: hits(c.search(f"p_{name}", [{1: 1.0}], anns_field="sp", limit=1)))
        rec(f"tune/{name}/describe", lambda name=name: c.describe_index(f"p_{name}", "sp"))
    c.create_collection("dz", schema=sp_schema(dense=True))
    c.insert("dz", [{"id": 1, "d": [1.0, 0.0], "sp": {1: 1.0}}, {"id": 2, "d": [3.0, 4.0], "sp": {1: 2.0}}])
    for metric in ["IP", "L2", "COSINE", "BM25", "NONE"]:
        rec(f"dense_sparse_index/{metric}", lambda metric=metric: (
            c.release_collection("dz"), c.list_indexes("dz") and c.drop_index("dz", "d"),
            index(c, "dz", field="d", metric=metric), c.load_collection("dz"),
            c.describe_index("dz", "d")["metric_type"],
            hits(c.search("dz", [[1.0, 1.0]], anns_field="d", limit=2)))[4:])
    c.create_collection("lz", schema=sp_schema(dense=True))
    c.insert("lz", [{"id": 1, "d": [1.0, 0.0], "sp": {1: 1.0}}, {"id": 2, "d": [0.0, 1.0], "sp": {1: 2.0, 2: 1.0}}])
    rec("load_rules/dense_only_indexed", lambda: (index(c, "lz", field="d", index_type="FLAT", metric="L2"), c.load_collection("lz"))[1])
    rec("load_rules/search_sparse_noindex", lambda: hits(c.search("lz", [{1: 1.0}], anns_field="sp", limit=3)))
    rec("load_rules/describe_sparse_index", lambda: c.describe_index("lz", "sp"))
    rec("load_rules/create_sparse_index_while_loaded", lambda: index(c, "lz"))
    rec("load_rules/search_after", lambda: hits(c.search("lz", [{1: 1.0}], anns_field="sp", limit=3)))
    rec("load_rules/drop_sparse_index_loaded", lambda: c.drop_index("lz", "sp"))
    rec("load_rules/release_drop", lambda: (c.release_collection("lz"), c.drop_index("lz", "sp")))
    rec("load_rules/load_after_drop", lambda: c.load_collection("lz"))
    rec("load_rules/search_after_drop", lambda: hits(c.search("lz", [{1: 1.0}], anns_field="sp", limit=3)))
    c.create_collection("nl", schema=sp_schema())
    c.insert("nl", [{"id": 1, "sp": {1: 1.0}}])
    rec("unloaded/state", lambda: c.get_load_state("nl"))
    rec("unloaded/search", lambda: hits(c.search("nl", [{1: 1.0}], anns_field="sp", limit=1)))
    c.create_collection("tie", schema=sp_schema())
    index(c, "tie")
    c.load_collection("tie")
    for ids in [(5, 3), (4, 1)]:
        c.insert("tie", [{"id": i, "sp": {1: 1.0}} for i in ids])
        c.flush("tie")
    c.insert("tie", [{"id": i, "sp": {1: 1.0}} for i in (9, 2)])
    rec("tie/search", lambda: hits(c.search("tie", [{1: 1.0}], anns_field="sp", limit=6)))
    rec("tie/search_limit3", lambda: hits(c.search("tie", [{1: 1.0}], anns_field="sp", limit=3)))
    c.create_collection("num", schema=sp_schema())
    c.insert("num", [{"id": 1, "sp": {1: 0.1, 2: 0.2, 3: 0.3}}, {"id": 2, "sp": {1: 1e-30, 2: 3e38}}])
    c.load_collection("num")
    rec("num/search", lambda: bits(c.search("num", [{1: 0.7, 2: 0.3, 3: 0.11}], anns_field="sp", limit=2)))
    rec("num/overflow", lambda: bits(c.search("num", [{2: 3e38}], anns_field="sp", limit=2)))
    rec("num/query", lambda: c.query("num", filter="id > 0", output_fields=["sp"]))

    def iterate(**kw):
        iterator = c.search_iterator("tie", [{1: 1.0}], anns_field="sp", **kw)
        pages = []
        while page := iterator.next():
            pages.append([(h["id"], h["distance"]) for h in page])
        iterator.close()
        return pages
    rec("iterator/search", lambda: iterate(batch_size=2, limit=5))
    rec("iterator/radius", lambda: iterate(batch_size=2, limit=5, search_params={"params": {"radius": 0.5}}))
    rec("hybrid/two_sparse", lambda: hits(c.hybrid_search("tie", [AnnSearchRequest([{1: 1.0}], "sp", {"metric_type": "IP"}, 3),
                                                                  AnnSearchRequest([{1: 2.0}], "sp", {}, 3)], RRFRanker(), limit=3)))
    rec("hybrid/bad_metric", lambda: hits(c.hybrid_search("tie", [AnnSearchRequest([{1: 1.0}], "sp", {"metric_type": "L2"}, 3)],
                                                          RRFRanker(), limit=3)))


@scenario
def scores_ties_and_underflow(c, rec):
    """double 分数、首个命中维度与跨源 float32 的次序（含原始 RPC 的未排序查询），float32 下溢与范围检索。"""
    s = sp_schema()
    c.create_collection("u", schema=s)
    c.load_collection("u")
    c.insert("u", [{"id": 1, "sp": {0: 1e-30}}, {"id": 2, "sp": {0: 1.0}}, {"id": 3, "sp": {0: 1e-30, 1: 1e-30}},
                   {"id": 4, "sp": {1: 1e-30}}, {"id": 5, "sp": {0: 1e-38}}])
    for stage in ["mem", "seg"]:
        rec(f"u/{stage}/q0", lambda: bits(c.search("u", [{0: 1e-30}], anns_field="sp", limit=10)))
        rec(f"u/{stage}/q10", lambda: bits(c.search("u", [{1: 1e-30, 0: 1e-30}], anns_field="sp", limit=10)))
        rec(f"u/{stage}/radius0", lambda: bits(c.search("u", [{0: 1e-30}], anns_field="sp", limit=10,
                                                         search_params={"params": {"radius": 0.0}})))
        rec(f"u/{stage}/range_neg", lambda: bits(c.search("u", [{0: 1e-30}], anns_field="sp", limit=10,
                                                           search_params={"params": {"radius": -1e-45, "range_filter": 1.0}})))
        c.flush("u")
    for name, tiny in [("t", 2.0**-12), ("t2", 2.0**-30)]:
        c.create_collection(name, schema=s)
        c.load_collection(name)
        c.insert(name, [{"id": 10, "sp": {0: 1.0}}, {"id": 11, "sp": {0: 1.0, 1: tiny}}])
        c.flush(name)
        c.insert(name, [{"id": 1, "sp": {0: 1.0}}, {"id": 2, "sp": {0: 1.0, 1: tiny}}])
        q = {0: 1.0, 1: tiny}
        for limit in [1, 3, 10]:
            rec(f"{name}/limit{limit}", lambda name=name, q=q, limit=limit: bits(c.search(name, [q], anns_field="sp", limit=limit)))
        rec(f"{name}/raw_sorted", lambda name=name, tiny=tiny: raw_search(c, name, [(0, 1.0), (1, tiny)], output=()))
        rec(f"{name}/raw_reversed", lambda name=name, tiny=tiny: raw_search(c, name, [(1, tiny), (0, 1.0)], output=()))
    c.create_collection("w", schema=s)
    c.load_collection("w")
    c.insert("w", [{"id": 1, "sp": {0: 1.0}}, {"id": 2, "sp": {0: 1.0, 1: 2.0**-12}}, {"id": 3, "sp": {1: 2.0**-12, 0: 1.0}}])
    c.create_collection("f", schema=s)
    c.load_collection("f")
    c.insert("f", [{"id": 1, "sp": {5: 1.0}}, {"id": 2, "sp": {0: 1.0}}, {"id": 3, "sp": {0: 0.5, 5: 0.5}}])
    for stage in ["mem", "seg"]:
        rec(f"w/{stage}", lambda: bits(c.search("w", [{0: 1.0, 1: 2.0**-12}], anns_field="sp", limit=10)))
        rec(f"w/{stage}_limit1", lambda: bits(c.search("w", [{0: 1.0, 1: 2.0**-12}], anns_field="sp", limit=1)))
        rec(f"f/{stage}/q05", lambda: raw_search(c, "f", [(0, 1.0), (5, 1.0)], output=()))
        rec(f"f/{stage}/q50", lambda: raw_search(c, "f", [(5, 1.0), (0, 1.0)], output=()))
        c.flush("w")
        c.flush("f")


@scenario
def top_k_filters_and_error_order(c, rec):
    """top_k 为 0 / 负数（含 offset、范围、分组），过滤的 MemTable Python 路线与 segment 路线，查询与调参项的错误。"""
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=True)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("sp", DataType.SPARSE_FLOAT_VECTOR)
    s.add_field("n", DataType.INT64)
    s.add_field("t", DataType.VARCHAR, max_length=8)
    c.create_collection("e", schema=s)
    c.load_collection("e")
    for stage in ["empty", "mem", "seg", "mixed"]:
        if stage == "mem":
            c.insert("e", [{"id": i, "sp": {1: float(i)}, "n": i, "t": str(i), "x": i} for i in range(1, 5)])
        if stage == "seg":
            c.flush("e")
        if stage == "mixed":
            c.insert("e", [{"id": i, "sp": {1: float(i), 2: 1.0}, "n": i, "t": str(i), "x": str(i)} for i in range(5, 8)])
        for topk, offset in [(0, None), (-1, None), (2, 1), (-1, 2), (1, -1), (0, 0), (3, 0)]:
            rec(f"{stage}/topk{topk}/off{offset}", lambda topk=topk, offset=offset: raw_search(c, "e", [(1, 1.0)], topk, offset))
        rec(f"{stage}/range_neg_topk", lambda: raw_search(c, "e", [(1, 1.0)], -1, extra={"params": json.dumps({"radius": 0.0})}))
        rec(f"{stage}/group_neg_topk", lambda: raw_search(c, "e", [(1, 1.0)], -1, extra={"group_by_field": "n"}))
        rec(f"{stage}/group_neg_size", lambda: raw_search(c, "e", [(1, 1.0)], 3, extra={"group_by_field": "n", "group_size": "-1"}))
        for name, expr in {"wide_int": "n > 99999999999999999999", "dyn_filter": "x > 2", "dyn_str_cmp": "x > '2'",
                           "bad_expr_type": "t > 1", "like": "t like '1%'", "in_list": "n in [1, 6]"}.items():
            rec(f"{stage}/{name}", lambda expr=expr: raw_search(c, "e", [(1, 1.0)], 5, expr=expr))
        rec(f"{stage}/drop_ratio", lambda: raw_search(c, "e", [(1, 1.0)], 5, extra={"params": json.dumps({"drop_ratio_search": 0.1})}))
        rec(f"{stage}/bad_query_and_params", lambda: raw_search(
            c, "e", [(1, -1.0)], 5, extra={"params": json.dumps({"drop_ratio_search": 0.1})}))
        rec(f"{stage}/bad_query_and_expr", lambda: raw_search(c, "e", [(1, -1.0)], 5, expr="t > 1"))
        for name, query in {"empty_query": (), "zero_query": ((1, 0.0),), "dup_query": ((2, 1.0), (1, 1.0), (2, 3.0)),
                            "unsorted_query": ((2, 1.0), (1, 1.0))}.items():
            rec(f"{stage}/{name}", lambda query=query: raw_search(c, "e", query, 5))


@scenario
def grouping_and_decay_on_sparse_fields(c, rec):
    """以稀疏字段为分组键、作为 decay 输入（Search 与 HybridSearch，MemTable 与 flush 后）。"""
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("d", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("sp", DataType.SPARSE_FLOAT_VECTOR)
    s.add_field("n", DataType.INT64)
    c.create_collection("h", schema=s)
    c.load_collection("h")
    c.insert("h", [{"id": i, "d": [1.0, float(i)], "sp": {1: float(i % 2 + 1)}, "n": i} for i in range(1, 6)])

    def reqs():
        return [AnnSearchRequest([{1: 1.0}], "sp", {}, 10), AnnSearchRequest([[1.0, 0.0]], "d", {}, 10)]
    decay = Function(name="dc", function_type=FunctionType.RERANK, input_field_names=["sp"],
                     params={"reranker": "decay", "function": "gauss", "origin": 0, "scale": 1})
    for stage in ["mem", "seg"]:
        rec(f"{stage}/hybrid_group", lambda: hits(c.hybrid_search("h", reqs(), RRFRanker(), limit=5, group_by_field="sp",
                                                                  output_fields=["n"])))
        rec(f"{stage}/search_group", lambda: hits(c.search("h", [{1: 1.0}], anns_field="sp", limit=5, group_by_field="sp")))
        rec(f"{stage}/decay_sp", lambda: hits(c.search("h", [[1.0, 0.0]], anns_field="d", limit=5, ranker=decay)))
        rec(f"{stage}/hybrid_decay_sp", lambda: hits(c.hybrid_search("h", reqs(), decay, limit=5)))
        c.flush("h")


@scenario
def partition_selection(c, rec):
    """稀疏检索先在全部分区上判版本再剪枝（稠密先选分区）；同一集合里两种字段对照。"""
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("v", DataType.SPARSE_FLOAT_VECTOR)
    s.add_field("d", DataType.FLOAT_VECTOR, dim=2)
    c.create_collection("c", schema=s)
    c.create_partition("c", "p1")
    c.load_collection("c")

    def row(i, x):
        return {"id": i, "v": {1: float(x)}, "d": [float(x), 1.0]}

    def both(name, parts):
        for field, q in [("v", {1: 1.0}), ("d", [1.0, 0.0])]:
            rec(f"{name}/{field}", lambda field=field, q=q: hits(c.search(
                "c", [q], anns_field=field, limit=10, partition_names=parts, search_params={"metric_type": "IP"})))
    c.insert("c", [row(1, 1), row(2, 2)], partition_name="_default")
    c.flush("c")
    c.upsert("c", [row(1, 5)], partition_name="p1")
    both("moved/default", ["_default"])
    both("moved/p1", ["p1"])
    both("moved/both", ["_default", "p1"])
    both("moved/all", None)
    rec("moved/hybrid_default", lambda: hits(c.hybrid_search("c", [AnnSearchRequest([{1: 1.0}], "v", {}, 10)],
                                                             RRFRanker(), limit=10, partition_names=["_default"])))
    c.flush("c")
    both("flushed/default", ["_default"])
    both("flushed/p1", ["p1"])
    c.delete("c", ids=[2])
    both("deleted/default", ["_default"])


@scenario
def partition_key_buckets(c, rec):
    """partition key 集合：换 key 把行移到别的桶后，按桶选择的稀疏检索先全局判版本（与稠密对照）。"""
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("sp", DataType.SPARSE_FLOAT_VECTOR)
    s.add_field("k", DataType.INT64, is_partition_key=True)
    s.add_field("d", DataType.FLOAT_VECTOR, dim=2)
    c.create_collection("c", schema=s, num_partitions=16)
    c.load_collection("c")
    parts = c.list_partitions("c")
    rec("parts", lambda: parts)
    selected = [p for p in parts if p != "_default"][:5]

    def row(i, k):
        return {"id": i, "sp": {i % 3: 1.0 + (i % 2), 3: 0.5}, "k": k, "d": [1.0, float(i % 4)]}

    def both(name, parts):
        for field, q in [("sp", {0: 1.0, 1: 1.0, 2: 0.5}), ("d", [1.0, 0.0])]:
            rec(f"{name}/{field}", lambda field=field, q=q: hits(c.search(
                "c", [q], anns_field=field, limit=50, partition_names=parts, search_params={"metric_type": "IP"})))
    c.insert("c", [row(i, i) for i in range(1, 25)])
    both("mem/all", None)
    both("mem/selected", selected)
    c.flush("c")
    c.upsert("c", [row(i, i + 1) for i in range(1, 25, 2)])
    both("moved/all", None)
    both("moved/selected", selected)
    both("moved/one", selected[:1])
    both("moved/default", ["_default"])
    c.flush("c")
    both("flushed/selected", selected)
    both("flushed/one", selected[:1])
    c.delete("c", ids=[2, 4, 6])
    both("deleted/selected", selected)


@scenario
def partition_scoped_pending_delete(c, rec):
    """指定其他分区、尚未 flush 的删除：3.x 稀疏检索仍返回该行（已登记差异），flush 后两边一致。"""
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("v", DataType.SPARSE_FLOAT_VECTOR)
    c.create_collection("c", schema=s)
    c.create_partition("c", "p1")
    c.create_partition("c", "p2")
    c.load_collection("c")
    c.insert("c", [{"id": 1, "v": {1: 1.0}}, {"id": 2, "v": {1: 2.0}}], partition_name="p2")
    c.flush("c")
    c.delete("c", ids=[1], partition_name="p1")
    for stage in ["pending", "flushed"]:
        rec(f"{stage}/all", lambda: hits(c.search("c", [{1: 1.0}], anns_field="v", limit=10)))
        rec(f"{stage}/p2", lambda: hits(c.search("c", [{1: 1.0}], anns_field="v", limit=10, partition_names=["p2"])))
        c.flush("c")


# 结果中含稠密距离（或由稠密距离合成的分数）的步骤：浮点按 0004「决策」的距离误差比较，其余部分仍逐字比较。
# 稀疏 IP 的分数两边按同样的次序做 double 累加，要求逐位相同，不在此列。
def dense(step):
    return (step.startswith(("search/default_field_dense", "hybrid/weighted", "dense_sparse_index/"))
            or step.endswith(("/decay_sp", "/hybrid_decay_sp", "/d")))


# 0030「与 3.x 的差异」：指定其他分区、尚未 flush 的删除。
REGISTERED = {
    ("partition_scoped_pending_delete", "pending/all"): {
        "rust": ("ok", [[(2, 2.0, {"v": {1: 2.0}})]]),
        "ref": ("ok", [[(2, 2.0, {"v": {1: 2.0}}), (1, 1.0, {"v": {1: 1.0}})]]),
    },
    ("partition_scoped_pending_delete", "pending/p2"): {
        "rust": ("ok", [[(2, 2.0, {"v": {1: 2.0}})]]),
        "ref": ("ok", [[(2, 2.0, {"v": {1: 2.0}}), (1, 1.0, {"v": {1: 1.0}})]]),
    },
}
