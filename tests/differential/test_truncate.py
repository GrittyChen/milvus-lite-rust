"""0023 A TruncateCollection：合同「参照实测 / 清空」逐行对照 3.x（43d1257）。

raw gRPC 比较 code、error_code 与文案；清空后两边的数据、load 状态、分区、索引定义、别名、auto_id 与重开后的结果
一致，`schema.json` 逐字节相同，manifest 除 `version` 外相同。范围外 schema 与 manifest `version` 作为预期差异
单独断言。
"""

import json
from pathlib import Path

import grpc
from pymilvus import DataType, MilvusClient

from test_aliases import Both, call, schema
from test_reopen import pair  # noqa: F401  （fixture）
from test_scenarios import index


def files(pair, relative):
    return [Path(root) / relative for root in (pair.rust_path, pair.ref_path)]


def same_bytes(pair, relative):
    rust, ref = (path.read_bytes() for path in files(pair, relative))
    assert rust == ref, f"{relative}\n  rust: {rust!r}\n  3.x:  {ref!r}"


def manifests(pair, name):
    return [json.loads(path.read_text()) for path in files(pair, f"collections/{name}/manifest.json")]


def rows(c, name, partition_names=None):
    return sorted(
        r["id"] for r in c.query(name, filter="id >= 0", output_fields=["id"], partition_names=partition_names)
    )


def vectors(ids):
    return [{"id": i, "vector": [1.0, float(i)]} for i in ids]


def auto_schema():
    s = MilvusClient.create_schema(auto_id=True)
    s.add_field("id", DataType.INT64, is_primary=True, auto_id=True)
    s.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    return s


def test_truncate_matches_3x(pair):
    d = pair.open()
    both = Both(pair)
    d.step("create c", lambda c: c.create_collection("c", schema=schema(), index_params=index("L2")))
    d.step("partition", lambda c: c.create_partition("c", "p"))
    d.step("insert", lambda c: c.insert("c", vectors(range(3))))
    d.step("insert p", lambda c: c.insert("c", vectors([10, 11]), partition_name="p"))
    d.step("flush", lambda c: c.flush("c"))
    d.step("delete", lambda c: c.delete("c", ids=[1]))
    d.step("flush again", lambda c: c.flush("c"))
    d.step("insert unflushed", lambda c: c.insert("c", vectors([5])))
    d.step("delete unflushed", lambda c: c.delete("c", ids=[2]))
    both("CreateAlias", collection_name="c", alias="a")
    d.step("create plain", lambda c: c.create_collection("plain", schema=schema()))
    d.step("insert plain", lambda c: c.insert("plain", vectors([1])))
    before = manifests(pair, "c")[0]

    # 判定顺序：解析别名 → 集合存在（非法名字视为不存在）。
    for name in ["nope", "", "a/b", ".."]:
        assert both("TruncateCollection", collection_name=name)[:3] == (
            100, 4, f"collection {name!r} does not exist"), name
    assert both("TruncateCollection", collection_name="a")[:3] == (0, 0, "")
    same_bytes(pair, "collections/c/schema.json")
    same_bytes(pair, "aliases.json")
    rust, ref = manifests(pair, "c")
    # manifest 的 `version`：3.x 重建目录后为 1，本实现在先 flush（未提交的数据会再提交一版）之后取旧值加 1
    # （合同「清空」第 2 步）；其余字段相同。
    assert ref["version"] == 1 and rust["version"] > before["version"]
    assert {**rust, "version": 0} == {**ref, "version": 0}

    # 清空后：数据为空，load 状态、分区、索引定义与别名保留，同一进程内立即可读写。
    for method, fields in [
        ("GetCollectionStatistics", {"collection_name": "c"}),
        ("GetLoadState", {"collection_name": "c"}),
        ("DescribeIndex", {"collection_name": "c"}),
        ("DescribeCollection", {"collection_name": "a"}),
        ("DescribeAlias", {"alias": "a"}),
        ("ListAliases", {"collection_name": "c"}),
        ("GetCollectionStatistics", {"collection_name": "plain"}),
    ]:
        both(method, **fields)
    d.step("partitions", lambda c: c.list_partitions("c"))
    d.step("query", lambda c: rows(c, "c"))
    d.step("query p", lambda c: rows(c, "c", ["p"]))
    d.step("search", lambda c: c.search("c", data=[[1.0, 0.0]], limit=5))
    d.step("reinsert", lambda c: c.insert("c", vectors([1, 2])))
    d.step("reinsert p", lambda c: c.insert("c", vectors([10]), partition_name="p"))
    d.step("delete new", lambda c: c.delete("c", ids=[2]))
    d.step("query new", lambda c: rows(c, "a"))
    d.step("flush new", lambda c: c.flush("c"))
    both("GetCollectionStatistics", collection_name="c")

    # 已 released 的集合清空后仍为 released，flush 照常。
    d.step("release", lambda c: c.release_collection("c"))
    assert both("TruncateCollection", collection_name="c")[:2] == (0, 0)
    both("GetLoadState", collection_name="c")
    d.step("query released", lambda c: rows(c, "c"), error=True)
    d.step("insert released", lambda c: c.insert("c", vectors([7])))
    d.step("flush released", lambda c: c.flush("c"))
    both("GetCollectionStatistics", collection_name="c")
    same_bytes(pair, "collections/c/schema.json")

    pair.close()
    d = pair.open()
    both("ShowCollections")
    both("GetLoadState", collection_name="c")
    both("GetCollectionStatistics", collection_name="c")
    d.step("load after reopen", lambda c: c.load_collection("c"))
    d.step("query after reopen", lambda c: rows(c, "c"))
    d.step("partitions after reopen", lambda c: c.list_partitions("c"))
    d.step("plain untouched", lambda c: rows(c, "plain"))


def test_auto_id_and_unsorted_schema_match_3x(pair):
    d = pair.open()
    both = Both(pair)
    d.step("create auto", lambda c: c.create_collection("auto", schema=auto_schema()))
    d.step("insert 5", lambda c: c.insert("auto", [{"vector": [1.0, 0.0]}] * 5)["ids"])
    d.step("flush", lambda c: c.flush("auto"))
    assert both("TruncateCollection", collection_name="auto")[:2] == (0, 0)
    # auto_id 从 1 重新分配，重开后继续递增。
    d.step("insert after truncate", lambda c: c.insert("auto", [{"vector": [1.0, 0.0]}] * 3)["ids"])
    d.step("props", lambda c: c.create_collection("props", schema=schema()))
    pair.close()

    # 0.3.0rc3 写出的未排序 `schema.json`：同一进程内沿用读入的顺序，磁盘与重开后为排序顺序。
    for path in files(pair, "collections/props/schema.json"):
        value = json.loads(path.read_text())
        value["properties"] = {"z": "1", "a": "2"}
        unsorted = {key: value.pop(key) for key in ["properties", "collection_name"]}
        path.write_text(json.dumps({**unsorted, **value}))
    same_bytes(pair, "collections/props/schema.json")
    d = pair.open()
    d.step("insert after reopen", lambda c: c.insert("auto", [{"vector": [1.0, 0.0]}] * 2)["ids"])
    both("DescribeCollection", collection_name="props")
    assert both("TruncateCollection", collection_name="props")[:2] == (0, 0)
    both("DescribeCollection", collection_name="props")
    same_bytes(pair, "collections/props/schema.json")
    pair.close()
    d = pair.open()
    both("DescribeCollection", collection_name="props")


def test_out_of_scope_schema_is_unimplemented_without_writes(pair):
    d = pair.open()
    d.step("create", lambda c: c.create_collection("c", schema=schema()))
    d.step("insert", lambda c: c.insert("c", vectors([1])))
    pair.close()
    for path in files(pair, "collections/c/schema.json"):
        value = json.loads(path.read_text())
        value["properties"] = {"n": 1}
        path.write_text(json.dumps(value))
    d = pair.open()
    rust_uri, ref_uri = pair.uris
    before = sorted(p for p in Path(pair.rust_path).rglob("*") if p.is_file())
    snapshot = {p: p.read_bytes() for p in before}
    # 0006 第 14 项的范围外 schema：本实现 UNIMPLEMENTED、零写入；3.x 照常清空。
    try:
        call(rust_uri, "TruncateCollection", collection_name="c")
    except grpc.RpcError as error:
        assert error.code() == grpc.StatusCode.UNIMPLEMENTED, error
    else:
        raise AssertionError("out-of-scope truncate must be UNIMPLEMENTED")
    assert sorted(p for p in Path(pair.rust_path).rglob("*") if p.is_file()) == before
    assert all(p.read_bytes() == data for p, data in snapshot.items())
    assert call(ref_uri, "TruncateCollection", collection_name="c")[:3] == (0, 0, "")
