"""0024 分组检索的场景表：先建数据，再按顺序执行 `setup` 与 `case`，两边的步骤完全相同。

场景的期望值来自真实 3.x（43d1257）；`BUGS` 是 3.x 回退取值缺陷的场景，本实现按字段值正常分组，
两边的输出都在测试里逐项断言（0024「与参照的差异」）。
"""

import random

from pymilvus import DataType, MilvusClient

ACTIONS = []
Q = [[1.0, 0.5, -0.5, 0.2]]


def setup(fn):
    ACTIONS.append(("setup", fn))


def case(name, collection="gb_cosine", **kw):
    kw.setdefault("data", Q)
    kw.setdefault("limit", 3)
    ACTIONS.append(("case", name, lambda c: c.search(collection, **kw)))


def custom(name, fn):
    ACTIONS.append(("case", name, fn))


def index(metric):
    def build(c):
        params = c.prepare_index_params()
        params.add_index("v", index_type="FLAT", metric_type=metric)
        return params
    return build


def make_gb(name, metric, rows):
    def build(c):
        s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=True)
        s.add_field("id", DataType.INT64, is_primary=True)
        s.add_field("v", DataType.FLOAT_VECTOR, dim=4)
        s.add_field("cat", DataType.VARCHAR, max_length=20)
        s.add_field("g", DataType.INT64)
        s.add_field("small", DataType.INT8)
        s.add_field("mid", DataType.INT16)
        s.add_field("i32", DataType.INT32)
        s.add_field("flag", DataType.BOOL)
        s.add_field("nv", DataType.VARCHAR, max_length=20, nullable=True)
        s.add_field("ni", DataType.INT64, nullable=True)
        s.add_field("f", DataType.FLOAT)
        s.add_field("d", DataType.DOUBLE)
        s.add_field("j", DataType.JSON)
        s.add_field("arr", DataType.ARRAY, element_type=DataType.INT64, max_capacity=4)
        c.create_collection(name, schema=s, index_params=index(metric)(c))
        c.insert(name, rows)
        c.load_collection(name)
    return build


def gb_rows():
    rng = random.Random(7)
    cats = [k for k, n in [("A", 25), ("B", 15), ("C", 10), ("D", 6), ("E", 3), ("F", 1)] for _ in range(n)]
    rng.shuffle(cats)
    rows = []
    for i, cat in enumerate(cats):
        gi = "ABCDEF".index(cat)
        rows.append({"id": i, "v": [round(rng.gauss(0, 1), 3) for _ in range(4)], "cat": cat, "g": gi,
                     "small": gi, "mid": gi, "i32": gi, "flag": bool(gi % 2),
                     "nv": None if i % 7 == 0 else cat, "ni": None if i % 5 == 0 else gi,
                     "f": float(gi), "d": float(gi), "j": {"k": gi}, "arr": [gi], "dyn": gi})
    return rows


ROWS = gb_rows()
for _metric in ("COSINE", "L2", "IP"):
    setup(make_gb("gb_" + _metric.lower(), _metric, ROWS))

# 1 基本、strict、度量
for _gs in (None, 1, 2, 3):
    for _lim in (1, 3, 6, 10):
        _kw = {"limit": _lim, "group_by_field": "cat", "output_fields": ["cat"]}
        if _gs:
            _kw["group_size"] = _gs
        case(f"basic lim{_lim} gs{_gs}", **_kw)
for _st in (False, True):
    for _gs in (2, 4, 10):
        case(f"strict{_st} gs{_gs} lim10", limit=10, group_by_field="cat", group_size=_gs,
             strict_group_size=_st, output_fields=["cat"])
for _m in ("l2", "ip"):
    case(f"metric {_m}", "gb_" + _m, limit=4, group_by_field="cat", group_size=2, output_fields=["cat"])

# 2 参数边界
for _v in (0, -1, "2", "abc", 2.5, True, 1000000):
    case(f"group_size {_v!r}", limit=3, group_by_field="cat", group_size=_v, output_fields=["cat"])
for _v in ("false", "", 0, 1, "true", None, 2):
    case(f"strict {_v!r}", limit=10, group_by_field="cat", group_size=3, strict_group_size=_v,
         output_fields=["cat"])
for _lim in (0, -1):
    case(f"limit {_lim}", limit=_lim, group_by_field="cat")

# 3 字段类型与错误
for _f in ("cat", "g", "small", "mid", "i32", "flag", "nv", "ni", "id", "f", "d", "j", "arr", "v", "dyn",
           "nosuch", 'j["k"]', "$meta", "", " "):
    case(f"field {_f!r}", limit=3, group_by_field=_f, output_fields=["id"])

# 4 offset
for _off in (0, 1, 2, 5, 100):
    for _gs in (1, 2):
        case(f"offset {_off} gs{_gs}", limit=2, offset=_off, group_by_field="cat", group_size=_gs,
             output_fields=["cat"])
case("offset neg", limit=2, offset=-1, group_by_field="cat", output_fields=["cat"])

# 5 过滤、空值、output_fields
case("filter", limit=10, filter="id <= 20", group_by_field="cat", group_size=2, output_fields=["id", "cat"])
case("filter empty", limit=10, filter="id < 0", group_by_field="cat", output_fields=["cat"])
case("nullable nv", limit=10, group_by_field="nv", group_size=1, output_fields=["nv"])
case("nullable nv gs3", limit=10, group_by_field="nv", group_size=3, output_fields=["nv"])
case("nullable ni", limit=10, group_by_field="ni", group_size=1, output_fields=["ni"])
case("nullable nv strict", limit=10, group_by_field="nv", group_size=3, strict_group_size=True,
     output_fields=["nv"])
for _of in (None, [], ["id"], ["cat"], ["*"], ["g", "cat"], ["dyn"], ["$meta"]):
    case(f"output_fields {_of!r}", limit=3, group_by_field="cat", group_size=1, output_fields=_of)
case("round_decimal", limit=3, group_by_field="cat", round_decimal=2, output_fields=["cat"])

# 6 多查询、范围、参数位置
case("nq2", data=[Q[0], [-1.0, 0.0, 1.0, 0.5]], limit=3, group_by_field="cat", group_size=2,
     output_fields=["cat"])
case("range cosine", limit=5, group_by_field="cat", group_size=2, output_fields=["cat"],
     search_params={"metric_type": "COSINE", "params": {"radius": 0.0, "range_filter": 1.0}})
case("range l2", "gb_l2", limit=5, group_by_field="cat", group_size=2, output_fields=["cat"],
     search_params={"metric_type": "L2", "params": {"radius": 6.0, "range_filter": 0.0}})
case("params group in search_params", limit=3, output_fields=["cat"],
     search_params={"metric_type": "COSINE", "params": {}, "group_by_field": "cat", "group_size": 2})

# 7 候选窗口：排名靠前的 X 组占满窗口
_WIN = [("X", 29), ("Y", 1), ("Z", 5), ("W", 40), ("V", 3), ("U", 30), ("T", 60)]


def make_small(name, schema_fn, rows, metric="L2"):
    def build(c):
        c.create_collection(name, schema=schema_fn(), index_params=index(metric)(c))
        c.insert(name, rows)
        c.load_collection(name)
    return build


def _win_schema():
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("grp", DataType.VARCHAR, max_length=8)
    return s


_rows, _i = [], 0
for _g, _n in _WIN:
    for _ in range(_n):
        _rows.append({"id": _i, "v": [round(_i * 0.01, 4), 0.0], "grp": _g})
        _i += 1
setup(make_small("gbw", _win_schema, _rows))
for _lim in (1, 2, 3, 4):
    for _gs in (1, 2, 3, 10):
        case(f"window lim{_lim} gs{_gs}", "gbw", data=[[0.0, 0.0]], limit=_lim, group_by_field="grp",
             group_size=_gs, output_fields=["grp"])
case("window strict", "gbw", data=[[0.0, 0.0]], limit=3, group_by_field="grp", group_size=4,
     strict_group_size=True, output_fields=["grp"])
for _off in (1, 2):
    case(f"window offset{_off}", "gbw", data=[[0.0, 0.0]], limit=2, offset=_off, group_by_field="grp",
         group_size=1, output_fields=["grp"])

# 8 并列距离：分组落在并列上时只比较组级不变量
setup(make_small("gbt", _win_schema, [{"id": k, "v": [1.0, 0.0], "grp": "ab"[k % 2]} for k in (5, 3, 4, 1, 2, 0)]))
case("ties", "gbt", data=[[0.0, 0.0]], limit=2, group_by_field="grp", group_size=3, output_fields=["grp"])
TIES = {"ties"}


# 9 字符串主键、删除与 upsert
def _str_pk_schema():
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("pk", DataType.VARCHAR, max_length=8, is_primary=True)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("grp", DataType.INT64)
    return s


def _build_gbs(c):
    make_small("gbs", _str_pk_schema, [{"pk": f"p{k}", "v": [k * 0.1, 0.0], "grp": k % 3} for k in range(9)])(c)
    c.delete("gbs", filter='pk == "p0"')
    c.upsert("gbs", [{"pk": "p1", "v": [0.05, 0.0], "grp": 2}])


setup(_build_gbs)
case("varchar pk + delete/upsert", "gbs", data=[[0.0, 0.0]], limit=3, group_by_field="grp", group_size=2,
     output_fields=["grp"])
case("group by pk (varchar)", "gbs", data=[[0.0, 0.0]], limit=3, group_by_field="pk", output_fields=["pk"])


def _iterator(c):
    it = c.search_iterator("gb_cosine", data=Q, batch_size=2, limit=4, output_fields=["cat"],
                           search_params={"metric_type": "COSINE", "params": {}, "group_by_field": "cat"})
    return [[{"id": h["id"], "distance": h["distance"], "entity": dict(h["entity"])} for h in it.next()]]


custom("search_iterator", _iterator)

# 10 错误先后顺序、分区、大 limit、释放后
case("bad filter + bad field", limit=3, filter="nosuchcol == 1", group_by_field="nosuch")
case("bad filter + ok field", limit=3, filter="nosuchcol == 1", group_by_field="cat")
case("syntax error filter + bad field", limit=3, filter="id ==", group_by_field="nosuch")
case("syntax error filter + ok field", limit=3, filter="id ==", group_by_field="cat")
case("bad anns + bad field", limit=3, anns_field="nosuchvec", group_by_field="nosuch")
case("bad anns + ok field", limit=3, anns_field="nosuchvec", group_by_field="cat")
case("bad size + bad field", limit=3, group_size="abc", group_by_field="nosuch")
case("bad metric + bad field", limit=3, search_params={"metric_type": "L2"}, group_by_field="nosuch")
case("bad metric + ok field", limit=3, search_params={"metric_type": "L2"}, group_by_field="cat")
case("invalid metric name + bad field", limit=3, search_params={"metric_type": "NOPE"}, group_by_field="nosuch")
case("invalid metric name + ok field", limit=3, search_params={"metric_type": "NOPE"}, group_by_field="cat")
setup(lambda c: c.create_partition("gb_cosine", "p1"))
case("partition", limit=3, partition_names=["p1"], group_by_field="cat")
case("partition missing", limit=3, partition_names=["nope"], group_by_field="cat")
case("huge limit", limit=16384, group_by_field="cat", group_size=100, output_fields=["cat"])
setup(lambda c: c.release_collection("gb_l2"))
case("unloaded + bad field", "gb_l2", limit=3, group_by_field="nosuch")
case("unloaded + ok field", "gb_l2", limit=3, group_by_field="cat")

# 11 内存表与 segment 并存
setup(lambda c: c.flush("gb_cosine"))
setup(lambda c: c.insert("gb_cosine", [
    {"id": 900 + k, "v": [1.0, 0.5, -0.5, 0.2], "cat": "NEW", "g": 9, "small": 9, "mid": 9, "i32": 9,
     "flag": True, "nv": "NEW", "ni": 9, "f": 9.0, "d": 9.0, "j": {}, "arr": [9]} for k in range(3)]))
case("memtable+segment", limit=2, group_by_field="cat", group_size=2, output_fields=["cat"])


# 12 3.x 回退取值缺陷的场景（本实现按字段值分组，见 BUGS）
def _pk_schema(name, kind):
    def schema():
        s = MilvusClient.create_schema(auto_id=False)
        kw = {"max_length": 8} if kind == DataType.VARCHAR else {}
        s.add_field(name, kind, is_primary=True, **kw)
        s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
        return s
    return schema


for _name, _kind in (("pk", DataType.INT64), ("id", DataType.VARCHAR)):
    _coll = f"gbpk_{_name}_{_kind.name.lower()}"
    setup(make_small(_coll, _pk_schema(_name, _kind),
                     [{_name: (k if _kind == DataType.INT64 else f"p{k}"), "v": [k * 0.1, 0.0]} for k in range(5)]))
    case(f"group by pk {_name} {_kind.name}", _coll, data=[[0.0, 0.0]], limit=3, group_by_field=_name,
         output_fields=[_name])


def _null_schema():
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("pk", DataType.INT64, is_primary=True)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("distance", DataType.INT64, nullable=True)
    s.add_field("id", DataType.INT64, nullable=True)
    return s


setup(make_small("gbn", _null_schema, [
    {"pk": k, "v": [k * 0.1, 0.0], "distance": None if k % 2 else 7, "id": None if k % 2 else 8} for k in range(6)]))
for _fld in ("distance", "id"):
    case(f"null named {_fld}", "gbn", data=[[0.0, 0.0]], limit=6, group_by_field=_fld, group_size=1,
         output_fields=["pk", _fld])

# 3.x 的输出（ids、分组值、有效位）与本实现的输出；其余场景两边必须完全一致。
BUGS = {
    "group by pk (varchar)": {
        "ref": {"ids": ["p1"], "group_vals": [""], "group_valid": [False]},
        "rust": {"ids": ["p1", "p2", "p3"], "group_vals": ["p1", "p2", "p3"], "group_valid": []},
    },
    "group by pk pk INT64": {
        "ref": {"ids": [0], "group_vals": [0], "group_valid": [False]},
        "rust": {"ids": [0, 1, 2], "group_vals": [0, 1, 2], "group_valid": []},
    },
    "null named distance": {
        "ref": {"ids": [0, 1, 3, 5], "group_vals": [7, 0, 0, 0], "group_valid": []},
        "rust": {"ids": [0, 1], "group_vals": [7, 0], "group_valid": [True, False]},
    },
    "null named id": {
        "ref": {"ids": [0, 1, 3, 5], "group_vals": [8, 1, 3, 5], "group_valid": []},
        "rust": {"ids": [0, 1], "group_vals": [8, 0], "group_valid": [True, False]},
    },
}
