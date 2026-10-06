"""读快照切片的结果合同，以真实 3.x v3.2.1（43d1257）作证。

受控并发由 Rust 测试固定；这里固定发布前后主键读取、投影、并列检索的对外顺序。
检索有意使用 exact：本场景返回全部存活主键，无并列截断，且距离均为精确的 0.0；
因此同时冻结两侧 repr 和非截断并列顺序，强于 0004 的通用距离误差口径。
若扩展到非精确距离或截断场景，须改用 compare.hits 并单独明确顺序断言。
"""

from pymilvus import DataType, MilvusClient
import grpc
import json
from pathlib import Path
from pymilvus.grpc_gen import milvus_pb2, milvus_pb2_grpc

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


def test_small_batches_keep_source_order_and_json_bytes_across_publication(diff, rust_uri, ref_uri, workdir):
    schema = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=True)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    schema.add_field("json", DataType.JSON)
    diff.step("create", lambda c: c.create_collection("blocks", schema=schema, index_params=index("L2")))
    latest, active, segments = {}, {}, []

    def check(label):
        ordered = [r for segment in segments for r in segment if latest.get(r["id"]) == r]
        ordered += list(active.values())
        result = diff.step(label + "_search", lambda c: c.search(
            "blocks", data=[[1.0, 0.0]], limit=len(latest), search_params={"metric_type": "L2"},
            output_fields=["*"],
        ))[1][0]
        assert [r["id"] for r in result] == [r["id"] for r in ordered]
        assert [r["distance"] for r in result] == [0.0] * len(ordered)
        ids = list(latest) + [999, next(iter(latest))]
        diff.step(label + "_get", lambda c: c.get("blocks", ids=ids, output_fields=["*"]))
        assert diff.step(label + "_count", lambda c: c.query("blocks", output_fields=["count(*)"]))[1] == [{"count(*)": len(latest)}]
        pk = next(iter(latest))
        expected = json.dumps(latest[pk]["json"], ensure_ascii=False).encode()
        for uri in (rust_uri, ref_uri):
            with grpc.insecure_channel(uri.removeprefix("http://"), options=[("grpc.enable_http_proxy", 0)]) as channel:
                reply = milvus_pb2_grpc.MilvusServiceStub(channel).Query(milvus_pb2.QueryRequest(
                    collection_name="blocks", expr=f"id in [{pk}]", output_fields=["json"],
                ))
                assert reply.status.code == 0
                field = next(f for f in reply.fields_data if f.field_name == "json")
                assert list(field.scalars.json_data.data) == [expected]

    # 参照实现四个 data 文件即触发 compaction；本切片只验收发布，不混入该重排。
    for step in range(1, 61):
        pk = step % 17
        if step % 9 == 0:
            diff.step(f"delete_{step}", lambda c: c.delete("blocks", ids=[pk]))
            active.pop(pk, None)
            latest.pop(pk, None)
        else:
            row = {"id": pk, "vector": [1.0, 0.0], "json": {"unicode": "三", "wide": 2**63,
                    "float": 73.63284301757812}, "tag": str(step)}
            if step % 5 == 0:
                diff.step(f"upsert_{step}", lambda c: c.upsert("blocks", [row]))
            else:
                diff.step(f"insert_{step}", lambda c: c.insert("blocks", [row]))
            active[pk] = latest[pk] = row
        if step % 20 == 0:
            check(f"before_{step}")
            diff.step(f"flush_{step}", lambda c: c.flush("blocks"))
            segments.append(list(active.values()))
            active.clear()
            manifest = json.loads((Path(workdir) / "3x/collections/blocks/manifest.json").read_text())
            assert len(manifest["partitions"]["_default"]["data_files"]) == step // 20 < 4
            check(f"after_{step}")
