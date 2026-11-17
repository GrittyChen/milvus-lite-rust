"""HasCollection 与 CheckHealth 对照 3.x：raw gRPC 比较 code、error_code、文案与返回值。

3.x 的 HasCollection 依次确认数据库、解析别名，再看 `<集合目录>/schema.json` 是否存在，不打开集合；
只有 `except Exception` 分支（1/0）。按拼出的路径判断带来的差异已登记（0006 差异表）：含路径分隔符或只差大小写
（大小写不敏感文件系统）的名字、符号链接目录与作为目录的 `schema.json`，本实现与 DescribeCollection 一样按不存在处理。
"""

from pathlib import Path

import grpc
from pymilvus.grpc_gen import milvus_pb2, milvus_pb2_grpc

from test_reopen import pair  # noqa: F401  （fixture）
from test_scenarios import index
from test_aliases import case_insensitive, schema


def has(uri, name, db_name="", metadata=None, **fields):
    with grpc.insecure_channel(uri.removeprefix("http://"), options=[("grpc.enable_http_proxy", 0)]) as channel:
        reply = milvus_pb2_grpc.MilvusServiceStub(channel).HasCollection(
            milvus_pb2.HasCollectionRequest(collection_name=name, db_name=db_name, **fields), metadata=metadata
        )
    return (reply.status.code, reply.status.error_code, reply.status.reason, reply.value)


def make_layout(pair):
    for root in (pair.rust_path, pair.ref_path):
        collections = Path(root) / "collections"
        (collections / "residue").mkdir(parents=True)
        (collections / "broken").mkdir()
        (collections / "broken" / "schema.json").write_bytes(b"{")
        (collections / "plain").write_text("x")
        bad = Path(root) / "databases" / "bad_aliases"
        (bad / "collections").mkdir(parents=True)
        (bad / "aliases.json").write_bytes(b"{")
        (Path(root) / "databases" / "d1" / "collections").mkdir(parents=True)
        (collections / "outer" / "inner").mkdir(parents=True)
        (collections / "outer" / "inner" / "schema.json").write_bytes(b"{}")
        (collections / "dirschema" / "schema.json").mkdir(parents=True)


def test_has_collection_matches_3x(pair):
    make_layout(pair)
    d = pair.open()
    d.step("create c", lambda c: c.create_collection("c", schema=schema(), index_params=index("L2")))
    d.step("create gone", lambda c: c.create_collection("gone", schema=schema(), index_params=index("L2")))
    d.step("alias a -> c", lambda c: c.create_alias("c", "a"))
    d.step("alias g -> gone", lambda c: c.create_alias("gone", "g"))
    d.step("drop gone", lambda c: c.drop_collection("gone"))
    d.step("use d1", lambda c: c.using_database("d1"))
    d.step("create d1.x", lambda c: c.create_collection("x", schema=schema(), index_params=index("L2")))

    def both(name, **kw):
        rust, ref = (has(uri, name, **kw) for uri in pair.uris)
        assert rust == ref, f"{name!r} {kw!r}\n  rust: {rust!r}\n  3.x:  {ref!r}"
        return ref

    assert both("c") == (0, 0, "", True)
    assert both("a") == (0, 0, "", True)
    # 打不开的集合也存在：只看 schema.json，不打开。
    assert both("broken") == (0, 0, "", True)
    # 被删集合的别名随之删除；没有 schema.json 的残留目录、普通文件、不合法或超长的名字都不存在。
    for name in ["gone", "g", "nope", "residue", "plain", "", ".", "..", "a/b", "x" * 256]:
        assert both(name) == (0, 0, "", False), name
    # time_stamp 不读。
    assert both("c", time_stamp=1) == (0, 0, "", True)

    # 数据库路由：请求字段优先于元数据；集合在各库内独立。
    assert both("x", db_name="d1") == (0, 0, "", True)
    assert both("c", db_name="d1") == (0, 0, "", False)
    assert both("x", metadata=[("dbname", "d1")]) == (0, 0, "", True)
    assert both("x", db_name="default", metadata=[("dbname", "d1")]) == (0, 0, "", False)
    # 数据库不存在、库名不合法：一律 1/0，value 为 false。
    assert both("c", db_name="nodb") == (1, 0, "database 'nodb' does not exist", False)
    assert both("c", db_name="a/b")[:2] == (1, 0)
    # 别名文件无法解析时的文案来自各自的 JSON 解析器（0023 已登记），只比较 code 与返回值。
    rust, ref = (has(uri, "c", db_name="bad_aliases") for uri in pair.uris)
    assert rust[:2] == ref[:2] == (1, 0) and rust[3] is ref[3] is False, (rust, ref)

    # 已登记的差异：3.x 按拼出的路径判断。
    for name in ["../collections/c", "outer/inner", "c/", "dirschema", "link"]:
        if name == "link":
            for root in (pair.rust_path, pair.ref_path):
                (Path(root) / "collections" / "link").symlink_to(Path(root) / "collections" / "c")
        rust, ref = (has(uri, name) for uri in pair.uris)
        assert ref == (0, 0, "", True) and rust == (0, 0, "", False), (name, rust, ref)
    rust, ref = (has(uri, "C") for uri in pair.uris)
    if case_insensitive(pair.root):
        assert ref == (0, 0, "", True) and rust == (0, 0, "", False), (rust, ref)
    else:
        assert rust == ref == (0, 0, "", False), (rust, ref)

    # SDK 的 has_collection 走 DescribeCollection，结果与 HasCollection 一致。
    d.step("use default", lambda c: c.using_database("default"))
    for name in ["c", "a", "gone", "nope"]:
        assert d.step(f"sdk has {name}", lambda c: c.has_collection(name))[1] == (name in ("c", "a"))


def test_check_health_matches_3x(pair):
    pair.open()
    replies = []
    for uri in pair.uris:
        with grpc.insecure_channel(uri.removeprefix("http://"), options=[("grpc.enable_http_proxy", 0)]) as channel:
            reply = milvus_pb2_grpc.MilvusServiceStub(channel).CheckHealth(milvus_pb2.CheckHealthRequest())
        replies.append(
            (reply.status.code, reply.status.error_code, reply.status.reason, reply.isHealthy,
             list(reply.reasons), len(reply.quota_states))
        )
    assert replies[0] == replies[1] == (0, 0, "", True, [], 0), replies
