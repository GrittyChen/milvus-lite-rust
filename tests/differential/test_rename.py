"""0023 A RenameCollection：合同「参照实测 / 改名」逐行对照 3.x（43d1257）。

raw gRPC 比较 code、error_code 与文案，以及判定顺序；成功的改名之后两边的 `schema.json` 与 `aliases.json`
逐字节相同，load 状态与重开后的结果一致。`newDBName`、残留目录与悬空别名作为新名的预期差异单独断言，
本实现一侧零写入。
"""

from pathlib import Path

import grpc
import pytest

from test_aliases import Both, call, case_insensitive, schema
from test_reopen import pair  # noqa: F401  （fixture）
from test_scenarios import index


def files(pair, relative):
    return [Path(root) / relative for root in (pair.rust_path, pair.ref_path)]


def same_bytes(pair, relative):
    rust, ref = (path.read_bytes() for path in files(pair, relative))
    assert rust == ref, f"{relative}\n  rust: {rust!r}\n  3.x:  {ref!r}"


def tree(root):
    root = Path(root)
    return {
        path.relative_to(root): None if path.is_dir() else path.read_bytes()
        for path in sorted(root.rglob("*"))
    }


def rows(c, name):
    return sorted(r["id"] for r in c.query(name, filter="id >= 0", output_fields=["id"]))


def test_rename_matches_3x(pair):
    d = pair.open()
    both = Both(pair)
    d.step("create c", lambda c: c.create_collection("c", schema=schema(), index_params=index("L2")))
    d.step("create plain", lambda c: c.create_collection("plain", schema=schema()))
    d.step("insert", lambda c: c.insert("c", [{"id": i, "vector": [1.0, float(i)]} for i in range(3)]))
    for alias in ["a", "b"]:
        both("CreateAlias", collection_name="c", alias=alias)
    both("CreateAlias", collection_name="plain", alias="z")

    # 判定顺序：解析旧名 → 旧名存在（100/4）→ 新名合法（1/0）→ 新名冲突（1/1）。
    cases = [
        ("nope", "bad/x", (100, 4, "collection 'nope' does not exist")),
        ("", "x", (100, 4, "collection '' does not exist")),
        ("a/b", "x", (100, 4, "collection 'a/b' does not exist")),
        ("c", "bad/x", (1, 0, "invalid collection name: 'bad/x'")),
        ("c", "", (1, 0, "collection name must not be empty")),
        ("c", "x" * 256, (1, 0, "collection name too long (256 bytes, max 255)")),
        ("c", "plain", (1, 1, "collection 'plain' already exists")),
        ("c", "z", (1, 1, "collection 'z' already exists")),
        ("c", "a", (1, 1, "collection 'a' already exists")),
        ("c", "c", (1, 1, "collection 'c' already exists")),
        ("a", "c", (1, 1, "collection 'c' already exists")),
    ]
    if case_insensitive(pair.ref_path) and case_insensitive(pair.rust_path):
        cases.append(("c", "C", (1, 1, "collection 'C' already exists")))
    for old, new, expected in cases:
        assert both("RenameCollection", oldName=old, newName=new)[:3] == expected, (old, new)
    both.files_equal()

    # 经别名改名：指向旧名的别名改指新名；有索引定义的集合按重开为 released（101），需重新 load。
    both("GetLoadState", collection_name="c")
    assert both("RenameCollection", oldName="a", newName="d")[:2] == (0, 0)
    both.files_equal()
    same_bytes(pair, "collections/d/schema.json")
    both("ShowCollections")
    for alias in ["a", "b", "z"]:
        both("DescribeAlias", alias=alias)
    both("ListAliases", collection_name="d")
    both("ListAliases", collection_name="c")
    both("DescribeCollection", collection_name="c")
    both("DescribeCollection", collection_name="a")
    both("GetLoadState", collection_name="d")
    d.step("has", lambda c: (c.has_collection("c"), c.has_collection("d")))
    d.step("query released", lambda c: rows(c, "d"), error=True)
    d.step("load", lambda c: c.load_collection("d"))
    d.step("query", lambda c: rows(c, "a"))
    d.step("insert renamed", lambda c: c.insert("d", [{"id": 9, "vector": [0.0, 1.0]}]))
    both("GetCollectionStatistics", collection_name="d")

    # 没有索引定义的集合改名后可直接读；客户端入口与 raw gRPC 结果相同。
    d.step("client rename", lambda c: c.rename_collection("plain", "p2"))
    both("GetLoadState", collection_name="p2")
    d.step("query plain", lambda c: rows(c, "p2"))
    both.files_equal()
    same_bytes(pair, "collections/p2/schema.json")
    d.step("rename back", lambda c: c.rename_collection("d", "c"))
    both.files_equal()
    same_bytes(pair, "collections/c/schema.json")
    d.step("recreate old name", lambda c: c.create_collection("d", schema=schema()))

    pair.close()
    d = pair.open()
    both("ShowCollections")
    both("GetLoadState", collection_name="c")
    d.step("load after reopen", lambda c: c.load_collection("c"))
    d.step("query after reopen", lambda c: rows(c, "b"))
    for alias in ["a", "b", "z"]:
        both("DescribeAlias", alias=alias)


def test_first_rename_writes_empty_aliases_and_expected_differences(pair):
    d = pair.open()
    both = Both(pair)
    for name in ["c", "e", "f", "g"]:
        d.step(f"create {name}", lambda c, name=name: c.create_collection(name, schema=schema()))
    # 3.x 改名总会写出 `aliases.json`，没有别名时为 `{}`。
    assert not any(path.exists() for path in files(pair, "aliases.json"))
    assert both("RenameCollection", oldName="c", newName="c2")[:2] == (0, 0)
    both.files_equal()
    assert files(pair, "aliases.json")[0].read_bytes() == b"{}"

    for root in (pair.rust_path, pair.ref_path):
        (Path(root) / "collections/empty_residue").mkdir()
        (Path(root) / "collections/full_residue").mkdir()
        (Path(root) / "collections/full_residue/x").write_bytes(b"x")
    pair.close()
    for path in files(pair, "aliases.json"):
        path.write_bytes(b'{"ghost": "missing"}')
    d = pair.open()
    rust_uri, ref_uri = pair.uris
    before = tree(pair.rust_path)

    def unimplemented(**fields):
        with pytest.raises(grpc.RpcError) as info:
            call(rust_uri, "RenameCollection", **fields)
        assert info.value.code() == grpc.StatusCode.UNIMPLEMENTED
        target = fields["newDBName"]
        assert info.value.details() == (
            f"MilvusLite does not support cross-database rename: renaming a collection into database {target!r} is not supported"
        ), info.value.details()

    # `newDBName` 非空且不等于当前数据库（取舍 4）：本实现在旧名判定之前 UNIMPLEMENTED；3.x 忽略该字段在当前库改名。
    unimplemented(oldName="nope", newName="x", newDBName="other")
    assert call(ref_uri, "RenameCollection", oldName="nope", newName="x", newDBName="other")[:2] == (100, 4)
    unimplemented(oldName="e", newName="e2", newDBName="other")
    assert call(ref_uri, "RenameCollection", oldName="e", newName="e2", newDBName="other")[:3] == (0, 0, "")
    rust_client, ref_client = pair.clients
    with pytest.raises(grpc.RpcError) as info:
        rust_client.rename_collection("f", "f2", target_db="other_db")
    assert info.value.code() == grpc.StatusCode.UNIMPLEMENTED
    ref_client.rename_collection("f", "f2", target_db="other_db")
    assert ref_client.has_collection("f2") and not ref_client.has_collection("f")

    # 新名是没有 `schema.json` 的残留目录：3.x 空目录被覆盖并成功、非空目录 1/0；本实现一律 1/1、不覆盖。
    for residue, ref_code in [("empty_residue", (0, 0)), ("full_residue", (1, 0))]:
        assert call(rust_uri, "RenameCollection", oldName="g", newName=residue)[:3] == (
            1, 1, f"collection {residue!r} already exists")
        ref = call(ref_uri, "RenameCollection", oldName="g", newName=residue)
        assert ref[:2] == ref_code, ref
        if ref_code == (0, 0):
            assert call(ref_uri, "RenameCollection", oldName=residue, newName="g")[:2] == (0, 0)

    # 新名是悬空别名：3.x 只判目标存在而照常改名，之后该名字被别名遮住、按名字不可访问；本实现报已存在。
    assert call(rust_uri, "RenameCollection", oldName="g", newName="ghost")[:3] == (
        1, 1, "collection 'ghost' already exists")
    assert call(ref_uri, "RenameCollection", oldName="g", newName="ghost")[:2] == (0, 0)
    assert call(ref_uri, "DescribeCollection", collection_name="ghost")[:3] == (
        100, 4, "collection 'missing' does not exist")
    # 本实现：数据目录零写入，跨库请求的同名集合未被改名。
    assert tree(pair.rust_path) == before
    assert rust_client.has_collection("e") and rust_client.has_collection("f") and rust_client.has_collection("g")
    # `newDBName` 为空或等于当前数据库时照常改名。
    for target in ["", "default"]:
        assert call(rust_uri, "RenameCollection", oldName="e", newName=f"e_{target}", newDBName=target)[:2] == (0, 0)
        assert call(rust_uri, "RenameCollection", oldName=f"e_{target}", newName="e", newDBName=target)[:2] == (0, 0)
