"""HNSW 检索（0041）：经真实 pymilvus。

图在后台构建，检索可能走图也可能还在走 FLAT；下面的断言对两者都成立：返回的距离与 FLAT 对同一主键逐位相同，
召回达到门槛。图是否真的被使用由 Rust 单测确认（`db::tests::graphs`）。
"""

import time

import numpy as np
from pymilvus import DataType, MilvusClient

from milvus_lite.server_manager import server_manager_instance

ROWS = 30_000
DIM = 16


def _collection(client, name, index_type):
    schema = client.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("v", DataType.FLOAT_VECTOR, dim=DIM)
    params = client.prepare_index_params()
    params.add_index("v", index_type=index_type, metric_type="L2")
    client.create_collection(name, schema=schema, index_params=params)


def _fill(client, name, data):
    for start in range(0, ROWS, 5000):
        client.insert(name, [{"id": i, "v": data[i].tolist()} for i in range(start, start + 5000)])
    client.flush(name)


def _search(client, name, queries, limit=10):
    # ef=32：段的有效行（3 万）多于 800 × ef，检索走图（0041 第 5 步的门槛）。
    params = {"metric_type": "L2", "params": {"ef": 32}}
    hits = client.search(name, data=queries, limit=limit, search_params=params)
    return [[(hit["id"], np.float32(hit["distance"])) for hit in row] for row in hits]


def _graph_files(path):
    return sorted(path.parent.glob("**/indexes/*.hnsw.mlrs"))


def test_hnsw_search_matches_flat_distances_and_recall(tmp_path):
    path = tmp_path / "hnsw.db"
    data = np.random.default_rng(42).random((ROWS, DIM), dtype=np.float32)
    queries = np.random.default_rng(7).random((50, DIM), dtype=np.float32).tolist()
    client = MilvusClient(str(path))
    _collection(client, "h", "HNSW")
    _collection(client, "f", "FLAT")
    _fill(client, "h", data)
    _fill(client, "f", data)
    deadline = time.monotonic() + 120
    while not _graph_files(path) and time.monotonic() < deadline:
        time.sleep(0.2)
    assert _graph_files(path), "the background build wrote no graph file"
    graph, flat = _search(client, "h", queries), _search(client, "f", queries)
    hit = 0
    for got, want in zip(graph, flat):
        assert len(got) == 10
        exact = dict(want)
        for pk, distance in got:
            if pk in exact:
                assert distance.tobytes() == exact[pk].tobytes()
        hit += len({pk for pk, _ in got} & set(exact))
    assert hit / 500 >= 0.95
    client.close()
    server_manager_instance.release_server(str(path))


def test_graph_file_survives_reopen_and_search_still_matches(tmp_path):
    path = tmp_path / "reopen.db"
    data = np.random.default_rng(3).random((ROWS, DIM), dtype=np.float32)
    queries = np.random.default_rng(4).random((20, DIM), dtype=np.float32).tolist()
    client = MilvusClient(str(path))
    _collection(client, "h", "HNSW")
    _fill(client, "h", data)
    deadline = time.monotonic() + 120
    while not _graph_files(path) and time.monotonic() < deadline:
        time.sleep(0.2)
    files = _graph_files(path)
    assert files
    before = [f.read_bytes() for f in files]
    first = _search(client, "h", queries)
    client.close()
    server_manager_instance.release_server(str(path))

    reopened = MilvusClient(str(path))
    reopened.load_collection("h")
    # 重开后读入原图，结果与关闭前相同，图文件没有被重写。
    assert _search(reopened, "h", queries) == first
    assert [f.read_bytes() for f in _graph_files(path)] == before
    reopened.close()
    server_manager_instance.release_server(str(path))
