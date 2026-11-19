"""nullable 稠密向量经真实 SDK 走完整生命周期（0038）。

空行存为 Arrow null、读出 None、检索排除，这是与 3.x 登记的差异：3.x 写入时把 None 换成零向量。
"""

from contextlib import contextmanager
import json
import subprocess
import sys

import pytest
from pymilvus import AnnSearchRequest, DataType, MilvusClient, RRFRanker
from milvus_lite.server_manager import ServerManager


@contextmanager
def client_at(path):
    manager = ServerManager()
    client = MilvusClient(manager.start_and_get_uri(str(path)), dedicated=True)
    try:
        yield client
    finally:
        client.close()
        manager.release_all()


def create(client, metric="L2", *, vectors=(("v", True),), name="c"):
    schema = client.create_schema(auto_id=False, enable_dynamic_field=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    for field, nullable in vectors:
        schema.add_field(field, DataType.FLOAT_VECTOR, dim=2, nullable=nullable)
    schema.add_field("g", DataType.INT64)
    indexes = client.prepare_index_params()
    for field, _ in vectors:
        indexes.add_index(field, index_type="FLAT", metric_type=metric)
    client.create_collection(name, schema=schema, index_params=indexes)


# id 1 显式 None、id 2 缺键、id 3 真实零向量；查询 [1, 0] 下 L2 与 IP 的名次互不相同。
ROWS = [
    {"id": 0, "v": [1.0, 0.0], "g": 0},
    {"id": 1, "v": None, "g": 1},
    {"id": 2, "g": 2},
    {"id": 3, "v": [0.0, 0.0], "g": 3},
    {"id": 4, "v": [0.0, 1.0], "g": 4},
    {"id": 5, "v": [-1.0, 0.0], "g": 5},
]
STORED = {row["id"]: row.get("v") for row in ROWS}
VALID = [row["id"] for row in ROWS if row.get("v") is not None]


def ids(hits):
    return [hit["id"] for hit in hits]


def check(client, metric):
    rows = sorted(client.query("c", filter="id >= 0", output_fields=["v"]), key=lambda r: r["id"])
    assert {row["id"]: row["v"] for row in rows} == STORED
    # 多行结果中空行之后的有效行不错位（3.x 的完整占位编码会让 pymilvus 读错）。
    assert list(client.get("c", ids=[1, 4, 2, 5], output_fields=["v"])) == [
        {"id": 1, "v": None}, {"id": 4, "v": [0.0, 1.0]}, {"id": 2, "v": None}, {"id": 5, "v": [-1.0, 0.0]}]
    assert client.query("c", filter="", output_fields=["count(*)"])[0]["count(*)"] == len(ROWS)

    params = {"metric_type": metric}
    for query in client.search("c", [[1.0, 0.0], [0.5, 0.5]], limit=10, output_fields=["v"], search_params=params):
        assert sorted(ids(query)) == VALID
        assert all(hit["entity"]["v"] == STORED[hit["id"]] for hit in query)
    radius = {"L2": 10.0, "IP": -10.0, "COSINE": -1.0}[metric]
    ranged = client.search("c", [[1.0, 0.0]], limit=10, search_params={**params, "params": {"radius": radius}})
    # COSINE 下 id 5 的相似度正好是 radius -1，range 不含边界。
    assert sorted(ids(ranged[0])) == [i for i in VALID if not (metric == "COSINE" and i == 5)]
    grouped = client.search("c", [[1.0, 0.0]], limit=10, group_by_field="g", search_params=params)
    assert sorted(ids(grouped[0])) == VALID
    iterator = client.search_iterator("c", [[1.0, 0.0]], batch_size=2, limit=10, search_params=params)
    seen = []
    while batch := iterator.next():
        seen.extend(ids(batch))
    iterator.close()
    assert sorted(seen) == VALID
    hybrid = client.hybrid_search("c", [
        AnnSearchRequest([[1.0, 0.0]], "v", params, limit=10),
        AnnSearchRequest([[0.0, 1.0]], "v", params, limit=10)], RRFRanker(), limit=10, output_fields=["v"])
    assert sorted(ids(hybrid[0])) == VALID
    iterator = client.query_iterator("c", batch_size=2, filter="id >= 0", output_fields=["v"])
    paged = []
    while batch := iterator.next():
        paged.extend(batch)
    iterator.close()
    assert {row["id"]: row["v"] for row in paged} == STORED


@pytest.mark.parametrize("metric", ["L2", "IP", "COSINE"])
def test_nulls_read_back_as_none_and_are_skipped_by_every_search(tmp_path, metric):
    path = tmp_path / "nullable.db"
    with client_at(path) as client:
        create(client, metric)
        described = {f["name"]: f for f in client.describe_collection("c")["fields"]}
        assert described["v"].get("nullable") is True
        client.insert("c", ROWS)
        check(client, metric)
        client.flush("c")
        check(client, metric)
    with client_at(path) as client:
        client.load_collection("c")
        check(client, metric)


def test_all_null_batches_upserts_and_all_null_results(tmp_path):
    with client_at(tmp_path / "upsert.db") as client:
        create(client)
        # 整批为空：pymilvus 不设置 float_vector，3.x 报错，本实现接受（0038 决策 2）。
        client.insert("c", [{"id": 10, "v": None, "g": 0}, {"id": 11, "g": 0}])
        assert list(client.query("c", filter="id >= 10", output_fields=["v"])) == [
            {"id": 10, "v": None}, {"id": 11, "v": None}]
        client.insert("c", [{"id": 1, "v": [1.0, 0.0], "g": 1}, {"id": 2, "v": [0.0, 1.0], "g": 2}])
        client.upsert("c", [{"id": 1, "v": None, "g": 1}])
        client.upsert("c", [{"id": 2, "g": 5}], partial_update=True)
        client.upsert("c", [{"id": 10, "v": [1.0, 1.0]}], partial_update=True)
        client.upsert("c", [{"id": 11, "g": 7}], partial_update=True)
        want = {1: None, 2: [0.0, 1.0], 10: [1.0, 1.0], 11: None}
        rows = client.query("c", filter="id >= 0", output_fields=["v", "g"])
        assert {row["id"]: row["v"] for row in rows} == want
        assert {row["id"]: row["g"] for row in rows} == {1: 1, 2: 5, 10: 0, 11: 7}
        assert sorted(ids(client.search("c", [[1.0, 0.0]], limit=10)[0])) == [2, 10]
        client.flush("c")
        assert {row["id"]: row["v"] for row in client.query("c", filter="id >= 0", output_fields=["v"])} == want


@pytest.mark.parametrize("layout", [(True, False), (False, True), (True, True)])
def test_multi_vector_layouts_null_only_nullable_fields(tmp_path, layout):
    with client_at(tmp_path / "multi.db") as client:
        create(client, vectors=(("a", layout[0]), ("b", layout[1])))
        rows = [{"id": i, "g": i, "a": [1.0, float(i)], "b": [float(i), 1.0]} for i in range(4)]
        for row in rows:
            for field, nullable in zip("ab", layout):
                if nullable and row["id"] % 2:
                    row[field] = None
        client.insert("c", rows)
        for field, nullable in zip("ab", layout):
            want = [0, 2] if nullable else [0, 1, 2, 3]
            hits = client.search("c", [[1.0, 1.0]], anns_field=field, limit=10)
            assert sorted(ids(hits[0])) == want, field
            got = client.query("c", filter="id >= 0", output_fields=[field])
            assert [row[field] is None for row in sorted(got, key=lambda r: r["id"])] == [
                nullable and i % 2 == 1 for i in range(4)]
        if not all(layout):
            # 非 nullable 的字段照旧拒绝空值。
            with pytest.raises(Exception, match="missing or None|float_vector data"):
                client.insert("c", [{"id": 9, "g": 9, "a": None, "b": None}])


def test_vector_filters_and_defaults_stay_rejected(tmp_path):
    with client_at(tmp_path / "reject.db") as client:
        create(client)
        client.insert("c", ROWS)
        with pytest.raises(Exception, match="cannot be used in scalar filter expressions"):
            client.query("c", filter="v is null", output_fields=["id"])
        schema = client.create_schema()
        schema.add_field("id", DataType.INT64, is_primary=True)
        with pytest.raises(Exception, match="Default value unsupported"):
            schema.add_field("v", DataType.FLOAT_VECTOR, dim=2, nullable=True, default_value=[0.0, 0.0])
            client.create_collection("d", schema=schema)


def test_first_null_write_warns_once_about_3x_pyarrow(tmp_path, capfd):
    with client_at(tmp_path / "warn.db") as client:
        create(client)
        client.insert("c", [{"id": 1, "v": [1.0, 0.0], "g": 0}])
        client.insert("c", [{"id": 2, "v": None, "g": 0}, {"id": 3, "v": None, "g": 0}])
        client.insert("c", [{"id": 4, "v": None, "g": 0}])
    err = capfd.readouterr().err
    assert err.count("now stores null vectors") == 1, err
    assert "pyarrow 26 or later" in err


# stderr 指向读端已关闭的管道时写提示会得到 EPIPE；提示只是提示，写入照常成功，集合仍可用。
BROKEN_STDERR = r"""
import json, os, sys
r, w = os.pipe(); os.close(r); os.dup2(w, 2)
from pymilvus import DataType, MilvusClient
from milvus_lite.server_manager import ServerManager
def rows(path, write):
    m = ServerManager(); c = MilvusClient(m.start_and_get_uri(path), dedicated=True)
    try:
        if write:
            s = c.create_schema(auto_id=False, enable_dynamic_field=False)
            s.add_field("id", DataType.INT64, is_primary=True)
            s.add_field("v", DataType.FLOAT_VECTOR, dim=2, nullable=True)
            c.create_collection("c", schema=s)
            c.insert("c", [{"id": 1, "v": None}, {"id": 2, "v": [1.0, 0.0]}])
            c.insert("c", [{"id": 3, "v": None}])
        else:
            c.load_collection("c")
        return sorted([r["id"], r["v"]] for r in c.query("c", filter="id >= 0", output_fields=["v"]))
    finally:
        c.close(); m.release_all()
print(json.dumps([rows(sys.argv[1], True), rows(sys.argv[1], False)]))
"""


def test_null_vector_warning_on_unwritable_stderr_does_not_fail_the_write(tmp_path):
    process = subprocess.run([sys.executable, "-c", BROKEN_STDERR, str(tmp_path / "pipe.db")],
                             capture_output=True, text=True, timeout=120)
    assert process.returncode == 0, process.stdout
    expected = [[1, None], [2, [1.0, 0.0]], [3, None]]
    assert json.loads(process.stdout.splitlines()[-1]) == [expected, expected]
