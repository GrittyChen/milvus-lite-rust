"""0023 A 别名：合同「参照实测 / 别名」逐行对照 3.x（43d1257）。

raw gRPC 比较 code、error_code、文案与响应字段，MilvusClient 覆盖客户端的解析；每次写入后两边的
`aliases.json` 逐字节相同。预期差异单独断言。
"""

import os
from pathlib import Path

import grpc
import pytest
from pymilvus import DataType, MilvusClient
from pymilvus.grpc_gen import common_pb2, milvus_pb2 as rpc, milvus_pb2_grpc

from compare import hits
from test_reopen import pair  # noqa: F401  （fixture）
from test_scenarios import index


def schema():
    s = MilvusClient.create_schema()
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    return s


def call(uri, method, **fields):
    """直接发请求，结果归一为 (code, error_code, reason, 响应字段...)。"""
    with grpc.insecure_channel(uri.removeprefix("http://"), options=[("grpc.enable_http_proxy", 0)]) as channel:
        reply = getattr(milvus_pb2_grpc.MilvusServiceStub(channel), method)(getattr(rpc, f"{method}Request")(**fields))
    status = reply if isinstance(reply, common_pb2.Status) else reply.status
    out = (status.code, status.error_code, status.reason)
    if method == "DescribeAlias":
        out += (reply.db_name, reply.alias, reply.collection)
    elif method == "ListAliases":
        out += (reply.db_name, reply.collection_name, list(reply.aliases))
    elif method == "DescribeCollection":
        out += (reply.collection_name, list(reply.aliases), [(kv.key, kv.value) for kv in reply.properties])
    elif method == "ShowCollections":
        out += (list(reply.collection_names),)
    elif method == "GetCollectionStatistics":
        out += ([(kv.key, kv.value) for kv in reply.stats],)
    elif method == "GetLoadState":
        out += (reply.state,)
    elif method == "DescribeIndex":
        out += ([d.field_name for d in reply.index_descriptions],)
    return out


class Both:
    def __init__(self, pair):
        self.pair = pair

    def __call__(self, method, codes_only=False, **fields):
        rust, ref = (call(uri, method, **fields) for uri in self.pair.uris)
        if codes_only:
            rust, ref = rust[:2], ref[:2]
        assert rust == ref, f"{method} {fields!r}\n  rust: {rust!r}\n  3.x:  {ref!r}"
        return ref

    def files_equal(self):
        rust, ref = (Path(root) / "aliases.json" for root in (self.pair.rust_path, self.pair.ref_path))
        assert rust.exists() == ref.exists()
        if ref.exists():
            assert rust.read_bytes() == ref.read_bytes()

    def write_file(self, data):
        for root in (self.pair.rust_path, self.pair.ref_path):
            (Path(root) / "aliases.json").write_bytes(data)


def case_insensitive(root):
    probe = Path(root) / "Case-Probe"
    probe.mkdir()
    try:
        return (Path(root) / "case-probe").exists()
    finally:
        probe.rmdir()


def test_alias_rpcs_and_resolution_match_3x(pair):
    d = pair.open()
    both = Both(pair)
    for name in ["c1", "c2"]:
        d.step(f"create {name}", lambda c: c.create_collection(name, schema=schema(), index_params=index("L2")))
    d.step("create c3", lambda c: c.create_collection("c3", schema=schema()))
    d.step("insert c1", lambda c: c.insert("c1", [{"id": 1, "vector": [1.0, 0.0]}]))
    d.step("insert c2", lambda c: c.insert("c2", [{"id": 2, "vector": [0.0, 1.0]}, {"id": 3, "vector": [1.0, 1.0]}]))
    for root in (pair.rust_path, pair.ref_path):
        (Path(root) / "collections" / "residue").mkdir()

    # 判定顺序：解析目标 → 别名名字 → 目标存在 → 与集合冲突 → 别名已存在。
    for bad in ["a/b", "a\\b", "", ".", "..", "x\x00y", "a" * 256]:
        for target in ["c1", "nope"]:
            assert both("CreateAlias", collection_name=target, alias=bad)[:2] == (1, 0)
    assert both("CreateAlias", collection_name="nope", alias="a1")[:2] == (100, 4)
    assert both("CreateAlias", collection_name="residue", alias="a1")[:2] == (100, 4)
    assert both("CreateAlias", collection_name="c1", alias="c2")[:2] == (1, 1)
    # 只差大小写的集合：大小写不敏感文件系统上两边都报冲突，否则都成功。
    both("CreateAlias", collection_name="c1", alias="C2")
    both("DropAlias", alias="C2")
    both("CreateAlias", collection_name="c1", alias="residue")
    both("CreateAlias", collection_name="c1", alias="a1")
    both.files_equal()
    for target in ["c1", "c2"]:
        assert both("CreateAlias", collection_name=target, alias="a1")[:2] == (1, 1)
    both("CreateAlias", collection_name="a1", alias="a2")
    assert both("DescribeAlias", alias="a2")[-1] == "c1"
    both.files_equal()

    for alias in ["zz", "a/b"]:
        assert both("AlterAlias", collection_name="nope", alias=alias)[:2] == (100, 4)
    assert both("AlterAlias", collection_name="nope", alias="a1")[:2] == (100, 4)
    both("AlterAlias", collection_name="c2", alias="a1")
    both("AlterAlias", collection_name="a1", alias="a2")
    both.files_equal()
    assert both("DescribeAlias", alias="zz")[:2] == (100, 4)
    both("ListAliases")
    both("ListAliases", collection_name="c2")
    both("ListAliases", collection_name="a1")
    both("ListAliases", collection_name="nope")
    for alias in ["zz", "a/b", "residue"]:
        both("DropAlias", alias=alias)
    both.files_equal()

    # 经别名访问的集合 RPC 作用于目标集合。
    both("DescribeCollection", collection_name="a1")
    both("DescribeCollection", collection_name="c2")
    both("GetCollectionStatistics", collection_name="a1")
    both("GetLoadState", collection_name="a1")
    both("ShowCollections", collection_names=["a1"])
    both("ShowCollections")
    both("DescribeIndex", collection_name="a1")
    both("CreateAlias", collection_name="c3", alias="a3")
    assert both("DescribeIndex", collection_name="a3")[:2] == (700, 11)
    d.step("query", lambda c: c.query("a1", filter="id >= 0", output_fields=["id"]))
    d.step("search", lambda c: c.search("a1", [[1.0, 0.0]], limit=2, output_fields=["id"]),
           check=hits("L2", [[1.0, 0.0]], {2: [0.0, 1.0], 3: [1.0, 1.0]}, k=2))
    d.step("insert", lambda c: c.insert("a1", [{"id": 4, "vector": [0.5, 0.5]}]))
    d.step("get", lambda c: c.get("c2", ids=[4], output_fields=["id"]))
    d.step("create as alias", lambda c: c.create_collection("a1", schema=schema()), error=True)
    d.step("client describe", lambda c: c.describe_alias("a2"))
    d.step("client list", lambda c: c.list_aliases("c2"))
    d.step("client alter", lambda c: c.alter_alias("c1", "a2"))
    d.step("client drop", lambda c: c.drop_alias("a2"))
    d.step("client create", lambda c: c.create_alias("c1", "a2"))
    both.files_equal()

    # 经别名 drop 删除目标集合及指向它的全部别名。
    both("CreateAlias", collection_name="c2", alias="b2")
    both("DropCollection", collection_name="a1")
    both("ListAliases")
    both("ShowCollections")
    both.files_equal()


def test_dangling_aliases_existing_files_and_reopen_match_3x(pair):
    d = pair.open()
    both = Both(pair)
    d.step("create", lambda c: c.create_collection("c", schema=schema(), index_params=index("L2")))
    pair.close()
    both.write_file(b'{"ghost":"missing","real":"c","bad":1}')
    pair.open()
    both("ListAliases")
    both("DescribeAlias", alias="ghost")
    assert both("DescribeCollection", collection_name="ghost")[:3] == (100, 4, "collection 'missing' does not exist")
    both("CreateAlias", collection_name="real", alias="new")
    both("AlterAlias", collection_name="c", alias="real")
    both("DropAlias", alias="new")
    # 别名 RPC 保留悬空别名，并按 3.x 的排序写出。
    both.files_equal()
    pair.close()
    pair.open()
    both("ListAliases")
    both("DescribeCollection", collection_name="real")

    pair.close()
    both.write_file(b"[]")
    pair.open()
    both("ListAliases")
    both("CreateAlias", collection_name="c", alias="x")
    both.files_equal()

    pair.close()
    both.write_file(b"{")
    pair.open()
    # 无法解析时所有需要解析别名的 RPC 都失败（1/0）；文案分别是 Python 与本实现的解析器原文。
    for method, fields in [
        ("DescribeCollection", {"collection_name": "c"}),
        ("GetCollectionStatistics", {"collection_name": "c"}),
        ("ShowCollections", {}),
        ("DropCollection", {"collection_name": "c"}),
        ("CreateAlias", {"collection_name": "c", "alias": "y"}),
        ("AlterAlias", {"collection_name": "c", "alias": "x"}),
        ("DropAlias", {"alias": "x"}),
        ("DescribeAlias", {"alias": "x"}),
        ("ListAliases", {}),
    ]:
        assert both(method, codes_only=True, **fields) == (1, 0), method
    for root in (pair.rust_path, pair.ref_path):
        assert (Path(root) / "aliases.json").read_bytes() == b"{"
        assert (Path(root) / "collections" / "c" / "schema.json").exists()


def test_alias_target_differing_only_in_case(pair):
    d = pair.open()
    d.step("create", lambda c: c.create_collection("c1", schema=schema(), index_params=index("L2")))
    rust, ref = (call(uri, "CreateAlias", collection_name="C1", alias="a") for uri in pair.uris)
    assert rust[:3] == (100, 4, "collection 'C1' does not exist")
    if case_insensitive(pair.ref_path):
        # 预期差异（合同「有意的请求语义差异」）：3.x 按路径命中并记下 'C1'，本实现只认精确名字。
        assert ref[:2] == (0, 0)
    else:
        assert ref == rust
