"""读快照切片的结果合同，以真实 3.x v3.2.1（43d1257）作证。

受控并发由 Rust 测试固定；这里固定发布前后主键读取、投影、并列检索的对外顺序。
检索有意使用 exact：本场景返回全部存活主键，无并列截断，且距离均为精确的 0.0；
因此同时冻结两侧 repr 和非截断并列顺序，强于 0004 的通用距离误差口径。
若扩展到非精确距离或截断场景，须改用 compare.hits 并单独明确顺序断言。
"""

from pymilvus import DataType, MilvusClient

from test_scenarios import index


def test_read_results_across_overwrite_delete_reinsert_and_flush(diff):
    schema = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=True)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    schema.add_field("json", DataType.JSON)
    diff.step("create", lambda c: c.create_collection("snap", schema=schema, index_params=index("L2")))

    def row(pk, tag):
        return {"id": pk, "vector": [1.0, 0.0], "json": {"wide": 2**63}, "tag": tag}

    diff.step("insert", lambda c: c.insert("snap", [row(3, "old3"), row(1, "old1"), row(2, "old2")]))

    def check(label, order):
        got = diff.step(label + "_get", lambda c: c.get("snap", ids=[2, 999, 3, 2], output_fields=["*"]))[1]
        assert [r["id"] for r in got] == [2, 3, 2]
        diff.step(label + "_query", lambda c: c.query("snap", filter="id in [2, 999, 3, 2]", output_fields=["*"]))
        assert diff.step(label + "_count", lambda c: c.query("snap", output_fields=["count(*)"]))[1] == [{"count(*)": 3}]
        result = diff.step(label + "_search", lambda c: c.search(
            "snap", data=[[1.0, 0.0]], limit=3,
            search_params={"metric_type": "L2"}, output_fields=["*"],
        ))[1][0]
        assert [r["id"] for r in result] == order
        assert [r["distance"] for r in result] == [0.0, 0.0, 0.0]

    check("active", [3, 1, 2])
    diff.step("overwrite", lambda c: c.insert("snap", [row(3, "new3")]))
    diff.step("delete", lambda c: c.delete("snap", ids=[1]))
    diff.step("reinsert", lambda c: c.insert("snap", [row(1, "new1")]))
    check("modified", [3, 2, 1])
    diff.step("flush", lambda c: c.flush("snap"))
    check("committed", [3, 2, 1])
    diff.step("upsert", lambda c: c.upsert("snap", [{"id": 2, "tag": "new2"}], partial_update=True))
    check("mixed", [3, 1, 2])
