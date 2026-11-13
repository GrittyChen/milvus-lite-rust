"""GEOMETRY 字段（0034 第 1 步）：经 pymilvus 的建表、写入校验、原文输出、默认值与重开。

与 3.x 的逐项对照在 tests/differential/test_geometry.py；这里固定用户可见的结果。
"""

import pytest
from milvus_lite.server_manager import ServerManager
from pymilvus import DataType, MilvusClient
from pymilvus.exceptions import MilvusException

SQUARE = "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))"


def create(client):
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    schema.add_field("geo", DataType.GEOMETRY, nullable=True)
    schema.add_field("g2", DataType.GEOMETRY, default_value="POINT(9 9)")
    index = client.prepare_index_params()
    index.add_index("v", index_type="FLAT", metric_type="L2")
    client.create_collection("c", schema=schema, index_params=index)


def rows(client):
    return sorted((r["id"], r["geo"], r["g2"]) for r in client.query("c", filter="id >= 0", output_fields=["geo", "g2"]))


def test_geometry_round_trip_validation_and_reopen(tmp_path):
    manager = ServerManager()
    path = str(tmp_path / "g.db")
    client = MilvusClient(uri=manager.start_and_get_uri(path))
    create(client)
    fields = {f["name"]: f for f in client.describe_collection("c")["fields"]}
    assert fields["geo"]["type"] == DataType.GEOMETRY and fields["geo"]["nullable"] is True
    assert fields["g2"]["default_value"].string_data == "POINT(9 9)"
    client.insert("c", [
        {"id": 1, "v": [0.0, 0.0], "geo": "  point ( 1e1   -2.5 )  "},
        {"id": 2, "v": [1.0, 0.0], "geo": None, "g2": None},
        {"id": 3, "v": [0.0, 1.0], "geo": SQUARE, "g2": "POINT(2 2)"},
    ])
    expected = [(1, "  point ( 1e1   -2.5 )  ", "POINT(9 9)"), (2, None, "POINT(9 9)"), (3, SQUARE, "POINT(2 2)")]
    assert rows(client) == expected
    for bad in ["not-wkt", "", "POINT(1)", "POINT(inf 1)", "LINESTRING(0 0,1 1)", "POLYGON((0 0,2 2,2 0,0 2,0 0))"]:
        with pytest.raises(MilvusException) as info:
            client.insert("c", [{"id": 9, "v": [0.0, 0.0], "geo": "POINT(1 1)"}, {"id": 10, "v": [0.0, 0.0], "geo": bad}])
        assert info.value.code == 6
        assert info.value.message == f"field 'geo' value {bad!r} does not match dtype DataType.GEOMETRY"
    assert rows(client) == expected
    hits = client.search("c", data=[[0.0, 0.0]], limit=3, output_fields=["geo"])[0]
    assert [(h["id"], h["entity"]["geo"]) for h in hits] == [(1, "  point ( 1e1   -2.5 )  "), (2, None), (3, SQUARE)]
    client.flush("c")
    client.close()
    manager.release_all()

    manager = ServerManager()
    client = MilvusClient(uri=manager.start_and_get_uri(path))
    client.load_collection("c")
    assert rows(client) == expected
    client.upsert("c", [{"id": 3, "geo": "POINT(8 8)"}], partial_update=True)
    assert rows(client)[2] == (3, "POINT(8 8)", "POINT(2 2)")
    client.close()
    manager.release_all()
