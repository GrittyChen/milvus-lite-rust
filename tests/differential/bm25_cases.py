"""0031 BM25 的差分场景：函数与 schema 校验、`analyzer_params`、写入时生成输出字段、读回与重开、索引与度量、
检索打分与各检索路径。

每个场景是 `fn(c, rec)`：在客户端 `c` 上按顺序执行，`rec(name, fn)` 记下一步的结果。测试在本实现与真实 3.x 上
各跑一遍，逐项比较。建表多用原始 RPC：pymilvus 会在客户端拦下一部分输入，原始 RPC 才能到达服务端的判定。
"""

import json
import random

from pymilvus import (AnnSearchRequest, DataType, Function, FunctionScore, FunctionType, MilvusClient, RRFRanker,
                      WeightedRanker)
from pymilvus.grpc_gen import common_pb2, milvus_pb2, schema_pb2

SCENARIOS = {}


def scenario(fn):
    SCENARIOS[fn.__name__] = fn
    return fn


def stub(c):
    return c._get_connection()._stub


def status(r):
    s = getattr(r, "status", r)
    return (s.code, s.reason)


def field(name, dtype, **kw):
    f = schema_pb2.FieldSchema(name=name, data_type=dtype)
    for k, v in kw.pop("params", {}).items():
        f.type_params.append(common_pb2.KeyValuePair(key=k, value=v))
    for k, v in kw.items():
        setattr(f, k, v)
    return f


def fn(name="bm", ftype=1, inputs=("text",), outputs=("sp",), params=None):
    f = schema_pb2.FunctionSchema(name=name, type=ftype, input_field_names=list(inputs), output_field_names=list(outputs))
    for k, v in (params or {}).items():
        f.params.append(common_pb2.KeyValuePair(key=k, value=v))
    return f


def text_field(analyzer=True, extra=None, **kw):
    params = {"max_length": "100"}
    if analyzer:
        params["enable_analyzer"] = "true"
    params.update(extra or {})
    return field("text", 21, params=params, **kw)


def base(text=None, sp_type=104, extra=()):
    return [field("id", 5, is_primary_key=True), text or text_field(),
            field("sp", sp_type, params={"dim": "2"} if sp_type == 101 else {})] + list(extra)


def create(c, name, fields, functions=(), dynamic=False):
    s = schema_pb2.CollectionSchema(name=name, fields=fields, functions=list(functions), enable_dynamic_field=dynamic)
    r = stub(c).CreateCollection(milvus_pb2.CreateCollectionRequest(collection_name=name, schema=s.SerializeToString()))
    return status(r) + (c.has_collection(name),)


def describe(c, name):
    d = c.describe_collection(name)
    fields = [(f["name"], f.get("params"), f.get("is_function_output", False)) for f in d["fields"]]
    functions = [(f["name"], f["type"], list(f["input_field_names"]), list(f["output_field_names"]), f["params"])
                 for f in d.get("functions", [])]
    return fields, functions


@scenario
def function_validation(c, rec):
    cases = {
        "no_analyzer": (base(text_field(analyzer=False)), [fn()]),
        "analyzer_false": (base(text_field(extra={"enable_analyzer": "false"}, analyzer=False)), [fn()]),
        "input_int": ([field("id", 5, is_primary_key=True), field("text", 5), field("sp", 104)], [fn()]),
        "output_dense": (base(sp_type=101), [fn()]),
        "two_inputs": (base(extra=[field("t2", 21, params={"max_length": "9", "enable_analyzer": "true"})]),
                       [fn(inputs=("text", "t2"))]),
        "zero_inputs": (base(), [fn(inputs=())]),
        "two_outputs": (base(extra=[field("sp2", 104)]), [fn(outputs=("sp", "sp2"))]),
        "missing_input": (base(), [fn(inputs=("nope",))]),
        "missing_output": (base(), [fn(outputs=("nope",))]),
        "dup_name": (base(extra=[field("sp2", 104)]), [fn(), fn(outputs=("sp2",))]),
        "dup_output": (base(), [fn(), fn(name="b2")]),
        "type0": (base(), [fn(ftype=0)]),
        "type3": (base(), [fn(ftype=3)]),
        "marked_without_function": ([field("id", 5, is_primary_key=True), field("sp", 104, is_function_output=True),
                                     field("v", 101, params={"dim": "2"})], []),
        # 函数校验在 schema 校验的最后：前面的错误先报。
        "no_pk_and_bad_function": ([field("text", 21, params={"max_length": "9"}), field("sp", 104)], [fn()]),
        "ok": (base(), [fn()]),
        "two_bm25_same_input": (base(extra=[field("sp2", 104)]), [fn(), fn(name="b2", outputs=("sp2",))]),
        "nullable_text": (base(text_field(nullable=True)), [fn()]),
        "nullable_output": ([field("id", 5, is_primary_key=True), text_field(), field("sp", 104, nullable=True)], [fn()]),
        "varchar_pk_input": ([field("text", 21, is_primary_key=True, params={"max_length": "100", "enable_analyzer": "true"}),
                              field("sp", 104)], [fn()]),
        "output_marked_too": ([field("id", 5, is_primary_key=True), text_field(), field("sp", 104, is_function_output=True)],
                              [fn()]),
    }
    for name, (fields, functions) in cases.items():
        rec(f"create/{name}", lambda name=name, fields=fields, functions=functions: create(c, name, fields, functions))
    for name in ["ok", "two_bm25_same_input", "nullable_output", "varchar_pk_input"]:
        rec(f"describe/{name}", lambda name=name: describe(c, name))


@scenario
def analyzer_params(c, rec):
    values = ["0", "1.5", "true", "false", "0.0", "-0", "[]", "[1]", "{}", '""', '"standard"', "null", "not json",
              '{"tokenizer": "standard"}', '{"tokenizer": "Standard"}', '{"tokenizer": "english"}', '{"type": "english"}',
              '{"tokenizer": 5}', '{"tokenizer": 1.5}', '{"tokenizer": true}', '{"tokenizer": []}', '{"tokenizer": 0}',
              '{"tokenizer": ""}', '{"tokenizer": null, "type": 7}', '{"tokenizer": {}}', '{"tokenizer": {"type": "x"}}',
              '{"tokenizer": {"type": null}}', '{"tokenizer": {"type": 5}}', '{"tokenizer": {"type": {"a": [1, "b"]}}}',
              '{"tokenizer": {"mode": "x"}}', '{"tokenizer": "jieba"}',
              '{"filter": [{"type": "stop", "stop_words": ["fox", 1, true, null, 1.5, "The", ["a"]]}]}',
              '{"filter": ["lowercase", {"type": "stop", "stop_words": ["dog"]}, {"type": "stop", "stop_words": ["cat"]}]}',
              '{"filter": {"type": "stop", "stop_words": ["fox"]}}', '{"filter": [{"type": "stop", "stop_words": "fox"}]}',
              '{"filter": [{"type": "stop"}]}', '{"filter": [{"type": "Stop", "stop_words": ["fox"]}]}']
    for i, value in enumerate(values):
        name = f"ap{i}"
        rec(f"create/{value}", lambda name=name, value=value: create(
            c, name, base(text_field(extra={"analyzer_params": value})), [fn()]))
        if c.has_collection(name):
            rec(f"write/{value}", lambda name=name: c.insert(name, [
                {"id": 1, "text": "The fox, the DOG and 1 cat: True None 1.5"}, {"id": 2, "text": "fox"}])["insert_count"])
            rec(f"read/{value}", lambda name=name: c.query(name, "id >= 1", output_fields=["sp"]))
    # 只有 BM25 的输入字段构造分词器；其他字段上的假值照 0029 当作没有设置。
    rec("create/non_input_falsy", lambda: create(c, "np", base(extra=[
        field("other", 21, params={"max_length": "9", "enable_analyzer": "true", "analyzer_params": "0"})]), [fn()]))
    rec("describe/non_input_falsy", lambda: describe(c, "np"))


@scenario
def function_params(c, rec):
    params = {"a": "1", "b": "x", "c": '{"k": [1, 2]}', "d": "true", "e": " 2 ", "f": "1e5", "g": "", "h": "[1,", "i": "null"}
    rec("create", lambda: create(c, "p", base(), [fn(params=params)]))
    rec("describe", lambda: describe(c, "p"))
    rec("create/nan", lambda: create(c, "pn", base(), [fn(params={"a": "NaN"})]))
    rec("create/inf", lambda: create(c, "pi", base(), [fn(params={"a": "[Infinity]"})]))
    rec("create/huge", lambda: create(c, "ph", base(), [fn(params={"a": "1e999"})]))


def writes_schema(nullable=False, default=None, dynamic=False):
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=dynamic)
    s.add_field("id", DataType.INT64, is_primary=True)
    kw = {"nullable": nullable} if nullable else {}
    if default is not None:
        kw["default_value"] = default
    s.add_field("text", DataType.VARCHAR, max_length=200, enable_analyzer=True, **kw)
    s.add_field("tag", DataType.VARCHAR, max_length=8, nullable=True)
    s.add_field("sp", DataType.SPARSE_FLOAT_VECTOR)
    s.add_function(Function(name="bm", function_type=FunctionType.BM25, input_field_names=["text"], output_field_names=["sp"]))
    return s


def raw_insert(c, name, text=("a b",), sp=False, meta=None, upsert=False, partial=False, ids=None):
    ids = list(ids or range(1, len(text or ("",)) + 1))
    n = len(ids)
    fds = [schema_pb2.FieldData(field_name="id", type=5, scalars=schema_pb2.ScalarField(long_data=schema_pb2.LongArray(data=ids)))]
    if text is not None:
        fds.append(schema_pb2.FieldData(field_name="text", type=21, scalars=schema_pb2.ScalarField(
            string_data=schema_pb2.StringArray(data=list(text)))))
    if sp:
        fds.append(schema_pb2.FieldData(field_name="sp", type=104, vectors=schema_pb2.VectorField(
            dim=2, sparse_float_vector=schema_pb2.SparseFloatArray(dim=2, contents=[b"\x01\x00\x00\x00\x00\x00\x80\x3f"] * n))))
    if meta is not None:
        fds.append(schema_pb2.FieldData(field_name="$meta", type=23, is_dynamic=True, scalars=schema_pb2.ScalarField(
            json_data=schema_pb2.JSONArray(data=[json.dumps(m).encode() for m in meta]))))
    if upsert:
        r = stub(c).Upsert(milvus_pb2.UpsertRequest(collection_name=name, fields_data=fds, num_rows=len(ids),
                                                    partial_update=partial))
    else:
        r = stub(c).Insert(milvus_pb2.InsertRequest(collection_name=name, fields_data=fds, num_rows=len(ids)))
    return status(r)


@scenario
def writes_and_reads(c, rec):
    rec("create", lambda: c.create_collection("w", schema=writes_schema(dynamic=True)))
    rec("raw/with_output", lambda: raw_insert(c, "w", sp=True))
    rec("raw/meta_output", lambda: raw_insert(c, "w", meta=[{"sp": {"1": 1.0}}]))
    rec("raw/meta_output_second_row", lambda: raw_insert(c, "w", text=("a", "b"), meta=[{"x": 1}, {"sp": 1}]))
    rec("raw/meta_other", lambda: raw_insert(c, "w", text=("a",), meta=[{"x": 1}], ids=[50]))
    # 输出字段检查先于主键缺失这类逐行校验。
    rec("raw/output_and_missing_text", lambda: raw_insert(c, "w", text=None, sp=True))
    rec("raw/upsert_with_output", lambda: raw_insert(c, "w", sp=True, upsert=True))
    rec("raw/partial_with_output", lambda: raw_insert(c, "w", sp=True, upsert=True, partial=True))
    docs = ["The quick brown fox", "fox FOX fox", "", "naïve café Ünïcödé ſtraße", "a_b c-d e.f 12 3.5",
            "only, punctuation!!!", "重复 重复 词", "x" * 150]
    rec("insert", lambda: c.insert("w", [{"id": 100 + i, "text": t, "tag": "a"} for i, t in enumerate(docs)])["insert_count"])
    rec("query", lambda: c.query("w", "id >= 100", output_fields=["text", "sp"]))
    rec("query_star", lambda: c.query("w", "id == 101", output_fields=["*"]))
    rec("upsert_partial_no_text", lambda: c.upsert("w", [{"id": 101, "tag": "b"}], partial_update=True)["upsert_count"])
    rec("upsert_partial_text", lambda: c.upsert("w", [{"id": 102, "text": "new words new"}], partial_update=True)["upsert_count"])
    rec("raw/upsert_no_text_column", lambda: raw_insert(c, "w", text=None, upsert=True, partial=True, ids=[100]))
    rec("upsert_full", lambda: c.upsert("w", [{"id": 103, "text": "brand new"}])["upsert_count"])
    rec("query_after_upserts", lambda: c.query("w", "id >= 100", output_fields=["text", "tag", "sp"]))
    rec("flush", lambda: c.flush("w"))
    rec("upsert_after_flush", lambda: c.upsert("w", [{"id": 104, "tag": "z"}], partial_update=True)["upsert_count"])
    rec("query_after_flush", lambda: c.query("w", "id >= 100", output_fields=["text", "tag", "sp"]))

    rec("create/nullable", lambda: c.create_collection("n", schema=writes_schema(nullable=True)))
    rec("insert/nullable", lambda: c.insert("n", [{"id": 1, "text": None}, {"id": 2, "text": "x y"}, {"id": 3}])["insert_count"])
    rec("query/nullable", lambda: c.query("n", "id >= 0", output_fields=["text", "sp"]))
    rec("create/default", lambda: c.create_collection("d", schema=writes_schema(default="dflt word")))
    rec("insert/default", lambda: c.insert("d", [{"id": 1}, {"id": 2, "text": "word"}])["insert_count"])
    rec("upsert/default_missing", lambda: c.upsert("d", [{"id": 5}])["upsert_count"])
    rec("query/default", lambda: c.query("d", "id >= 0", output_fields=["text", "sp"]))


@scenario
def stop_words_and_two_functions(c, rec):
    fields = [field("id", 5, is_primary_key=True),
              text_field(extra={"analyzer_params": json.dumps({"filter": [{"type": "stop", "stop_words": ["the", "1", "and"]}]})}),
              field("sp", 104), field("sp2", 104)]
    rec("create", lambda: create(c, "s", fields, [fn(), fn(name="b2", outputs=("sp2",))]))
    rec("insert", lambda: c.insert("s", [{"id": 1, "text": "The cat and THE dog 1 10"}, {"id": 2, "text": "the and 1"}])["insert_count"])
    rec("query", lambda: c.query("s", "id >= 1", output_fields=["sp", "sp2"]))


def kvs(params):
    return [common_pb2.KeyValuePair(key=k, value=v) for k, v in params.items()]


def raw_index(c, name, metric="BM25", index_type="SPARSE_INVERTED_INDEX", params=None, search_params=None, keep=False):
    """原始 CreateIndex：`params` 原样作为 JSON 文本发出（pymilvus 会把它拆成独立的 kv）。成功后默认删掉，下一条从无索引开始。"""
    kv = {"index_type": index_type, "metric_type": metric}
    if params is not None:
        kv["params"] = params
    if search_params is not None:
        kv["search_params"] = search_params
    r = stub(c).CreateIndex(milvus_pb2.CreateIndexRequest(collection_name=name, field_name="sp", extra_params=kvs(kv)))
    if r.code == 0 and not keep:
        c.release_collection(name)
        return status(r), status(stub(c).DropIndex(milvus_pb2.DropIndexRequest(collection_name=name, field_name="sp")))
    return status(r)


def search_request(name, blobs, ph_type=21, field="sp", extra=None):
    ph = common_pb2.PlaceholderGroup(placeholders=[common_pb2.PlaceholderValue(tag="$0", type=ph_type, values=blobs)])
    params = {"topk": "10", "anns_field": field}
    params.update(extra or {})
    return milvus_pb2.SearchRequest(collection_name=name, dsl_type=1, placeholder_group=ph.SerializeToString(),
                                    search_params=kvs(params))


SPARSE_ONE = b"\x01\x00\x00\x00\x00\x00\x80\x3f"


def raw_text_search(c, name, blobs, **kw):
    """只用于报错的请求：文本检索的结果随第 3 步的打分一起比较。"""
    return status(stub(c).Search(search_request(name, blobs, **kw)))


def raw_hybrid(c, name, blobs, **kw):
    r = stub(c).HybridSearch(milvus_pb2.HybridSearchRequest(
        collection_name=name, requests=[search_request(name, blobs, **kw)],
        rank_params=kvs({"strategy": "rrf", "limit": "10"})))
    return status(r)


@scenario
def index_metric_and_queries(c, rec):
    """0031 第 2 步：BM25 输出字段的索引定义、度量与 k1 / b 校验、文本 placeholder 的解码与查询种类。"""
    rec("create", lambda: create(c, "b", base(), [fn()]))
    rec("create/plain", lambda: create(c, "p", [field("id", 5, is_primary_key=True), field("sp", 104)]))
    rec("create/dense", lambda: create(c, "d", [field("id", 5, is_primary_key=True),
                                                 field("v", 101, params={"dim": "2"})]))
    for metric, index_type in (("IP", "SPARSE_INVERTED_INDEX"), ("BM25", "AUTOINDEX"), ("bm25", "sparse_inverted_index"),
                               ("L2", "SPARSE_INVERTED_INDEX")):
        rec(f"index/{index_type}/{metric}", lambda: raw_index(c, "b", metric, index_type))
    for params in ('{"bm25_k1": "2"}', '{"bm25_k1": 1}', '{"bm25_k1": 0}', '{"bm25_k1": -1}', '{"bm25_k1": true}',
                   '{"bm25_k1": null}', '{"bm25_k1": [1]}', '{"bm25_k1": "x"}', '{"bm25_k1": " 3 "}', '{"bm25_k1": "1_0"}',
                   '{"bm25_k1": 1e308}', '{"bm25_k1": 1e999}', '{"bm25_k1": "inf"}', '{"bm25_k1": "nan"}',
                   '{"bm25_k1": %s}' % ("1" + "0" * 400), '{"bm25_b": -0.0}', '{"bm25_b": 1}', '{"bm25_b": "0.5"}',
                   '{"bm25_b": 2}', '{"bm25_b": false}', '{"bm25_b": -1e-300}', '{"bm25_b": 0.5, "bm25_k1": 0}',
                   '{"bm25_k1": 0, "drop_ratio_build": 1}', '{"drop_ratio_build": 0.1}', '{"drop_ratio_build": 1e999}',
                   '{"inverted_index_algo": "DAAT"}', '{"inverted_index_algo": 1e999}', '{"bm25_k1": NaN}',
                   '{"bm25_k1": Infinity}', '{"bm25_k1": -Infinity}', "not json"):
        rec(f"index/params {params[:40]}", lambda: raw_index(c, "b", params=params))
    rec("index/search_params", lambda: raw_index(c, "b", search_params='{"bm25_b": 2}'))
    rec("index/plain/BM25", lambda: raw_index(c, "p", "BM25"))
    rec("index/plain/k1", lambda: raw_index(c, "p", "IP", params='{"bm25_k1": 1.2}'))
    rec("index/plain/k1_huge", lambda: raw_index(c, "p", "IP", params='{"bm25_k1": 1e999}'))
    rec("index/final", lambda: raw_index(c, "b", "bm25", params='{"bm25_k1": 1.2, "bm25_b": 0.5}', keep=True))
    rec("index/describe", lambda: c.describe_index("b", "sp"))
    rec("index/plain/final", lambda: raw_index(c, "p", "IP", keep=True))
    for name in ("b", "p", "d"):
        rec(f"load/{name}", lambda: c.load_collection(name))

    rec("insert", lambda: raw_insert(c, "b", text=("fox",)))
    for metric in ("IP", "ip", "", "L2", "COSINE"):
        rec(f"search/metric {metric!r}", lambda: raw_text_search(c, "b", [b"fox"], extra={"metric_type": metric}))
    for params in ('{"bm25_b": 2}', '{"bm25_k1": true}', '{"bm25_k1": 1e999}', '{"drop_ratio_search": 0.5}',
                   '{"bm25_k1": NaN}', '{"bm25_k1": Infinity}', '{"bm25_k1": -Infinity}'):
        rec(f"search/params {params}", lambda: raw_text_search(c, "b", [SPARSE_ONE], ph_type=104, extra={"params": params}))
    rec("search/sparse_query", lambda: raw_text_search(c, "b", [SPARSE_ONE], ph_type=104))
    rec("search/dense_query", lambda: raw_text_search(c, "b", [b"\x00\x00\x80\x3f"], ph_type=101))
    for blobs in ([b"\xff"], [b"ok", b"\xe2\x82"], [b"\xe2\x82\x28"], [b"\xed\xa0\x80"], [b"a\xf0\x9f\x98"]):
        rec(f"search/utf8 {blobs!r}", lambda: raw_text_search(c, "b", blobs))
        rec(f"hybrid/utf8 {blobs!r}", lambda: raw_hybrid(c, "b", blobs))
    rec("hybrid/metric IP", lambda: raw_hybrid(c, "b", [b"fox"], extra={"metric_type": "IP"}))
    rec("hybrid/sparse_query", lambda: raw_hybrid(c, "b", [b""], ph_type=104))
    rec("plain/text", lambda: raw_text_search(c, "p", [b"fox"]))
    rec("plain/metric BM25", lambda: raw_text_search(c, "p", [b"fox"], extra={"metric_type": "BM25"}))
    rec("plain/k1", lambda: raw_text_search(c, "p", [b""], ph_type=104, extra={"params": '{"bm25_k1": 1}'}))
    rec("dense/metric BM25", lambda: raw_text_search(c, "d", [b"\x00\x00\x80\x3f" * 2], ph_type=101, field="v",
                                                      extra={"metric_type": "BM25"}))


def bm25_schema(extra_text=False):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("g", DataType.INT64)
    s.add_field("text", DataType.VARCHAR, max_length=500, enable_analyzer=True, nullable=True)
    s.add_field("sp", DataType.SPARSE_FLOAT_VECTOR)
    s.add_function(Function(name="bm", function_type=FunctionType.BM25, input_field_names=["text"], output_field_names=["sp"]))
    if extra_text:
        s.add_field("t2", DataType.VARCHAR, max_length=200, enable_analyzer=True)
        s.add_field("sp2", DataType.SPARSE_FLOAT_VECTOR)
        s.add_function(Function(name="bm2", function_type=FunctionType.BM25, input_field_names=["t2"],
                                output_field_names=["sp2"]))
    return s


def hits(res):
    """距离按 repr 逐位比较（float32 的对外距离转成 Python float 后是精确的）。"""
    return [[(h["id"], h["distance"]) for h in q] for q in res]


WORDS = ["fox", "dog", "the", "cat", "a", "b", "c", "quick", "brown", "lazy", "x1", "重复", "naïve", "y", "z"]


@scenario
def scoring_random(c, rec):
    """随机写入（含 upsert、删除、空文本与 null）、随机 flush（多 segment、一个 segment 多个块）、随机 k1 / b
    与过滤，逐位比较 MemTable 与 segment 上的分数与次序。不触发 compaction（每轮最多 3 次 flush）。"""
    rng = random.Random(31)
    for r in range(30):
        name = f"r{r}"
        rec(f"{name}/create", lambda: c.create_collection(name, schema=bm25_schema()))
        if rng.random() < 0.7:
            params = {}
            if rng.random() < 0.7:
                params["bm25_k1"] = rng.choice([0.5, 1.2, 1.5, 2.0, rng.uniform(0.01, 5)])
            if rng.random() < 0.7:
                params["bm25_b"] = rng.choice([0.0, 0.75, 1.0, rng.random()])
            c.release_collection(name)
            rec(f"{name}/index", lambda: raw_index(c, name, params=json.dumps(params), keep=True))
            c.load_collection(name)
        nid = 0
        for batch in range(rng.randint(1, 4)):
            rows = {}
            for _ in range(rng.randint(1, 30)):
                k = rng.random()
                text = None if k < 0.05 else ("" if k < 0.1 else " ".join(
                    rng.choice(WORDS) for _ in range(rng.randint(1, 12))))
                pk = nid if rng.random() < 0.85 or nid == 0 else rng.randrange(nid)
                nid += 1
                rows[pk] = {"id": pk, "g": rng.randrange(3), "text": text}
            rec(f"{name}/upsert{batch}", lambda: c.upsert(name, list(rows.values()))["upsert_count"])
            if rng.random() < 0.3:
                ids = [rng.randrange(nid) for _ in range(3)]
                rec(f"{name}/delete{batch}", lambda: c.delete(name, filter=f"id in {ids}") and None)
            if batch < 3 and rng.random() < 0.6:
                rec(f"{name}/flush{batch}", lambda: c.flush(name))
        for i in range(4):
            q = [" ".join(rng.choice(WORDS) for _ in range(rng.randint(0, 5))) for _ in range(rng.randint(1, 3))]
            kw = {"limit": rng.randint(1, 15)}
            if rng.random() < 0.4:
                kw["filter"] = f"g == {rng.randrange(3)}"
            rec(f"{name}/search{i}", lambda: hits(c.search(name, data=q, anns_field="sp", **kw)))
        c.drop_collection(name)


@scenario
def search_paths(c, rec):
    """BM25 在 offset、range、round_decimal、分组、boost、L2 rerank 链、HybridSearch 与 search iterator 中的结果。"""
    docs = ["fox dog", "fox", "dog dog cat", "the fox jumps over the dog", "cat", "fox fox fox", "quick brown fox",
            "lazy dog", "a b c", "fox cat dog"]
    rec("create", lambda: c.create_collection("c", schema=bm25_schema(extra_text=True)))
    rec("insert", lambda: c.insert("c", [{"id": i, "g": i % 3, "text": docs[i % 10], "t2": docs[i * 3 % 10]}
                                         for i in range(20)])["insert_count"])
    rec("flush", lambda: c.flush("c"))
    rec("insert_memtable", lambda: c.insert("c", [{"id": 100 + i, "g": i % 3, "text": docs[(i + 4) % 10],
                                                   "t2": docs[i % 10]} for i in range(8)])["insert_count"])
    q = ["fox dog", "cat"]
    search = lambda **kw: hits(c.search("c", data=q, anns_field="sp", **kw))  # noqa: E731
    rec("plain", lambda: search(limit=6))
    rec("offset", lambda: search(limit=4, offset=3))
    rec("range", lambda: search(limit=10, search_params={"params": {"radius": -0.3, "range_filter": -3}}))
    rec("range_reversed", lambda: search(limit=10, search_params={"params": {"radius": -3, "range_filter": -0.3}}))
    rec("round_decimal", lambda: search(limit=5, round_decimal=2))
    rec("group", lambda: search(limit=3, group_by_field="g", group_size=2))
    boost = Function("b", FunctionType.RERANK, [], params={"reranker": "boost", "weight": 2.0, "filter": "g == 1"})
    boost_sum = Function("b", FunctionType.RERANK, [], params={"reranker": "boost", "weight": 0.5, "filter": "g == 2",
                                                               "boost_mode": "sum"})
    rrf = Function("r", FunctionType.RERANK, [], params={"reranker": "rrf", "k": 60})
    weighted = Function("w", FunctionType.RERANK, [], params={"reranker": "weighted", "weights": [1.0],
                                                              "norm_score": True})
    decay = Function("d", FunctionType.RERANK, ["g"], params={"reranker": "decay", "function": "gauss", "origin": 0,
                                                              "scale": 2})
    for name, functions in (("boost", [boost]), ("boost_sum", [boost_sum]), ("rrf", [rrf]),
                            ("weighted_norm", [weighted]), ("decay", [decay]), ("boost_rrf", [boost, rrf])):
        rec(f"ranker/{name}", lambda: search(limit=6, ranker=FunctionScore(functions)))
    reqs = [AnnSearchRequest(data=q, anns_field="sp", param={}, limit=8),
            AnnSearchRequest(data=q, anns_field="sp2", param={}, limit=8)]
    rec("hybrid/rrf", lambda: hits(c.hybrid_search("c", reqs, RRFRanker(), limit=6)))
    rec("hybrid/weighted", lambda: hits(c.hybrid_search("c", reqs, WeightedRanker(0.3, 0.7), limit=6)))
    rec("hybrid/weighted_norm", lambda: hits(c.hybrid_search("c", reqs, WeightedRanker(0.3, 0.7, norm_score=True),
                                                             limit=6)))
    rec("hybrid/group", lambda: hits(c.hybrid_search("c", reqs, RRFRanker(), limit=3, group_by_field="g")))

    def iterate():
        it = c.search_iterator("c", data=["fox"], anns_field="sp", batch_size=2, limit=12,
                               search_params={"metric_type": "BM25"})
        out = []
        while batch := it.next():
            out.extend((h["id"], h["distance"]) for h in batch)
        it.close()
        return out
    rec("iterator", iterate)
    rec("empty_and_stop_words", lambda: hits(c.search("c", data=["", "the"], anns_field="sp", limit=3)))


@scenario
def partitions(c, rec):
    """带分区的检索：segment 的统计属于各自的 segment，MemTable 只统计所选分区里存活且通过过滤的行。"""
    rec("create", lambda: c.create_collection("pt", schema=bm25_schema()))
    for name in ("p1", "p2"):
        rec(f"partition/{name}", lambda: c.create_partition("pt", name))
    docs = ["fox dog", "fox", "dog dog cat", "the fox", "cat fox fox"]
    rec("insert/p1", lambda: c.insert("pt", [{"id": i, "g": i % 2, "text": docs[i % 5]} for i in range(6)],
                                      partition_name="p1")["insert_count"])
    rec("insert/p2", lambda: c.insert("pt", [{"id": 10 + i, "g": i % 2, "text": docs[(i + 2) % 5]} for i in range(4)],
                                      partition_name="p2")["insert_count"])
    q = ["fox dog", "cat"]
    for phase in ("memtable", "flushed", "mixed"):
        if phase == "flushed":
            rec("flush", lambda: c.flush("pt"))
        if phase == "mixed":
            rec("insert/mixed", lambda: c.insert("pt", [{"id": 20 + i, "g": 1, "text": docs[i]} for i in range(3)],
                                                 partition_name="p2")["insert_count"])
        for parts in (None, ["p1"], ["p2"], ["p1", "p2"]):
            for flt in ("", "g == 1"):
                rec(f"{phase}/{parts}/{flt}", lambda: hits(c.search("pt", data=q, anns_field="sp", limit=10, filter=flt,
                                                                    partition_names=parts)))


REOPEN_DOCS = ["fox dog", "fox", "dog dog cat", "the quick brown fox", "lazy dog", "cat cat fox"]


def reopened(c, rec):
    rec("describe", lambda: describe(c, "p"))
    rec("query", lambda: c.query("w", "id >= 100", output_fields=["text", "tag", "sp"]))
    # 重开后 segment 从 Parquet 读回，WAL 中的行回放进 MemTable，统计口径不变。
    c.load_collection("s")
    rec("search", lambda: hits(c.search("s", data=["fox dog", "cat"], anns_field="sp", limit=10)))
    rec("search_filter", lambda: hits(c.search("s", data=["fox dog"], anns_field="sp", limit=10, filter="g == 1")))


def before_reopen(c, rec):
    function_params(c, lambda name, fn: rec(f"params/{name}", fn))
    writes_and_reads(c, lambda name, fn: rec(f"writes/{name}", fn))
    rec("scores/create", lambda: c.create_collection("s", schema=bm25_schema()))
    c.release_collection("s")
    rec("scores/index", lambda: raw_index(c, "s", params='{"bm25_k1": 1.2, "bm25_b": 0.6}', keep=True))
    c.load_collection("s")
    rec("scores/insert", lambda: c.insert("s", [{"id": i, "g": i % 2, "text": t}
                                               for i, t in enumerate(REOPEN_DOCS)])["insert_count"])
    rec("scores/flush", lambda: c.flush("s"))
    rec("scores/wal", lambda: c.insert("s", [{"id": 100 + i, "g": i % 2, "text": t}
                                            for i, t in enumerate(REOPEN_DOCS[:3])])["insert_count"])
    rec("scores/search", lambda: hits(c.search("s", data=["fox dog", "cat"], anns_field="sp", limit=10)))
    rec("scores/search_filter", lambda: hits(c.search("s", data=["fox dog"], anns_field="sp", limit=10,
                                                      filter="g == 1")))


# 0031「与 3.x 的差异」：jieba 分词器；0029 已登记的非严格 JSON 同样适用于函数参数。两边的结果都逐项断言。
_UNREPRESENTABLE = ("rpc", "UNIMPLEMENTED", "MilvusLite does not support function params that strict JSON cannot "
                    "represent: not supported in this build of milvus-lite-rust yet")
REGISTERED = {
    ("analyzer_params", 'create/{"tokenizer": "jieba"}'): {
        "rust": ("rpc", "UNIMPLEMENTED", "MilvusLite does not support jieba tokenizer: the jieba tokenizer is not "
                 "supported in this build of milvus-lite-rust"),
        "ref": ("ok", [1, "JiebaAnalyzer requires the 'jieba' package. Install it with: pip install jieba", False]),
    },
    **{("function_params", f"create/{name}"): {"rust": _UNREPRESENTABLE, "ref": ("ok", [0, "", True])}
       for name in ("nan", "inf", "huge")},
    **{("before_reopen", f"params/create/{name}"): {"rust": _UNREPRESENTABLE, "ref": ("ok", [0, "", True])}
       for name in ("nan", "inf", "huge")},
    # 0030 / 0031：调参项的 `NaN` / `Infinity` / `-Infinity` 字面量不是严格 JSON，解码时 UNIMPLEMENTED；3.x 读成浮点后报 invalid。
    # 同样溢出的 `1e999` 是严格 JSON，与 3.x 一致地报 invalid（不在此列）。
    **{("index_metric_and_queries", f"index/params {params}"): {
        "rust": ("rpc", "UNIMPLEMENTED", "MilvusLite does not support index params that are not strict JSON: "
                 "not supported in this build of milvus-lite-rust yet"),
        "ref": ("ok", [6, "field 'sp': invalid bm25_k1"])} for params in ('{"bm25_k1": NaN}', '{"bm25_k1": Infinity}', '{"bm25_k1": -Infinity}')},
    **{("index_metric_and_queries", f"search/params {params}"): {
        "rust": ("rpc", "UNIMPLEMENTED", "MilvusLite does not support search params that are not strict JSON: "
                 "not supported in this build of milvus-lite-rust yet"),
        "ref": ("ok", [6, "field 'sp': invalid bm25_k1"])} for params in ('{"bm25_k1": NaN}', '{"bm25_k1": Infinity}', '{"bm25_k1": -Infinity}')},
}
