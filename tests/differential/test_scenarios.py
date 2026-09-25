"""端到端差分场景（0004「验收」第 1 项）。

在 3.x README 的 Quick Start 基础上去掉过滤与删除，补上显式 schema、auto_id、IP 与 L2、VARCHAR 主键、
先建表后建索引、release 后再 load，以及常见的错误。约束：
- 每个 collection 累计写入少于 1 万行：3.x 满 1 万行才 flush，之前都在 memtable 上暴力检索；
- 维度不超过 16384，超过时 0004 的距离误差不成立；
- 0004「决策」已登记的差异不放进来，它们各有单独的用例；
- 3.x 写入失败也会消耗 auto_id（0004「决策」），所以出错的步骤不与比较 auto_id 的步骤共用 collection。
"""

import numpy as np
import pytest
from pymilvus import DataType, MilvusClient

from compare import hits


def index(metric, index_type="FLAT", field="vector"):
    params = MilvusClient.prepare_index_params()
    params.add_index(field_name=field, index_type=index_type, metric_type=metric)
    return params


def test_quick_start(diff):
    rows = [
        {"id": 1, "vector": [0.1, 0.2, 0.3, 0.4], "text": "python tutorial", "category": "tech"},
        {"id": 2, "vector": [0.2, 0.1, 0.4, 0.3], "text": "machine learning", "category": "ai"},
        {"id": 3, "vector": [0.9, 0.1, 0.1, 0.1], "text": "travel notes", "category": "life"},
    ]
    vectors = {r["id"]: r["vector"] for r in rows}
    q = [[0.1, 0.2, 0.3, 0.4]]
    diff.step("create", lambda c: c.create_collection(collection_name="docs", dimension=4))
    diff.step("describe", lambda c: c.describe_collection("docs"))
    diff.step("describe_index", lambda c: c.describe_index("docs", "vector"))
    diff.step("load_state", lambda c: c.get_load_state("docs"))
    diff.step("insert", lambda c: c.insert("docs", rows))
    diff.step("search", lambda c: c.search(collection_name="docs", data=q, limit=2, output_fields=["text", "category"]),
              hits("COSINE", q, vectors, k=2))
    diff.step("search_default_output", lambda c: c.search("docs", data=q, limit=3), hits("COSINE", q, vectors, k=3))
    diff.step("get", lambda c: c.get("docs", ids=[3, 1, 99], output_fields=["text"]))
    diff.step("get_all_fields", lambda c: c.get("docs", ids=[2]))
    diff.step("count", lambda c: c.query("docs", filter="", output_fields=["count(*)"]))
    diff.step("stats", lambda c: c.get_collection_stats("docs"))
    diff.step("list", lambda c: c.list_collections())
    diff.step("has", lambda c: c.has_collection("docs"))
    diff.step("drop", lambda c: c.drop_collection("docs"))
    diff.step("has_after_drop", lambda c: c.has_collection("docs"))


def explicit_schema(dynamic):
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=dynamic)
    s.add_field("pk", DataType.INT64, is_primary=True)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=3)
    s.add_field("t", DataType.VARCHAR, max_length=16)
    s.add_field("j", DataType.JSON)
    s.add_field("b", DataType.BOOL)
    s.add_field("i8", DataType.INT8)
    s.add_field("i16", DataType.INT16)
    s.add_field("i32", DataType.INT32)
    s.add_field("i64", DataType.INT64)
    s.add_field("f", DataType.FLOAT)
    s.add_field("d", DataType.DOUBLE)
    return s


def test_explicit_schema_and_dynamic_fields(diff):
    rng = np.random.default_rng(1)
    rows = [
        {
            "pk": i,
            "v": rng.standard_normal(3).astype(np.float32).tolist(),
            "t": f"t{i}",
            # JSON 字段的顶层标量与字符串是已登记的差异（0004「决策」），这里只用对象与数组。
            "j": [{"a": i, "b": [1, "x", None]}, [i, i + 0.5], {}, [], {"n": None, "s": "中"}][i % 5],
            "b": i % 2 == 0,
            "i8": i % 128 - 64,
            "i16": -i * 3,
            "i32": i * 1000,
            "i64": i * 10**12,
            "f": i / 3,
            "d": i / 7,
            **({"page": i, "tag": ["a", "b"][i % 2]} if i % 3 else {"nested": {"k": [i]}}),
        }
        for i in range(60)
    ]
    vectors = {r["pk"]: r["v"] for r in rows}
    q = rng.standard_normal((3, 3)).astype(np.float32).tolist()
    diff.step("create", lambda c: c.create_collection("ex", schema=explicit_schema(True), index_params=index("L2", field="v")))
    diff.step("describe", lambda c: c.describe_collection("ex"))
    diff.step("insert_a", lambda c: c.insert("ex", rows[:40]))
    diff.step("insert_b", lambda c: c.insert("ex", rows[40:]))
    # 同一主键以后写者为准，同一批内与跨批次都如此。
    overwrite = [dict(rows[5], t="new5", page=500), dict(rows[6], t="first6"), dict(rows[6], t="second6")]
    diff.step("overwrite", lambda c: c.insert("ex", overwrite))
    vectors.update({r["pk"]: r["v"] for r in overwrite})
    for name, fields in [("none", None), ("star", ["*"]), ("meta", ["$meta"]), ("keys", ["tag", "page", "t"]),
                         ("scalars", ["b", "i8", "i16", "i32", "i64", "f", "d", "j"])]:
        diff.step(f"search_{name}", lambda c: c.search("ex", data=q, limit=8, output_fields=fields),
                  hits("L2", q, vectors, k=8))
    for metric in ["IP", "COSINE"]:
        diff.step(f"search_request_metric_{metric}",
                  lambda c: c.search("ex", data=q, limit=5, search_params={"metric_type": metric}),
                  hits(metric, q, vectors, k=5))
    diff.step("search_offset", lambda c: c.search("ex", data=q, limit=5, offset=7),
              hits("L2", q, vectors, k=5, offset=7))
    diff.step("search_offset_beyond", lambda c: c.search("ex", data=q, limit=5, offset=100),
              hits("L2", q, vectors, k=0, offset=100))
    for rd in [0, 2, 5]:
        diff.step(f"search_round_{rd}", lambda c: c.search("ex", data=q, limit=6, round_decimal=rd),
                  hits("L2", q, vectors, k=6, round_decimal=rd))
    diff.step("get_star", lambda c: c.get("ex", ids=[0, 5, 6, 59, 1000], output_fields=["*"]))
    diff.step("get_keys", lambda c: c.get("ex", ids=[1, 3], output_fields=["page", "nested", "j"]))
    diff.step("get_duplicates", lambda c: c.get("ex", ids=[2, 2, 1]))
    diff.step("query_in", lambda c: c.query("ex", filter="pk in [4, 3]", output_fields=["t", "$meta"]))
    diff.step("count", lambda c: c.query("ex", filter="", output_fields=["count(*)"]))
    diff.step("stats", lambda c: c.get_collection_stats("ex"))
    diff.step("describe_index", lambda c: c.describe_index("ex", "v"))
    diff.step("list_indexes", lambda c: c.list_indexes("ex"))


def test_static_schema(diff):
    rng = np.random.default_rng(2)
    rows = [dict(pk=i, v=rng.random(3).astype(np.float32).tolist(), t="", j={"i": i}, b=False, i8=0, i16=0,
                 i32=0, i64=-i, f=0.0, d=-0.0) for i in range(20)]
    vectors = {r["pk"]: r["v"] for r in rows}
    q = [[0.5, 0.5, 0.5]]
    diff.step("create", lambda c: c.create_collection("st", schema=explicit_schema(False), index_params=index("IP", field="v")))
    diff.step("describe", lambda c: c.describe_collection("st"))
    diff.step("insert", lambda c: c.insert("st", rows))
    diff.step("search_star", lambda c: c.search("st", data=q, limit=4, output_fields=["*"]), hits("IP", q, vectors, k=4))
    diff.step("get_star", lambda c: c.get("st", ids=[3], output_fields=["*"]))


def test_auto_id(diff):
    s = MilvusClient.create_schema(auto_id=True, enable_dynamic_field=True)
    s.add_field("id", DataType.INT64, is_primary=True, auto_id=True)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("label", DataType.VARCHAR, max_length=16)
    diff.step("create", lambda c: c.create_collection("ai", schema=s, index_params=index("L2", field="v")))
    diff.step("describe", lambda c: c.describe_collection("ai"))
    batches = [[{"v": [float(i), float(b)], "label": f"b{b}i{i}", "extra": i} for i in range(n)]
               for b, n in enumerate([3, 1, 5])]
    ids = []
    for b, batch in enumerate(batches):
        ids += diff.step(f"insert_{b}", lambda c: c.insert("ai", batch))[1]["ids"]
    vectors = {pk: row["v"] for pk, row in zip(ids, [r for batch in batches for r in batch])}
    q = [[1.0, 1.0], [0.0, 2.0]]
    diff.step("search", lambda c: c.search("ai", data=q, limit=4, output_fields=["label", "extra"]),
              hits("L2", q, vectors, k=4))
    diff.step("get", lambda c: c.get("ai", ids=ids[::2]))
    diff.step("count", lambda c: c.query("ai", filter="", output_fields=["count(*)"]))


def test_varchar_primary_key(diff):
    rows = [{"id": k, "vector": v} for k, v in [("a", [1, 0]), ("b", [0, 1]), ("中文", [0.6, 0.8]),
                                                ("", [0.3, 0.3]), ("a", [0.5, 0.5])]]
    vectors = {r["id"]: r["vector"] for r in rows}
    q = [[1.0, 0.0], [0.2, 0.9]]
    diff.step("create", lambda c: c.create_collection("vc", dimension=2, id_type="string", max_length=16,
                                                      metric_type="IP"))
    diff.step("describe", lambda c: c.describe_collection("vc"))
    diff.step("insert", lambda c: c.insert("vc", rows))
    diff.step("get", lambda c: c.get("vc", ids=["a", "zz", "中文", ""]))
    # 主键 "a" 写了两次，只有 4 行。
    diff.step("search", lambda c: c.search("vc", data=q, limit=5), hits("IP", q, vectors, k=4))
    diff.step("count", lambda c: c.query("vc", filter="", output_fields=["count(*)"]))
    diff.step("insert_too_long", lambda c: c.insert("vc", [{"id": "x" * 17, "vector": [1, 1]}]), error=True)


def test_index_after_create_and_release_then_load(diff):
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=True)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    rows = [{"id": i, "v": [float(i % 5), float(i // 5)], "zz": i} for i in range(25)]
    vectors = {r["id"]: r["v"] for r in rows}
    q = [[1.0, 1.0]]
    diff.step("create", lambda c: c.create_collection("ni", schema=s))
    diff.step("state_no_index", lambda c: c.get_load_state("ni"))
    diff.step("describe_index_none", lambda c: c.describe_index("ni", "v"))
    diff.step("insert", lambda c: c.insert("ni", rows))
    diff.step("search_no_index", lambda c: c.search("ni", data=q, limit=5, output_fields=["zz"]),
              hits("COSINE", q, vectors, k=5))
    diff.step("release_no_index", lambda c: c.release_collection("ni"))
    diff.step("state_after_release_no_index", lambda c: c.get_load_state("ni"))
    diff.step("create_index", lambda c: c.create_index("ni", index("IP", "AUTOINDEX", field="v")))
    diff.step("state_after_index", lambda c: c.get_load_state("ni"))
    diff.step("describe_index", lambda c: c.describe_index("ni", "v"))
    diff.step("search_ip", lambda c: c.search("ni", data=q, limit=5), hits("IP", q, vectors, k=5))
    diff.step("release", lambda c: c.release_collection("ni"))
    diff.step("state_released", lambda c: c.get_load_state("ni"))
    diff.step("search_released", lambda c: c.search("ni", data=q, limit=5), error=True)
    diff.step("get_released", lambda c: c.get("ni", ids=[1]), error=True)
    diff.step("count_released", lambda c: c.query("ni", filter="", output_fields=["count(*)"]))
    diff.step("insert_released", lambda c: c.insert("ni", [{"id": 100, "v": [9.0, 9.0]}]))
    vectors[100] = [9.0, 9.0]
    diff.step("load", lambda c: c.load_collection("ni"))
    diff.step("state_loaded", lambda c: c.get_load_state("ni"))
    diff.step("search_after_load", lambda c: c.search("ni", data=q, limit=30), hits("IP", q, vectors, k=26))
    diff.step("create_index_again", lambda c: c.create_index("ni", index("IP", field="v")), error=True)


@pytest.mark.parametrize("metric", ["L2", "IP", "COSINE"])
@pytest.mark.parametrize("dim, n, dist", [(1536, 2000, "normal"), (1536, 2000, "uniform"), (16384, 300, "uniform")])
def test_distances_high_dim(diff, metric, dim, n, dist):
    rng = np.random.default_rng(dim + n)
    draw = rng.standard_normal if dist == "normal" else rng.random
    data = draw((n, dim)).astype(np.float32)
    queries = np.vstack([draw((4, dim)), data[3:4]]).astype(np.float32)
    vectors = {i: data[i] for i in range(n)}
    q = queries.tolist()
    name = f"d{dim}_{metric}"
    diff.step("create", lambda c: c.create_collection(name, dimension=dim, metric_type=metric))
    for start in range(0, n, 500):
        diff.step(f"insert_{start}", lambda c: c.insert(name, [{"id": i, "vector": data[i].tolist()}
                                                              for i in range(start, min(start + 500, n))]))
    diff.step("search_10", lambda c: c.search(name, data=q, limit=10), hits(metric, queries, vectors, k=10))
    diff.step("search_100_offset", lambda c: c.search(name, data=q, limit=100, offset=20),
              hits(metric, queries, vectors, k=100, offset=20))
    diff.step("search_round", lambda c: c.search(name, data=q[:2], limit=10, round_decimal=4),
              hits(metric, queries, vectors, k=10, round_decimal=4))


@pytest.mark.parametrize("metric", ["IP", "COSINE"])
def test_near_zero_similarity(diff, metric):
    """命中结果的 IP / COSINE 都接近 0：正负项相消，|b| 远小于 ‖q‖·‖v‖，检验 0004 对 IP 按模长定的误差。"""
    dim, n = 1536, 300
    rng = np.random.default_rng(7)
    q = rng.standard_normal(dim)
    base = rng.standard_normal((n, dim))
    # 去掉每行在 q 上的分量，再留 1% 的原始分量：相似度只剩原来的约 1%。
    data = (base - np.outer(base @ q / (q @ q), q) + 0.01 * base).astype(np.float32)
    queries = q[None, :].astype(np.float32)
    vectors = {i: data[i] for i in range(n)}
    name = f"z_{metric}"
    diff.step("create", lambda c: c.create_collection(name, dimension=dim, metric_type=metric))
    diff.step("insert", lambda c: c.insert(name, [{"id": i, "vector": data[i].tolist()} for i in range(n)]))
    diff.step("search", lambda c: c.search(name, data=queries.tolist(), limit=50),
              hits(metric, queries, vectors, k=50))


def test_errors(diff):
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("t", DataType.VARCHAR, max_length=4)
    s.add_field("n", DataType.INT32)
    good = {"id": 1, "v": [1.0, 0.0], "t": "ok", "n": 1}
    diff.step("describe_missing", lambda c: c.describe_collection("nope"), error=True)
    diff.step("has_missing", lambda c: c.has_collection("nope"))
    diff.step("drop_missing", lambda c: c.drop_collection("nope"))
    diff.step("insert_missing", lambda c: c.insert("nope", [good]), error=True)
    diff.step("search_missing", lambda c: c.search("nope", data=[[1.0, 0.0]], limit=1), error=True)
    diff.step("get_missing", lambda c: c.get("nope", ids=[1]), error=True)
    diff.step("count_missing", lambda c: c.query("nope", filter="", output_fields=["count(*)"]), error=True)
    diff.step("stats_missing", lambda c: c.get_collection_stats("nope"), error=True)
    diff.step("load_missing", lambda c: c.load_collection("nope"), error=True)
    diff.step("release_missing", lambda c: c.release_collection("nope"), error=True)
    diff.step("state_missing", lambda c: c.get_load_state("nope"))
    diff.step("index_missing", lambda c: c.create_index("nope", index("L2", field="v")), error=True)
    diff.step("describe_index_missing", lambda c: c.describe_index("nope", "v"), error=True)

    diff.step("create", lambda c: c.create_collection("er", schema=s))
    diff.step("create_duplicate", lambda c: c.create_collection("er", schema=s), error=True)
    diff.step("create_quick_duplicate", lambda c: c.create_collection("er", dimension=2), error=True)
    vc_auto = MilvusClient.create_schema(auto_id=True)
    vc_auto.add_field("id", DataType.VARCHAR, is_primary=True, max_length=8, auto_id=True)
    vc_auto.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    diff.step("create_varchar_auto_id", lambda c: c.create_collection("va", schema=vc_auto), error=True)
    no_vec = MilvusClient.create_schema()
    no_vec.add_field("id", DataType.INT64, is_primary=True)
    diff.step("create_without_vector", lambda c: c.create_collection("nv", schema=no_vec), error=True)

    diff.step("insert_good", lambda c: c.insert("er", [good]))
    diff.step("insert_wrong_dim", lambda c: c.insert("er", [dict(good, v=[1.0, 0.0, 0.0])]), error=True)
    diff.step("insert_too_long", lambda c: c.insert("er", [dict(good, id=4, t="abcde")]), error=True)
    # 以下四种错误由 pymilvus 在客户端按 DescribeCollection 返回的 schema 报出，请求不到达服务端；
    # 比较的是两边返回的 schema 让 pymilvus 做出同样的判断。服务端的对应检查另有 Rust 与 tests/python 的用例。
    diff.step("insert_missing_field", lambda c: c.insert("er", [{"id": 2, "v": [1.0, 0.0], "t": "x"}]),
              error=True)
    diff.step("insert_extra_field", lambda c: c.insert("er", [dict(good, id=3, zz=1)]), error=True)
    diff.step("insert_wrong_type", lambda c: c.insert("er", [dict(good, id=5, n="x")]), error=True)
    diff.step("insert_int32_overflow", lambda c: c.insert("er", [dict(good, id=6, n=2**31)]), error=True)
    diff.step("count", lambda c: c.query("er", filter="", output_fields=["count(*)"]))

    diff.step("search_wrong_anns_field", lambda c: c.search("er", data=[[1.0, 0.0]], anns_field="t", limit=1),
              error=True)
    diff.step("search_unknown_anns_field", lambda c: c.search("er", data=[[1.0, 0.0]], anns_field="zz", limit=1),
              error=True)
    diff.step("search_bad_metric", lambda c: c.search("er", data=[[1.0, 0.0]], limit=1,
                                                      search_params={"metric_type": "HAMMING"}), error=True)
    diff.step("search_unknown_output_field", lambda c: c.search("er", data=[[1.0, 0.0]], limit=1,
                                                                output_fields=["zz"]))
    diff.step("get_unknown_output_field", lambda c: c.get("er", ids=[1], output_fields=["zz"]))
    diff.step("describe_index_none", lambda c: c.describe_index("er", "v"))
    diff.step("index_bad_metric", lambda c: c.create_index("er", index("HAMMING", field="v")), error=True)
    diff.step("index_on_scalar", lambda c: c.create_index("er", index("L2", field="t")), error=True)
    diff.step("index", lambda c: c.create_index("er", index("L2", field="v")))
    diff.step("index_duplicate", lambda c: c.create_index("er", index("IP", field="v")), error=True)
    diff.step("describe_index_scalar", lambda c: c.describe_index("er", "t"))
