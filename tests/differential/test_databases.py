"""0023 B 数据库的读与属性写：ListDatabases / DescribeDatabase / AlterDatabase 对照 3.x（43d1257）。

数据库目录在两边启动之前手工建好（CreateDatabase 由后续步骤实现），覆盖合同「参照实测 / 数据库」与设计
「调查补充」中属性语义的各行：raw gRPC 比较 code、error_code、文案与属性顺序，每次写入后两边的
`database_properties.json` 逐字节相同。已登记的差异（属性文件的非字符串值与无法解析的文案、写 `timezone`）单独断言。
"""

from pathlib import Path

import grpc
import pytest
from pymilvus.grpc_gen import common_pb2, milvus_pb2, milvus_pb2_grpc

from test_reopen import pair  # noqa: F401  （fixture）
from test_scenarios import index
from test_aliases import case_insensitive, schema


def kv(*items):
    return [common_pb2.KeyValuePair(key=k, value=v) for k, v in items]


def call(uri, method, **fields):
    with grpc.insecure_channel(uri.removeprefix("http://"), options=[("grpc.enable_http_proxy", 0)]) as channel:
        stub = milvus_pb2_grpc.MilvusServiceStub(channel)
        reply = getattr(stub, method)(getattr(milvus_pb2, f"{method}Request")(**fields))
    status = reply if isinstance(reply, common_pb2.Status) else reply.status
    out = (status.code, status.error_code, status.reason)
    if method == "ListDatabases":
        out += (list(reply.db_names),)
    elif method == "DescribeDatabase":
        out += (reply.db_name, [(p.key, p.value) for p in reply.properties])
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

    def props_file(self, name):
        files = []
        for root in (self.pair.rust_path, self.pair.ref_path):
            base = Path(root) if name == "default" else Path(root) / "databases" / name
            path = base / "database_properties.json"
            files.append(path.read_bytes() if path.exists() else None)
        assert files[0] == files[1], f"{name}\n  rust: {files[0]!r}\n  3.x:  {files[1]!r}"
        return files[0]


LAYOUT = {
    "p_unsorted": b'{"z": "1", "a": "2"}',
    "p_list": b"[]",
    "p_tz": b'{"timezone": "Asia/Shanghai", "k": "v"}',
    "p_bad": b"{",
    "p_nonstr": b'{"a": 1, "b": true, "c": null}',
    "p_empty_key": b'{"": "e", "a": "1"}',
}


def make_layout(pair):
    for root in (pair.rust_path, pair.ref_path):
        databases = Path(root) / "databases"
        for name in ["d1", "hand", ".hidden"]:
            (databases / name).mkdir(parents=True)
        (databases / "d1" / "collections").mkdir()
        (databases / "file").write_text("x")
        for name, text in LAYOUT.items():
            (databases / name).mkdir()
            (databases / name / "database_properties.json").write_bytes(text)


def test_list_and_describe_match_3x(pair):
    make_layout(pair)
    d = pair.open()
    both = Both(pair)
    listed = both("ListDatabases")
    assert listed[3][0] == ".hidden" and "default" in listed[3] and "file" not in listed[3]
    for name in ["", "default", "d1", "hand", ".hidden", "p_unsorted", "p_list", "p_tz", "p_empty_key"]:
        both("DescribeDatabase", db_name=name)
    # 不存在的库、普通文件、非法名与超长名：两边完全一致。
    both("DescribeDatabase", db_name="nope")
    both("DescribeDatabase", db_name="file")
    for name in ["a/b", "x" * 256]:
        assert both("DescribeDatabase", db_name=name)[:2] == (1, 0)
    # 只差大小写的名字是已登记的差异：大小写不敏感文件系统上 3.x 按路径命中并回显请求名，本实现按精确名字报不存在；
    # 大小写敏感时两边一致。
    rust, ref = (call(uri, "DescribeDatabase", db_name="D1") for uri in pair.uris)
    if case_insensitive(pair.root):
        assert ref[:3] == (0, 0, "") and ref[3] == "D1", ref
        assert rust[:3] == (6, 6, "database 'D1' does not exist"), rust
    else:
        assert rust == ref and rust[:2] == (6, 6), (rust, ref)
    # 经元数据路由。
    for uri in pair.uris:
        with grpc.insecure_channel(uri.removeprefix("http://"), options=[("grpc.enable_http_proxy", 0)]) as channel:
            reply = milvus_pb2_grpc.MilvusServiceStub(channel).DescribeDatabase(
                milvus_pb2.DescribeDatabaseRequest(), metadata=[("dbname", "p_tz")]
            )
            assert reply.db_name == "p_tz" and [(p.key, p.value) for p in reply.properties][0][0] == "timezone"

    d.step("list_databases", lambda c: c.list_databases())
    d.step("describe d1", lambda c: c.describe_database("d1"))
    d.step("describe p_tz", lambda c: c.describe_database("p_tz"))
    d.step("describe missing", lambda c: c.describe_database("nope"), error=True)
    d.step("using_database d1", lambda c: c.using_database("d1"))
    d.step("list_collections in d1", lambda c: c.list_collections())
    d.step("create in d1", lambda c: c.create_collection("same", schema=schema(), index_params=index("L2")))
    d.step("list in d1", lambda c: c.list_collections())
    d.step("using_database missing", lambda c: c.using_database("nope"), error=True)
    d.step("list still in d1", lambda c: c.list_collections())
    d.step("using_database default", lambda c: c.using_database("default"))
    d.step("list in default", lambda c: c.list_collections())
    # 创建集合不读属性文件：3.x 的 `create_collection` 读取，文件无法解析时失败（已登记的更宽松差异）。
    both("ShowCollections", db_name="p_bad", codes_only=True)


def test_alter_database_matches_3x(pair):
    make_layout(pair)
    d = pair.open()
    both = Both(pair)

    # 默认库：没有变化不创建文件；先有效修改才创建，键序落盘，进程内按插入顺序。
    both("AlterDatabase", db_name="default")
    both("AlterDatabase", db_name="default", delete_keys=["nokey", ""])
    assert both.props_file("default") is None
    both("AlterDatabase", db_name="default", properties=kv(("zeta", "1"), ("alpha", "2"), ("", "x"), ("mid", "3"), ("zeta", "5")))
    assert both("DescribeDatabase", db_name="default")[4] == [("zeta", "5"), ("alpha", "2"), ("mid", "3")]
    both.props_file("default")
    # 设为现值、删除后重设原值：没有变化，不重写、顺序不变。
    both("AlterDatabase", db_name="default", properties=kv(("zeta", "5")))
    both("AlterDatabase", db_name="default", properties=kv(("mid", "3")), delete_keys=["mid"])
    assert both("DescribeDatabase", db_name="default")[4] == [("zeta", "5"), ("alpha", "2"), ("mid", "3")]
    # 同一请求先删后设同一键且值不同：该键移到末尾。
    both("AlterDatabase", db_name="default", properties=kv(("zeta", "9")), delete_keys=["zeta", "missing"])
    assert both("DescribeDatabase", db_name="default")[4] == [("alpha", "2"), ("mid", "3"), ("zeta", "9")]
    both.props_file("default")
    both("AlterDatabase", db_name="default", properties=kv(("键", "值"), ("empty", "")))
    both.props_file("default")

    # 手工布局：未排序文件读回按文件顺序、写出后排序；顶层非对象按空表；已有空键保留，删除按精确匹配。
    assert both("DescribeDatabase", db_name="p_unsorted")[4] == [("z", "1"), ("a", "2")]
    both("AlterDatabase", db_name="p_unsorted", properties=kv(("new", "v")))
    assert both("DescribeDatabase", db_name="p_unsorted")[4] == [("z", "1"), ("a", "2"), ("new", "v")]
    both.props_file("p_unsorted")
    both("AlterDatabase", db_name="p_list", properties=kv(("n", "v")))
    both.props_file("p_list")
    both("AlterDatabase", db_name="hand", properties=kv(("n", "v")))
    both.props_file("hand")
    both("AlterDatabase", db_name="p_empty_key", delete_keys=[""])
    both("DescribeDatabase", db_name="p_empty_key")
    both.props_file("p_empty_key")
    # 已有的 timezone 可以展示、保留与删除；写入其他属性不受影响。
    both("AlterDatabase", db_name="p_tz", properties=kv(("k2", "v2")))
    both.props_file("p_tz")
    both("AlterDatabase", db_name="p_tz", delete_keys=["timezone"])
    both.props_file("p_tz")
    both("DescribeDatabase", db_name="p_tz")

    # 不存在的库先于属性判定。
    both("AlterDatabase", db_name="nope", properties=kv(("k", "v")))
    both("AlterDatabase", db_name="file", properties=kv(("k", "v")))

    # 各库互不影响；经元数据路由。
    assert both("DescribeDatabase", db_name="d1")[4] == []
    d.step("alter via client", lambda c: c.alter_database_properties("d1", {"owner": "x", "tier": "1"}))
    d.step("describe d1", lambda c: c.describe_database("d1"))
    d.step("drop props via client", lambda c: c.drop_database_properties("d1", ["owner", "missing"]))
    d.step("describe d1 again", lambda c: c.describe_database("d1"))
    both.props_file("d1")
    pair.close()
    pair.open()
    both("DescribeDatabase", db_name="default")
    both("DescribeDatabase", db_name="d1")
    both("DescribeDatabase", db_name="p_unsorted")


def test_registered_differences(pair):
    make_layout(pair)
    pair.open()
    rust, ref = pair.uris
    # 属性文件无法解析：两边 Describe / Alter 都是 1/0，文案不同；创建与列出集合不读它。
    for method, fields in [("DescribeDatabase", {"db_name": "p_bad"}), ("AlterDatabase", {"db_name": "p_bad", "properties": kv(("k", "v"))})]:
        a, b = call(rust, method, **fields), call(ref, method, **fields)
        assert a[:2] == b[:2] == (1, 0), (a, b)
    # 非字符串值：3.x 按 Python str() 展示，本实现报 1/0 且不写文件。
    a = call(rust, "DescribeDatabase", db_name="p_nonstr")
    b = call(ref, "DescribeDatabase", db_name="p_nonstr")
    assert a[:2] == (1, 0) and b[:2] == (0, 0)
    assert b[4] == [("a", "1"), ("b", "True"), ("c", "None")]
    before = (Path(pair.rust_path) / "databases/p_nonstr/database_properties.json").read_bytes()
    assert call(rust, "AlterDatabase", db_name="p_nonstr", properties=kv(("k", "v")))[:2] == (1, 0)
    assert (Path(pair.rust_path) / "databases/p_nonstr/database_properties.json").read_bytes() == before
    # 写 timezone：3.x 校验后保存，本实现 UNIMPLEMENTED 且零写入（取舍 1）；判定在数据库存在之后。
    assert call(ref, "AlterDatabase", db_name="hand", properties=kv(("timezone", "UTC")))[:2] == (0, 0)
    with pytest.raises(grpc.RpcError) as info:
        call(rust, "AlterDatabase", db_name="hand", properties=kv(("timezone", "UTC")))
    assert info.value.code() == grpc.StatusCode.UNIMPLEMENTED
    assert not (Path(pair.rust_path) / "databases/hand/database_properties.json").exists()
    assert call(rust, "AlterDatabase", db_name="nope", properties=kv(("timezone", "UTC")))[:2] == (6, 6)
    assert call(ref, "AlterDatabase", db_name="nope", properties=kv(("timezone", "UTC")))[:2] == (6, 6)
