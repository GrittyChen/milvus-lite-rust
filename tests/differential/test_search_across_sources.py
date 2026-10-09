"""多数据源下覆盖、删除与发布前后的检索，以 3.x v3.2.1（43d1257）作证。"""

import numpy as np
import pytest
from pymilvus import DataType, MilvusClient

from compare import hits
from test_scenarios import index


@pytest.mark.parametrize("metric", ["L2", "IP", "COSINE"])
def test_topk_across_committed_and_active_sources_with_overwritten_positions(diff, metric):
    schema = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=17)
    schema.add_field("tag", DataType.INT64)
    diff.step("create", lambda c: c.create_collection("lazy_top", schema=schema, index_params=index(metric)))
    rng = np.random.default_rng(42)
    values = rng.integers(-8, 9, size=(320, 17)).astype(np.float32).tolist()
    # 重复向量触及并列；超过 64 个候选后触及已建立的筛选阈值。
    for row in range(64, 320, 7):
        values[row] = values[row % 64].copy()
    queries = values[:5]
    model = {}

    def insert(label, ids, start):
        rows = [{"id": pk, "vector": values[start + i], "tag": start + i} for i, pk in enumerate(ids)]
        diff.step(label, lambda c: c.insert("lazy_top", rows))
        model.update({row["id"]: row["vector"] for row in rows})

    def check(label):
        assert diff.step(label + "_count", lambda c: c.query("lazy_top", output_fields=["count(*)"]))[1] == [{"count(*)": len(model)}]
        # nq=4 走批量核，nq=5 同时覆盖末尾标量路径；分页仍使用完整排序键。
        for nq, limit, offset in [(1, 10, 0), (4, 10, 0), (5, 7, 3)]:
            diff.step(f"{label}_{nq}_{offset}", lambda c: c.search(
                "lazy_top", queries[:nq], limit=limit,
                search_params={"metric_type": metric}, offset=offset,
                output_fields=["tag"],
            ), hits(metric, queries[:nq], model, k=limit, offset=offset))

    insert("insert_first", list(range(128)), 0)
    diff.step("flush_first", lambda c: c.flush("lazy_top"))
    insert("insert_next", list(range(128, 256)), 128)
    insert("overwrite", list(range(32)), 256)
    removed = list(range(80, 96))
    diff.step("delete", lambda c: c.delete("lazy_top", ids=removed))
    for pk in removed:
        del model[pk]
    check("mixed")
    diff.step("flush_next", lambda c: c.flush("lazy_top"))
    check("committed")
