"""非 key 重复主键 Upsert：各行独立合并请求前状态。"""

import pytest
from pymilvus import DataType, MilvusClient

from test_reopen import pair
from test_scenarios import index


@pytest.mark.parametrize("varchar_pk", [False, True])
@pytest.mark.parametrize("state", ["active", "segment", "reopen", "partition"])
@pytest.mark.parametrize("partial", [False, True])
@pytest.mark.parametrize("existing", [False, True])
def test_duplicate_upsert_uses_request_base(pair, varchar_pk, state, partial, existing):
    d = pair.open()
    schema = MilvusClient.create_schema(enable_dynamic_field=True)
    schema.add_field("id", DataType.VARCHAR if varchar_pk else DataType.INT64,
                     is_primary=True, **({"max_length": 64} if varchar_pk else {}))
    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    pk = "同键" if varchar_pk else 1
    d.step("create", lambda c: c.create_collection("c", schema=schema, index_params=index("L2")))
    partition = "p" if state == "partition" else "_default"
    if state == "partition":
        d.step("create-partition", lambda c: c.create_partition("c", partition))
    if existing:
        d.step("base", lambda c: c.insert("c", [{"id": pk, "vector": [0., 1.], "x": 0, "y": 0}], partition_name=partition))
    if state != "active":
        d.step("flush-base", lambda c: c.flush("c"))
    if state == "reopen":
        pair.close()
        d = pair.open()
        d.step("load-base", lambda c: c.load_collection("c"))
    updates = [{"id": pk, "x": 1}, {"id": pk, "y": 2}]
    if not partial or not existing:
        for row in updates:
            row["vector"] = [1., 0.]
    d.step("upsert", lambda c: c.upsert("c", updates, partial_update=partial, partition_name=partition))
    expected = [{"id": pk, **({"x": 0} if existing else {}), "y": 2}]
    read = lambda c: c.get("c", ids=[pk], output_fields=["x", "y"])
    assert d.step("read", read) == ("ok", expected)
    assert d.step("count", lambda c: c.query("c", output_fields=["count(*)"])) == ("ok", [{"count(*)": int(bool(expected))}])
    d.step("flush-result", lambda c: c.flush("c"))
    pair.close()
    d = pair.open()
    d.step("load-result", lambda c: c.load_collection("c"))
    assert d.step("recovered", read) == ("ok", expected)
