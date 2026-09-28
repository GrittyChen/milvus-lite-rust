"""真实 pymilvus 入口的 M2 目录生命周期。"""

from pymilvus import DataType, MilvusClient
from pymilvus.grpc_gen import milvus_pb2, milvus_pb2_grpc
import grpc

from milvus_lite.server_manager import server_manager_instance


def test_create_flush_stop_reopen_get_count_and_search(tmp_path):
    path = tmp_path / "persist.db"
    client = MilvusClient(str(path))
    client.create_collection("c", dimension=2, metric_type="L2")
    client.insert("c", [{"id": 1, "vector": [1.0, 0.0]}, {"id": 2, "vector": [0.0, 1.0]}])
    client.flush("c")
    client.insert("c", [{"id": 3, "vector": [2.0, 0.0]}])
    client.close()
    server_manager_instance.release_server(str(path))

    reopened = MilvusClient(str(path))
    assert reopened.list_collections() == ["c"]
    assert reopened.query("c", output_fields=["count(*)"]) == [{"count(*)": 3}]
    reopened.load_collection("c")
    assert [row["id"] for row in reopened.get("c", ids=[3, 1])] == [3, 1]
    assert [hit["id"] for hit in reopened.search("c", data=[[1.0, 0.0]], limit=3)[0]] == [1, 3, 2]
    reopened.close()
    server_manager_instance.release_server(str(path))


def test_drop_and_recreate_do_not_resurrect_old_rows(tmp_path):
    path = tmp_path / "drop.db"
    client = MilvusClient(str(path))
    client.create_collection("c", dimension=2)
    client.insert("c", [{"id": 1, "vector": [1.0, 0.0]}])
    client.drop_collection("c")
    client.create_collection("c", dimension=2)
    assert client.query("c", output_fields=["count(*)"]) == [{"count(*)": 0}]
    client.close()
    server_manager_instance.release_server(str(path))

    reopened = MilvusClient(str(path))
    assert reopened.query("c", output_fields=["count(*)"]) == [{"count(*)": 0}]
    reopened.close()
    server_manager_instance.release_server(str(path))


def test_show_collections_reports_broken_manifest_as_unavailable(tmp_path):
    path = tmp_path / "broken.db"
    client = MilvusClient(str(path))
    for name in ("broken", "healthy"):
        schema = MilvusClient.create_schema()
        schema.add_field("id", DataType.INT64, is_primary=True)
        schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
        client.create_collection(name, schema=schema)
    client.close()
    server_manager_instance.release_server(str(path))
    (path / "collections" / "broken" / "manifest.json").write_text("{")

    uri = server_manager_instance.start_and_get_uri(str(path))
    assert uri is not None
    channel = grpc.insecure_channel(uri.removeprefix("http://"))
    response = milvus_pb2_grpc.MilvusServiceStub(channel).ShowCollections(
        milvus_pb2.ShowCollectionsRequest(db_name="default")
    )
    states = dict(zip(response.collection_names, response.inMemory_percentages))
    assert states == {"broken": 0, "healthy": 100}
    channel.close()
    server_manager_instance.release_server(str(path))
