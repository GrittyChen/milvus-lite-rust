"""0014：索引定义与 DropIndex 的 RPC 语义；ANN 的检索结果按已登记的 FLAT 差异验证。"""

import pytest
from pymilvus import DataType, MilvusClient


def schema():
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("tag", DataType.VARCHAR, max_length=32)
    return s


def params(kind, field="vector"):
    p = MilvusClient.prepare_index_params()
    if field == "vector":
        p.add_index(
            field, index_type=kind, metric_type="L2", params={"M": 16, "nlist": 2}
        )
    else:
        p.add_index(field, index_type=kind)
    return p


@pytest.mark.parametrize("kind", ["HNSW", "HNSW_SQ", "IVF_FLAT", "IVF_SQ8"])
def test_create_describe_list_and_drop_definitions(diff, kind):
    diff.step("create", lambda c: c.create_collection("idx", schema=schema()))
    diff.step("vector", lambda c: c.create_index("idx", params(kind)))
    diff.step("scalar", lambda c: c.create_index("idx", params("INVERTED", "tag")))
    diff.step("describe-vector", lambda c: c.describe_index("idx", "vector"))
    diff.step("describe-scalar", lambda c: c.describe_index("idx", "tag"))
    diff.step("list", lambda c: c.list_indexes("idx"))
    diff.step("loaded-drop", lambda c: c.drop_index("idx", "vector_idx"), error=True)
    diff.step("release", lambda c: c.release_collection("idx"))
    diff.step("targeted-drop", lambda c: c.drop_index("idx", "tag_idx"))
    diff.step("remaining", lambda c: c.list_indexes("idx"))
    diff.step("state-with-vector", lambda c: c.get_load_state("idx"))
    diff.step(
        "drop-all-by-ordinary-field-name", lambda c: c.drop_index("idx", "vector")
    )
    diff.step("empty-list", lambda c: c.list_indexes("idx"))
    diff.step("auto-loaded", lambda c: c.get_load_state("idx"))
    diff.step("no-index", lambda c: c.drop_index("idx", "nope"), error=True)


@pytest.mark.parametrize("name", ["nope", "tag", "vector", ""])
def test_nonmatching_names_drop_all_definitions(diff, name):
    diff.step("create", lambda c: c.create_collection("idx", schema=schema()))
    diff.step("vector", lambda c: c.create_index("idx", params("HNSW")))
    diff.step("scalar", lambda c: c.create_index("idx", params("INVERTED", "tag")))
    diff.step("release", lambda c: c.release_collection("idx"))
    diff.step("drop", lambda c: c.drop_index("idx", name))
    diff.step("all-gone", lambda c: c.list_indexes("idx"))
    diff.step("auto-loaded", lambda c: c.get_load_state("idx"))
