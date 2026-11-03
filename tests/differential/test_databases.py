"""0023 B 数据库的读与属性写：ListDatabases / DescribeDatabase / AlterDatabase 对照 3.x（43d1257）。

数据库目录在两边启动之前手工建好（CreateDatabase 由后续步骤实现），覆盖合同「参照实测 / 数据库」与设计
「调查补充」中属性语义的各行：raw gRPC 比较 code、error_code、文案与属性顺序，每次写入后两边的
`database_properties.json` 逐字节相同。已登记的差异（属性文件的非字符串值与无法解析的文案）单独断言；`timezone` 自 0025 起与 3.x 一致。
"""

import os
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


def test_unknown_timezone_in_the_properties_file_matches_3x(pair):
    for root in (pair.rust_path, pair.ref_path):
        (Path(root) / "databases/badtz").mkdir(parents=True)
        (Path(root) / "databases/badtz/database_properties.json").write_bytes(b'{"timezone": "Mars/Base", "k": "v"}')
    pair.open()
    both = Both(pair)
    # 3.x 加载属性时就校验 timezone：Describe 与任何 Alter（包括删除或改成合法值）都报 6。
    assert both("DescribeDatabase", db_name="badtz")[:3] == (6, 6, "unknown timezone 'Mars/Base'")
    assert both("AlterDatabase", db_name="badtz", properties=kv(("other", "x")))[:2] == (6, 6)
    assert both("AlterDatabase", db_name="badtz", properties=kv(("timezone", "UTC")))[:2] == (6, 6)
    assert both("AlterDatabase", db_name="badtz", delete_keys=["timezone"])[:2] == (6, 6)


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
    # timezone（0025 起与 3.x 一致）：先判数据库存在，再校验值；坏值零写入，合法值原样保存。
    for zone, expected in [
        ("Mars/Base", (6, 6, "unknown timezone 'Mars/Base'")),
        ("", (6, 6, "timezone property must be a non-empty string")),
        ("UTC", (0, 0, "")),
    ]:
        a = call(rust, "AlterDatabase", db_name="hand", properties=kv(("timezone", zone)))
        b = call(ref, "AlterDatabase", db_name="hand", properties=kv(("timezone", zone)))
        assert a[:3] == b[:3] == expected, (zone, a, b)
        files = [Path(root) / "databases/hand/database_properties.json" for root in (pair.rust_path, pair.ref_path)]
        assert files[0].exists() == files[1].exists()
        if files[1].exists():
            assert files[0].read_bytes() == files[1].read_bytes()
    assert call(rust, "AlterDatabase", db_name="nope", properties=kv(("timezone", "Mars/Base")))[:2] == (6, 6)
    assert call(ref, "AlterDatabase", db_name="nope", properties=kv(("timezone", "Mars/Base")))[:2] == (6, 6)
    assert call(rust, "AlterDatabase", db_name="nope", properties=kv(("timezone", "Mars/Base")))[2] == "database 'nope' does not exist"


def tree(root):
    return sorted(str(p.relative_to(root)) + ("/" if p.is_dir() else "") for p in Path(root).rglob("*"))


def test_create_database_matches_3x(pair):
    d = pair.open()
    both = Both(pair)
    # 成功：属性解码（空 key 丢弃、同名保留首位取末值、空值保留、非 ASCII），落盘字节与目录形状两边相同。
    both("CreateDatabase", db_name="d1", properties=kv(("zz", "1"), ("", "dropped"), ("aa", ""), ("zz", "2"), ("键", "值")))
    both("CreateDatabase", db_name="d2")
    for name in ["d1", "d2"]:
        assert both("DescribeDatabase", db_name=name)[3] == name
        both.props_file(name)
    assert both("DescribeDatabase", db_name="d1")[4] == [("zz", "2"), ("aa", ""), ("键", "值")]
    assert both.props_file("d2") == b"{}"
    rust_tree = tree(Path(pair.rust_path) / "databases")
    assert rust_tree == tree(Path(pair.ref_path) / "databases")
    assert "d1/collections/" in rust_tree and "d1/database_properties.json" in rust_tree
    assert tree(Path(pair.rust_path) / ".database-staging") == tree(Path(pair.ref_path) / ".database-staging") == []

    # 已存在与名字校验：两边完全一致（含文案）。空名即 default。
    for name in ["default", "", "d1"]:
        assert both("CreateDatabase", db_name=name)[:2] == (6, 6)
    assert both("CreateDatabase", db_name="d1")[2] == "database 'd1' already exists"
    both("CreateDatabase", db_name="d1", properties=kv(("timezone", "UTC")))
    for name in ["a/b", "a\\b", ".", "..", "x" * 256]:
        assert both("CreateDatabase", db_name=name)[:2] == (1, 0), name
    for name in [" sp ", "a:b", "库一", "y" * 255]:
        assert both("CreateDatabase", db_name=name)[:2] == (0, 0), name
    if case_insensitive(pair.root):
        both("CreateDatabase", db_name="D1")
    # CreateDatabase 只读请求体：dbname 元数据被忽略，空名仍是 default。
    for uri in pair.uris:
        with grpc.insecure_channel(uri.removeprefix("http://"), options=[("grpc.enable_http_proxy", 0)]) as channel:
            stub = milvus_pb2_grpc.MilvusServiceStub(channel)
            via_metadata = stub.CreateDatabase(milvus_pb2.CreateDatabaseRequest(db_name="m1"), metadata=[("dbname", "d1")])
            assert (via_metadata.code, via_metadata.error_code) == (0, 0)
            ignored = stub.CreateDatabase(milvus_pb2.CreateDatabaseRequest(), metadata=[("dbname", "fresh")])
            assert (ignored.code, ignored.error_code) == (6, 6)
    names = both("ListDatabases")[3]
    assert names == sorted(names) and "default" in names and "m1" in names

    # 新库内的集合、别名、同名隔离与 SDK 入口。
    d.step("create in default", lambda c: c.create_collection("same", schema=schema(), index_params=index("L2")))
    d.step("using d1", lambda c: c.using_database("d1"))
    d.step("create in d1", lambda c: c.create_collection("same", schema=schema(), index_params=index("L2")))
    d.step("insert in d1", lambda c: c.insert("same", [{"id": 7, "vector": [1.0, 0.0]}]))
    d.step("alias in d1", lambda c: c.create_alias("same", "al"))
    d.step("query alias", lambda c: c.query("al", filter="id >= 0", output_fields=["id"]))
    d.step("list in d1", lambda c: c.list_collections())
    d.step("using default", lambda c: c.using_database("default"))
    d.step("alias missing in default", lambda c: c.describe_alias("al"), error=True)
    d.step("query default empty", lambda c: c.query("same", filter="id >= 0", output_fields=["id"]))
    d.step("create_database via client", lambda c: c.create_database("viaclient", properties={"owner": "x"}))
    d.step("describe viaclient", lambda c: c.describe_database("viaclient"))
    d.step("list_databases", lambda c: c.list_databases())
    both.props_file("viaclient")

    # 重开：数据库、属性（文件键序）、别名与其中的集合照常可见。
    pair.close()
    pair.open()
    for name in ["d1", "d2", "viaclient", "m1"]:
        both("DescribeDatabase", db_name=name)
        both.props_file(name)
    both("ListDatabases")
    both("ShowCollections", db_name="d1")
    both("ListAliases", db_name="d1")


def test_create_database_registered_differences(pair):
    pair.open()
    rust, ref = pair.uris
    # 目标路径被普通文件占用：3.x 的 os.rename 失败（1/0，文案带绝对路径），本实现报已存在（6/6），不覆盖。
    for root in (pair.rust_path, pair.ref_path):
        (Path(root) / "databases").mkdir(exist_ok=True)
        (Path(root) / "databases" / "afile").write_bytes(b"keep")
    assert call(ref, "CreateDatabase", db_name="afile")[:2] == (1, 0)
    assert call(rust, "CreateDatabase", db_name="afile") == (6, 6, "database 'afile' already exists")
    assert (Path(pair.rust_path) / "databases/afile").read_bytes() == b"keep"
    # 写 timezone（0025 起与 3.x 一致）：合法值创建并保存，坏值 6/6 且不创建。
    for name, zone, expected in [
        ("tz", "UTC", (0, 0, "")),
        ("tzbad", "Mars/Base", (6, 6, "unknown timezone 'Mars/Base'")),
        ("tzempty", "", (6, 6, "timezone property must be a non-empty string")),
    ]:
        a = call(rust, "CreateDatabase", db_name=name, properties=kv(("timezone", zone)))
        b = call(ref, "CreateDatabase", db_name=name, properties=kv(("timezone", zone)))
        assert a[:3] == b[:3] == expected, (name, a, b)
        assert (Path(pair.rust_path) / "databases" / name).exists() == (expected[0] == 0)


def test_drop_database_matches_3x(pair):
    # 手工布局：残留目录、悬空别名、损坏的属性与别名文件都不阻止删除。
    for root in (pair.rust_path, pair.ref_path):
        databases = Path(root) / "databases"
        residue = databases / "residue"
        (residue / "collections" / "junk").mkdir(parents=True)
        (residue / "aliases.json").write_bytes(b'{"dangling": "nope"}')
        (residue / "database_properties.json").write_bytes(b"{")
        (databases / "badalias" / "collections").mkdir(parents=True)
        (databases / "badalias" / "aliases.json").write_bytes(b"{")
    d = pair.open()
    both = Both(pair)
    both("CreateDatabase", db_name="d1", properties=kv(("k", "v")))
    both("CreateDatabase", db_name="empty")
    d.step("using d1", lambda c: c.using_database("d1"))
    d.step("create in d1", lambda c: c.create_collection("same", schema=schema(), index_params=index("L2")))
    d.step("back to default", lambda c: c.using_database("default"))

    # default 与空名：ValueError（1/0），文案一致；不存在 6/6；非空 6/6；非法名与超长名 1/0。
    for name in ["default", ""]:
        assert both("DropDatabase", db_name=name) == (1, 0, "cannot drop default database")
    assert both("DropDatabase", db_name="nope") == (6, 6, "database 'nope' does not exist")
    assert both("DropDatabase", db_name="d1") == (6, 6, "database 'd1' is not empty")
    for name in ["a/b", "x" * 256]:
        assert both("DropDatabase", db_name=name)[:2] == (1, 0), name
    # DropDatabase 只读请求体：dbname 元数据被忽略。
    for uri in pair.uris:
        with grpc.insecure_channel(uri.removeprefix("http://"), options=[("grpc.enable_http_proxy", 0)]) as channel:
            stub = milvus_pb2_grpc.MilvusServiceStub(channel)
            reply = stub.DropDatabase(milvus_pb2.DropDatabaseRequest(), metadata=[("dbname", "empty")])
            assert (reply.code, reply.error_code) == (1, 0), "empty name routes to default, never to metadata"
    assert both("ListDatabases")[3] == ["badalias", "d1", "default", "empty", "residue"]

    # 成功：空库、带残留与损坏元数据的库；目录与 staging 两边一致。
    for name in ["empty", "residue", "badalias"]:
        assert both("DropDatabase", db_name=name) == (0, 0, ""), name
    assert both("ListDatabases")[3] == ["d1", "default"]
    for root in (pair.rust_path, pair.ref_path):
        assert sorted(p.name for p in (Path(root) / "databases").iterdir()) == ["d1"]
        assert list((Path(root) / ".database-staging").iterdir()) == []
    both("DescribeDatabase", db_name="empty")
    d.step("using dropped database", lambda c: c.using_database("empty"), error=True)

    # 集合删除后库为空；删除后同名重建得到全新的库，重开后一致。
    d.step("using d1", lambda c: c.using_database("d1"))
    d.step("drop same", lambda c: c.drop_collection("same"))
    d.step("using default again", lambda c: c.using_database("default"))
    d.step("drop_database via client", lambda c: c.drop_database("d1"))
    d.step("list_databases", lambda c: c.list_databases())
    both("CreateDatabase", db_name="d1")
    assert both("DescribeDatabase", db_name="d1")[4] == []
    both.props_file("d1")
    pair.close()
    pair.open()
    both("ListDatabases")
    both("DescribeDatabase", db_name="d1")


def test_drop_database_registered_differences(pair):
    make_layout(pair)
    pair.open()
    rust, ref = pair.uris
    # 只差大小写的名字：大小写不敏感文件系统上 3.x 按路径命中并删除，本实现按精确名字报不存在、不删除。
    for uri in pair.uris:
        assert call(uri, "CreateDatabase", db_name="casey")[:2] == (0, 0)
    if case_insensitive(pair.root):
        assert call(ref, "DropDatabase", db_name="CASEY") == (0, 0, "")
        assert not (Path(pair.ref_path) / "databases/casey").exists()
        assert call(rust, "DropDatabase", db_name="CASEY") == (6, 6, "database 'CASEY' does not exist")
        assert (Path(pair.rust_path) / "databases/casey").is_dir()
    else:
        assert call(rust, "DropDatabase", db_name="CASEY") == call(ref, "DropDatabase", db_name="CASEY")
    # 符号链接形式的空库：两边都拒绝（1/0，文案不同），链接与目标都保留。
    for root in (pair.rust_path, pair.ref_path):
        target = Path(root).parent / f"{Path(root).name}-linked-target"
        (target / "collections").mkdir(parents=True)
        os.symlink(target, Path(root) / "databases" / "linked")
    # 注册表在打开时建立，运行期手工建立的库重开后才可见。
    pair.close()
    pair.open()
    rust_reply = call(pair.uris[0], "DropDatabase", db_name="linked")
    ref_reply = call(pair.uris[1], "DropDatabase", db_name="linked")
    assert rust_reply[:2] == ref_reply[:2] == (1, 0), (rust_reply, ref_reply)
    for root in (pair.rust_path, pair.ref_path):
        assert (Path(root) / "databases" / "linked").is_symlink()
        assert (Path(root).parent / f"{Path(root).name}-linked-target" / "collections").is_dir()
