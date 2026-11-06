# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_hybrid_search.py，Apache-2.0 许可。
# 只保留当前支持范围内的用例（逐条状态见 INVENTORY.md），并删去随之不再使用的 import 与 helper。
import pytest

from pymilvus import (
    DataType,
    Function,
    FunctionType,
    MilvusClient,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _create_hybrid_l2_projection_collection(client, name):
    """Collection for testing hidden fields needed by top-level L2 rerank."""
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("text", DataType.VARCHAR, max_length=65535)
    schema.add_field("label", DataType.VARCHAR, max_length=64)
    schema.add_field("ts", DataType.INT64)
    schema.add_field("dense", DataType.FLOAT_VECTOR, dim=2)

    idx = client.prepare_index_params()
    idx.add_index(field_name="dense", index_type="BRUTE_FORCE",
                  metric_type="IP", params={})
    client.create_collection(name, schema=schema, index_params=idx)
    client.insert(name, [
        {"id": 1, "text": "bad document", "label": "a", "ts": 0,
         "dense": [1.0, 0.0]},
        {"id": 2, "text": "good document", "label": "b", "ts": 100,
         "dense": [0.9, 0.0]},
        {"id": 3, "text": "ordinary document", "label": "c", "ts": 100,
         "dense": [0.8, 0.0]},
    ])
    client.load_collection(name)
    return name


def test_search_top_level_decay_uses_hidden_input_field(milvus_client):
    """Search L2 decay reranker can use a non-output input field."""
    name = _create_hybrid_l2_projection_collection(
        milvus_client, "search_decay_hidden"
    )
    ranker = Function(
        name="decay_l2",
        function_type=FunctionType.RERANK,
        input_field_names=["ts"],
        output_field_names=[],
        params={
            "reranker": "decay",
            "function": "linear",
            "origin": 100,
            "scale": 1,
            "decay": 0.5,
        },
    )

    results = milvus_client.search(
        name,
        data=[[1.0, 0.0]],
        anns_field="dense",
        search_params={"metric_type": "IP", "params": {}},
        limit=3,
        output_fields=["label"],
        ranker=ranker,
    )

    expected_labels = {1: "a", 2: "b", 3: "c"}
    ids = [hit["id"] for hit in results[0]]
    distances = [hit["distance"] for hit in results[0]]

    assert ids[:2] == [2, 3]
    assert distances[0] == pytest.approx(0.9)
    assert distances[1] == pytest.approx(0.8)
    assert all("ts" not in hit["entity"] for hit in results[0])
    for hit in results[0]:
        assert hit["entity"]["label"] == expected_labels[hit["id"]]
    milvus_client.drop_collection(name)
