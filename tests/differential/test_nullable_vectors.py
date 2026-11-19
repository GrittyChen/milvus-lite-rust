"""0038：nullable 稠密向量对照 3.x（1320d26）。

一致的部分逐步对照：建表、Describe、`schema.json`、count、有效行的输出与检索、各项报错。
登记的差异分别断言两边（见 0038「与 3.x 的差异」）：3.x 写入时把空向量换成零向量，读出 `[0, 0]`、
检索时当零向量参与；本实现存为空值，读出 None、检索排除。整批为空时 3.x 报错，本实现接受。
"""

from pathlib import Path

import pytest
from pymilvus import AnnSearchRequest, DataType, MilvusClient, RRFRanker

import compare
from compare import run
from test_reopen import pair  # noqa: F401  （fixture）

ROWS = [
    {"id": 0, "v": [1.0, 0.0], "g": 0},
    {"id": 1, "v": None, "g": 1},
    {"id": 2, "g": 2},
    {"id": 3, "v": [0.0, 0.0], "g": 3},
    {"id": 4, "v": [0.0, 1.0], "g": 4},
    {"id": 5, "v": [-1.0, 0.5], "g": 5},
]
VALID = "id in [0, 3, 4, 5]"
VECTORS = {row["id"]: row["v"] for row in ROWS if row.get("v") is not None}
QUERIES = [[1.0, 0.0], [0.3, 0.7]]
NULLS = [1, 2]


def collection(client, name, metric="L2", vectors=(("v", True),)):
    schema = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    for field, nullable in vectors:
        schema.add_field(field, DataType.FLOAT_VECTOR, dim=2, nullable=nullable)
    schema.add_field("g", DataType.INT64)
    params = client.prepare_index_params()
    for field, _ in vectors:
        params.add_index(field_name=field, index_type="FLAT", metric_type=metric)
    client.create_collection(name, schema=schema, index_params=params)
    client.load_collection(name)


def query(client, name, expr="id >= 0", field="v"):
    return sorted((r["id"], r[field]) for r in client.query(name, filter=expr, output_fields=[field]))


def hits(results):
    return [[(h["id"], round(h["distance"], 5), h["entity"].get("v")) for h in q] for q in results]


@pytest.mark.parametrize("metric", ["L2", "IP", "COSINE"])
def test_valid_rows_match_3x_and_nulls_differ_as_registered(pair, metric):
    d = pair.open()
    d.step("create", lambda c: collection(c, "c", metric))
    d.step("not nullable", lambda c: collection(c, "nn", metric, vectors=(("v", False),)))
    d.step("describe", lambda c: [(f["name"], f.get("nullable", False)) for f in c.describe_collection("c")["fields"]])
    d.step("insert", lambda c: c.insert("c", ROWS)["insert_count"])

    def stages(d):
        d.step("count", lambda c: c.query("c", filter="", output_fields=["count(*)"]))
        d.step("valid rows", lambda c: query(c, "c", VALID))
        d.step("get valid", lambda c: list(c.get("c", ids=[4, 0, 5], output_fields=["v"])))
        # 有效行之间的检索：过滤到有效行时两边一致。
        params = {"metric_type": metric}
        d.step("search valid", lambda c: c.search("c", QUERIES, limit=10, filter=VALID, output_fields=["v"],
                                                 search_params=params),
               check=compare.hits(metric, QUERIES, VECTORS, k=len(VECTORS)))
        d.step("hybrid valid", lambda c: hits(c.hybrid_search("c", [
            AnnSearchRequest([[1.0, 0.0]], "v", params, limit=10, expr=VALID),
            AnnSearchRequest([[0.0, 1.0]], "v", params, limit=10, expr=VALID)],
            RRFRanker(), limit=10, output_fields=["v"])))
        # 登记的差异：空行的输出与检索。
        rust, ref = run(lambda c: query(c, "c", f"id in {NULLS}"), d.rust), run(lambda c: query(c, "c", f"id in {NULLS}"), d.ref)
        assert rust == ("ok", [[1, None], [2, None]]), rust
        assert ref == ("ok", [[1, [0.0, 0.0]], [2, [0.0, 0.0]]]), ref

        def ranked(c):
            return sorted(h["id"] for h in c.search("c", [[1.0, 0.0]], limit=10, search_params=params)[0])
        assert run(ranked, d.rust) == ("ok", [0, 3, 4, 5])
        assert run(ranked, d.ref) == ("ok", [0, 1, 2, 3, 4, 5])

    stages(d)
    d.step("flush", lambda c: c.flush("c"))
    stages(d)
    pair.close()
    d = pair.open()
    d.step("load", lambda c: c.load_collection("c"))
    stages(d)
    files = [(Path(root) / "collections/c/schema.json").read_bytes() for root in (pair.rust_path, pair.ref_path)]
    assert files[0] == files[1], files
    assert b'"nullable": true' in files[0]


def test_errors_match_3x_and_all_null_batches_differ(pair):
    d = pair.open()
    d.step("create", lambda c: collection(c, "c"))
    d.step("not nullable", lambda c: collection(c, "nn", vectors=(("v", False),)))
    d.step("none into not nullable", lambda c: c.insert("nn", [{"id": 1, "v": None, "g": 0}]), error=True)
    d.step("missing from not nullable", lambda c: c.insert("nn", [{"id": 1, "g": 0}]), error=True)
    d.step("wrong dim", lambda c: c.insert("c", [{"id": 1, "v": [1.0], "g": 0}, {"id": 2, "v": None, "g": 0}]), error=True)
    d.step("vector filter", lambda c: c.query("c", filter="v is null", output_fields=["id"]), error=True)
    d.step("mixed batch", lambda c: c.insert("c", [{"id": 1, "v": None, "g": 0}, {"id": 2, "v": [1.0, 1.0], "g": 0}])["insert_count"])
    d.step("partial upsert keeps the vector", lambda c: (
        c.upsert("c", [{"id": 2, "g": 5}], partial_update=True), query(c, "c", "id == 2"))[1])

    # 登记的差异：整批为空时 pymilvus 不设置 float_vector，3.x 报错，本实现写入空值。
    def all_null(c):
        c.insert("c", [{"id": 10, "v": None, "g": 0}, {"id": 11, "g": 0}])
        return query(c, "c", "id >= 10")
    assert run(all_null, d.rust) == ("ok", [[10, None], [11, None]])
    ref = run(all_null, d.ref)
    assert ref[0] == "err" and ref[-1] == "FieldData 'v' declared FloatVector but no float_vector data is set", ref


@pytest.mark.parametrize("layout", [(True, False), (False, True), (True, True)])
def test_multi_vector_layouts_match_3x_on_valid_rows(pair, layout):
    d = pair.open()
    d.step("create", lambda c: collection(c, "c", vectors=(("a", layout[0]), ("b", layout[1]))))
    rows = [{"id": i, "g": i, "a": [1.0, float(i)], "b": [float(i), 1.0]} for i in range(4)]
    for row in rows:
        for field, nullable in zip("ab", layout):
            if nullable and row["id"] % 2:
                row[field] = None
    d.step("insert", lambda c: c.insert("c", rows)["insert_count"])
    d.step("flush", lambda c: c.flush("c"))
    for field, nullable in zip("ab", layout):
        valid = "id in [0, 2]" if nullable else "id >= 0"
        d.step(f"{field} valid rows", lambda c, f=field, v=valid: query(c, "c", v, f))
        d.step(f"{field} search valid", lambda c, f=field, v=valid: [
            [(h["id"], round(h["distance"], 5)) for h in q]
            for q in c.search("c", [[1.0, 1.0]], anns_field=f, limit=10, filter=v)])
