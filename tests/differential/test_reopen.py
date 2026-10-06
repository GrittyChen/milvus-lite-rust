"""独立服务的重开差分：每次关闭两个引擎，再从原数据目录启动。"""

import io
import json
import os
import shutil
import subprocess
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import grpc
import pytest
from pymilvus import DataType, MilvusClient
from pymilvus.grpc_gen import milvus_pb2, milvus_pb2_grpc, schema_pb2

from compare import Diff, run
from conftest import ENV, REF_VERSION
from milvus_lite.server_manager import ServerManager
from test_scenarios import index


class ReopenPair:
    def __init__(self, root):
        self.root = root
        self.manager = ServerManager()
        self.rust_path = str(root / "rust.db")
        self.ref_path = str(root / "3x")
        self.ref_log = root / "server3x.log"
        self.proc = None
        self.clients = None
        self.uris = None

    def open(self):
        assert self.clients is None
        log = open(self.ref_log, "ab")
        try:
            self.proc = subprocess.Popen(
                [os.environ[ENV], os.path.join(os.path.dirname(__file__), "server3x.py"), self.ref_path],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log,
            )
            line = self.proc.stdout.readline().decode().split()
            self.proc.stdout.close()
            assert len(line) == 2, f"3.x startup failed: {self.ref_log.read_text(errors='replace')}"
            port, version = line
            assert version == REF_VERSION, f"3.x version {version}, expected {REF_VERSION}"
            rust_uri = self.manager.start_and_get_uri(self.rust_path)
            assert rust_uri is not None
            self.uris = (rust_uri, f"http://127.0.0.1:{port}")
            self.clients = tuple(MilvusClient(uri=uri) for uri in self.uris)
            return Diff(*self.clients)
        except BaseException:
            # 启动失败的退出码属于原始错误；清理时不再用它覆盖诊断。
            self.close(check_exit=False)
            raise
        finally:
            log.close()

    def close(self, *, check_exit=True):
        if self.clients is not None:
            for client in self.clients:
                client.close()
            self.clients = None
        self.uris = None
        self.manager.release_server(self.rust_path)
        if self.proc is not None:
            self.proc.stdin.close()
            try:
                code = self.proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                code = self.proc.wait()
            self.proc = None
            if check_exit:
                assert code == 0, f"3.x exit {code}: {self.ref_log.read_text(errors='replace')}"


@pytest.fixture
def pair(tmp_path):
    pair = ReopenPair(tmp_path)
    yield pair
    pair.close()


def schema(auto_id=False):
    s = MilvusClient.create_schema(auto_id=auto_id, enable_dynamic_field=True)
    s.add_field("id", DataType.INT64, is_primary=True, auto_id=auto_id)
    s.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    return s


def test_concurrent_lazy_opens_preserve_rows_and_same_name_recreation(pair):
    diff = pair.open()
    for name in ["a", "b"]:
        diff.step(name + "_create", lambda c: c.create_collection(name, schema=schema(), index_params=index("L2")))
        diff.step(name + "_insert", lambda c: c.insert(name, [{"id": 1, "vector": [1.0, 0.0]}]))
    pair.close()
    diff = pair.open()

    def concurrent_counts(c):
        names = ["a", "a", "b", "b"] * 4
        with ThreadPoolExecutor(max_workers=8) as workers:
            return list(workers.map(lambda name: c.query(name, filter="", output_fields=["count(*)"]), names))

    # Rust 的并发首次打开由受控 core 用例证明；参照侧串行取基准，避免触发其重复实例缺陷。
    expected = [diff.ref.query(name, filter="", output_fields=["count(*)"]) for name in ["a", "a", "b", "b"] * 4]
    assert repr(concurrent_counts(diff.rust)) == repr(expected)
    for name in ["a", "b"]:
        diff.step(name + "_unloaded", lambda c: c.get_load_state(name))
        diff.step(name + "_load", lambda c: c.load_collection(name))
        assert diff.step(name + "_get", lambda c: c.get(name, ids=[1], output_fields=["id"])) == ("ok", [{"id": 1}])
    diff.step("drop_a", lambda c: c.drop_collection("a"))
    diff.step("recreate_a", lambda c: c.create_collection("a", schema=schema(), index_params=index("L2")))
    assert diff.step("new_a_empty", lambda c: c.query("a", filter="", output_fields=["count(*)"])) == ("ok", [{"count(*)": 0}])
    assert diff.step("b_untouched", lambda c: c.get("b", ids=[1], output_fields=["id"])) == ("ok", [{"id": 1}])


@pytest.mark.parametrize("bad_schema", [b"{", b"[]", b"{}"], ids=["syntax", "shape", "required-field"])
def test_cached_fail_closed_survives_external_repair_until_recreation(pair, bad_schema):
    diff = pair.open()
    for name in ["a", "b"]:
        diff.step(name + "_create", lambda c: c.create_collection(name, schema=schema(), index_params=index("L2")))
        diff.step(name + "_insert", lambda c: c.insert(name, [{"id": 1, "vector": [1.0, 0.0]}]))
    pair.close()
    paths = [Path(root) / "collections/a/schema.json" for root in [pair.rust_path, pair.ref_path]]
    originals = [path.read_bytes() for path in paths]
    for path in paths:
        path.write_bytes(bad_schema)
    diff = pair.open()
    count = lambda c: c.query("a", filter="", output_fields=["count(*)"])
    first = run(count, diff.rust)
    # 0006「schema.json 的 JSON 语法错误」登记既有的磁盘错误与 schema 校验错误映射差异。
    assert first[:3] == ("err", 1, 0)
    assert run(count, diff.ref)[:3] == ("err", 6, 6)
    assert all(path.read_bytes() == bad_schema for path in paths)
    for path, original in zip(paths, originals):
        path.write_bytes(original)
    # 0006 登记的缓存差异：外部修复不使同一服务实例重新解析；3.x 再次访问即可打开。
    assert run(count, diff.rust) == first
    assert run(count, diff.ref) == ("ok", [{"count(*)": 1}])
    assert diff.step("healthy_b", lambda c: c.query("b", filter="", output_fields=["count(*)"])) == ("ok", [{"count(*)": 1}])
    diff.step("drop_cached_a", lambda c: c.drop_collection("a"))
    diff.step("recreate_a", lambda c: c.create_collection("a", schema=schema(), index_params=index("L2")))
    assert diff.step("fresh_a", count) == ("ok", [{"count(*)": 0}])


@pytest.mark.parametrize("change", ["move", "delete"])
def test_cached_failure_does_not_mask_externally_removed_directory(pair, change):
    diff = pair.open()
    diff.step("create", lambda c: c.create_collection("a", schema=schema(), index_params=index("L2")))
    diff.step("insert", lambda c: c.insert("a", [{"id": 1, "vector": [1.0, 0.0]}]))
    pair.close()
    directories = [Path(root) / "collections/a" for root in [pair.rust_path, pair.ref_path]]
    originals = [(directory / "schema.json").read_bytes() for directory in directories]
    for directory in directories:
        (directory / "schema.json").write_bytes(b"{")
    diff = pair.open()
    count = lambda c: c.query("a", filter="", output_fields=["count(*)"])
    first = run(count, diff.rust)
    assert first[:3] == ("err", 1, 0)
    assert run(count, diff.ref)[:3] == ("err", 6, 6)
    saved = []
    for directory in directories:
        target = directory.parent.parent / "saved-a"
        if change == "move":
            directory.rename(target)
            saved.append(target)
        else:
            shutil.rmtree(directory)
    assert diff.step("absent_has", lambda c: c.has_collection("a")) == ("ok", False)
    assert diff.step("absent_describe", lambda c: c.describe_collection("a"), error=True) == ("err", 100, 4, "collection 'a' does not exist")
    assert str(diff.step("absent_load_state", lambda c: c.get_load_state("a"))[1]["state"]) == "NotExist"
    assert diff.step("absent_stats", lambda c: c.get_collection_stats("a"), error=True) == ("err", 100, 4, "collection 'a' does not exist")
    if change == "move":
        for target, directory, original in zip(saved, directories, originals):
            target.rename(directory)
            (directory / "schema.json").write_bytes(original)
        # 0006 的缓存边界：不存在时不报旧错误，但移回修复后的路径并不清除判定。
        assert run(count, diff.rust) == first
        assert run(count, diff.ref) == ("ok", [{"count(*)": 1}])
    diff.step("drop", lambda c: c.drop_collection("a"))
    diff.step("recreate", lambda c: c.create_collection("a", schema=schema(), index_params=index("L2")))
    assert diff.step("new_count", count) == ("ok", [{"count(*)": 0}])


def raw_upsert(uri, pk=None):
    """绕过 pymilvus 的 auto_id 主键检查，验证服务端 Upsert。"""
    request = milvus_pb2.UpsertRequest(collection_name="a", num_rows=1)
    if pk is not None:
        primary = schema_pb2.FieldData(type=DataType.INT64, field_name="id")
        primary.scalars.long_data.data.append(pk)
        request.fields_data.append(primary)
    vector = schema_pb2.FieldData(type=DataType.FLOAT_VECTOR, field_name="vector")
    vector.vectors.dim = 2
    vector.vectors.float_vector.data.extend([7.0, 0.0])
    request.fields_data.append(vector)
    channel = grpc.insecure_channel(uri.removeprefix("http://"))
    try:
        response = milvus_pb2_grpc.MilvusServiceStub(channel).Upsert(request)
        return response.status.code, response.upsert_cnt, list(response.IDs.int_id.data)
    finally:
        channel.close()


def raw_json_insert(uri, collection, pk, text):
    primary = schema_pb2.FieldData(type=DataType.INT64, field_name="id")
    primary.scalars.long_data.data.append(pk)
    vector = schema_pb2.FieldData(type=DataType.FLOAT_VECTOR, field_name="vector")
    vector.vectors.dim = 2
    vector.vectors.float_vector.data.extend([1.0, 0.0])
    value = schema_pb2.FieldData(type=DataType.JSON, field_name="json")
    value.scalars.json_data.data.append(text.encode())
    with grpc.insecure_channel(uri.removeprefix("http://"), options=[("grpc.enable_http_proxy", 0)]) as channel:
        return milvus_pb2_grpc.MilvusServiceStub(channel).Insert(milvus_pb2.InsertRequest(
            collection_name=collection, num_rows=1, fields_data=[primary, vector, value],
        ))


def raw_json_query(uri, collection, pk):
    with grpc.insecure_channel(uri.removeprefix("http://"), options=[("grpc.enable_http_proxy", 0)]) as channel:
        return milvus_pb2_grpc.MilvusServiceStub(channel).Query(milvus_pb2.QueryRequest(
            collection_name=collection, expr=f"id in [{pk}]", output_fields=["json"],
        ))


def test_reopen_preserves_manifest_ties_and_finite_json_bytes(pair):
    diff = pair.open()
    s = schema()
    s.add_field("json", DataType.JSON)
    diff.step("create", lambda c: c.create_collection("c", schema=s, index_params=index("L2")))
    raw = '{"a":1e5,"wide":9223372036854775808,"中文":"三"}'
    expected_json = json.dumps(json.loads(raw), ensure_ascii=False).encode()
    for pk in [5, 3, 1, 2]:
        for uri in pair.uris:
            assert raw_json_insert(uri, "c", pk, raw).status.code == 0
    diff.step("first_flush", lambda c: c.flush("c"))
    diff.step("delete", lambda c: c.delete("c", ids=[5, 1]))
    for pk in [6, 4, 6]:
        for uri in pair.uris:
            assert raw_json_insert(uri, "c", pk, raw).status.code == 0
    diff.step("second_flush", lambda c: c.flush("c"))
    for pk in [7, 8, 8]:
        for uri in pair.uris:
            assert raw_json_insert(uri, "c", pk, raw).status.code == 0

    def check(label):
        result = diff.step(label + "_ties", lambda c: c.search("c", data=[[1.0, 0.0]], limit=6,
                          search_params={"metric_type": "L2"}, output_fields=["*"]))[1][0]
        # 返回全部存活行且距离精确为零，有意固定两侧非截断的并列顺序。
        assert [r["id"] for r in result] == [3, 2, 6, 4, 7, 8]
        assert [r["distance"] for r in result] == [0.0] * 6
        for pk in [3, 2, 6, 4, 7, 8]:
            for uri in pair.uris:
                response = raw_json_query(uri, "c", pk)
                assert response.status.code == 0
                field = next(f for f in response.fields_data if f.field_name == "json")
                assert list(field.scalars.json_data.data) == [expected_json]

    check("before_close")
    pair.close()
    # close 会提交最后的活跃代，总计三个 data 文件，未达到 3.x compaction 阈值。
    manifest = json.loads((Path(pair.ref_path) / "collections/c/manifest.json").read_text())
    assert len(manifest["partitions"]["_default"]["data_files"]) == 3
    diff = pair.open()
    assert str(diff.step("unloaded", lambda c: c.get_load_state("c"))[1]["state"]) == "NotLoad"
    diff.step("load", lambda c: c.load_collection("c"))
    check("after_reopen")


def test_raw_json_float_overflow_rejects_before_wal_and_finite_input_can_retry(pair):
    diff = pair.open()
    s = schema()
    s.add_field("json", DataType.JSON)
    diff.step("create", lambda c: c.create_collection("c", schema=s, index_params=index("L2")))
    for text in ['{"a":1e999}', '{"a":-1e999}', '[{"a":1e999}]']:
        reply = raw_json_insert(pair.uris[0], "c", 99, text)
        assert reply.status.code == 1
        assert 'JSON number outside the finite float64 range' in reply.status.reason
        assert not reply.status.reason.startswith('storage error:')
        assert not (Path(pair.rust_path) / "collections/c/wal").exists()
    assert diff.rust.query("c", output_fields=["count(*)"]) == [{"count(*)": 0}]
    # 0006 登记的有意差异：3.x 接受写入，但 raw Query 返回非标准 JSON 常量。
    diff.ref.create_collection("overflow_reference", schema=s, index_params=index("L2"))
    assert raw_json_insert(pair.uris[1], "overflow_reference", 99, '{"a":1e999}').status.code == 0
    reference_reply = raw_json_query(pair.uris[1], "overflow_reference", 99)
    assert reference_reply.status.code == 0
    reference_field = next(f for f in reference_reply.fields_data if f.field_name == "json")
    assert list(reference_field.scalars.json_data.data) == [b'{"a": Infinity}']
    valid = '{"a":1e5,"wide":9223372036854775808,"中文":"三"}'
    expected = json.dumps(json.loads(valid), ensure_ascii=False).encode()
    for uri in pair.uris:
        reply = raw_json_insert(uri, "c", 1, valid)
        assert reply.status.code == 0
    for stage in range(3):
        for uri in pair.uris:
            reply = raw_json_query(uri, "c", 1)
            assert reply.status.code == 0
            field = next(f for f in reply.fields_data if f.field_name == "json")
            assert list(field.scalars.json_data.data) == [expected]
        if stage == 0:
            diff.step("flush", lambda c: c.flush("c"))
            manifest = json.loads((Path(pair.rust_path) / "collections/c/manifest.json").read_text())
            assert manifest["current_seq"] == 1
        elif stage == 1:
            pair.close()
            diff = pair.open()
            diff.step("load", lambda c: c.load_collection("c"))


def test_delete_upsert_and_load_state_after_reopen(pair):
    diff = pair.open()
    diff.step("create", lambda c: c.create_collection("c", schema=schema(), index_params=index("L2")))
    diff.step("insert", lambda c: c.insert("c", [
        {"id": 1, "vector": [1.0, 0.0], "tag": "old"},
        {"id": 2, "vector": [0.0, 1.0], "tag": "keep"},
        {"id": 3, "vector": [2.0, 0.0]},
    ]))
    diff.step("flush", lambda c: c.flush("c"))
    diff.step("delete", lambda c: c.delete("c", ids=[1]))
    diff.step("partial_upsert", lambda c: c.upsert("c", [{"id": 2, "tag": "new"}], partial_update=True))
    diff.step("reinsert", lambda c: c.insert("c", [{"id": 1, "vector": [3.0, 0.0], "tag": "back"}]))
    pair.close()

    diff = pair.open()
    assert str(diff.step("load_state", lambda c: c.get_load_state("c"))[1]["state"]) == "NotLoad"
    diff.step("get_unloaded", lambda c: c.get("c", ids=[1]), error=True)
    diff.step("load", lambda c: c.load_collection("c"))
    diff.step("get", lambda c: c.get("c", ids=[1, 2, 3], output_fields=["vector", "tag"]))
    assert diff.step("count", lambda c: c.query("c", output_fields=["count(*)"]))[1] == [{"count(*)": 3}]
    # 主键 in 快路径在 3.x 中忽略 limit；只对照重开后的主键读回结果。
    rows = diff.step("query_pk_order", lambda c: c.query("c", filter="id in [1, 2, 3]"))[1]
    assert [row["id"] for row in rows] == [1, 2, 3]
    search = diff.step("search", lambda c: c.search("c", data=[[3.0, 0.0]], limit=3,
                                                  search_params={"metric_type": "L2"}))
    assert len(search[1][0]) == 3


def test_auto_id_continues_after_delete_and_reopen(pair):
    diff = pair.open()
    diff.step("create", lambda c: c.create_collection("a", schema=schema(True), index_params=index("L2")))
    rows = [{"vector": [float(i), 0.0], "tag": str(i)} for i in range(3)]
    rust_ids = run(lambda c: c.insert("a", rows), diff.rust)[1]["ids"]
    ref_ids = run(lambda c: c.insert("a", rows), diff.ref)[1]["ids"]
    assert rust_ids == ref_ids == [1, 2, 3]
    for client, ids in ((diff.rust, rust_ids), (diff.ref, ref_ids)):
        assert run(lambda c: c.delete("a", ids=[ids[1]]), client)[0] == "ok"
    extra = [{"vector": [4.0, 0.0], "tag": "four"}]
    rust_next = run(lambda c: c.insert("a", extra), diff.rust)[1]["ids"]
    ref_next = run(lambda c: c.insert("a", extra), diff.ref)[1]["ids"]
    assert rust_next == [5] and ref_next == [4]  # 0004 已登记：delete 只在本实现消耗 auto_id。
    for uri, ids in zip(pair.uris, (rust_next, ref_next)):
        assert raw_upsert(uri, ids[0]) == (0, 1, ids)
    pair.close()

    diff = pair.open()
    assert str(diff.step("load_state", lambda c: c.get_load_state("a"))[1]["state"]) == "NotLoad"
    diff.step("load", lambda c: c.load_collection("a"))
    for client, ids in ((diff.rust, rust_ids + rust_next), (diff.ref, ref_ids + ref_next)):
        rows = run(lambda c: c.get("a", ids=ids, output_fields=["tag", "vector"]), client)
        assert rows == ("ok", [{"id": ids[0], "tag": "0", "vector": [0.0, 0.0]},
                               {"id": ids[2], "tag": "2", "vector": [2.0, 0.0]},
                               {"id": ids[3], "tag": "four", "vector": [7.0, 0.0]}])
    resumed = diff.step("insert_after_reopen", lambda c: c.insert("a", [{"vector": [8.0, 0.0]}]))
    assert resumed[1]["ids"] == [7]


def test_raw_grpc_upsert_generates_auto_id(pair):
    diff = pair.open()
    diff.step("create", lambda c: c.create_collection("a", schema=schema(True), index_params=index("L2")))
    results = [raw_upsert(uri) for uri in pair.uris]
    assert results == [(0, 1, [1]), (0, 1, [1])]
    pair.close()
    diff = pair.open()
    assert str(diff.step("load_state", lambda c: c.get_load_state("a"))[1]["state"]) == "NotLoad"
    diff.step("load", lambda c: c.load_collection("a"))
    assert diff.step("get_generated", lambda c: c.get("a", ids=[1], output_fields=["vector"]))[1] == [
        {"id": 1, "vector": [7.0, 0.0]},
    ]


def test_quick_create_reopens_unloaded(pair):
    diff = pair.open()
    # 快速建表会创建 AUTOINDEX；只核对重开状态，不做索引检索。
    diff.step("quick_create", lambda c: c.create_collection("quick", dimension=2))
    diff.step("insert", lambda c: c.insert("quick", [{"id": 1, "vector": [1.0, 0.0]}]))
    pair.close()

    diff = pair.open()
    assert str(diff.step("load_state", lambda c: c.get_load_state("quick"))[1]["state"]) == "NotLoad"
    diff.step("get_unloaded", lambda c: c.get("quick", ids=[1]), error=True)
    diff.step("load", lambda c: c.load_collection("quick"))
    assert diff.step("get", lambda c: c.get("quick", ids=[1]))[1][0]["id"] == 1


@pytest.mark.parametrize("index_metric,request_metric,rows,rust_expected,ref_expected", [
    ("IP", "L2", [[1.0, 0.0], [3.0, 0.0], [0.0, 1.0]],
     [(1, 0.0), (3, 2.0), (2, 4.0)], [(2, 9.0), (1, 1.0), (3, 0.0)]),
    ("L2", "IP", [[1.0, 0.0], [3.0, 0.0], [0.0, 0.0]],
     [(2, 3.0), (1, 1.0), (3, 0.0)], [(1, -0.0), (3, -1.0), (2, -2.0)]),
    ("IP", "COSINE", [[1.0, 0.0], [0.0, 2.0], [-2.0, 0.0]],
     [(1, 1.0), (2, 0.0), (3, -1.0)], [(1, 2.0), (2, 1.0), (3, -1.0)]),
])
def test_request_metric_overrides_flushed_index_metric(pair, index_metric, request_metric, rows,
                                                       rust_expected, ref_expected):
    diff = pair.open()
    diff.step("create", lambda c: c.create_collection("c", schema=schema(), index_params=index(index_metric)))
    diff.step("insert", lambda c: c.insert("c", [
        {"id": pk, "vector": vector} for pk, vector in enumerate(rows, start=1)
    ]))
    diff.step("flush", lambda c: c.flush("c"))
    pair.close()

    diff = pair.open()
    assert str(diff.step("load_state", lambda c: c.get_load_state("c"))[1]["state"]) == "NotLoad"
    diff.step("load", lambda c: c.load_collection("c"))
    search = lambda c: c.search("c", data=[[1.0, 0.0]], limit=3,
                                search_params={"metric_type": request_metric})
    rust, ref = run(search, diff.rust), run(search, diff.ref)
    assert rust[0] == ref[0] == "ok"
    rust_hits = [(hit["id"], hit["distance"]) for hit in rust[1][0]]
    ref_hits = [(hit["id"], hit["distance"]) for hit in ref[1][0]]
    assert repr(rust_hits) == repr(rust_expected)
    # 3.x 的已知缺陷：segment 用索引度量计算内部距离并排序，再按请求度量换算对外分数。
    assert repr(ref_hits) == repr(ref_expected)


def test_failed_reference_startup_preserves_error(pair, monkeypatch):
    class FailedProcess:
        stdout = io.BytesIO(b"")
        stdin = io.BytesIO()

        def wait(self, timeout=None):
            return 7

    monkeypatch.setattr(subprocess, "Popen", lambda *args, **kwargs: FailedProcess())
    with pytest.raises(AssertionError, match="3.x startup failed"):
        pair.open()
    assert pair.proc is None
