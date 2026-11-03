"""0025 第 4 步：TIMESTAMPTZ 的时区（请求 / 集合 / 数据库）、`time_fields` 与校验时机对照 3.x（43d1257）。

场景来自第 3 步的仓库外探针（对跑真实 3.x 逐项核对过）：请求时区的取值、夏令时与历史时区、集合与数据库 `timezone`
属性、数据库时区改动的即时生效、校验时机与错误优先级、`time_fields`、Delete / Upsert 与 type 21。

3.x 的时区库是运行平台的系统 tzdata，本实现内置 tzdata（0025 差异表）。比较时只取各平台的 3.x 都会给出同一结果的
时区名；名字大小写变体、`posixrules`、`zone.tab` 等平台文件、`Factory` 与已移入 `tzdata-legacy` 的旧别名在 3.x 一侧随平台
变化，只断言本实现这一侧（`test_platform_dependent_zone_names`）。
"""

from pathlib import Path

from pymilvus import DataType, MilvusClient
from pymilvus.grpc_gen import common_pb2, milvus_pb2, schema_pb2

from compare import run, same
from test_reopen import pair  # noqa: F401  （fixture）
from test_scenarios import index
from test_timestamptz import collection, query

V = [0.0, 0.0]
ROWS = [
    ("2025-01-01T00:00:00Z", 1), ("2025-01-02T12:30:00.5Z", 2), ("2024-12-31T16:00:00Z", 3),
    ("2025-06-15T00:00:00+02:00", 4), (None, 5),
]
NAIVE = "ts >= ISO '2025-01-01T08:00:00'"
# 各平台的 3.x 结果相同的时区名与路径形态：只取 zone.tab 里列出的区域与 `Etc/`、`UTC`（发行版会把旧别名拆进 tzdata-legacy），
# 以及不论平台都不存在或不合规范的写法。
PORTABLE_ZONES = [
    None, "UTC", "Asia/Shanghai", "America/New_York", "Etc/GMT+8", "Asia/Kolkata", "", " UTC", "UTC ",
    "Nope/Zone", "../etc/passwd", "/etc/localtime", "America", "Asia/Shanghai/", "America//New_York", "UTC\x00", "Z", "+08:00",
]


class Matrix:
    """逐项在两边执行并要求完全一致；记下成功与失败的个数，避免两边一起写错场景（全部报错或全部为空）。"""

    def __init__(self, d):
        self.d = d
        self.ok = self.err = 0

    def __call__(self, name, fn):
        a, b = run(fn, self.d.rust), run(fn, self.d.ref)
        assert same(a, b), f"{name}\n  rust: {a!r}\n  3.x:  {b!r}"
        if b[0] == "ok":
            self.ok += 1
        else:
            self.err += 1
        return b


def table(client, name="t"):
    collection(client, name)
    client.insert(name, [{"id": i, "vector": V, "ts": t} for t, i in ROWS])


def rows(client, name, expr, **kwargs):
    return sorted((r["id"], r["ts"]) for r in client.query(name, filter=expr, output_fields=["id", "ts"], **kwargs))


def raw_string_insert(client, collection_name, values, row_id, database=""):
    """type 21（VARCHAR 槽）：3.x 由引擎按 schema、集合 / 数据库时区逐行解析。"""
    stub = client._get_connection()._stub
    ids = schema_pb2.FieldData(type=5, field_name="id")
    ids.scalars.long_data.data.extend(range(row_id, row_id + len(values)))
    vec = schema_pb2.FieldData(type=101, field_name="vector")
    vec.vectors.dim = 2
    vec.vectors.float_vector.data.extend([0.0] * (2 * len(values)))
    ts = schema_pb2.FieldData(type=21, field_name="ts")
    ts.scalars.string_data.data.extend(values)
    reply = stub.Insert(milvus_pb2.InsertRequest(
        db_name=database, collection_name=collection_name, num_rows=len(values), fields_data=[ids, vec, ts]))
    return reply.status.code, reply.status.reason


def test_request_timezone_applies_to_every_read_request_like_3x(pair):
    d = pair.open()
    d.step("create", lambda c: table(c))
    m = Matrix(d)
    for zone in PORTABLE_ZONES:
        kwargs = {} if zone is None else {"timezone": zone}
        m(f"query {zone!r}", lambda c, kw=kwargs: rows(c, "t", NAIVE, **kw))
    for zone in ("Asia/Shanghai", "bad"):
        m(f"search {zone!r}", lambda c, z=zone: [[h["id"] for h in x] for x in c.search("t", data=[V], limit=5, filter=NAIVE, timezone=z)])
        m(f"count {zone!r}", lambda c, z=zone: c.query("t", filter=NAIVE, output_fields=["count(*)"], timezone=z))
        m(f"by ids {zone!r}", lambda c, z=zone: len(c.query("t", filter="id in [1,2]", timezone=z, output_fields=["ts"])))
        # 坏时区只在非空表达式时校验，且不论表达式里有没有无偏移字面量。
        m(f"aware only {zone!r}", lambda c, z=zone: rows(c, "t", "ts >= ISO '2025-01-01T00:00:00Z'", timezone=z))
        m(f"no time at all {zone!r}", lambda c, z=zone: rows(c, "t", "id > 0", timezone=z))
    assert m.ok >= 10 and m.err >= 15, (m.ok, m.err)


def test_platform_dependent_zone_names(pair):
    """3.x 一侧随平台变化的时区名（0025 差异表「时区库」）：只断言本实现，名字大小写不敏感，平台文件按找不到。"""
    d = pair.open()
    d.step("create", lambda c: table(c))
    expected = rows(d.rust, "t", NAIVE, timezone="Asia/Shanghai")
    assert len(expected) == 3
    for zone in ("asia/shanghai", "ASIA/SHANGHAI", "Asia/Shanghai"):
        assert rows(d.rust, "t", NAIVE, timezone=zone) == expected
    assert rows(d.rust, "t", NAIVE, timezone="utc") == rows(d.rust, "t", NAIVE, timezone="UTC")
    assert rows(d.rust, "t", NAIVE, timezone="Asia/Calcutta") == rows(d.rust, "t", NAIVE, timezone="Asia/Kolkata")
    for name in ("zone.tab", "tzdata.zi", "posixrules", "localtime"):
        outcome = run(lambda c, n=name: rows(c, "t", NAIVE, timezone=n), d.rust)
        assert outcome[:2] == ("err", 6) and outcome[3] == f"unknown timezone '{name}'", outcome
    # tzdata 里的 `Factory` 是真实的区域（偏移 -00），内置库收录。
    assert run(lambda c: rows(c, "t", NAIVE, timezone="Factory"), d.rust)[0] == "ok"


DST_CASES = [
    ("America/New_York", "2025-03-09T02:30:00"), ("America/New_York", "2025-03-09T01:59:59.999999"),
    ("America/New_York", "2025-03-09T03:00:00"), ("America/New_York", "2025-11-02T01:30:00"),
    ("America/New_York", "2025-11-02T00:59:59"), ("America/New_York", "2025-11-02T02:00:00"),
    ("Europe/London", "2025-03-30T01:30:00"), ("Europe/London", "2025-10-26T01:30:00"),
    ("Australia/Lord_Howe", "2025-10-05T02:15:00"), ("Australia/Lord_Howe", "2025-04-06T01:45:00"),
    ("America/New_York", "1800-01-01T00:00:00"), ("Europe/Paris", "1930-01-01T00:00:00"),
    ("Asia/Shanghai", "1940-06-01T12:00:00"), ("Asia/Shanghai", "1986-05-04T02:30:00"), ("Asia/Shanghai", "1991-09-15T01:30:00"),
    ("America/New_York", "2100-07-01T12:00:00"), ("America/New_York", "2100-03-14T02:30:00"),
    ("America/New_York", "2400-11-04T01:30:00"), ("Asia/Kathmandu", "2025-01-01T00:00:00"),
    ("Pacific/Apia", "2011-12-30T12:00:00"), ("Pacific/Apia", "2011-12-29T12:00:00"), ("Africa/Casablanca", "2025-03-20T12:00:00"),
    ("America/Sao_Paulo", "2019-02-16T23:30:00"), ("Europe/Moscow", "2014-10-26T01:30:00"),
    ("America/New_York", "0001-01-01T00:00:00"), ("America/New_York", "9999-12-31T23:59:59"),
    ("Asia/Tokyo", "0001-01-01T00:00:00"), ("Pacific/Kiritimati", "9999-12-31T23:59:59"),
    ("Pacific/Honolulu", "1896-01-01T00:00:00"), ("Etc/GMT+12", "0001-01-01T00:00:00"), ("Etc/GMT-14", "9999-12-31T23:59:59"),
]


def test_daylight_saving_and_historical_zones_match_3x(pair):
    """重复的本地时间取第一次、不存在的本地时间沿用变换前的偏移（fold=0）；写入与过滤字面量走同一套换算。"""
    d = pair.open()
    m = Matrix(d)
    zones = sorted({zone for zone, _ in DST_CASES})
    for number, zone in enumerate(zones):
        def create(c, number=number, zone=zone):
            schema = MilvusClient.create_schema(auto_id=False)
            schema.add_field("id", DataType.INT64, is_primary=True)
            schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
            schema.add_field("ts", DataType.TIMESTAMPTZ, nullable=True)
            c.create_collection(f"z{number}", schema=schema, index_params=index("L2"), properties={"timezone": zone})
            c.load_collection(f"z{number}")
        d.step(f"create {zone}", create)
    d.step("create plain", lambda c: collection(c, "plain"))
    d.step("seed", lambda c: c.insert("plain", [{"id": 1, "vector": V, "ts": "2000-01-01T00:00:00Z"}]))
    for row, (zone, literal) in enumerate(DST_CASES):
        name = f"z{zones.index(zone)}"
        m(f"insert {zone} {literal}", lambda c, name=name, row=row, literal=literal: (
            c.insert(name, [{"id": row, "vector": V, "ts": literal}]),
            query(c, name, f"id == {row}"))[1])
        m(f"filter {zone} {literal}", lambda c, zone=zone, literal=literal: [
            sorted(r["id"] for r in c.query("plain", filter=f"ts {op} ISO '{literal}'", output_fields=["id"], timezone=zone))
            for op in ("==", "<", ">")])
    # 越界的换算（例如东京的 0001-01-01 因 LMT 越界）两边同样报错，其余成功。
    assert m.ok > 40 and m.err >= 2, (m.ok, m.err)


def test_collection_and_database_timezone_properties_match_3x(pair):
    d = pair.open()
    m = Matrix(d)

    def props(c, name):
        return dict(c.describe_collection(name).get("properties") or {})

    def make(c, name, properties=None, default=None):
        schema = MilvusClient.create_schema(auto_id=False)
        schema.add_field("id", DataType.INT64, is_primary=True)
        schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
        kwargs = {"nullable": True}
        if default is not None:
            kwargs["default_value"] = default
        schema.add_field("ts", DataType.TIMESTAMPTZ, **kwargs)
        c.create_collection(name, schema=schema, index_params=index("L2"), **({"properties": properties} if properties else {}))
        c.load_collection(name)

    def ins(c, name, row, value):
        c.insert(name, [{"id": row, "vector": V, "ts": value}])
        return query(c, name, f"id == {row}")

    m("create c1", lambda c: make(c, "c1", {"timezone": "Asia/Shanghai"}))
    m("describe c1", lambda c: props(c, "c1"))
    m("naive insert", lambda c: ins(c, "c1", 1, "2025-01-01T08:00:00"))
    m("naive filter", lambda c: rows(c, "c1", "ts == ISO '2025-01-01T08:00:00'"))
    m("request overrides", lambda c: rows(c, "c1", "ts == ISO '2025-01-01T08:00:00'", timezone="UTC"))
    m("alter to New York", lambda c: (c.alter_collection_properties("c1", {"timezone": "America/New_York"}), props(c, "c1"))[1])
    # 改时区不改已存数据；之后的字面量与写入按新时区。
    m("stored unchanged", lambda c: query(c, "c1"))
    m("filter after alter", lambda c: rows(c, "c1", "ts == ISO '2025-01-01T08:00:00'"))
    m("insert after alter", lambda c: ins(c, "c1", 2, "2025-01-01T08:00:00"))
    for zone in ("", "bad", "UTC", " UTC", 5):
        m(f"alter {zone!r}", lambda c, z=zone: (c.alter_collection_properties("c1", {"timezone": z}), props(c, "c1"))[1])
    m("drop property", lambda c: (c.drop_collection_properties("c1", ["timezone"]), props(c, "c1"))[1])
    m("create with unknown zone", lambda c: make(c, "bad", {"timezone": "Nope"}))
    m("create with empty zone", lambda c: make(c, "empty", {"timezone": ""}))
    # 默认值：创建时按集合时区（其次数据库时区）换算成微秒，之后改时区不影响。
    m("default with zone", lambda c: (make(c, "d1", {"timezone": "Asia/Shanghai"}, "2025-01-01T08:00:00"),
                                      c.insert("d1", [{"id": 1, "vector": V}]), query(c, "d1"))[2])
    m("default naive without zone", lambda c: make(c, "d2", None, "2025-01-01T08:00:00"))
    m("default aware", lambda c: (make(c, "d3", None, "2025-01-01T08:00:00+08:00"),
                                  c.insert("d3", [{"id": 1, "vector": V}, {"id": 2, "vector": V, "ts": None}]), query(c, "d3"))[2])
    m("default invalid", lambda c: make(c, "d4", None, "xx"))
    # 数据库时区。
    m("create database", lambda c: c.create_database("dbtz", properties={"timezone": "Asia/Shanghai"}))
    m("describe database", lambda c: c.describe_database("dbtz"))
    m("use database", lambda c: c.use_database("dbtz"))
    m("collection in database", lambda c: make(c, "t1"))
    m("db zone insert", lambda c: ins(c, "t1", 1, "2025-01-01T08:00:00"))
    m("alter database", lambda c: (c.alter_database_properties("dbtz", {"timezone": "America/New_York"}), c.describe_database("dbtz"))[1])
    # 数据库时区改动对已打开的集合立即生效。
    m("db zone filter", lambda c: rows(c, "t1", "ts == ISO '2025-01-01T08:00:00'"))
    m("db zone insert after alter", lambda c: ins(c, "t1", 2, "2025-01-01T08:00:00"))
    m("collection overrides database", lambda c: (c.alter_collection_properties("t1", {"timezone": "UTC"}),
                                                 rows(c, "t1", "ts == ISO '2025-01-01T08:00:00'"))[1])
    m("unknown zone on create database", lambda c: c.create_database("dbbad", properties={"timezone": "Nope"}))
    m("unknown zone on alter database", lambda c: c.alter_database_properties("dbtz", {"timezone": "Nope"}))
    m("naive default from database zone", lambda c: (make(c, "dd", None, "2025-07-01T12:00:00"),
                                                      c.insert("dd", [{"id": 1, "vector": V}]), query(c, "dd"))[2])
    m("drop database zone", lambda c: (c.drop_database_properties("dbtz", ["timezone"]), c.describe_database("dbtz"))[1])
    m("naive insert without zone", lambda c: ins(c, "t1", 3, "2025-01-01T08:00:00"))
    m("naive default without zone", lambda c: make(c, "dd2", None, "2025-07-01T12:00:00"))
    m("use default database", lambda c: c.use_database("default"))
    assert m.ok > 20 and m.err > 8, (m.ok, m.err)


def test_delete_upsert_and_database_zone_changes_match_3x(pair):
    d = pair.open()
    m = Matrix(d)
    d.step("create database", lambda c: c.create_database("dbz", properties={"timezone": "America/New_York"}))
    d.step("use database", lambda c: c.use_database("dbz"))
    d.step("create", lambda c: collection(c, "t"))
    d.step("insert", lambda c: c.insert("t", [{"id": i, "vector": V, "ts": t} for i, t in enumerate(
        ["2025-01-01T00:00:00Z", "2025-01-01T13:00:00Z", "2025-01-01T12:00:00Z"])]))
    # Delete 没有请求时区，按集合 → 数据库时区；Search 的过滤同理。
    m("search naive", lambda c: [[h["id"] for h in x] for x in c.search("t", data=[V], limit=5, filter="ts >= ISO '2025-01-01T08:00:00'")])
    m("delete naive", lambda c: (c.delete("t", filter="ts == ISO '2025-01-01T08:00:00'"), query(c, "t"))[1])
    m("upsert naive", lambda c: (c.upsert("t", [{"id": 1, "vector": V, "ts": "2025-06-01T08:00:00"}]), query(c, "t"))[1])
    d.step("alter database", lambda c: c.alter_database_properties("dbz", {"timezone": "Asia/Tokyo"}))
    m("insert after alter", lambda c: (c.insert("t", [{"id": 5, "vector": V, "ts": "2025-01-01T09:00:00"}]), query(c, "t", "id == 5"))[1])
    m("count after alter", lambda c: c.query("t", filter="ts == ISO '2025-01-01T09:00:00'", output_fields=["count(*)"]))
    d.step("drop database zone", lambda c: c.drop_database_properties("dbz", ["timezone"]))
    m("naive insert fails without zone", lambda c: c.insert("t", [{"id": 6, "vector": V, "ts": "2025-01-01T09:00:00"}]))
    m("naive delete fails without zone", lambda c: c.delete("t", filter="ts == ISO '2025-01-01T09:00:00'"))
    m("time_fields default UTC", lambda c: [dict(r) for r in c.query("t", filter="id >= 0", output_fields=["id", "ts"], time_fields="hour")])
    # type 21 经数据库时区；原始 gRPC 不经 pymilvus 的类型转换。
    d.step("alter database again", lambda c: c.alter_database_properties("dbz", {"timezone": "Europe/London"}))
    for row, values in enumerate((["2025-07-01T12:00:00"], ["2025-07-01T12:00:00+01:00"], ["9999-12-31T23:59:59"], ["not a time"], [""])):
        a = raw_string_insert(d.rust, "t", values, 100 * (row + 1), database="dbz")
        b = raw_string_insert(d.ref, "t", values, 100 * (row + 1), database="dbz")
        assert a == b, f"{values!r}\n  rust: {a!r}\n  3.x:  {b!r}"
    m("rows after type 21", lambda c: query(c, "t"))
    d.step("use default", lambda c: c.use_database("default"))
    assert m.ok >= 7 and m.err >= 2, (m.ok, m.err)


def test_validation_points_and_error_order_match_3x(pair):
    """请求时区与 `time_fields` 的校验时机、错误先后（0025「校验时机与错误优先级」）。"""
    d = pair.open()

    def build(c):
        schema = MilvusClient.create_schema(auto_id=False)
        schema.add_field("id", DataType.INT64, is_primary=True)
        schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
        schema.add_field("ts", DataType.TIMESTAMPTZ, nullable=True)
        schema.add_field("cat", DataType.VARCHAR, max_length=8)
        for name in ("t", "empty", "allnull", "rel"):
            c.create_collection(name, schema=schema, index_params=index("L2"))
        c.insert("t", [{"id": 1, "vector": V, "ts": "2025-01-01T00:00:00Z", "cat": "a"}, {"id": 2, "vector": V, "ts": None, "cat": "b"}])
        c.insert("allnull", [{"id": 1, "vector": V, "ts": None, "cat": "a"}])
        c.release_collection("rel")
    d.step("build", build)
    m = Matrix(d)

    def q(c, collection_name="t", expr="id >= 0", fields=("id", "ts"), **kw):
        return [dict(x) for x in c.query(collection_name, filter=expr, output_fields=list(fields), **kw)]

    def s(c, collection_name="t", expr="", fields=("ts",), **kw):
        return [[dict(h["entity"]) for h in x] for x in c.search(
            collection_name, data=[V], limit=3, filter=expr, output_fields=list(fields), **kw)]

    def count(c, expr, **kw):
        return c.query("t", filter=expr, output_fields=["count(*)"], **kw)

    # count(*)：空表达式两个校验点都不经过。
    m("count empty expr bad zone", lambda c: count(c, "", timezone="bad"))
    m("count id>0 bad zone", lambda c: count(c, "id > 0", timezone="bad"))
    m("count empty expr good zone", lambda c: count(c, "", timezone="UTC"))
    m("count empty expr bad time_fields", lambda c: count(c, "", time_fields="weekday"))
    # 与其他错误的先后。
    m("query missing collection", lambda c: q(c, "nope", timezone="bad"))
    m("query unloaded", lambda c: q(c, "rel", timezone="bad"))
    m("query bad filter and zone", lambda c: q(c, expr="id >", timezone="bad"))
    m("query bad filter good zone", lambda c: q(c, expr="id >", timezone="UTC"))
    m("query type error and zone", lambda c: q(c, expr="ts > 1", timezone="bad"))
    m("query bad output field and zone", lambda c: q(c, fields=("nosuch",), timezone="bad"))
    m("query empty result bad zone", lambda c: q(c, expr="id > 100", timezone="bad"))
    m("search missing collection", lambda c: s(c, "nope", timezone="bad"))
    m("search unloaded", lambda c: s(c, "rel", timezone="bad"))
    m("search bad filter and zone", lambda c: s(c, expr="id >", timezone="bad"))
    m("search bad group and zone", lambda c: s(c, group_by_field="nosuch", timezone="bad"))
    m("search bad anns and zone", lambda c: s(c, anns_field="nosuch", timezone="bad"))
    m("search bad metric and zone", lambda c: s(c, search_params={"metric_type": "NOPE"}, timezone="bad"))
    m("search no filter bad zone", lambda c: s(c, timezone="bad"))
    m("search empty collection bad zone", lambda c: s(c, "empty", timezone="bad"))
    m("search bad group and bad filter", lambda c: s(c, expr="id >", group_by_field="nosuch"))
    # time_fields：坏组件只在编码非空时间戳时才校验。
    for label, fn in (
        ("rows with ts output", lambda c: q(c, time_fields="weekday")),
        ("rows without ts output", lambda c: q(c, fields=("id", "cat"), time_fields="weekday")),
        ("zero rows", lambda c: q(c, expr="id > 100", time_fields="weekday")),
        ("empty collection", lambda c: q(c, "empty", time_fields="weekday")),
        ("all-null column", lambda c: q(c, "allnull", time_fields="weekday")),
        ("only null rows hit", lambda c: q(c, expr="id == 2", time_fields="weekday")),
        ("non-null row hit", lambda c: q(c, expr="id == 1", time_fields="weekday")),
        ("search with rows", lambda c: s(c, time_fields="weekday")),
        ("search without ts output", lambda c: s(c, fields=("cat",), time_fields="weekday")),
        ("search empty collection", lambda c: s(c, "empty", time_fields="weekday")),
        ("good, all-null", lambda c: q(c, "allnull", time_fields="hour")),
        ("good, zero rows", lambda c: q(c, expr="id > 100", time_fields="hour")),
        ("bad component and bad zone", lambda c: q(c, time_fields="weekday", timezone="bad")),
        ("only commas", lambda c: q(c, time_fields=",,")),
        ("commas and spaces", lambda c: q(c, time_fields=" , ")),
        ("with filter", lambda c: q(c, expr="ts > ISO '2024-01-01T00:00:00Z'", time_fields="year,month")),
        ("with limit", lambda c: q(c, time_fields="year", limit=1)),
    ):
        m(f"time_fields {label}", fn)
    assert m.ok >= 15 and m.err >= 20, (m.ok, m.err)


def test_time_fields_output_matches_3x(pair):
    d = pair.open()
    d.step("create", lambda c: (collection(c, "t"), c.insert("t", [
        {"id": 1, "vector": V, "ts": "2025-03-09T07:30:15.123456Z"}, {"id": 2, "vector": V, "ts": None}])))
    m = Matrix(d)
    for fields in ("year", "year,month,day,hour,minute,second,microsecond", "year, month", "YEAR", "hour minute", "Year,Month",
                   "weekday", "", " ", "year,,month", "year;month", "millisecond", "day,day", "second,microsecond,year"):
        m(f"time_fields {fields!r}", lambda c, f=fields: c.query(
            "t", filter="id >= 1", output_fields=["id", "ts"], time_fields=f, timezone="America/New_York"))
    m("default zone", lambda c: c.query("t", filter="id >= 1", output_fields=["id", "ts"], time_fields="hour"))
    m("bad zone", lambda c: c.query("t", filter="id >= 1", output_fields=["id", "ts"], time_fields="hour", timezone="bad"))
    m("not a time field", lambda c: c.query("t", filter="id >= 1", output_fields=["id", "vector"], time_fields="hour"))
    m("search", lambda c: [[dict(h["entity"]) for h in x] for x in c.search(
        "t", data=[V], limit=2, output_fields=["ts"], time_fields="year,hour", timezone="Asia/Shanghai")])

    def with_collection_zone(c):
        schema = MilvusClient.create_schema(auto_id=False)
        schema.add_field("id", DataType.INT64, is_primary=True)
        schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
        schema.add_field("ts", DataType.TIMESTAMPTZ, nullable=True)
        c.create_collection("tz", schema=schema, index_params=index("L2"), properties={"timezone": "Asia/Shanghai"})
        c.load_collection("tz")
        c.insert("tz", [{"id": 1, "vector": V, "ts": "2025-03-09T07:30:15Z"}])
        return c.query("tz", filter="id>=1", output_fields=["ts"], time_fields="hour")
    m("collection zone", with_collection_zone)
    # Query iterator 把请求参数带进每一页的 Query：time_fields、timezone 与无偏移字面量同样生效。
    def iterate(c, **kwargs):
        iterator = c.query_iterator("t", batch_size=1, output_fields=["id", "ts"], **kwargs)
        result = []
        try:
            for _ in range(10):
                page = [dict(row) for row in iterator.next()]
                if not page:
                    return result
                result.append(page)
            raise AssertionError("iterator did not finish")
        finally:
            iterator.close()
    m("iterator time_fields", lambda c: iterate(c, filter="id >= 1", time_fields="year,hour", timezone="Asia/Shanghai"))
    m("iterator time_fields default zone", lambda c: iterate(c, filter="id >= 1", time_fields="hour"))
    m("iterator naive filter", lambda c: iterate(c, filter="ts >= ISO '2025-03-09T00:00:00'", timezone="Asia/Shanghai"))
    m("iterator bad zone", lambda c: iterate(c, filter="ts >= ISO '2025-03-09T00:00:00'", timezone="bad"))
    m("iterator bad time_fields", lambda c: iterate(c, filter="id >= 1", time_fields="weekday"))
    # 类型字段：Array<Int64>（type 22），空值整行无效。
    for client in (d.rust, d.ref):
        stub = client._get_connection()._stub
        request = milvus_pb2.QueryRequest(collection_name="t", expr="id >= 1", output_fields=["ts"])
        request.query_params.append(common_pb2.KeyValuePair(key="time_fields", value="year,hour"))
        request.query_params.append(common_pb2.KeyValuePair(key="timezone", value="Asia/Shanghai"))
        field = next(f for f in stub.Query(request).fields_data if f.field_name == "ts")
        assert field.type == 22 and list(field.valid_data) == [True, False]
        assert [list(a.long_data.data) for a in field.scalars.array_data.data][0] == [2025, 15]
    assert m.ok >= 13 and m.err >= 6, (m.ok, m.err)


def test_index_keys_and_group_by_on_timestamptz_match_3x(pair):
    d = pair.open()
    d.step("create", lambda c: table(c))
    d.step("inverted index", lambda c: (c.create_index("t", index_params=_ts_index(c, "INVERTED")), c.describe_index("t", "ts"))[1])
    d.step("second index", lambda c: c.create_index("t", index_params=_ts_index(c, "STL_SORT", "ts_stl")), error=True)
    d.step("filter after index", lambda c: query(c, "t", "ts >= ISO '2025-01-01T00:00:00Z'"))
    d.step("flush", lambda c: c.flush("t"))
    d.step("flushed", lambda c: query(c, "t", "ts > ISO '2025-01-01T00:00:00Z'"))
    for field in ("is_primary", "is_partition_key"):
        def bad(c, field=field):
            schema = MilvusClient.create_schema(auto_id=False)
            schema.add_field("id", DataType.INT64, is_primary=field != "is_primary")
            schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
            schema.add_field("ts2", DataType.TIMESTAMPTZ, **{field: True})
            c.create_collection("bad_" + field, schema=schema)
        d.step(f"timestamptz {field}", bad, error=True)
    d.step("group by", lambda c: c.search("t", data=[V], limit=2, group_by_field="ts"), error=True)
    d.step("get", lambda c: [(x["id"], x.get("ts")) for x in c.get("t", ids=[2, 3, 5])])
    d.step("describe fields", lambda c: [(f["name"], str(f["type"])) for f in c.describe_collection("t")["fields"]])
    d.step("upsert", lambda c: (c.upsert("t", [{"id": 1, "vector": V, "ts": "2030-01-01T00:00:00Z"}]), query(c, "t", "id == 1"))[1])
    d.step("delete by ts", lambda c: (c.delete("t", filter="ts == ISO '2030-01-01T00:00:00Z'"), query(c, "t"))[1])
    pair.close()
    d = pair.open()
    d.step("reopened", lambda c: (c.load_collection("t"), query(c, "t"))[1])
    d.step("reopened filter", lambda c: query(c, "t", "ts > ISO '2024-12-31T00:00:00Z'"))


def _ts_index(client, index_type, name=None):
    params = client.prepare_index_params()
    kwargs = {"index_name": name} if name else {}
    params.add_index("ts", index_type=index_type, **kwargs)
    return params


def test_invalid_stored_database_zone_follows_the_registered_rule(pair):
    """手改的数据库属性文件里 `timezone` 取值不合法（路径形态、未知名字、空串）：3.x 加载属性就校验，打开该库的任何集合都报同一个错误；
    本实现只在需要时区的操作里报错，其余操作照常，数据本身的错误（不合法的时间字符串、越界）先于属性错误（0025 差异表）。"""
    cases = [
        ('{"timezone": "../etc/passwd"}', 1, "ZoneInfo keys must refer to subdirectories of TZPATH, got: ../etc/passwd"),
        ('{"timezone": "Mars/Base"}', 6, "unknown timezone 'Mars/Base'"),
        ('{"timezone": ""}', 6, "timezone property must be a non-empty string"),
    ]
    d = pair.open()
    for i in range(len(cases)):
        d.step(f"db {i}", lambda c, i=i: (c.create_database(f"dbp{i}"), c.use_database(f"dbp{i}"), collection(c, "t"), c.use_database("default")))
    pair.close()
    for i, (text, _, _) in enumerate(cases):
        for root in (pair.rust_path, pair.ref_path):
            (Path(root) / f"databases/dbp{i}/database_properties.json").write_text(text)
    d = pair.open()
    for i, (_, code, message) in enumerate(cases):
        db, error = f"dbp{i}", (code, message)
        a = run(lambda c: c.describe_database(db), d.rust)
        b = run(lambda c: c.describe_database(db), d.ref)
        assert a[:2] == b[:2] == ("err", code) and same(a, b), (a, b)

        def load(c):
            reply = c._get_connection()._stub.LoadCollection(milvus_pb2.LoadCollectionRequest(db_name=db, collection_name="t"))
            return reply.code, reply.reason[:160]

        def status(shape):
            return tuple(shape["status"])

        # 3.x：库里的每个操作（含 Load 与不涉及时间的读写）都在打开集合时报这个错误。
        theirs = {
            "load": load(d.ref),
            "t21 aware": raw_string_insert(d.ref, "t", ["2025-01-01T00:00:00Z"], 10, database=db),
            "t21 naive": raw_string_insert(d.ref, "t", ["2025-01-01T00:00:00"], 20, database=db),
            "t21 bad": raw_string_insert(d.ref, "t", ["not a time"], 30, database=db),
            "t26 aware": raw_ts_insert(d.ref, "t", "string_data", ["2025-01-01T00:00:00Z"], 50, database=db),
            "t26 naive": raw_ts_insert(d.ref, "t", "string_data", ["2025-01-01T00:00:00"], 60, database=db),
            "t26 bad": raw_ts_insert(d.ref, "t", "string_data", ["not a time"], 70, database=db),
            "t26 int": raw_ts_insert(d.ref, "t", "timestamptz_data", [5], 80, database=db),
            "query id": status(raw_ts_query(d.ref, "t", "id >= 0", database=db, output_fields=("id",))),
            "query time_fields": status(raw_ts_query(d.ref, "t", "id >= 0", database=db, time_fields="year")),
        }
        assert set(theirs.values()) == {error}, theirs
        # 本实现：需要时区的操作（无偏移字符串、time_fields）与 3.x 完全相同；不需要的照常；
        # 本身就不合法的时间字符串先报数据错误（3.x 报属性错误）。
        assert load(d.rust) == (0, "")
        assert raw_string_insert(d.rust, "t", ["2025-01-01T00:00:00Z"], 10, database=db) == (0, "")
        assert raw_string_insert(d.rust, "t", ["2025-01-01T00:00:00"], 20, database=db) == error
        assert raw_ts_insert(d.rust, "t", "string_data", ["2025-01-01T00:00:00Z"], 50, database=db) == (0, "")
        assert raw_ts_insert(d.rust, "t", "string_data", ["2025-01-01T00:00:00"], 60, database=db) == error
        assert raw_ts_insert(d.rust, "t", "timestamptz_data", [5], 80, database=db) == (0, "")
        # 带偏移但越界的值同样是数据错误（1），先于属性错误；3.x 对这些值也报属性错误。
        for text_in in ("0001-01-01T00:00:00+01:00", "9999-12-31T23:59:59-08:00"):
            assert raw_string_insert(d.rust, "t", [text_in], 31, database=db) == (1, "date value out of range")
            assert raw_ts_insert(d.rust, "t", "string_data", [text_in], 71, database=db) == (1, "date value out of range")
            assert raw_string_insert(d.ref, "t", [text_in], 31, database=db) == error
            assert raw_ts_insert(d.ref, "t", "string_data", [text_in], 71, database=db) == error
        bad21 = raw_string_insert(d.rust, "t", ["not a time"], 30, database=db)
        assert bad21 == (6, "field 'ts' value 'not a time' does not match dtype DataType.TIMESTAMPTZ"), bad21
        assert raw_string_insert(d.rust, "t", [""], 40, database=db)[0] == 6
        bad26 = raw_ts_insert(d.rust, "t", "string_data", ["not a time"], 70, database=db)
        assert bad26 == (6, "TIMESTAMPTZ value 'not a time' is not a valid ISO 8601 timestamp"), bad26
        assert status(raw_ts_query(d.rust, "t", "id >= 0", database=db, output_fields=("id",))) == (0, "")
        assert status(raw_ts_query(d.rust, "t", "id >= 0", database=db, time_fields="year")) == error
        # 文件被修正后，下一个需要时区的操作直接成功（3.x 加载失败不缓存；本实现的失败同样不缓存）。
        (Path(pair.rust_path) / f"databases/{db}/database_properties.json").write_text('{"timezone": "UTC"}')
        assert raw_ts_insert(d.rust, "t", "string_data", ["2025-01-01T00:00:00"], 90, database=db) == (0, "")
        assert status(raw_ts_query(d.rust, "t", "id >= 0", database=db, time_fields="year")) == (0, "")


def raw_ts_insert(client, name, slot, values, row_id, valid=None, field_type=26, database=""):
    stub = client._get_connection()._stub
    n = len(valid) if valid is not None else len(values)
    ids = schema_pb2.FieldData(type=5, field_name="id")
    ids.scalars.long_data.data.extend(range(row_id, row_id + n))
    vec = schema_pb2.FieldData(type=101, field_name="vector")
    vec.vectors.dim = 2
    vec.vectors.float_vector.data.extend([0.0] * (2 * n))
    ts = schema_pb2.FieldData(type=field_type, field_name="ts")
    getattr(ts.scalars, slot).data.extend(values)
    if valid is not None:
        ts.valid_data.extend(valid)
    reply = stub.Insert(milvus_pb2.InsertRequest(db_name=database, collection_name=name, num_rows=n, fields_data=[ids, vec, ts]))
    return reply.status.code, reply.status.reason[:160]


def raw_ts_query(client, name, expr, database="", output_fields=("id", "ts"), **params):
    """响应里的字段形态：类型、使用的数据槽、字符串 / 整数 / 有效位（MilvusClient 只暴露转换后的行）。"""
    request = milvus_pb2.QueryRequest(db_name=database, collection_name=name, expr=expr, output_fields=list(output_fields))
    for key, value in params.items():
        request.query_params.append(common_pb2.KeyValuePair(key=key, value=value))
    reply = client._get_connection()._stub.Query(request)
    shape = {"status": [reply.status.code, reply.status.reason[:160]]}
    for field in reply.fields_data:
        if field.field_name == "id":
            shape["id"] = list(field.scalars.long_data.data)
        if field.field_name == "ts":
            scalars = field.scalars
            shape["ts"] = {
                "type": field.type, "slot": scalars.WhichOneof("data"), "strings": list(scalars.string_data.data),
                "longs": list(scalars.long_data.data), "valid": list(field.valid_data),
                "arrays": [list(a.long_data.data) for a in scalars.array_data.data],
            }
    return shape


def test_raw_grpc_data_slots_and_response_shape_match_3x(pair):
    d = pair.open()
    d.step("create", lambda c: collection(c, "t"))
    d.step("create zoned", lambda c: (c.create_collection("z", schema=_nullable_schema(), index_params=index("L2"),
                                                          properties={"timezone": "Asia/Shanghai"}), c.load_collection("z")))
    cases = [
        ("timestamptz_data", [0, 1735689600000000, -1], None), ("long_data", [5], None),
        ("string_data", ["2025-01-01T00:00:00Z", "2025-01-01T00:00:00"], None), ("string_data", ["2025-01-01T00:00:00"], None),
        ("string_data", ["2025-01-01T00:00:00+08:00", "2025-01-01T00:00:00.5Z"], None), ("string_data", ["bad"], None),
        ("timestamptz_data", [253402300799999999, -62135596800000000], None), ("timestamptz_data", [7], [True, False]),
        ("string_data", ["2025-01-01T00:00:00Z"], [False, True]),
    ]
    for name in ("t", "z"):
        for i, (slot, values, valid) in enumerate(cases):
            a = raw_ts_insert(d.rust, name, slot, values, 100 * (i + 1), valid)
            b = raw_ts_insert(d.ref, name, slot, values, 100 * (i + 1), valid)
            assert a == b, f"{name} {slot} {values!r}\n  rust: {a!r}\n  3.x:  {b!r}"
    # type 21 的整数槽被拒绝，与 3.x 相同。
    a = raw_ts_insert(d.rust, "t", "long_data", [5], 900, field_type=21)
    assert a == raw_ts_insert(d.ref, "t", "long_data", [5], 900, field_type=21)
    d.step("create clean", lambda c: (collection(c, "clean"), c.insert("clean", [
        {"id": 1, "vector": V, "ts": "2025-01-01T00:00:00Z"}, {"id": 2, "vector": V, "ts": "0001-01-01T00:00:00Z"},
        {"id": 3, "vector": V, "ts": "9999-12-31T23:59:59.999999Z"}, {"id": 4, "vector": V, "ts": None}])))
    exprs = [
        "id >= 0", "ts > ISO '9999-12-31T23:59:59.999999Z'", "ts < ISO '0001-01-01T00:00:00Z'",
        "ts + INTERVAL 'P0D' >= ISO '9999-12-31T23:59:59.999999Z'", "ts - INTERVAL 'PT1S' < ISO '0001-01-01T00:00:00Z'",
        "ts + INTERVAL 'PT1S' > ISO '0001-01-01T00:00:00Z'", "ts in [ISO '0001-01-01T00:00:00Z', ISO '2025-01-01T00:00:00Z']",
        "ts in [ISO '2025-01-01T00:00:00Z', 5]", "ts in [INTERVAL 'P1D']", "ts is null", "ts is not null",
    ]
    kinds = set()
    for expr in exprs:
        for params in ({}, {"time_fields": "year,hour", "timezone": "Asia/Shanghai"}):
            a = raw_ts_query(d.rust, "clean", expr, **params)
            b = raw_ts_query(d.ref, "clean", expr, **params)
            assert a == b, f"{expr} {params}\n  rust: {a!r}\n  3.x:  {b!r}"
            kinds.add((b["status"][0], b.get("ts", {}).get("type"), b.get("ts", {}).get("slot")))
    # 字符串、整数组数据和错误三种形态都出现过。
    assert (0, 26, "string_data") in kinds and (0, 22, "array_data") in kinds and any(k[0] == 6 for k in kinds), kinds
    for name in ("t", "z"):
        a = raw_ts_query(d.rust, name, "id in [100, 200, 500]")
        assert a == raw_ts_query(d.ref, name, "id in [100, 200, 500]")


def _nullable_schema():
    schema = MilvusClient.create_schema(auto_id=False)
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    schema.add_field("ts", DataType.TIMESTAMPTZ, nullable=True)
    return schema
