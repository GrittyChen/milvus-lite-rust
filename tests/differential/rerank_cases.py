"""0026 的对照场景：设计阶段探针（2822 个场景）与引擎分组同名字段的补充探针（164 个场景）。

`collect(client)` / `collect_s4(client)` 对一个服务依次建表、写入并发出全部请求，返回 `{场景名: 结果}`；
test_rerank_scenarios.py 对本实现与 3.x（43d1257）各调用一次再逐场景比较。场景的请求、数据与名字与探针相同，
分类与断言见该测试与 0026「测试矩阵」。每次调用重新以同一种子创建随机数生成器，两边的数据逐位相同。
"""

import grpc
import numpy as np
from pymilvus import (AnnSearchRequest, DataType, Function, FunctionScore, FunctionType, MilvusClient, RRFRanker,
                      WeightedRanker)
from pymilvus.client.abstract import BaseRanker
from pymilvus.client.prepare import Prepare
from pymilvus.exceptions import MilvusException
from pymilvus.grpc_gen import common_pb2


def failure(e):
    """异常的可比较形式。UNIMPLEMENTED 不经 pymilvus 转换，原始 RpcError 的调试串含时间与地址，只取码与说明。"""
    if isinstance(e, MilvusException):
        return [e.code, str(e.message)]
    if isinstance(e, grpc.RpcError):
        return [str(e.code()), e.details()]
    return [type(e).__name__, str(e)]


def group_values(gv):
    """`group_by_field_value` 的各值；ARRAY 组值逐个展开成列表。"""
    which = gv.scalars.WhichOneof("data")
    if not which:
        return []
    if which == "array_data":
        return [list(getattr(v, v.WhichOneof("data")).data) if v.WhichOneof("data") else []
                for v in gv.scalars.array_data.data]
    return [v.decode() if isinstance(v, bytes) else v for v in getattr(gv.scalars, which).data]


def collect(c):
    results = {}
    conn = c._get_connection()
    RAW = []
    MOD = {"fn": None}

    def _gv_values(gv):
        which = gv.scalars.WhichOneof("data")
        if not which:
            return []
        out = []
        for v in getattr(gv.scalars, which).data:
            if isinstance(v, bytes):
                out.append(v.decode("utf-8", "replace"))
            elif which == "array_data":
                inner = v.WhichOneof("data")
                out.append(list(getattr(v, inner).data) if inner else [])
            else:
                out.append(v)
        return out

    def _fd_repr(f):
        out = {"name": f.field_name, "type": f.type, "dyn": f.is_dynamic, "valid": list(f.valid_data)}
        kind = f.WhichOneof("field")
        if kind == "vectors":
            out["kind"] = "vectors"
            out["data"] = list(f.vectors.float_vector.data)
            return out
        if kind == "scalars":
            which = f.scalars.WhichOneof("data")
            out["kind"] = which
            vals = []
            if which:
                for v in getattr(f.scalars, which).data:
                    if isinstance(v, bytes):
                        vals.append(v.decode("utf-8", "replace"))
                    elif which == "array_data":
                        inner = v.WhichOneof("data")
                        vals.append(list(getattr(v, inner).data) if inner else [])
                    else:
                        vals.append(v)
            out["data"] = vals
            if which == "array_data":
                out["element_type"] = f.scalars.array_data.element_type
        return out

    def _wrap(name):
        orig = getattr(conn._stub, name)

        def spy(req, *a, **k):
            if MOD["fn"] is not None:
                MOD["fn"](req)
            r = orig(req, *a, **k)
            d = r.results
            sent = {kv.key: kv.value for kv in (req.search_params if name == "Search" else req.rank_params)}
            gv = d.group_by_field_value
            RAW.append({
                "rpc": name, "sent": sent,
                "status": [r.status.code, r.status.reason],
                "top_k": d.top_k, "nq": d.num_queries, "topks": list(d.topks),
                "ids": list(d.ids.int_id.data) or list(d.ids.str_id.data),
                "scores": list(d.scores),
                "output_fields": list(d.output_fields),
                "group_has": d.HasField("group_by_field_value"),
                "group_name": gv.field_name,
                "group_type": gv.type,
                "group_kind": gv.scalars.WhichOneof("data"),
                "group_element_type": gv.scalars.array_data.element_type if gv.scalars.WhichOneof("data") == "array_data" else None,
                "group_vals": _gv_values(gv),
                "group_valid": list(gv.valid_data),
                "fields": [f.field_name for f in d.fields_data],
                "fields_data": [_fd_repr(f) for f in d.fields_data],
            })
            return r
        setattr(conn._stub, name, spy)

    _wrap("Search"); _wrap("HybridSearch")

    def kv(key, value):
        return common_pb2.KeyValuePair(key=key, value=value)

    def _n(v):
        if isinstance(v, list):
            return [_n(x) for x in v]
        if isinstance(v, dict):
            return {k: _n(x) for k, x in v.items()}
        return v

    def hit_repr(h):
        ent = _n(h.get("entity", {}))
        return {"id": h.get("id", h.get("pk")), "d": h["distance"], "e": ent}

    def run(name, fn, mod=None):
        RAW.clear()
        # 显式 mod 与 H 包装的 FunctionScore 注入都要生效，先注入再改写
        wrapped = getattr(fn, "mod", None)
        if mod is not None and wrapped is not None:
            MOD["fn"] = lambda req, a=wrapped, b=mod: (a(req), b(req))
        else:
            MOD["fn"] = mod or wrapped
        try:
            res = fn()
            results[name] = {"hits": [[hit_repr(h) for h in q] for q in res], "raw": list(RAW)}
        except Exception as e:
            results[name] = {"error": failure(e), "raw": list(RAW)}
        finally:
            MOD["fn"] = None

    rng = np.random.default_rng(26)
    DIM = 4
    Q1 = [[1.0, 0.5, -0.5, 0.2]]
    Q2 = [[0.2, -0.4, 0.9, 0.1]]
    Q12 = Q1 + Q2

    def make(name, metric, n=120, extra_schema=None, load=True, dynamic=True):
        s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=dynamic)
        s.add_field("id", DataType.INT64, is_primary=True)
        s.add_field("v", DataType.FLOAT_VECTOR, dim=DIM)
        s.add_field("cat", DataType.VARCHAR, max_length=20)
        s.add_field("g", DataType.INT64)
        s.add_field("age", DataType.INT32)
        s.add_field("price", DataType.DOUBLE)
        s.add_field("small", DataType.INT8)
        s.add_field("tag", DataType.INT64)
        s.add_field("nf", DataType.FLOAT, nullable=True)
        s.add_field("name", DataType.VARCHAR, max_length=20)
        s.add_field("flag", DataType.BOOL)
        s.add_field("j", DataType.JSON)
        ip = c.prepare_index_params(); ip.add_index("v", index_type="FLAT", metric_type=metric)
        c.create_collection(name, schema=s, index_params=ip)
        rows = []
        for i in range(n):
            gi = i % 5
            rows.append({"id": i, "v": rng.standard_normal(DIM).round(3).tolist(), "cat": "ABCDE"[gi], "g": gi,
                         "age": i % 17, "price": float(i % 11) * 1.5, "small": i % 7, "tag": 1 if i % 4 == 0 else 0,
                         "nf": None if i % 6 == 0 else float(i % 9), "name": f"n{i % 13}", "flag": bool(i % 2),
                         "j": {"k": i % 3}, **({"dyn": i % 10} if dynamic else {})})
        c.insert(name, rows)
        if load:
            c.load_collection(name)

    for m in ("COSINE", "L2", "IP"):
        make("rr_" + m.lower(), m)

    # ---------- 构造函数 ----------
    def boost(weight, filt=None, name="b", **extra):
        p = {"reranker": "boost", "weight": weight}
        if filt is not None:
            p["filter"] = filt
        p.update(extra)
        return Function(name, FunctionType.RERANK, [], params=p)

    def FS(*fns, **params):
        return FunctionScore(list(fns), params or None)

    def decay(field, function="gauss", origin=0, scale=10, name="d", **kw):
        p = {"reranker": "decay", "function": function, "origin": origin, "scale": scale}
        p.update(kw)
        return Function(name, FunctionType.RERANK, [field], params=p)

    def rrf_fn(k=None, name="r", **kw):
        p = {"reranker": "rrf"}
        if k is not None:
            p["k"] = k
        p.update(kw)
        return Function(name, FunctionType.RERANK, [], params=p)

    def wtd_fn(weights, norm=None, name="w", **kw):
        p = {"reranker": "weighted", "weights": weights}
        if norm is not None:
            p["norm_score"] = norm
        p.update(kw)
        return Function(name, FunctionType.RERANK, [], params=p)

    def A(coll="rr_cosine", q=Q1, limit=10, expr=None, field="v", param=None):
        return AnnSearchRequest(data=q, anns_field=field, param=param or {}, limit=limit, expr=expr)

    def S(coll="rr_cosine", **kw):
        kw.setdefault("data", Q1); kw.setdefault("limit", 5)
        return lambda: c.search(coll, **kw)

    def H(coll="rr_cosine", reqs=None, ranker=None, limit=5, **kw):
        reqs = reqs if reqs is not None else [A(), A(q=Q2)]
        ranker = ranker if ranker is not None else RRFRanker()
        mod = None
        if isinstance(ranker, FunctionScore):
            # pymilvus 的 hybrid_search 只收 Function / Ranker：用占位 Function，再把请求里的 function_score 换成完整的
            fsm = Prepare.function_score_schema(ranker)
            mod = lambda req: req.function_score.CopyFrom(fsm)
            ranker = rrf_fn()
        f = lambda: c.hybrid_search(coll, reqs=reqs, ranker=ranker, limit=limit, **kw)
        f.mod = mod
        return f

    OF = ["id", "cat", "g", "age", "tag"]

    # ====== 一、Hybrid rank_params：RRF ======
    for m in ("cosine", "l2", "ip"):
        run(f"h rrf default {m}", H("rr_" + m, [A("rr_" + m), A("rr_" + m, q=Q2)], RRFRanker(), output_fields=OF))
    for k in (1, 10, 60, 100, 1000, 16383, 16384, 0, -1, 60.5, "60", None, 1e10):
        rk = RRFRanker.__new__(RRFRanker); rk._strategy = "rrf"; rk._k = k
        run(f"h rrf k={k!r}", H(ranker=rk, output_fields=["id"]))
    run("h rrf same query twice", H(reqs=[A(), A()], output_fields=["id"]))
    run("h rrf three routes", H(reqs=[A(), A(q=Q2), A(q=Q1, limit=3)], output_fields=["id"], limit=8))
    run("h rrf route limits differ", H(reqs=[A(limit=3), A(q=Q2, limit=20)], limit=8, output_fields=["id"]))
    run("h rrf route limit 1", H(reqs=[A(limit=1), A(q=Q2, limit=1)], limit=5, output_fields=["id"]))
    run("h rrf limit>candidates", H(reqs=[A(limit=2), A(q=Q2, limit=2)], limit=10, output_fields=["id"]))
    run("h rrf filters per route", H(reqs=[A(expr="g == 1"), A(q=Q2, expr="g == 2")], limit=8, output_fields=OF))
    run("h rrf one route empty", H(reqs=[A(expr="g == 99"), A(q=Q2)], limit=5, output_fields=["id"]))
    run("h rrf all routes empty", H(reqs=[A(expr="g == 99"), A(q=Q2, expr="g == 98")], limit=5, output_fields=["id"]))
    run("h rrf single route", H(reqs=[A()], limit=5, output_fields=["id"]))
    run("h rrf offset", H(limit=3, offset=2, output_fields=["id"]))
    run("h rrf offset big", H(limit=3, offset=1000, output_fields=["id"]))
    for lim in (0, -1, 1, 200):
        run(f"h rrf limit {lim}", H(limit=lim, output_fields=["id"]))
    run("h rrf offset neg", H(limit=3, offset=-1, output_fields=["id"]))
    run("h rrf nq2 routes", H(reqs=[A(q=Q12), A(q=Q12[::-1])], limit=4, output_fields=["id"]))
    run("h rrf nq mismatch", H(reqs=[A(q=Q12), A(q=Q1)], limit=4, output_fields=["id"]))
    run("h rrf partition", H(limit=4, output_fields=["id"], partition_names=["_default"]))
    run("h rrf partition missing", H(limit=4, output_fields=["id"], partition_names=["nope"]))
    for rd in (-1, 0, 2, 5):
        run(f"h rrf round_decimal {rd}", H(limit=3, round_decimal=rd, output_fields=["id"]))
    for of in (None, [], ["id"], ["cat"], ["*"], ["v"], ["dyn"], ["id", "dyn", "j"], ["nosuch"], ["$meta"]):
        run(f"h rrf output {of!r}", H(limit=2, output_fields=of))

    # ====== 二、Hybrid rank_params：Weighted ======
    for m in ("cosine", "l2", "ip"):
        for norm in (True, False):
            run(f"h wtd {m} norm={norm}", H("rr_" + m, [A(q=Q1), A(q=Q2)], WeightedRanker(0.3, 0.7, norm_score=norm), output_fields=["id"]))
    run("h wtd weights 1,1", H(ranker=WeightedRanker(1, 1), output_fields=["id"]))
    run("h wtd weights 0,0", H(ranker=WeightedRanker(0, 0), output_fields=["id"]))
    run("h wtd weights 0,1", H(ranker=WeightedRanker(0, 1), output_fields=["id"]))
    run("h wtd weights 2,3", H(ranker=WeightedRanker(2, 3), output_fields=["id"]))
    run("h wtd weights -1,1", H(ranker=WeightedRanker(-1, 1), output_fields=["id"]))
    run("h wtd weights 1 of 2", H(ranker=WeightedRanker(0.5), output_fields=["id"]))
    run("h wtd weights 3 of 2", H(ranker=WeightedRanker(0.2, 0.3, 0.5), output_fields=["id"]))
    run("h wtd weights none", H(ranker=WeightedRanker(), output_fields=["id"]))
    run("h wtd single route", H(reqs=[A()], ranker=WeightedRanker(1.0), output_fields=["id"]))
    run("h wtd different filters", H(reqs=[A(expr="g < 3"), A(q=Q2, expr="g > 1")], ranker=WeightedRanker(0.6, 0.4), limit=8, output_fields=["id", "g"]))
    run("h wtd route empty", H(reqs=[A(expr="g == 99"), A(q=Q2)], ranker=WeightedRanker(0.6, 0.4), output_fields=["id"]))
    for name, mk in (("norm str", "True"), ("norm int1", 1), ("norm none", None), ("norm str false", "false"), ("norm junk", "maybe")):
        def mkr(mk=mk):
            w = WeightedRanker(0.5, 0.5); w._norm_score = mk
            return w
        run(f"h wtd {name}", H(ranker=mkr(), output_fields=["id"]))
    # 路由级 metric_type 不同（混合度量）
    run("h wtd mixed metric cos+l2 norm", H(reqs=[A(param={"metric_type": "COSINE"}), A(q=Q2, param={"metric_type": "L2"})], ranker=WeightedRanker(0.5, 0.5, norm_score=True), output_fields=["id"]))
    run("h wtd mixed metric cos+l2 raw", H(reqs=[A(param={"metric_type": "COSINE"}), A(q=Q2, param={"metric_type": "L2"})], ranker=WeightedRanker(0.5, 0.5, norm_score=False), output_fields=["id"]))
    run("h wtd mixed metric ip+l2 raw", H(reqs=[A(param={"metric_type": "IP"}), A(q=Q2, param={"metric_type": "L2"})], ranker=WeightedRanker(0.5, 0.5, norm_score=False), output_fields=["id"]))
    run("h wtd mixed metric cos+ip raw", H(reqs=[A(param={"metric_type": "COSINE"}), A(q=Q2, param={"metric_type": "IP"})], ranker=WeightedRanker(0.5, 0.5, norm_score=False), output_fields=["id"]))
    run("h wtd l2+l2 raw", H("rr_l2", [A(q=Q1, param={"metric_type": "L2"}), A(q=Q2, param={"metric_type": "L2"})], WeightedRanker(0.5, 0.5, norm_score=False), output_fields=["id"]))
    run("h rrf mixed metric", H(reqs=[A(param={"metric_type": "COSINE"}), A(q=Q2, param={"metric_type": "L2"})], output_fields=["id"]))
    # 度量与索引不一致
    run("h metric mismatch index", H(reqs=[A(param={"metric_type": "L2"})] , ranker=RRFRanker(), output_fields=["id"]))

    # ====== 三、Hybrid 策略/请求错误 ======
    class FakeRanker:
        def __init__(self, d): self._d = d
        def dict(self): return self._d
    class RawRank(BaseRanker):
        def __init__(self, d): self._d = d
        def dict(self): return dict(self._d)
    run("h strategy unknown", H(ranker=RawRank({"strategy": "foo", "params": {}}), output_fields=["id"]))
    run("h strategy missing", H(ranker=RawRank({"params": {}}), output_fields=["id"]))
    run("h strategy upper RRF", H(ranker=RawRank({"strategy": "RRF", "params": {"k": 60}}), output_fields=["id"]))
    run("h strategy upper Weighted", H(ranker=RawRank({"strategy": "Weighted", "params": {"weights": [0.5, 0.5]}}), output_fields=["id"]))
    run("h strategy weighted no norm key", H(ranker=RawRank({"strategy": "weighted", "params": {"weights": [0.5, 0.5]}}), output_fields=["id"]))
    run("h strategy params list", H(ranker=RawRank({"strategy": "rrf", "params": [1]}), output_fields=["id"]))
    run("h strategy params missing", H(ranker=RawRank({"strategy": "rrf"}), output_fields=["id"]))
    run("h strategy max/sum/avg", H(ranker=RawRank({"strategy": "max", "params": {}}), output_fields=["id"]))
    run("h empty reqs", H(reqs=[], output_fields=["id"]))
    run("h bad anns_field", H(reqs=[A(field="nosuch"), A(q=Q2)], output_fields=["id"]))
    run("h bad anns_field second", H(reqs=[A(), A(q=Q2, field="nosuch")], output_fields=["id"]))
    run("h non-vector anns_field", H(reqs=[A(field="g")], output_fields=["id"]))
    run("h bad filter route2", H(reqs=[A(), A(q=Q2, expr="g ==")], output_fields=["id"]))
    run("h bad filter route1 bad anns2", H(reqs=[A(expr="g =="), A(q=Q2, field="nosuch")], output_fields=["id"]))
    run("h bad dim", H(reqs=[A(q=[[1.0, 2.0]])], output_fields=["id"]))
    run("h missing collection", H("nosuch", [A()], output_fields=["id"]))
    run("h bad output + bad ranker", H(ranker=RawRank({"strategy": "foo", "params": {}}), output_fields=["nosuch"]))
    run("h bad strategy + bad filter", H(reqs=[A(expr="g ==")], ranker=RawRank({"strategy": "foo", "params": {}}), output_fields=["id"]))
    run("h bad limit str", H(ranker=RawRank({"strategy": "rrf", "params": {}}), output_fields=["id"]), mod=lambda r: [setattr(kv, "value", '"x"') for kv in r.rank_params if kv.key == "limit"])
    run("h function_chains rejected", H(output_fields=["id"]), mod=lambda r: r.function_chains.add())
    run("h function_chains + bad strategy", H(ranker=RawRank({"strategy": "foo", "params": {}}), output_fields=["id"]), mod=lambda r: r.function_chains.add())

    run("h function_chains + missing collection", H("nosuch", [A()], output_fields=["id"]), mod=lambda r: r.function_chains.add())
    run("h bad limit + missing collection", H("nosuch", [A()], RRFRanker(), output_fields=["id"]), mod=lambda r: [setattr(kv, "value", '"x"') for kv in r.rank_params if kv.key == "limit"])
    run("h bad limit + bad route", H(reqs=[A(field="nosuch")], output_fields=["id"]), mod=lambda r: [setattr(kv, "value", '"x"') for kv in r.rank_params if kv.key == "limit"])
    run("h bad function_score + bad route", H(reqs=[A(field="nosuch")], ranker=FS(boost("x")), output_fields=["id"]))
    run("h bad function_score + missing coll", H("nosuch", [A()], FS(boost("x")), output_fields=["id"]))
    run("s bad boost + missing coll", S("nosuch", ranker=FS(boost("x")), output_fields=["id"]))
    run("s bad boost + bad anns", S(ranker=FS(boost("x")), anns_field="nosuch", output_fields=["id"]))
    run("s bad L2 + bad anns", S(ranker=decay("age", "foo", 0, 8), anns_field="nosuch", output_fields=["id"]))
    run("s bad L2 + bad filter", S(ranker=decay("age", "foo", 0, 8), filter="g ==", output_fields=["id"]))
    run("s unknown reranker + bad filter", S(ranker=Function("x", FunctionType.RERANK, [], params={"reranker": "foo"}), filter="g ==", output_fields=["id"]))
    run("h limit as kw 0 raw", H(output_fields=["id"], limit=5), mod=lambda r: [setattr(kv, "value", "0") for kv in r.rank_params if kv.key == "limit"])
    run("h limit raw -1", H(output_fields=["id"], limit=5), mod=lambda r: [setattr(kv, "value", "-1") for kv in r.rank_params if kv.key == "limit"])
    run("h limit raw missing", H(output_fields=["id"], limit=5), mod=lambda r: [r.rank_params.remove(kv) for kv in list(r.rank_params) if kv.key == "limit"])
    run("h offset raw str", H(output_fields=["id"], limit=5), mod=lambda r: [setattr(kv, "value", '"x"') for kv in r.rank_params if kv.key == "offset"])
    run("h sub request limit 0 raw", H(output_fields=["id"], limit=5), mod=lambda r: [setattr(k_, "value", "0") for k_ in r.requests[0].search_params if k_.key == "topk"])
    run("h sub request limit missing", H(output_fields=["id"], limit=5), mod=lambda r: [r.requests[0].search_params.remove(k_) for k_ in list(r.requests[0].search_params) if k_.key == "topk"])
    # ====== 评审补充：非动态集合、Search L2 / boost 分组矩阵、优先级与拒绝时机 ======
    make("rr_nodyn", "COSINE", n=60, dynamic=False)
    for flt in ("nosuch > 1", "dyn > 5", "age > 5", "age + 1 > 5", "text_match(name, 'n1')", "j['k'] == 1", "$meta['x'] > 1"):
        run(f"nodyn boost filter {flt}", S("rr_nodyn", ranker=FS(boost(2.0, flt)), output_fields=["id"], limit=6))
        run(f"dyn boost filter {flt}", S("rr_cosine", ranker=FS(boost(2.0, flt)), output_fields=["id"], limit=6))
    run("nodyn search filter unknown", S("rr_nodyn", filter="nosuch > 1", output_fields=["id"]))
    for fld in ("nosuch", "v", "price", "j", "dyn", "g", "nf", "cat", "id", "flag"):
        for st in (None, True):
            kw = {} if st is None else {"strict_group_size": st}
            run(f"s l2 group field {fld} strict={st}", S(ranker=decay("age", "gauss", 0, 8), group_by_field=fld, group_size=2, limit=3, output_fields=["id"], **kw))
        run(f"s boost group field {fld}", S(ranker=FS(boost(2.0, "g == 1")), group_by_field=fld, group_size=2, limit=3, output_fields=["id"]))
        run(f"s l2 rrf group field {fld}", S(ranker=rrf_fn(), group_by_field=fld, group_size=2, limit=3, output_fields=["id"]))
    for gs in (0, -1, -2, 1000):
        run(f"s l2 group_size {gs}", S(ranker=decay("age", "gauss", 0, 8), group_by_field="cat", group_size=gs, limit=3, output_fields=["id", "cat"]))
    for off in (-1, -3, 1, 100):
        run(f"s l2 group offset {off}", S(ranker=decay("age", "gauss", 0, 8), group_by_field="cat", group_size=2, offset=off, limit=2, output_fields=["id", "cat"]))
        run(f"s l2 offset {off}", S(ranker=decay("age", "gauss", 0, 8), offset=off, limit=4, output_fields=["id"]))
        run(f"s boost offset {off}", S(ranker=FS(boost(2.0, "g == 1")), offset=off, limit=4, output_fields=["id"]))
    for lim in ("0", "-1"):
        mk = lambda r, lim=lim: [setattr(k_, "value", lim) for k_ in r.search_params if k_.key == "topk"]
        run(f"s l2 limit raw {lim}", S(ranker=decay("age", "gauss", 0, 8), output_fields=["id"]), mod=mk)
        run(f"s l2 limit raw {lim} group", S(ranker=decay("age", "gauss", 0, 8), group_by_field="cat", output_fields=["id"]), mod=mk)
        run(f"s boost limit raw {lim}", S(ranker=FS(boost(2.0, "g == 1")), output_fields=["id"]), mod=mk)
        run(f"h group limit raw {lim}", H(output_fields=["id", "cat"], limit=5, group_by_field="cat", group_size=2), mod=lambda r, lim=lim: [setattr(k_, "value", lim) for k_ in r.rank_params if k_.key == "limit"])
    run("h group offset -1", H(limit=2, offset=-1, group_by_field="cat", group_size=2, output_fields=["id", "cat"]))
    run("h group offset -3", H(limit=2, offset=-3, group_by_field="cat", group_size=2, output_fields=["id", "cat"]))
    run("h group_size -2", H(limit=3, group_by_field="cat", group_size=-2, output_fields=["id", "cat"]))
    # function_score 与 rank_params 同时存在
    run("h fs L2 + rank_params foo", H(ranker=decay("age", "gauss", 0, 8), output_fields=["id", "age"]), mod=lambda r: r.rank_params.extend([kv("strategy", '"foo"')]))
    run("h fs L2 + rank_params weighted", H(ranker=decay("age", "gauss", 0, 8), output_fields=["id", "age"]), mod=lambda r: r.rank_params.extend([kv("strategy", '"weighted"'), kv("params", '{"weights":[1]}')]))
    run("h fs boost + rank_params wtd", H(ranker=FS(boost(2.0, "g == 1")), output_fields=["id"]), mod=lambda r: r.rank_params.extend([kv("strategy", '"weighted"'), kv("params", '{"weights":[0.5,0.5],"norm_score":false}')]))
    run("h fs rrf + rank_params limit", H(ranker=rrf_fn(), output_fields=["id"], limit=3))
    # 分区：子请求优先，其次顶层
    run("h partition top only", H(output_fields=["id"], partition_names=["_default"]))
    run("h partition top missing", H(output_fields=["id"], partition_names=["nope"]))
    run("h partition route overrides top", H(output_fields=["id"], partition_names=["nope"]), mod=lambda r: [r.requests[i].partition_names.append("_default") for i in range(len(r.requests))] and None)
    run("h partition route missing top ok", H(output_fields=["id"], partition_names=["_default"]), mod=lambda r: r.requests[0].partition_names.append("nope"))
    # 子请求自带 function_score / output_fields 的处理
    run("h route boost merges", H(output_fields=["id", "g"]), mod=lambda r: r.requests[0].function_score.CopyFrom(Prepare.function_score_schema(FS(boost(2.0, "g == 1")))))
    run("h route L2 ignored", H(output_fields=["id", "g"]), mod=lambda r: r.requests[0].function_score.CopyFrom(Prepare.function_score_schema(FS(decay("age", "gauss", 0, 8)))))
    run("h route bad L2 ignored", H(output_fields=["id", "g"]), mod=lambda r: r.requests[0].function_score.CopyFrom(Prepare.function_score_schema(FS(Function("x", FunctionType.RERANK, [], params={"reranker": "foo"})))))
    run("h route output_fields ignored", H(output_fields=["id"]), mod=lambda r: r.requests[0].output_fields.append("cat"))
    run("h route boost + top boost", H(ranker=FS(boost(3.0, "g == 2")), output_fields=["id", "g"]), mod=lambda r: r.requests[0].function_score.CopyFrom(Prepare.function_score_schema(FS(boost(2.0, "g == 1"), boost_mode="sum"))))
    # model / random_score 的位置
    mod_fn = lambda **p: Function("m", FunctionType.RERANK, ["name"], params={"reranker": "model", **p})
    run("s model valid + bad anns", S(ranker=mod_fn(provider="cohere", queries=["a"]), anns_field="nosuch", output_fields=["id"]))
    run("s model bad field", S(ranker=Function("m", FunctionType.RERANK, ["age"], params={"reranker": "model", "provider": "cohere", "queries": ["a"]}), output_fields=["id"]))
    run("s model valid", S(ranker=mod_fn(provider="cohere", queries=["a"]), output_fields=["id"]))
    run("h model valid", H(ranker=mod_fn(provider="cohere", queries=["a"]), output_fields=["id"]))
    run("h model valid + bad route", H(reqs=[A(field="nosuch")], ranker=mod_fn(provider="cohere", queries=["a"]), output_fields=["id"]))
    run("s random_score valid", S(ranker=FS(boost(2.0, None, random_score={"seed": 1})), output_fields=["id"]))
    run("s random_score + bad filter", S(ranker=FS(boost(2.0, "age >>", random_score={"seed": 1})), output_fields=["id"]))
    run("s random_score + bad anns", S(ranker=FS(boost(2.0, None, random_score={"seed": 1})), anns_field="nosuch", output_fields=["id"]))
    # 3.x 未加载
    make("rr_unloaded", "COSINE", n=10, load=False)
    c.release_collection("rr_unloaded")
    run("h unloaded", H("rr_unloaded", [A()], output_fields=["id"]))
    run("h unloaded bad strategy", H("rr_unloaded", [A()], RawRank({"strategy": "foo", "params": {}}), output_fields=["id"]))
    run("h unloaded bad anns", H("rr_unloaded", [A(field="nosuch")], output_fields=["id"]))
    run("s unloaded boost", S("rr_unloaded", ranker=FS(boost(2.0)), output_fields=["id"]))
    run("s unloaded bad boost", S("rr_unloaded", ranker=FS(boost("x")), output_fields=["id"]))

    # ====== 四、Hybrid 分组 ======
    for gs in (None, 1, 2, 3):
        for lim in (2, 4):
            kw = dict(group_by_field="cat", output_fields=["id", "cat"])
            if gs: kw["group_size"] = gs
            run(f"h group rrf lim{lim} gs{gs}", H(limit=lim, **kw))
            run(f"h group wtd lim{lim} gs{gs}", H(ranker=WeightedRanker(0.5, 0.5), limit=lim, **kw))
    run("h group offset", H(limit=2, offset=1, group_by_field="cat", group_size=2, output_fields=["id", "cat"]))
    for st in (True, False, "true", "false", 1, 0, None):
        run(f"h group strict {st!r}", H(limit=5, group_by_field="cat", group_size=30, strict_group_size=st, output_fields=["id", "cat"]))
    run("h group hidden field", H(limit=3, group_by_field="cat", group_size=2, output_fields=["id"]))
    run("h group no output", H(limit=3, group_by_field="cat", group_size=2))
    run("h group star", H(limit=3, group_by_field="cat", group_size=2, output_fields=["*"]))
    run("h group bad field", H(limit=3, group_by_field="nosuch", output_fields=["id"]))
    for fld in ("g", "small", "flag", "age", "price", "j", "nf", "v", "dyn"):
        run(f"h group field {fld}", H(limit=3, group_by_field=fld, group_size=1, output_fields=["id"]))
    run("h group nullable nf", H(limit=10, group_by_field="nf", group_size=1, output_fields=["id", "nf"]))
    run("h group scorer sum", H(limit=3, group_by_field="cat", group_size=2, output_fields=["id", "cat"], rank_group_scorer="sum"))
    run("h group scorer avg", H(limit=3, group_by_field="cat", group_size=2, output_fields=["id", "cat"], rank_group_scorer="avg"))
    run("h group scorer bad", H(limit=3, group_by_field="cat", group_size=2, output_fields=["id", "cat"], rank_group_scorer="min"))
    for gs in (0, -1, "2", "abc", 1000):
        run(f"h group_size {gs!r}", H(limit=3, group_by_field="cat", group_size=gs, output_fields=["id", "cat"]))
    run("h group + filters", H(reqs=[A(expr="g < 4"), A(q=Q2, expr="g > 0")], limit=3, group_by_field="cat", group_size=2, output_fields=["id", "cat"]))
    run("h group metric l2 wtd", H("rr_l2", [A("rr_l2"), A("rr_l2", q=Q2)], WeightedRanker(0.5, 0.5, norm_score=False), limit=3, group_by_field="cat", group_size=2, output_fields=["id", "cat"]))

    # ====== 五、Hybrid function_score（顶层） ======
    for name, fn in (("rrf", rrf_fn()), ("rrf k=10", rrf_fn(10)), ("rrf k=0", rrf_fn(0)), ("rrf k=16384", rrf_fn(16384)),
                     ("rrf k=-5", rrf_fn(-5)), ("rrf k=abc", rrf_fn("abc")),
                     ("wtd norm F", wtd_fn([0.3, 0.7])), ("wtd norm T", wtd_fn([0.3, 0.7], True)),
                     ("wtd norm str", wtd_fn([0.3, 0.7], "true")), ("wtd norm bad", wtd_fn([0.3, 0.7], "maybe")),
                     ("wtd len mismatch", wtd_fn([0.3])), ("wtd none", wtd_fn([])), ("wtd range", wtd_fn([0.3, 1.5])),
                     ("wtd negative", wtd_fn([-0.1, 0.5])), ("wtd str weights", wtd_fn("[0.3,0.7]")),
                     ("wtd junk weights", wtd_fn("abc")), ("wtd dict weights", wtd_fn({"a": 1}))):
        run(f"h fs {name}", H(ranker=fn, output_fields=["id"]))
    for m in ("cosine", "l2", "ip"):
        for norm in (None, True, False):
            run(f"h fs wtd {m} norm={norm}", H("rr_" + m, [A("rr_" + m), A("rr_" + m, q=Q2)], wtd_fn([0.4, 0.6], norm), output_fields=["id"]))
    for fs_ in ("gauss", "exp", "linear"):
        for mode in (None, "max", "sum", "avg"):
            kw = {} if mode is None else {"score_mode": mode}
            run(f"h fs decay {fs_} mode={mode}", H(ranker=decay("age", fs_, 0, 8, **kw), output_fields=["id", "age"]))
    run("h fs decay norm_score", H(ranker=decay("age", "gauss", 0, 8, norm_score=True), output_fields=["id", "age"]))
    run("h fs decay norm_score F", H(ranker=decay("age", "gauss", 0, 8, norm_score=False), output_fields=["id", "age"]))
    run("h fs decay hidden", H(ranker=decay("age", "gauss", 0, 8), output_fields=["id"]))
    run("h fs decay no output", H(ranker=decay("age", "gauss", 0, 8)))
    run("h fs decay star", H(ranker=decay("age", "gauss", 0, 8), output_fields=["*"], limit=2))
    run("h fs decay l2", H("rr_l2", [A("rr_l2"), A("rr_l2", q=Q2)], decay("age", "gauss", 0, 8), output_fields=["id", "age"]))
    run("h fs decay ip", H("rr_ip", [A("rr_ip"), A("rr_ip", q=Q2)], decay("age", "linear", 0, 8), output_fields=["id", "age"]))
    run("h fs decay group", H(ranker=decay("age", "gauss", 0, 8), limit=3, group_by_field="cat", group_size=2, output_fields=["id", "cat", "age"]))
    run("h fs decay offset", H(ranker=decay("age", "gauss", 0, 8), limit=3, offset=2, output_fields=["id", "age"]))
    for field in ("price", "small", "g", "nf", "name", "flag", "j", "v", "nosuch", "dyn", "id", "cat"):
        run(f"h fs decay field {field}", H(ranker=decay(field, "gauss", 0, 8), output_fields=["id"]))
    for kw in ({"offset": 2}, {"offset": -1}, {"decay": 0.1}, {"decay": 0}, {"decay": 1}, {"decay": 2}, {"scale": 0}, {"scale": -1},
               {"origin": 5}, {"origin": "x"}, {"scale": "x"}, {"function": "foo"}, {"function": "GAUSS"}, {"offset": "x"}, {"decay": "x"}):
        p = dict(function="gauss", origin=0, scale=8); p.update(kw)
        run(f"h fs decay params {kw!r}", H(ranker=Function("d", FunctionType.RERANK, ["age"], params={"reranker": "decay", **p}), output_fields=["id", "age"]))
    for drop in ("function", "origin", "scale"):
        p = {"reranker": "decay", "function": "gauss", "origin": 0, "scale": 8}; del p[drop]
        run(f"h fs decay missing {drop}", H(ranker=Function("d", FunctionType.RERANK, ["age"], params=p), output_fields=["id"]))
    run("h fs decay 2 inputs", H(ranker=Function("d", FunctionType.RERANK, ["age", "price"], params={"reranker": "decay", "function": "gauss", "origin": 0, "scale": 8}), output_fields=["id"]))
    run("h fs decay 0 inputs", H(ranker=Function("d", FunctionType.RERANK, [], params={"reranker": "decay", "function": "gauss", "origin": 0, "scale": 8}), output_fields=["id"]))
    run("h fs decay score_mode bad", H(ranker=decay("age", "gauss", 0, 8, score_mode="min"), output_fields=["id"]))
    # boost
    for bm, fm in ((None, None), ("multiply", "multiply"), ("sum", "sum"), ("multiply", "sum"), ("sum", "multiply"), ("Sum", "Multiply"), ("bad", None), (None, "bad")):
        p = {}
        if bm: p["boost_mode"] = bm
        if fm: p["function_mode"] = fm
        run(f"h fs boost modes {bm},{fm}", H(ranker=FS(boost(2.0, "g == 1"), boost(3.0, "cat == 'B'"), **p), output_fields=["id", "g"], reqs=[A(), A(q=Q2)]))
    run("h fs boost + rrf", H(ranker=FS(boost(2.0, "g == 1"), rrf_fn()), output_fields=["id", "g"]))
    run("h fs boost + wtd", H(ranker=FS(boost(2.0, "g == 1"), wtd_fn([0.5, 0.5])), output_fields=["id", "g"]))
    run("h fs boost + decay", H(ranker=FS(boost(2.0, "g == 1"), decay("age", "gauss", 0, 8)), output_fields=["id", "g"]))
    run("h fs boost only", H(ranker=FS(boost(2.0, "g == 1")), output_fields=["id", "g"]))
    run("h fs boost only metrics l2", H("rr_l2", [A("rr_l2"), A("rr_l2", q=Q2)], FS(boost(2.0, "g == 1")), output_fields=["id", "g"]))
    run("h fs two L2", H(ranker=FS(rrf_fn(), wtd_fn([0.5, 0.5])), output_fields=["id"]))
    run("h fs two L2 rrf rrf", H(ranker=FS(rrf_fn(name="r1"), rrf_fn(name="r2")), output_fields=["id"]))
    run("h fs unknown reranker", H(ranker=Function("x", FunctionType.RERANK, [], params={"reranker": "foo"}), output_fields=["id"]))
    run("h fs no reranker", H(ranker=Function("x", FunctionType.RERANK, [], params={}), output_fields=["id"]))
    run("h fs reranker upper", H(ranker=Function("x", FunctionType.RERANK, [], params={"reranker": "RRF"}), output_fields=["id"]))
    run("h fs strategy param only", H(ranker=Function("x", FunctionType.RERANK, [], params={"strategy": "weighted", "weights": [0.5, 0.5]}), output_fields=["id"]))
    run("h fs model no provider", H(ranker=Function("m", FunctionType.RERANK, ["name"], params={"reranker": "model"}), output_fields=["id"]))
    run("h fs model bad queries", H(ranker=Function("m", FunctionType.RERANK, ["name"], params={"reranker": "model", "queries": []}), output_fields=["id"]))
    run("h fs model provider unknown", H(ranker=Function("m", FunctionType.RERANK, ["name"], params={"reranker": "model", "provider": "nosuch", "queries": ["a"]}), output_fields=["id"]))
    run("h fs boost non-empty input", H(ranker=Function("b", FunctionType.RERANK, ["age"], params={"reranker": "boost", "weight": 2}), output_fields=["id"]))
    run("h fs non-rerank function type", H(ranker=Function("b", FunctionType.BM25, ["name"], params={"reranker": "rrf"}), output_fields=["id"]))
    run("h fs two functions via FunctionScore", H(ranker=FS(rrf_fn()), output_fields=["id"]))
    run("h fs empty functions", H(ranker=FS(), output_fields=["id"]), mod=lambda r: r.function_score.SetInParent())

    # ====== 六、Search + function_score：boost ======
    for m in ("cosine", "l2", "ip"):
        coll = "rr_" + m
        run(f"s boost none {m}", S(coll, output_fields=OF))
        for w in (0.5, 2.0, 10.0, 0, -1, 1):
            run(f"s boost w={w} g==1 {m}", S(coll, ranker=FS(boost(w, "g == 1")), output_fields=OF))
        run(f"s boost nofilter w=2 {m}", S(coll, ranker=FS(boost(2.0)), output_fields=OF))
        for bm in ("multiply", "sum"):
            for fm in ("multiply", "sum"):
                run(f"s boost 2fn {bm}/{fm} {m}", S(coll, ranker=FS(boost(2.0, "g == 1"), boost(0.5, "tag == 1"), boost_mode=bm, function_mode=fm), output_fields=OF, limit=8))
            run(f"s boost sum w=0.3 {m} {bm}", S(coll, ranker=FS(boost(0.3, "g == 1"), boost_mode=bm), output_fields=OF, limit=8))
    run("s boost filter hidden field", S(ranker=FS(boost(2.0, "age > 10")), output_fields=["id"]))
    run("s boost filter hidden no output", S(ranker=FS(boost(2.0, "age > 10"))))
    run("s boost filter star", S(ranker=FS(boost(2.0, "age > 10")), output_fields=["*"], limit=2))
    run("s boost filter dyn", S(ranker=FS(boost(2.0, "dyn > 5")), output_fields=["id"]))
    run("s boost filter json", S(ranker=FS(boost(2.0, "j['k'] == 1")), output_fields=["id"]))
    run("s boost filter pk", S(ranker=FS(boost(3.0, "id < 50")), output_fields=["id"]))
    run("s boost filter pk hidden", S(ranker=FS(boost(3.0, "id in [1,2,3,4,5,6,7,8]")), output_fields=["cat"]))
    for of in (None, [], ["*"], ["$meta"], ["dyn"], ["dyn2"], ["id", "dyn"], ["cat"]):
        run(f"s boost filter dyn output {of!r}", S(ranker=FS(boost(2.0, "dyn > 5")), output_fields=of, limit=6))
        run(f"s boost filter json output {of!r}", S(ranker=FS(boost(2.0, "j['k'] == 1")), output_fields=of, limit=6))
        run(f"s boost filter meta output {of!r}", S(ranker=FS(boost(2.0, "$meta['dyn'] > 5")), output_fields=of, limit=6))
    run("s boost filter dyn nq2 star", S(data=Q12, ranker=FS(boost(2.0, "dyn > 5")), output_fields=["*"], limit=4))
    run("s boost filter null cmp", S(ranker=FS(boost(2.0, "nf is null")), output_fields=["id", "nf"], limit=8))
    run("s boost filter not null", S(ranker=FS(boost(2.0, "not (nf > 3)")), output_fields=["id", "nf"], limit=8))
    run("s boost filter like", S(ranker=FS(boost(2.0, "name like 'n1%'")), output_fields=["id", "name"], limit=8))
    run("s boost filter in", S(ranker=FS(boost(2.0, "cat in ['A','C']")), output_fields=["id", "cat"], limit=8))
    run("s boost filter timezone naive", S(ranker=FS(boost(2.0, "age > 5 and g == 1")), output_fields=["id"], limit=8))
    run("s boost filter text_match", S(ranker=FS(boost(2.0, "text_match(name, 'n1')")), output_fields=["id"], limit=8))
    run("s boost filter arithmetic", S(ranker=FS(boost(2.0, "age + 1 > 5")), output_fields=["id"], limit=8))
    run("s boost filter int overflow", S(ranker=FS(boost(2.0, "age > 99999999999999999999")), output_fields=["id"], limit=8))
    run("s boost filter bool", S(ranker=FS(boost(2.0, "flag == true")), output_fields=["id"], limit=8))
    run("s boost filter bad+unloaded", S("rr_unloaded", ranker=FS(boost(2.0, "age >>")), output_fields=["id"]))
    run("s boost filter bad+search filter bad", S(ranker=FS(boost(2.0, "age >>")), filter="g ==", output_fields=["id"]))
    run("s boost filter search filter bad", S(ranker=FS(boost(2.0, "age > 1")), filter="g ==", output_fields=["id"]))
    run("s boost filter bad field in search", S(ranker=FS(boost(2.0, "age > 1")), anns_field="nosuch", output_fields=["id"]))
    run("s boost filter bad only if hits", S(ranker=FS(boost(2.0, "age >>")), filter="age < 0", output_fields=["id"]))
    run("s boost filter bad compile (hits)", S(ranker=FS(boost(2.0, "age >>")), output_fields=["id"]))
    run("s boost filter type error", S(ranker=FS(boost(2.0, "age == 'x'")), output_fields=["id"], limit=8))
    run("s boost filter type error name", S(ranker=FS(boost(2.0, "name > 3")), output_fields=["id"], limit=8))
    run("s boost filter json cmp str", S(ranker=FS(boost(2.0, "j['k'] == 'a'")), output_fields=["id"], limit=8))
    run("s boost filter array func", S(ranker=FS(boost(2.0, "json_contains(j['k'], 1)")), output_fields=["id"], limit=8))
    run("s boost filter bad", S(ranker=FS(boost(2.0, "age >>")), output_fields=["id"]))
    run("s boost filter bad later", S(ranker=FS(boost(2.0, "age >>")), output_fields=["id"]))
    run("s boost filter unknown field", S(ranker=FS(boost(2.0, "nosuch > 1")), output_fields=["id"]))
    run("s boost filter nullable", S(ranker=FS(boost(2.0, "nf > 3")), output_fields=["id", "nf"], limit=8))
    run("s boost filter empty str", S(ranker=FS(boost(2.0, "")), output_fields=["id"]))
    run("s boost filter nonstr", S(ranker=FS(boost(2.0, 5)), output_fields=["id"]))
    run("s boost weight str ok", S(ranker=FS(boost("2.5", "g == 1")), output_fields=["id"]))
    run("s boost weight bad", S(ranker=FS(boost("abc", "g == 1")), output_fields=["id"]))
    run("s boost weight missing", S(ranker=FS(Function("b", FunctionType.RERANK, [], params={"reranker": "boost", "filter": "g == 1"})), output_fields=["id"]))
    run("s boost weight nan", S(ranker=FS(boost("nan", "g == 1")), output_fields=["id"]))
    run("s boost weight inf", S(ranker=FS(boost("inf", "g == 1")), output_fields=["id"]))
    run("s boost reranker upper", S(ranker=FS(Function("b", FunctionType.RERANK, [], params={"reranker": "BOOST", "weight": 2})), output_fields=["id"]))
    run("s boost no reranker", S(ranker=FS(Function("b", FunctionType.RERANK, [], params={"weight": 2})), output_fields=["id"]))
    run("s boost non-empty input", S(ranker=FS(Function("b", FunctionType.RERANK, ["age"], params={"reranker": "boost", "weight": 2})), output_fields=["id"]))
    run("s boost random_score", S(ranker=FS(boost(2.0, "g == 1", random_score={"seed": 1, "field": "id"})), output_fields=["id"]))
    run("s boost random_score noseed", S(ranker=FS(boost(2.0, None, random_score={})), output_fields=["id"]))
    run("s boost random_score bad", S(ranker=FS(boost(2.0, None, random_score="x")), output_fields=["id"]))
    run("s boost 3 fns", S(ranker=FS(boost(2.0, "g == 1"), boost(3.0, "g == 1"), boost(0.5, "g == 2")), output_fields=OF, limit=8))
    run("s boost 0 weight filter", S(ranker=FS(boost(0.0, "g == 1")), output_fields=OF, limit=8))
    run("s boost function_score params only", S(ranker=FS(boost(2.0, "g == 1"), boost_mode="Multiply", function_mode="Sum"), output_fields=OF))
    run("s boost offset", S(ranker=FS(boost(2.0, "g == 1")), offset=3, output_fields=OF))
    run("s boost offset big", S(ranker=FS(boost(2.0, "g == 1")), offset=500, output_fields=OF))
    run("s boost round_decimal", S(ranker=FS(boost(2.0, "g == 1")), output_fields=["id"], search_params={"metric_type": "COSINE", "round_decimal": 2}))
    run("s boost round_decimal kw", S(ranker=FS(boost(2.0, "g == 1")), output_fields=["id"], round_decimal=2))
    run("s boost group", S(ranker=FS(boost(2.0, "g == 1")), group_by_field="cat", group_size=2, limit=3, output_fields=["id", "cat"]))
    run("s boost group hidden", S(ranker=FS(boost(2.0, "g == 1")), group_by_field="cat", group_size=2, limit=3, output_fields=["id"]))
    run("s boost range", S(ranker=FS(boost(2.0, "g == 1")), limit=5, output_fields=["id"], search_params={"metric_type": "COSINE", "params": {"radius": 0.0, "range_filter": 1.0}}))
    run("s boost range l2", S("rr_l2", ranker=FS(boost(2.0, "g == 1")), limit=5, output_fields=["id"], search_params={"metric_type": "L2", "params": {"radius": 6.0, "range_filter": 0.0}}))
    run("s boost filter+search filter", S(ranker=FS(boost(2.0, "g == 1")), filter="age < 8", output_fields=OF))
    run("s boost nq2", S(data=Q12, ranker=FS(boost(2.0, "g == 1")), output_fields=OF))
    run("s boost partition", S(ranker=FS(boost(2.0, "g == 1")), partition_names=["_default"], output_fields=OF))
    run("s boost empty result", S(ranker=FS(boost(2.0, "g == 1")), filter="age < 0", output_fields=OF))
    run("s boost limit 0", S(ranker=FS(boost(2.0, "g == 1")), limit=0, output_fields=["id"]))
    run("s boost vector output", S(ranker=FS(boost(2.0, "g == 1")), output_fields=["v"], limit=2))
    run("s boost no output", S(ranker=FS(boost(2.0, "g == 1")), limit=3))
    run("s boost output []", S(ranker=FS(boost(2.0, "g == 1")), output_fields=[], limit=3))
    run("s boost output dyn", S(ranker=FS(boost(2.0, "g == 1")), output_fields=["dyn", "id"], limit=3))
    run("s boost output nosuch", S(ranker=FS(boost(2.0, "g == 1")), output_fields=["nosuch"], limit=3))
    run("s non-boost bad reranker", S(ranker=Function("x", FunctionType.RERANK, [], params={"reranker": "foo"}), output_fields=["id"]))
    run("s non-rerank function type", S(ranker=Function("x", FunctionType.BM25, ["name"], params={"reranker": "rrf"}), output_fields=["id"]))
    run("s boost ranker + function_chains", S(ranker=FS(boost(2.0, "g == 1")), output_fields=["id"]), mod=lambda r: r.function_chains.add())

    # ====== 七、Search + L2 rerank（rrf / weighted / decay） ======
    for m in ("cosine", "l2", "ip"):
        coll = "rr_" + m
        run(f"s l2 rrf {m}", S(coll, ranker=rrf_fn(), output_fields=["id"], limit=5))
        run(f"s l2 rrf k=5 {m}", S(coll, ranker=rrf_fn(5), output_fields=["id"], limit=5))
        for norm in (None, True, False):
            run(f"s l2 wtd {m} norm={norm}", S(coll, ranker=wtd_fn([0.7], norm), output_fields=["id"], limit=5))
        for fn_ in ("gauss", "exp", "linear"):
            for mode in (None, "sum", "avg"):
                kw = {} if mode is None else {"score_mode": mode}
                run(f"s l2 decay {fn_} {m} {mode}", S(coll, ranker=decay("age", fn_, 0, 8, **kw), output_fields=["id", "age"], limit=5))
            run(f"s l2 decay {fn_} norm {m}", S(coll, ranker=decay("age", fn_, 0, 8, norm_score=True), output_fields=["id", "age"], limit=5))
    run("s l2 decay offset", S(ranker=decay("age", "gauss", 0, 8), offset=3, output_fields=["id", "age"], limit=4))
    run("s l2 decay offset big", S(ranker=decay("age", "gauss", 0, 8), offset=500, output_fields=["id", "age"], limit=4))
    run("s l2 decay round_decimal", S(ranker=decay("age", "gauss", 0, 8), output_fields=["id"], limit=4, round_decimal=3))
    run("s l2 decay round_decimal sp", S(ranker=decay("age", "gauss", 0, 8), output_fields=["id"], limit=4, search_params={"metric_type": "COSINE", "round_decimal": 3}))
    run("s l2 decay group", S(ranker=decay("age", "gauss", 0, 8), group_by_field="cat", group_size=2, limit=3, output_fields=["id", "cat", "age"]))
    run("s l2 decay group hidden", S(ranker=decay("age", "gauss", 0, 8), group_by_field="cat", group_size=2, limit=3, output_fields=["id"]))
    run("s l2 decay group strict", S(ranker=decay("age", "gauss", 0, 8), group_by_field="cat", group_size=30, strict_group_size=True, limit=3, output_fields=["id", "cat"]))
    run("s l2 decay group offset", S(ranker=decay("age", "gauss", 0, 8), group_by_field="cat", group_size=2, offset=1, limit=2, output_fields=["id", "cat"]))
    run("s l2 decay group nullable", S(ranker=decay("age", "gauss", 0, 8), group_by_field="nf", group_size=1, limit=4, output_fields=["id", "nf"]))
    run("s l2 decay group bad field", S(ranker=decay("age", "gauss", 0, 8), group_by_field="nosuch", limit=3, output_fields=["id"]))
    run("s l2 decay group + boost", S(ranker=FS(boost(2.0, "g == 1"), decay("age", "gauss", 0, 8)), group_by_field="cat", group_size=2, limit=3, output_fields=["id", "cat"]))
    run("s l2 decay + boost", S(ranker=FS(boost(2.0, "g == 1"), decay("age", "gauss", 0, 8)), output_fields=OF, limit=5))
    run("s l2 decay range", S(ranker=decay("age", "gauss", 0, 8), limit=5, output_fields=["id"], search_params={"metric_type": "COSINE", "params": {"radius": 0.0, "range_filter": 1.0}}))
    run("s l2 decay nq2", S(data=Q12, ranker=decay("age", "gauss", 0, 8), output_fields=["id", "age"], limit=4))
    run("s l2 decay filter", S(ranker=decay("age", "gauss", 0, 8), filter="g < 3", output_fields=["id", "age"], limit=5))
    run("s l2 decay empty", S(ranker=decay("age", "gauss", 0, 8), filter="age < 0", output_fields=["id", "age"], limit=5))
    run("s l2 decay hidden", S(ranker=decay("age", "gauss", 0, 8), output_fields=["id"], limit=4))
    run("s l2 decay no output", S(ranker=decay("age", "gauss", 0, 8), limit=4))
    run("s l2 decay star", S(ranker=decay("age", "gauss", 0, 8), output_fields=["*"], limit=2))
    run("s l2 decay dyn star", S(ranker=decay("age", "gauss", 0, 8), output_fields=["dyn"], limit=2))
    run("s l2 decay nullable", S(ranker=decay("nf", "gauss", 0, 5), output_fields=["id", "nf"], limit=8))
    for field in ("price", "small", "g", "name", "flag", "j", "v", "nosuch", "dyn", "id"):
        run(f"s l2 decay field {field}", S(ranker=decay(field, "gauss", 0, 8), output_fields=["id"]))
    run("s l2 decay bad scale", S(ranker=decay("age", "gauss", 0, 0), output_fields=["id"]))
    run("s l2 decay bad decay", S(ranker=decay("age", "gauss", 0, 8, decay=1), output_fields=["id"]))
    run("s l2 decay bad fn", S(ranker=decay("age", "foo", 0, 8), output_fields=["id"]))
    run("s l2 rrf k bad", S(ranker=rrf_fn(0), output_fields=["id"]))
    run("s l2 wtd len2", S(ranker=wtd_fn([0.5, 0.5]), output_fields=["id"]))
    run("s l2 wtd range", S(ranker=wtd_fn([1.5]), output_fields=["id"]))
    run("s l2 two", S(ranker=FS(rrf_fn(), decay("age", "gauss", 0, 8)), output_fields=["id"]))
    run("s l2 rrf offset", S(ranker=rrf_fn(), offset=2, output_fields=["id"], limit=3))
    run("s l2 rrf group", S(ranker=rrf_fn(), group_by_field="cat", group_size=2, limit=3, output_fields=["id", "cat"]))
    run("s l2 model", S(ranker=Function("m", FunctionType.RERANK, ["name"], params={"reranker": "model", "provider": "cohere", "queries": ["a"]}), output_fields=["id"]))
    run("s l2 model bad", S(ranker=Function("m", FunctionType.RERANK, ["name"], params={"reranker": "model"}), output_fields=["id"]))
    run("s l2 normal search ref", S(output_fields=["id"], limit=5))

    # ====== 八、候选窗口（大集合：按距离名次放置标记行） ======
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("age", DataType.INT32)
    s.add_field("tag", DataType.INT64)
    s.add_field("grp", DataType.VARCHAR, max_length=8)
    ip = c.prepare_index_params(); ip.add_index("v", index_type="FLAT", metric_type="L2")
    c.create_collection("rr_win", schema=s, index_params=ip)
    NW = 400
    rows = [{"id": i, "v": [round(i * 0.01, 4), 0.0], "age": 1000, "tag": 0, "grp": "g%d" % (i // 40)} for i in range(NW)]
    c.insert("rr_win", rows); c.load_collection("rr_win")
    QW = [[0.0, 0.0]]  # id 即距离名次
    marks = [10, 49, 50, 51, 79, 80, 81, 120, 150, 151, 199, 200, 201, 299, 300]
    for i in marks:
        rows[i]["age"] = 0; rows[i]["tag"] = 1
    c.upsert("rr_win", [rows[i] for i in marks])
    c.load_collection("rr_win")

    def SW(**kw):
        kw.setdefault("data", QW); kw.setdefault("limit", 5)
        return lambda: c.search("rr_win", **kw)

    def AW(limit=10, expr=None):
        return AnnSearchRequest(data=QW, anns_field="v", param={}, limit=limit, expr=expr)

    def HW(reqs, ranker, limit=5, **kw):
        return H("rr_win", reqs, ranker, limit, **kw)

    DEC = decay("age", "gauss", 0, 5)
    for lim in (1, 3, 5, 10, 20):
        for off in (0, 3, 30):
            run(f"win s decay lim{lim} off{off}", SW(ranker=DEC, limit=lim, offset=off, output_fields=["id"]))
            run(f"win s boost lim{lim} off{off}", SW(ranker=FS(boost(0.0001, "tag == 1")), limit=lim, offset=off, output_fields=["id"]))
    for lim in (2, 5):
        for gs in (1, 2, 5):
            run(f"win s decay group lim{lim} gs{gs}", SW(ranker=DEC, limit=lim, group_by_field="grp", group_size=gs, output_fields=["id", "grp"]))
            run(f"win s boost group lim{lim} gs{gs}", SW(ranker=FS(boost(0.0001, "tag == 1")), limit=lim, group_by_field="grp", group_size=gs, output_fields=["id", "grp"]))
    for lim in (1, 3, 5):
        run(f"win h rrf routes{lim}", HW([AW(limit=lim), AW(limit=lim)], RRFRanker(), limit=5, output_fields=["id"]))
        run(f"win h rrf route limit {lim} filter tag", HW([AW(limit=lim, expr="tag == 1"), AW(limit=lim)], RRFRanker(), limit=5, output_fields=["id"]))
    for lim in (2, 5, 10):
        run(f"win h decay lim{lim}", HW([AW(limit=60)], DEC, limit=lim, output_fields=["id"]))
        run(f"win h decay routelimit5 lim{lim}", HW([AW(limit=5)], DEC, limit=lim, output_fields=["id"]))
        run(f"win h boost lim{lim}", HW([AW(limit=60)], FS(boost(0.0001, "tag == 1")), limit=lim, output_fields=["id"]))
        run(f"win h boost routelimit5 lim{lim}", HW([AW(limit=5)], FS(boost(0.0001, "tag == 1"), rrf_fn()), limit=lim, output_fields=["id"]))
    for lim in (2, 4):
        for gs in (1, 3):
            run(f"win h group rrf lim{lim} gs{gs}", HW([AW(limit=lim), AW(limit=lim)], RRFRanker(), limit=lim, group_by_field="grp", group_size=gs, output_fields=["id", "grp"]))
            run(f"win h group decay lim{lim} gs{gs}", HW([AW(limit=60)], DEC, limit=lim, group_by_field="grp", group_size=gs, output_fields=["id", "grp"]))
    run("win h rrf offset", HW([AW(limit=10), AW(limit=10)], RRFRanker(), limit=5, offset=8, output_fields=["id"]))
    run("win h rrf offset>route", HW([AW(limit=10), AW(limit=10)], RRFRanker(), limit=5, offset=15, output_fields=["id"]))
    run("win h decay offset", HW([AW(limit=60)], DEC, limit=3, offset=5, output_fields=["id"]))

    # ====== 九、TIMESTAMPTZ：Hybrid 与 Search+rerank 的 timezone / time_fields ======
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("dense", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("ts", DataType.TIMESTAMPTZ, nullable=False)
    s.add_field("age", DataType.INT32)
    c.create_collection("rr_ts", schema=s, properties={"timezone": "UTC"})
    c.insert("rr_ts", [
        {"id": 1, "dense": [1.0, 0.0], "ts": "2025-01-01T00:00:00Z", "age": 1},
        {"id": 2, "dense": [0.9, 0.0], "ts": "2024-12-31T16:00:00Z", "age": 2},
        {"id": 3, "dense": [0.5, 0.5], "ts": "2025-06-15T12:30:45Z", "age": 3},
    ])
    ip = c.prepare_index_params(); ip.add_index("dense", index_type="FLAT", metric_type="COSINE")
    c.create_index("rr_ts", ip); c.load_collection("rr_ts")
    QT = [[1.0, 0.0]]

    def AT(**kw):
        return AnnSearchRequest(data=QT, anns_field="dense", param=kw.pop("param", {}), limit=kw.pop("limit", 3), expr=kw.pop("expr", None))

    run("ts h base", lambda: c.hybrid_search("rr_ts", reqs=[AT()], ranker=RRFRanker(), limit=3, output_fields=["ts"]))
    run("ts h tz", lambda: c.hybrid_search("rr_ts", reqs=[AT()], ranker=RRFRanker(), limit=3, output_fields=["ts"], timezone="Asia/Shanghai"))
    run("ts h tz+fields", lambda: c.hybrid_search("rr_ts", reqs=[AT()], ranker=RRFRanker(), limit=3, output_fields=["ts"], timezone="Asia/Shanghai", time_fields="year, month, day, hour"))
    run("ts h filter naive tz", lambda: c.hybrid_search("rr_ts", reqs=[AT(expr="ts == ISO '2025-01-01T00:00:00'")], ranker=RRFRanker(), limit=3, output_fields=["ts"], timezone="Asia/Shanghai", time_fields="year, month, day, hour"))
    run("ts h route tz (param)", lambda: c.hybrid_search("rr_ts", reqs=[AT(param={"timezone": "America/New_York", "time_fields": "hour"})], ranker=RRFRanker(), limit=3, output_fields=["ts"]))
    run("ts h route tz priority", lambda: c.hybrid_search("rr_ts", reqs=[AT(param={"timezone": "America/New_York", "time_fields": "hour"})], ranker=RRFRanker(), limit=3, output_fields=["ts"], timezone="Asia/Shanghai", time_fields="year"))
    run("ts h two routes diff tz", lambda: c.hybrid_search("rr_ts", reqs=[AT(param={"timezone": "America/New_York"}), AT(param={"timezone": "Asia/Tokyo", "time_fields": "hour"})], ranker=RRFRanker(), limit=3, output_fields=["ts"]))
    run("ts h route2 tz only", lambda: c.hybrid_search("rr_ts", reqs=[AT(), AT(param={"timezone": "Asia/Tokyo", "time_fields": "hour"})], ranker=RRFRanker(), limit=3, output_fields=["ts"]))
    run("ts h bad tz", lambda: c.hybrid_search("rr_ts", reqs=[AT()], ranker=RRFRanker(), limit=3, output_fields=["ts"], timezone="Mars/Base"))
    run("ts h bad route tz", lambda: c.hybrid_search("rr_ts", reqs=[AT(param={"timezone": "Mars/Base"})], ranker=RRFRanker(), limit=3, output_fields=["ts"]))
    run("ts h bad time_fields", lambda: c.hybrid_search("rr_ts", reqs=[AT()], ranker=RRFRanker(), limit=3, output_fields=["ts"], time_fields="nosuch"))
    run("ts h time_fields no output", lambda: c.hybrid_search("rr_ts", reqs=[AT()], ranker=RRFRanker(), limit=3, time_fields="year"))
    run("ts h decay", lambda: c.hybrid_search("rr_ts", reqs=[AT()], ranker=decay("age", "gauss", 0, 2), limit=3, output_fields=["ts"], timezone="Asia/Shanghai", time_fields="hour"))
    run("ts h decay on ts", lambda: c.hybrid_search("rr_ts", reqs=[AT()], ranker=decay("ts", "gauss", 1735689600000000, 86400000000), limit=3, output_fields=["ts"]))
    run("ts s decay on ts", lambda: c.search("rr_ts", data=QT, limit=3, anns_field="dense", ranker=decay("ts", "gauss", 1735689600000000, 86400000000), output_fields=["ts"]))
    run("ts s decay on age + tz", lambda: c.search("rr_ts", data=QT, limit=3, anns_field="dense", ranker=decay("age", "gauss", 0, 2), output_fields=["ts"], timezone="Asia/Shanghai", time_fields="year, hour"))
    run("ts s boost filter ts", lambda: c.search("rr_ts", data=QT, limit=3, anns_field="dense", ranker=FS(boost(5.0, "ts == ISO '2025-06-15T12:30:45Z'")), output_fields=["ts"]))
    run("ts s boost filter naive tz", lambda: c.search("rr_ts", data=QT, limit=3, anns_field="dense", ranker=FS(boost(5.0, "ts == ISO '2025-06-15T20:30:45'")), output_fields=["ts"], timezone="Asia/Shanghai"))
    run("ts h boost filter naive tz", H("rr_ts", [AT()], FS(boost(5.0, "ts == ISO '2025-06-15T20:30:45'")), limit=3, output_fields=["ts"], timezone="Asia/Shanghai"))
    run("ts h boost filter naive route tz", H("rr_ts", [AT(param={"timezone": "Asia/Shanghai"})], FS(boost(5.0, "ts == ISO '2025-06-15T20:30:45'")), limit=3, output_fields=["ts"]))
    run("ts h boost filter ts utc", H("rr_ts", [AT()], FS(boost(5.0, "ts == ISO '2025-06-15T12:30:45Z'")), limit=3, output_fields=["ts"]))

    # ====== 九b、Hybrid 的 timezone / time_fields 原始位置（rank_params 与路由 search_params） ======

    def rank_kv(**pairs):
        return lambda req: req.rank_params.extend([kv(k, v) for k, v in pairs.items()])

    def route_kv(idx, **pairs):
        return lambda req: req.requests[idx].search_params.extend([kv(k, v) for k, v in pairs.items()])

    def both(*mods):
        return lambda req: [m(req) for m in mods]

    def TS2(**kw):
        return H("rr_ts", [AT(), AT()], RRFRanker(), limit=3, output_fields=["ts"], **kw)

    run("tsr rank tz only", TS2(), mod=rank_kv(timezone="Asia/Shanghai"))
    run("tsr rank tz+fields", TS2(), mod=rank_kv(timezone="Asia/Shanghai", time_fields="year,hour"))
    run("tsr rank fields only", TS2(), mod=rank_kv(time_fields="hour"))
    run("tsr rank tz bad", TS2(), mod=rank_kv(timezone="Mars/Base"))
    run("tsr rank tz empty", TS2(), mod=rank_kv(timezone="", time_fields=""))
    run("tsr rank fields bad", TS2(), mod=rank_kv(time_fields="nosuch"))
    run("tsr route1 tz+fields", TS2(), mod=route_kv(0, timezone="Asia/Shanghai", time_fields="year,hour"))
    run("tsr route2 tz+fields", TS2(), mod=route_kv(1, timezone="Asia/Shanghai", time_fields="year,hour"))
    run("tsr routes diff tz", TS2(), mod=both(route_kv(0, timezone="Asia/Shanghai", time_fields="hour"), route_kv(1, timezone="America/New_York", time_fields="minute")))
    run("tsr rank vs route", TS2(), mod=both(rank_kv(timezone="Asia/Tokyo", time_fields="year"), route_kv(0, timezone="Asia/Shanghai", time_fields="hour")))
    run("tsr route bad tz", TS2(), mod=route_kv(0, timezone="Mars/Base"))
    run("tsr route2 bad tz", TS2(), mod=route_kv(1, timezone="Mars/Base"))
    run("tsr route bad fields", TS2(), mod=route_kv(0, time_fields="nosuch"))
    run("tsr route2 bad fields", TS2(), mod=route_kv(1, time_fields="nosuch"))
    run("tsr filter naive route1 tz", H("rr_ts", [AT(expr="ts == ISO '2025-01-01T08:00:00'"), AT()], RRFRanker(), limit=3, output_fields=["ts"]), mod=route_kv(0, timezone="Asia/Shanghai"))
    run("tsr filter naive route2 tz", H("rr_ts", [AT(), AT(expr="ts == ISO '2025-01-01T08:00:00'")], RRFRanker(), limit=3, output_fields=["ts"]), mod=route_kv(1, timezone="Asia/Shanghai"))
    run("tsr filter naive rank tz", H("rr_ts", [AT(expr="ts == ISO '2025-01-01T08:00:00'"), AT()], RRFRanker(), limit=3, output_fields=["ts"]), mod=rank_kv(timezone="Asia/Shanghai"))
    run("tsr filter naive rank+route tz", H("rr_ts", [AT(expr="ts == ISO '2025-01-01T08:00:00'"), AT(expr="ts == ISO '2025-01-01T08:00:00'")], RRFRanker(), limit=3, output_fields=["ts"]), mod=both(rank_kv(timezone="Asia/Tokyo"), route_kv(0, timezone="Asia/Shanghai")))
    run("tsr boost naive rank tz", H("rr_ts", [AT(), AT()], FS(boost(5.0, "ts == ISO '2025-06-15T20:30:45'")), limit=3, output_fields=["ts"]), mod=rank_kv(timezone="Asia/Shanghai"))
    run("tsr boost naive route tz", H("rr_ts", [AT(), AT()], FS(boost(5.0, "ts == ISO '2025-06-15T20:30:45'")), limit=3, output_fields=["ts"]), mod=route_kv(0, timezone="Asia/Shanghai"))
    run("tsr decay time_fields rank", H("rr_ts", [AT()], decay("age", "gauss", 0, 2), limit=3, output_fields=["ts"]), mod=rank_kv(timezone="Asia/Shanghai", time_fields="hour"))
    run("tsr collection tz default", H("rr_ts", [AT(expr="ts == ISO '2025-01-01T00:00:00'")], RRFRanker(), limit=3, output_fields=["ts"]))

    # ====== 十、杂项 ======
    run("misc h dense metric default cosine nospec", H(reqs=[A(param={})], output_fields=["id"]))
    run("misc h search_params ef", H(reqs=[A(param={"ef": 64})], output_fields=["id"]))
    run("misc h route radius", H(reqs=[A(param={"radius": 0.0, "range_filter": 1.0})], output_fields=["id"]))
    run("misc h route range metric params", H(reqs=[A(param={"metric_type": "COSINE", "params": {"radius": 0.0, "range_filter": 1.0}})], output_fields=["id"]))
    run("misc h partition route", H(reqs=[A()], output_fields=["id"], partition_names=["_default"]))
    run("misc h tie-break equal scores", H(reqs=[A(q=Q1, limit=4), A(q=Q1, limit=4)], ranker=WeightedRanker(0.5, 0.5, norm_score=False), limit=6, output_fields=["id"]))
    run("misc h id duplicates entity route", H(reqs=[A(expr="g == 0", limit=3), A(expr="g == 0", limit=3)], output_fields=["id", "cat", "g"], limit=6))
    run("misc s l2 rrf entity", S(ranker=rrf_fn(), output_fields=["id", "cat"], limit=3))
    run("misc h consistency kw", H(output_fields=["id"], consistency_level="Strong"))
    run("misc h guarantee_timestamp", H(output_fields=["id"], guarantee_timestamp=0))

    run("s model valid + unloaded", S("rr_unloaded", ranker=mod_fn(provider="cohere", queries=["a"]), output_fields=["id"]))
    run("s random_score + unloaded", S("rr_unloaded", ranker=FS(boost(2.0, None, random_score={"seed": 1})), output_fields=["id"]))

    # ====== 复审 r2 补充：组键语义、limit<=0 分路径、子请求 L2 只解码 ======
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=True)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("age", DataType.INT32)
    s.add_field("f", DataType.FLOAT)
    s.add_field("d", DataType.DOUBLE)
    s.add_field("j", DataType.JSON)
    s.add_field("arr", DataType.ARRAY, element_type=DataType.INT64, max_capacity=4)
    s.add_field("vc", DataType.VARCHAR, max_length=8, nullable=True)
    ip = c.prepare_index_params(); ip.add_index("v", index_type="FLAT", metric_type="COSINE")
    c.create_collection("rr_gk", schema=s, index_params=ip)
    GK = [  # (dyn 值, j 值, f, d)
        (True, {"a": 1, "b": 2}, 0.0, 0.0),
        (1, {"b": 2, "a": 1}, -0.0, -0.0),
        (1.0, {"a": 1.0, "b": 2}, 1.5, 1.5),
        (2.0, '{"a": 1,  "b": 2}', 1.5, float("nan")),
        (0, {"x": 1}, float("nan"), float("nan")),
        (False, [1, 2], float("nan"), 2.5),
        ("1", {"n": 1}, 3.0, 3.0),
        (None, {"n": 1.0}, 3.0, -0.0),
    ]
    rows = []
    for i, (dv, jv, fv, dvv) in enumerate(GK):
        r = {"id": i, "v": [1.0, round(0.05 * i, 3)], "age": i, "f": fv, "d": dvv, "j": jv, "arr": [i % 2], "vc": None if i % 3 == 0 else "s%d" % (i % 2)}
        if dv is not None:
            r["dyn"] = dv
        r["dlist"] = [i] if i == 2 else i
        r["ddict"] = {"k": i} if i == 3 else i
        r["dbig"] = {0: 9007199254740993, 1: 9007199254740992.0, 2: 9007199254740992}.get(i, 7)
        r["dneg"] = {0: 0, 1: -0.0, 2: 0.0, 3: False}.get(i, 7)
        r["dstr"] = {0: "a", 1: "a", 2: "A", 3: ""}.get(i, 7)
        r["dnan"] = {0: float("nan"), 1: float("nan"), 2: float("inf"), 3: float("inf")}.get(i, 7)
        rows.append(r)
    for r in rows:
        try:
            c.insert("rr_gk", [r])
        except Exception as e:
            results[f"gk insert {r['id']}"] = {"error": failure(e), "raw": []}
            # 换成普通值，保证其余场景可以继续
            c.insert("rr_gk", [dict(r, j={"fallback": r["id"]}, d=0.5 if r["d"] != r["d"] else r["d"])])
    c.load_collection("rr_gk")
    QG = [[1.0, 0.0]]

    def SG(**kw):
        kw.setdefault("data", QG); kw.setdefault("limit", 10); kw.setdefault("group_size", 10)
        return lambda: c.search("rr_gk", **kw)

    def HG(**kw):
        kw.setdefault("limit", 10); kw.setdefault("group_size", 10)
        return H("rr_gk", [AnnSearchRequest(data=QG, anns_field="v", param={}, limit=8)], kw.pop("ranker", RRFRanker()), **kw)

    for fld in ("dyn", "j", "f", "d", "arr", "dlist", "ddict", "id", "vc", "nosuch", "v"):
        run(f"gk s l2 {fld}", SG(ranker=decay("age", "gauss", 0, 100), group_by_field=fld, output_fields=["id", fld] if fld not in ("nosuch",) else ["id"]))
        run(f"gk h {fld}", HG(group_by_field=fld, output_fields=["id", fld] if fld not in ("nosuch",) else ["id"]))
        run(f"gk h hidden {fld}", HG(group_by_field=fld, output_fields=["id"]))

    # group_size=1 时命中数即组数，用来判定组键是否相等
    for fld in ("dyn", "j", "f", "d", "vc", "id", "nosuch", "dbig", "dneg", "dstr", "dnan"):
        run(f"gk1 s l2 {fld}", SG(ranker=decay("age", "gauss", 0, 100), group_by_field=fld, group_size=1, output_fields=["id"]))
        run(f"gk1 h {fld}", HG(group_by_field=fld, group_size=1, output_fields=["id"]))
    # 不可哈希的值只有进入候选才报错
    run("gk dict row outside candidates s l2", SG(ranker=decay("age", "gauss", 0, 100), group_by_field="ddict", filter="id != 3", output_fields=["id"]))
    run("gk dict row outside candidates h", H("rr_gk", [AnnSearchRequest(data=QG, anns_field="v", param={}, limit=8, expr="id != 3")], RRFRanker(), limit=10, group_by_field="ddict", group_size=10, output_fields=["id"]))
    run("gk unhashable + bad L2 s", SG(ranker=decay("age", "foo", 0, 100), group_by_field="ddict", output_fields=["id"]))
    run("gk unhashable no candidates", SG(ranker=decay("age", "gauss", 0, 100), group_by_field="ddict", filter="id < 0", output_fields=["id"]))
    run("gk dyn group pk string output None", SG(ranker=decay("age", "gauss", 0, 100), group_by_field="dyn", group_size=1, output_fields=None))
    run("gk dyn group output star", SG(ranker=decay("age", "gauss", 0, 100), group_by_field="dyn", group_size=1, output_fields=["*"]))
    # 解码阶段的先后
    run("fs mode bad + bad fn", S(ranker=FS(boost("x"), boost_mode="bad"), output_fields=["id"]))
    run("fs k abc + bad anns h", H(reqs=[A(field="nosuch")], ranker=rrf_fn("abc"), output_fields=["id"]))
    run("fs weight elem bad", H(ranker=wtd_fn([0.5, "x"]), output_fields=["id"]))
    run("fs provider only", S(ranker=Function("x", FunctionType.RERANK, ["name"], params={"reranker": "foo", "provider": "cohere", "queries": ["a"]}), output_fields=["id"]))
    run("fs provider only no queries", S(ranker=Function("x", FunctionType.RERANK, ["name"], params={"reranker": "foo", "provider": "cohere"}), output_fields=["id"]))
    run("fs model 2 inputs", S(ranker=Function("m", FunctionType.RERANK, ["name", "cat"], params={"reranker": "model", "provider": "cohere", "queries": ["a"]}), output_fields=["id"]))
    run("fs model no field", S(ranker=Function("m", FunctionType.RERANK, ["nosuch"], params={"reranker": "model", "provider": "cohere", "queries": ["a"]}), output_fields=["id"]))
    run("fs model api_key param", S(ranker=Function("m", FunctionType.RERANK, ["name"], params={"reranker": "model", "provider": "nosuch", "queries": ["a"], "api_key": "x"}), output_fields=["id"]))
    run("fs random_score no hits", S(ranker=FS(boost(2.0, None, random_score={"seed": 1})), filter="id < 0", output_fields=["id"]))

    # limit<=0 分路径（原始请求改写，绕过 SDK 的客户端校验）
    def set_topk(val):
        return lambda r: [setattr(k_, "value", val) for k_ in r.search_params if k_.key == "topk"]

    def set_offset(val):
        def f(r):
            found = False
            for k_ in r.search_params:
                if k_.key == "offset":
                    k_.value = val; found = True
            if not found:
                r.search_params.append(kv("offset", val))
        return f

    for lim in ("0", "-1", "-3"):
        for off in ("0", "5"):
            run(f"lim s l2 limit {lim} offset {off}", S(ranker=decay("age", "gauss", 0, 8), output_fields=["id"]), mod=both(set_topk(lim), set_offset(off)))
            run(f"lim s l2 group limit {lim} offset {off}", S(ranker=decay("age", "gauss", 0, 8), group_by_field="cat", group_size=2, output_fields=["id", "cat"]), mod=both(set_topk(lim), set_offset(off)))
            run(f"lim s boost limit {lim} offset {off}", S(ranker=FS(boost(2.0, "g == 1")), output_fields=["id"]), mod=both(set_topk(lim), set_offset(off)))
            run(f"lim s boost group limit {lim} offset {off}", S(ranker=FS(boost(2.0, "g == 1")), group_by_field="cat", output_fields=["id", "cat"]), mod=both(set_topk(lim), set_offset(off)))
        for off in ("0", "1", "-1"):
            rl = lambda r, lim=lim, off=off: [setattr(k_, "value", lim if k_.key == "limit" else off) for k_ in r.rank_params if k_.key in ("limit", "offset")]
            run(f"lim h limit {lim} offset {off}", H(output_fields=["id"]), mod=rl)
            run(f"lim h group limit {lim} offset {off}", H(output_fields=["id", "cat"], group_by_field="cat", group_size=2), mod=rl)
    for rl_ in ("0", "-1", "1"):
        for strat in ("rrf", "boost", "group"):
            kw = {}
            rk = RRFRanker()
            if strat == "boost":
                rk = FS(boost(2.0, "g == 1"))
            if strat == "group":
                kw = dict(group_by_field="cat", group_size=2)
            mods = lambda r, rl_=rl_: [setattr(k_, "value", rl_) for k_ in r.requests[0].search_params if k_.key == "topk"]
            run(f"lim h route0 limit {rl_} {strat}", H(reqs=[A(), A(q=Q2)], ranker=rk, output_fields=["id", "cat"], limit=5, **kw), mod=mods)
            run(f"lim h single route limit {rl_} {strat}", H(reqs=[A()], ranker=rk, output_fields=["id", "cat"], limit=5, **kw), mod=mods)

    # rank_params 的异常类型清单
    def RK(strategy, params):
        return RawRank({"strategy": strategy, "params": params})

    for k_ in (True, False, [1], {"a": 1}, -5, -2, -1.5, -0.5):
        run(f"rp rrf k={k_!r}", H(ranker=RK("rrf", {"k": k_}), output_fields=["id"]))
        run(f"rp rrf k={k_!r} short", H(reqs=[A(limit=2)], ranker=RK("rrf", {"k": k_}), output_fields=["id"]))
    for w_ in ("ab", 5, None, ["x", 1], [None, 1], [True, 1], [1, 2.5], "[0.5,0.5]", {"a": 1, "b": 2}):
        run(f"rp wtd weights={w_!r}", H(ranker=RK("weighted", {"weights": w_}), output_fields=["id"]))
    for n_ in (0, 1, 0.0, "", "x", [], [0], {}, None, True, False):
        run(f"rp wtd norm={n_!r}", H(ranker=RK("weighted", {"weights": [0.5, 0.5], "norm_score": n_}), output_fields=["id"]))
    for pv in ('"x"', "null", "5", "[]"):
        def mp(r, pv=pv):
            for k_ in r.rank_params:
                if k_.key == "params":
                    k_.value = pv
        run(f"rp params raw {pv}", H(ranker=RK("rrf", {}), output_fields=["id"]), mod=mp)
    for sv in ("5", "null", "[]", '""', '"rrf"', "true"):
        def ms(r, sv=sv):
            for k_ in r.rank_params:
                if k_.key == "strategy":
                    k_.value = sv
        run(f"rp strategy raw {sv}", H(ranker=RK("rrf", {}), output_fields=["id"]), mod=ms)
    for key in ("limit", "offset"):
        for vv in ("null", "2.5", "true", "[]", '"3"', '" 3 "', '"3.5"', "1e2"):
            def ml(r, key=key, vv=vv):
                for k_ in r.rank_params:
                    if k_.key == key:
                        k_.value = vv
            run(f"rp {key} raw {vv}", H(output_fields=["id"]), mod=ml)
    for vv in ("null", "2.5", "true", "[]", '"3"', '"x"'):
        def mg(r, vv=vv):
            for k_ in r.rank_params:
                if k_.key == "group_size":
                    k_.value = vv
        run(f"rp group_size raw {vv}", H(output_fields=["id", "cat"], group_by_field="cat", group_size=2, limit=2), mod=mg)
    for vv in ("5", "null", '""', "true", "[]"):
        def mf(r, vv=vv):
            for k_ in r.rank_params:
                if k_.key == "group_by_field":
                    k_.value = vv
        run(f"rp group_by_field raw {vv}", H(output_fields=["id", "cat"], group_by_field="cat", limit=2), mod=mf)
    run("rp raw non-json limit", H(output_fields=["id"]), mod=lambda r: [setattr(k_, "value", "abc") for k_ in r.rank_params if k_.key == "limit"])
    run("rp raw non-json strategy", H(output_fields=["id"]), mod=lambda r: [setattr(k_, "value", "rrf") for k_ in r.rank_params if k_.key == "strategy"])
    run("rp raw non-json strategy foo", H(output_fields=["id"]), mod=lambda r: [setattr(k_, "value", "foo") for k_ in r.rank_params if k_.key == "strategy"])
    # decay 参数的 JSON 类型
    for key, vals in (("origin", ("true", "null", "[1]", '"5"')), ("scale", ("true", "null", "[1]", '"5"', "1e400")), ("offset", ("true", "null", "[1]", '"5"')), ("decay", ("true", "null", "[1]", '"0.5"', "0.5")), ("function", ("5", "null", "true"))):
        for vv in vals:
            def md(r, key=key, vv=vv):
                for k_ in r.function_score.functions[0].params:
                    if k_.key == key:
                        k_.value = vv
                        return
                r.function_score.functions[0].params.append(kv(key, vv))
            run(f"decay param {key}={vv}", H(ranker=decay("age", "gauss", 0, 8), output_fields=["id"]), mod=md)
    for vv in ("true", "false", "0", "1", '""', '"x"', "null", "[]", "False", "True", "false "):
        def mn(r, vv=vv):
            r.function_score.functions[0].params.append(kv("norm_score", vv))
        run(f"decay norm raw {vv}", H(ranker=decay("age", "gauss", 0, 8), output_fields=["id"]), mod=mn)
    for vv in ("5", "null", '"MAX"', '"Sum"'):
        def mm(r, vv=vv):
            r.function_score.functions[0].params.append(kv("score_mode", vv))
        run(f"decay score_mode raw {vv}", H(ranker=decay("age", "gauss", 0, 8), output_fields=["id"]), mod=mm)

    # 类型错误只在真正用到时才报：所有路由都没有候选
    EMPTY = [A(expr="g == 99"), A(q=Q2, expr="g == 98")]
    run("empty rp rrf k=str", H(reqs=EMPTY, ranker=RK("rrf", {"k": "60"}), output_fields=["id"]))
    run("empty rp wtd elem str", H(reqs=EMPTY, ranker=RK("weighted", {"weights": ["x", 1]}), output_fields=["id"]))
    run("empty rp wtd weights=5", H(reqs=EMPTY, ranker=RK("weighted", {"weights": 5}), output_fields=["id"]))
    def _mm_max(r):
        r.function_score.functions[0].params.append(kv("score_mode", '"MAX"'))
    run("empty decay score_mode MAX", H(reqs=EMPTY, ranker=decay("age", "gauss", 0, 8), output_fields=["id"]), mod=_mm_max)

    # 子请求 function_score：只解码
    def route_fs(fs_obj):
        return lambda r: r.requests[0].function_score.CopyFrom(Prepare.function_score_schema(fs_obj))

    for name, fn in (("rrf k=0", rrf_fn(0)), ("rrf k=abc", rrf_fn("abc")), ("wtd []", wtd_fn([])), ("wtd [5]", wtd_fn([5])),
                     ("wtd norm bad", wtd_fn([0.5], "maybe")), ("wtd weights junk", wtd_fn("abc")),
                     ("decay bad fn", decay("age", "foo", 0, 8)), ("decay bad field", decay("name", "gauss", 0, 8)),
                     ("decay no input", Function("d", FunctionType.RERANK, [], params={"reranker": "decay"})),
                     ("model no queries", Function("m", FunctionType.RERANK, ["name"], params={"reranker": "model"})),
                     ("model valid", Function("m", FunctionType.RERANK, ["name"], params={"reranker": "model", "provider": "cohere", "queries": ["a"]})),
                     ("unknown", Function("x", FunctionType.RERANK, [], params={"reranker": "foo"})),
                     ("two L2", FS(rrf_fn(), rrf_fn(name="r2"))),
                     ("boost bad weight", FS(boost("x"))), ("boost random", FS(boost(2.0, None, random_score={"seed": 1}))),
                     ("boost bad filter", FS(boost(2.0, "age >>")))):
        fs_obj = fn if isinstance(fn, FunctionScore) else FS(fn)
        run(f"route fs {name}", H(output_fields=["id"]), mod=route_fs(fs_obj))

    # ====== 复审 r3 补充：字段类型全矩阵（组键与组值）、主键形态 ======
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=True)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("ts", DataType.TIMESTAMPTZ, nullable=True)
    s.add_field("ts2", DataType.TIMESTAMPTZ)
    s.add_field("age", DataType.INT32)
    s.add_field("i8", DataType.INT8, nullable=True)
    s.add_field("i16", DataType.INT16, nullable=True)
    s.add_field("i32", DataType.INT32, nullable=True)
    s.add_field("i64", DataType.INT64, nullable=True)
    s.add_field("b", DataType.BOOL, nullable=True)
    s.add_field("f", DataType.FLOAT, nullable=True)
    s.add_field("d", DataType.DOUBLE, nullable=True)
    s.add_field("vc", DataType.VARCHAR, max_length=8, nullable=True)
    s.add_field("j", DataType.JSON, nullable=True)
    s.add_field("arr", DataType.ARRAY, element_type=DataType.INT64, max_capacity=4, nullable=True)
    ip = c.prepare_index_params(); ip.add_index("v", index_type="FLAT", metric_type="COSINE")
    TSV = ["2025-01-01T00:00:00Z", "2025-01-01T08:00:00+08:00", "2025-01-02T00:00:00Z", "2025-01-02T00:00:00Z", None, None, "1969-12-31T23:59:59.5Z", "2025-01-01T00:00:00.000001Z"]
    rows = []
    for i, tv in enumerate(TSV):
        nul = i in (4, 5)
        rows.append({"id": i, "v": [1.0, round(0.05 * i, 3)], "ts": tv, "ts2": "2025-03-0%dT00:00:00Z" % (1 + i % 2), "age": i,
                     "i8": None if nul else i % 3, "i16": None if nul else i % 3, "i32": None if nul else i % 3, "i64": None if nul else i % 3,
                     "b": None if nul else bool(i % 2), "f": None if nul else float(i % 3), "d": None if nul else float(i % 3),
                     "vc": None if nul else "s%d" % (i % 3), "j": None if nul else {"k": i % 3}, "arr": None if nul else [i % 3]})
    for cname, props in (("rr_gt", None), ("rr_gt_sh", {"timezone": "Asia/Shanghai"})):
        c.create_collection(cname, schema=s, index_params=ip, **({"properties": props} if props else {}))
        c.insert(cname, rows); c.load_collection(cname)

    def SGT(coll="rr_gt", **kw):
        kw.setdefault("data", QG); kw.setdefault("limit", 10); kw.setdefault("group_size", 10)
        kw.setdefault("ranker", decay("age", "gauss", 0, 100))
        return lambda: c.search(coll, **kw)

    def HGT(coll="rr_gt", **kw):
        kw.setdefault("limit", 10); kw.setdefault("group_size", 10)
        return H(coll, [AnnSearchRequest(data=QG, anns_field="v", param={}, limit=8)], kw.pop("ranker", RRFRanker()), **kw)

    for fld in ("ts", "ts2", "i8", "i16", "i32", "i64", "b", "f", "d", "vc", "j", "arr", "age", "v", "id", "dyn", "nosuch"):
        for gs in (10, 1):
            run(f"gt s l2 {fld} gs{gs}", SGT(group_by_field=fld, group_size=gs, output_fields=["id"]))
            run(f"gt h {fld} gs{gs}", HGT(group_by_field=fld, group_size=gs, output_fields=["id"]))
        run(f"gt s boost {fld}", SGT(ranker=FS(boost(2.0, "age > 1")), group_by_field=fld, group_size=2, output_fields=["id"]))
        run(f"gt s plain {fld}", SGT(ranker=None, group_by_field=fld, group_size=2, output_fields=["id"]))
    for fld in ("ts", "ts2"):
        for name, kw in (("tz", dict(timezone="Asia/Shanghai")), ("fields", dict(time_fields="year,hour")), ("tz+fields", dict(timezone="Asia/Shanghai", time_fields="year,hour"))):
            run(f"gt s l2 {fld} {name}", SGT(group_by_field=fld, group_size=2, output_fields=["id", "ts"], **kw))
            run(f"gt h {fld} {name}", HGT(group_by_field=fld, group_size=2, output_fields=["id", "ts"], **kw))
        run(f"gt s l2 {fld} coll tz", SGT("rr_gt_sh", group_by_field=fld, group_size=2, output_fields=["id"]))
        run(f"gt h {fld} coll tz", HGT("rr_gt_sh", group_by_field=fld, group_size=2, output_fields=["id"]))
        run(f"gt h {fld} coll tz fields", HGT("rr_gt_sh", group_by_field=fld, group_size=2, output_fields=["id", "ts"], time_fields="hour"))
        run(f"gt s plain {fld} tz", SGT(ranker=None, group_by_field=fld, group_size=2, output_fields=["id"], timezone="Asia/Shanghai"))

    # 主键形态：VARCHAR 主键、主键不叫 id、普通字段叫 id / distance
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("pk", DataType.VARCHAR, is_primary=True, max_length=16)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("id", DataType.INT64, nullable=True)
    s.add_field("distance", DataType.INT64, nullable=True)
    s.add_field("score", DataType.DOUBLE, nullable=True)
    s.add_field("age", DataType.INT32)
    c.create_collection("rr_pk", schema=s, index_params=ip)
    c.insert("rr_pk", [{"pk": "k%d" % i, "v": [1.0, round(0.05 * i, 3)], "id": None if i % 3 == 0 else i % 2, "distance": None if i % 4 == 0 else i % 2, "score": float(i), "age": i} for i in range(12)])
    c.load_collection("rr_pk")
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("pk", DataType.INT64, is_primary=True)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("age", DataType.INT32)
    c.create_collection("rr_pki", schema=s, index_params=ip)
    c.insert("rr_pki", [{"pk": i, "v": [1.0, round(0.05 * i, 3)], "age": i} for i in range(12)])
    c.load_collection("rr_pki")
    QP = [[1.0, 0.0]]

    def SP(coll="rr_pk", **kw):
        kw.setdefault("data", QP); kw.setdefault("limit", 4)
        return lambda: c.search(coll, **kw)

    def HP(coll="rr_pk", **kw):
        return H(coll, [AnnSearchRequest(data=QP, anns_field="v", param={}, limit=8), AnnSearchRequest(data=QP, anns_field="v", param={}, limit=5)], kw.pop("ranker", RRFRanker()), limit=kw.pop("limit", 4), **kw)

    for coll in ("rr_pk", "rr_pki"):
        run(f"pk {coll} h rrf", HP(coll, output_fields=["age"]))
        run(f"pk {coll} h wtd", HP(coll, ranker=WeightedRanker(0.5, 0.5), output_fields=["age"]))
        run(f"pk {coll} h star", HP(coll, output_fields=["*"]))
        run(f"pk {coll} h none", HP(coll))
        run(f"pk {coll} h output pk", HP(coll, output_fields=["pk"]))
        run(f"pk {coll} s l2", SP(coll, ranker=decay("age", "gauss", 0, 100), output_fields=["age"]))
        run(f"pk {coll} s l2 output pk", SP(coll, ranker=decay("age", "gauss", 0, 100), output_fields=["pk"]))
        run(f"pk {coll} s l2 decay on pk", SP(coll, ranker=decay("pk", "gauss", 0, 3), output_fields=["age"]))
        run(f"pk {coll} s boost pk filter", SP(coll, ranker=FS(boost(3.0, "pk in ['k5','k6']" if coll == "rr_pk" else "pk in [5,6]")), output_fields=["age"]))
        run(f"pk {coll} s boost pk filter hidden", SP(coll, ranker=FS(boost(3.0, "pk like 'k1%'" if coll == "rr_pk" else "pk > 8")), output_fields=["pk"]))
        run(f"pk {coll} s l2 group pk", SP(coll, ranker=decay("age", "gauss", 0, 100), group_by_field="pk", group_size=2, output_fields=["age"]))
        run(f"pk {coll} h group pk", HP(coll, group_by_field="pk", group_size=2, output_fields=["age"]))
        run(f"pk {coll} h group pk output", HP(coll, group_by_field="pk", group_size=2, output_fields=["pk"]))
        run(f"pk {coll} s boost group pk", SP(coll, ranker=FS(boost(3.0, "age > 3")), group_by_field="pk", group_size=2, output_fields=["age"]))
        run(f"pk {coll} s plain group pk", SP(coll, group_by_field="pk", group_size=2, output_fields=["age"]))
    for fld in ("id", "distance", "score"):
        for gs in (5, 1):
            run(f"pk field {fld} s l2 gs{gs}", SP(ranker=decay("age", "gauss", 0, 100), group_by_field=fld, group_size=gs, output_fields=["age"], limit=6))
            run(f"pk field {fld} h gs{gs}", HP(group_by_field=fld, group_size=gs, output_fields=["age"], limit=6))
        run(f"pk field {fld} output", HP(output_fields=[fld], limit=4))
        run(f"pk field {fld} s l2 output", SP(ranker=decay("age", "gauss", 0, 100), output_fields=[fld]))
        run(f"pk field {fld} boost filter", SP(ranker=FS(boost(3.0, f"{fld} > 0")), output_fields=[fld]))
        run(f"pk field {fld} decay input", SP(ranker=decay(fld, "gauss", 0, 3), output_fields=["age"]))

    # ====== 复审 r3 补充：平局规则、数值边界、引擎分组的 0024 路径 ======
    for fld in ("id", "distance", "score"):
        for gs in (5, 1):
            run(f"pk field {fld} s boost gs{gs}", SP(ranker=FS(boost(2.0, "age > 3")), group_by_field=fld, group_size=gs, output_fields=["age"], limit=6))
            run(f"pk field {fld} s plain gs{gs}", SP(group_by_field=fld, group_size=gs, output_fields=["age"], limit=6))
    # 平局：全 0 权重、相同向量；VARCHAR 主键按字符串升序
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("pk", DataType.VARCHAR, is_primary=True, max_length=16)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("g", DataType.INT64)
    s.add_field("age", DataType.INT32)
    c.create_collection("rr_tie", schema=s, index_params=ip)
    TIE = ["k9", "k10", "k2", "Z", "a", "\u00e9", "\u4e2d", "k1"]
    c.insert("rr_tie", [{"pk": p_, "v": [1.0, 0.0], "g": i % 2, "age": 5} for i, p_ in enumerate(TIE)])
    c.load_collection("rr_tie")
    QT2 = [[1.0, 0.0]]
    def ATI(limit=8, q=QT2):
        return AnnSearchRequest(data=q, anns_field="v", param={}, limit=limit)
    run("tie h rrf str pk", H("rr_tie", [ATI()], RRFRanker(), limit=8, output_fields=["g"]))
    run("tie h rrf two routes", H("rr_tie", [ATI(), ATI()], RRFRanker(), limit=8, output_fields=["g"]))
    run("tie h wtd zero", H("rr_tie", [ATI()], WeightedRanker(0), limit=8, output_fields=["g"]))
    run("tie h wtd", H("rr_tie", [ATI()], WeightedRanker(1), limit=8, output_fields=["g"]))
    run("tie h group", H("rr_tie", [ATI()], WeightedRanker(1), limit=2, group_by_field="g", group_size=2, output_fields=["g"]))
    run("tie h group order", H("rr_tie", [ATI()], WeightedRanker(1), limit=8, group_by_field="g", group_size=8, output_fields=["g"]))
    run("tie s decay", lambda: c.search("rr_tie", data=QT2, limit=8, ranker=decay("age", "gauss", 0, 8), output_fields=["g"]))
    run("tie s decay group", lambda: c.search("rr_tie", data=QT2, limit=8, ranker=decay("age", "gauss", 0, 8), group_by_field="g", group_size=8, output_fields=["g"]))
    run("tie s boost", lambda: c.search("rr_tie", data=QT2, limit=8, ranker=FS(boost(2.0)), output_fields=["g"]))
    run("tie s boost some", lambda: c.search("rr_tie", data=QT2, limit=8, ranker=FS(boost(2.0, "g == 1")), output_fields=["g"]))
    run("tie s boost group", lambda: c.search("rr_tie", data=QT2, limit=8, ranker=FS(boost(2.0)), group_by_field="g", group_size=8, output_fields=["g"]))
    run("tie s plain", lambda: c.search("rr_tie", data=QT2, limit=8, output_fields=["g"]))
    run("tie s plain group", lambda: c.search("rr_tie", data=QT2, limit=8, group_by_field="g", group_size=8, output_fields=["g"]))
    # 数值边界
    run("num boost inf + decay", S(ranker=FS(boost("inf", "g == 1"), decay("age", "gauss", 0, 8)), output_fields=["id"]))
    run("num boost nan + decay", S(ranker=FS(boost("nan", "g == 1"), decay("age", "gauss", 0, 8)), output_fields=["id"]))
    run("num boost -inf + decay", S(ranker=FS(boost("-inf", "g == 1"), decay("age", "gauss", 0, 8)), output_fields=["id"]))
    run("num boost inf + rrf", S(ranker=FS(boost("inf", "g == 1"), rrf_fn()), output_fields=["id"]))
    run("num boost nan + rrf", S(ranker=FS(boost("nan", "g == 1"), rrf_fn()), output_fields=["id"]))
    run("num boost nan + wtd", S(ranker=FS(boost("nan", "g == 1"), wtd_fn([1.0])), output_fields=["id"]))
    run("num boost inf + wtd norm", S(ranker=FS(boost("inf", "g == 1"), wtd_fn([1.0], True)), output_fields=["id"]))
    run("num boost inf l2", S("rr_l2", ranker=FS(boost("inf", "g == 1")), output_fields=["id"]))
    run("num boost nan l2", S("rr_l2", ranker=FS(boost("nan", "g == 1")), output_fields=["id"]))
    run("num boost inf ip", S("rr_ip", ranker=FS(boost("inf", "g == 1")), output_fields=["id"]))
    run("num boost nan group", S(ranker=FS(boost("nan", "g == 1")), group_by_field="cat", group_size=2, output_fields=["id", "cat"]))
    run("num boost nan rrf group", S(ranker=FS(boost("nan", "g == 1"), rrf_fn()), group_by_field="cat", group_size=2, output_fields=["id", "cat"]))
    run("num boost nan wtd group", S(ranker=FS(boost("nan", "g == 1"), wtd_fn([1.0])), group_by_field="cat", group_size=2, output_fields=["id", "cat"]))
    run("num decay scale tiny", S(ranker=decay("age", "gauss", 0, 1e-200), output_fields=["id"]))
    run("num decay scale tiny exp", S(ranker=decay("age", "exp", 0, 1e-200), output_fields=["id"]))
    run("num decay scale tiny linear", S(ranker=decay("age", "linear", 0, 1e-200), output_fields=["id"]))
    run("num decay scale huge", S(ranker=decay("age", "gauss", 0, 1e200), output_fields=["id"]))
    run("num decay origin huge", S(ranker=decay("age", "gauss", 1e308, 1.0), output_fields=["id"]))
    run("num decay decay tiny", S(ranker=decay("age", "gauss", 0, 8, decay=1e-300), output_fields=["id"]))
    run("num decay on float nan", lambda: c.search("rr_gk", data=QG, limit=8, ranker=decay("f", "gauss", 0, 2), output_fields=["id", "f"]))
    run("num decay on double nan", lambda: c.search("rr_gk", data=QG, limit=8, ranker=decay("d", "linear", 0, 2), output_fields=["id", "d"]))
    run("num rrf k inf", H(ranker=RK("rrf", {"k": 1e400}), output_fields=["id"]))
    run("num wtd weight inf", H(ranker=RK("weighted", {"weights": [1e400, 1]}), output_fields=["id"]))
    run("num wtd weight huge overflow", H("rr_ip", [A("rr_ip"), A("rr_ip", q=Q2)], RK("weighted", {"weights": [1e308, 1e308], "norm_score": False}), output_fields=["id"]))
    run("num fs rrf k inf", H(ranker=rrf_fn(1e400), output_fields=["id"]))
    run("num fs wtd nan", H(ranker=wtd_fn(["nan", 0.5]), output_fields=["id"]))
    run("num round nan", S(ranker=FS(boost("nan", "g == 1"), rrf_fn()), output_fields=["id"], round_decimal=2))
    run("num round inf", S(ranker=FS(boost("inf", "g == 1")), output_fields=["id"], round_decimal=2))

    # 可空字段这一页全有值：valid_data 应为空
    for fld in ("vc", "ts", "i64", "j", "b", "f"):
        run(f"gt s l2 {fld} no-null page", SGT(group_by_field=fld, group_size=2, filter="age not in [4, 5]", output_fields=["id"]))
        run(f"gt h {fld} no-null page", H("rr_gt", [AnnSearchRequest(data=QG, anns_field="v", param={}, limit=8, expr="age not in [4, 5]")], RRFRanker(), limit=10, group_by_field=fld, group_size=2, output_fields=["id"]))

    # ====== 复审 r4：链式候选行的虚拟列、同名冲突、ARRAY、引擎路径的负参数 ======
    ROUTES = lambda coll, f="v": [AnnSearchRequest(data=QG, anns_field=f, param={}, limit=8)]
    CHAINS = (("decay", lambda: decay("age", "gauss", 0, 100)), ("rrf", lambda: rrf_fn()), ("wtd", lambda: wtd_fn([1.0])))
    for vname in ("$id", "$score", "_decay_score", "$group_score", "$distance", "$meta"):
        for gs in (10, 1):
            for cname, mk in CHAINS:
                run(f"vc s {cname} {vname} gs{gs}", SGT(ranker=mk(), group_by_field=vname, group_size=gs, output_fields=["id"]))
                run(f"vc h fs {cname} {vname} gs{gs}", H("rr_gt", ROUTES("rr_gt"), mk(), limit=10, group_by_field=vname, group_size=gs, output_fields=["id"]))
            run(f"vc h rank rrf {vname} gs{gs}", H("rr_gt", ROUTES("rr_gt"), RRFRanker(), limit=10, group_by_field=vname, group_size=gs, output_fields=["id"]))
            run(f"vc h rank wtd {vname} gs{gs}", H("rr_gt", ROUTES("rr_gt"), WeightedRanker(1.0), limit=10, group_by_field=vname, group_size=gs, output_fields=["id"]))
        run(f"vc s boost {vname}", SGT(ranker=FS(boost(2.0, "age > 1")), group_by_field=vname, group_size=2, output_fields=["id"]))
        run(f"vc s plain {vname}", SGT(ranker=None, group_by_field=vname, group_size=2, output_fields=["id"]))
        run(f"vc s star {vname}", SGT(group_by_field=vname, group_size=1, output_fields=["*"]))
        run(f"vc h star {vname}", H("rr_gt", ROUTES("rr_gt"), RRFRanker(), limit=10, group_by_field=vname, group_size=1, output_fields=["*"]))

    # ARRAY：全空、混合、窗口外
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("age", DataType.INT32)
    s.add_field("arr", DataType.ARRAY, element_type=DataType.INT64, max_capacity=4, nullable=True)
    s.add_field("arrs", DataType.ARRAY, element_type=DataType.VARCHAR, max_capacity=4, max_length=8, nullable=True)
    c.create_collection("rr_arr", schema=s, index_params=ip)
    c.insert("rr_arr", [{"id": i, "v": [1.0, round(0.01 * i, 3)], "age": 1, "arr": None if i < 12 else [i], "arrs": None if i < 12 else ["x"]} for i in range(30)])
    c.load_collection("rr_arr")
    def SA(**kw):
        kw.setdefault("data", QG); kw.setdefault("group_size", 2); kw.setdefault("ranker", decay("age", "gauss", 0, 100))
        return lambda: c.search("rr_arr", **kw)
    def HA(route_limit, **kw):
        return H("rr_arr", [AnnSearchRequest(data=QG, anns_field="v", param={}, limit=route_limit, expr=kw.pop("expr", None))], RRFRanker(), limit=kw.pop("limit", 3), **kw)
    for fld in ("arr", "arrs"):
        run(f"arr s window null {fld}", SA(group_by_field=fld, limit=1, output_fields=["id"]))
        run(f"arr s window mixed {fld}", SA(group_by_field=fld, limit=2, output_fields=["id"]))
        run(f"arr s all null filter {fld}", SA(group_by_field=fld, limit=5, filter="id < 12", output_fields=["id"]))
        run(f"arr s non-null only {fld}", SA(group_by_field=fld, limit=5, filter="id >= 12", output_fields=["id"]))
        run(f"arr s no candidates {fld}", SA(group_by_field=fld, limit=5, filter="id < 0", output_fields=["id"]))
        run(f"arr h window null {fld}", HA(5, group_by_field=fld, group_size=2, limit=1, output_fields=["id"]))
        run(f"arr h window mixed {fld}", HA(20, group_by_field=fld, group_size=2, limit=3, output_fields=["id"]))
        run(f"arr h all null filter {fld}", HA(30, expr="id < 12", group_by_field=fld, group_size=2, output_fields=["id"]))
        run(f"arr h non-null only {fld}", HA(30, expr="id >= 12", group_by_field=fld, group_size=2, output_fields=["id"]))
        run(f"arr s boost {fld}", SA(ranker=FS(boost(2.0)), group_by_field=fld, limit=2, output_fields=["id"]))

    # 引擎路径（Search + boost、普通 Search）的负参数
    for label, rk in (("boost", lambda: FS(boost(2.0, "g == 1"))), ("plain", lambda: None)):
        for gs in (0, -1, -2, 1, 2):
            run(f"eng {label} group_size {gs}", S(ranker=rk(), group_by_field="cat", group_size=gs, limit=3, output_fields=["id", "cat"]))
        for off in (-1, -3, -100, 1, 2):
            run(f"eng {label} group offset {off}", S(ranker=rk(), group_by_field="cat", group_size=2, limit=3, offset=off, output_fields=["id", "cat"]))
            run(f"eng {label} offset {off}", S(ranker=rk(), limit=4, offset=off, output_fields=["id"]))
        for lim in ("0", "-1"):
            run(f"eng {label} limit raw {lim}", S(ranker=rk(), output_fields=["id"]), mod=set_topk(lim))
            run(f"eng {label} group limit raw {lim}", S(ranker=rk(), group_by_field="cat", output_fields=["id", "cat"]), mod=set_topk(lim))

    # Search 的非整数参数（沿用现有 search_params 解析，只验证）
    def sp_set(key, val):
        def f(r):
            for k_ in r.search_params:
                if k_.key == key:
                    k_.value = val
                    return
            r.search_params.append(kv(key, val))
        return f
    for val in ("2.5", '"2"', "true", '"abc"', "[]", "null"):
        for label, kwx in (("decay", dict(ranker=decay("age", "gauss", 0, 8))), ("boost", dict(ranker=FS(boost(2.0, "g == 1"))))):
            run(f"sp {label} group_size {val}", S(group_by_field="cat", group_size=2, limit=3, output_fields=["id", "cat"], **kwx), mod=sp_set("group_size", val))
            run(f"sp {label} offset {val}", S(limit=3, output_fields=["id"], **kwx), mod=sp_set("offset", val))
            run(f"sp {label} round_decimal {val}", S(limit=3, output_fields=["id"], **kwx), mod=sp_set("round_decimal", val))

    # 同名冲突：动态键
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=True)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("age", DataType.INT32)
    s.add_field("g", DataType.INT64)
    c.create_collection("rr_cf", schema=s, index_params=ip)
    try:
        c.insert("rr_cf", [{"id": i, "v": [1.0, round(0.05 * i, 3)], "age": i, "g": i % 2, "$id": 1000 + i % 3, "$score": 0.5 * i, "_decay_score": 7.0, "$group_score": 9.0, "$distance": 3.0} for i in range(10)])
        results["cf dynamic insert"] = {"hits": [], "raw": [{"status": [0, ""]}]}
    except Exception as e:
        results["cf dynamic insert"] = {"error": failure(e), "raw": [{"status": [1, ""]}]}
        c.insert("rr_cf", [{"id": i, "v": [1.0, round(0.05 * i, 3)], "age": i, "g": i % 2, "_decay_score": 7.0} for i in range(10)])
    c.load_collection("rr_cf")
    CF = ("$id", "$score", "_decay_score", "$group_score", "$distance")
    def CFP(ranker=None, **kw):
        return lambda: c.search("rr_cf", data=QG, limit=kw.pop("limit", 5), ranker=ranker, **kw)
    def CFH(ranker=None, **kw):
        return H("rr_cf", [AnnSearchRequest(data=QG, anns_field="v", param={}, limit=8)], ranker or RRFRanker(), limit=kw.pop("limit", 5), **kw)
    for of in (["id"], ["*"], None, [], ["$id"], ["$score"], ["_decay_score"], ["$group_score"], ["$distance"]):
        for label, mk in (("s decay", lambda **k: CFP(decay("age", "gauss", 0, 100), **k)), ("s rrf", lambda **k: CFP(rrf_fn(), **k)), ("s wtd", lambda **k: CFP(wtd_fn([1.0]), **k)),
                          ("s boost", lambda **k: CFP(FS(boost(2.0, "age > 1")), **k)), ("s plain", lambda **k: CFP(None, **k)),
                          ("h rrf", lambda **k: CFH(None, **k)), ("h wtd", lambda **k: CFH(WeightedRanker(1.0), **k)), ("h decay", lambda **k: CFH(decay("age", "gauss", 0, 100), **k))):
            run(f"cf dyn {label} output {of!r}", mk(output_fields=of))
    for fld in CF:
        for label, mk in (("s decay", lambda **k: CFP(decay("age", "gauss", 0, 100), **k)), ("h rrf", lambda **k: CFH(None, **k))):
            run(f"cf dyn {label} group {fld} hidden", mk(group_by_field=fld, group_size=1, limit=10, output_fields=["id"]))
            run(f"cf dyn {label} group {fld} star", mk(group_by_field=fld, group_size=1, limit=10, output_fields=["*"]))
            run(f"cf dyn {label} group {fld} self", mk(group_by_field=fld, group_size=1, limit=10, output_fields=[fld]))

    # 同名冲突：schema 字段与主键
    for vname in CF + ("$meta",):
        for role in ("field", "pk"):
            cname = f"rr_sf_{CF.index(vname) if vname in CF else 9}_{role}"
            sc = MilvusClient.create_schema(auto_id=False)
            if role == "pk":
                sc.add_field(vname, DataType.INT64, is_primary=True)
            else:
                sc.add_field("id", DataType.INT64, is_primary=True)
                sc.add_field(vname, DataType.INT64)
            sc.add_field("v", DataType.FLOAT_VECTOR, dim=2)
            sc.add_field("age", DataType.INT32)
            try:
                c.create_collection(cname, schema=sc, index_params=ip)
                pkname = vname if role == "pk" else "id"
                data = []
                for i in range(10):
                    row = {pkname: i, "v": [1.0, round(0.05 * i, 3)], "age": i}
                    if role == "field":
                        row[vname] = 100 + i % 3
                    data.append(row)
                c.insert(cname, data); c.load_collection(cname)
                results[f"sf create {vname} {role}"] = {"hits": [], "raw": [{"status": [0, ""]}]}
            except Exception as e:
                results[f"sf create {vname} {role}"] = {"error": failure(e), "raw": [{"status": [1, ""]}]}
                continue
            for of in (None, [], ["age"], [vname]):
                run(f"sf {vname} {role} s decay {of!r}", lambda cn=cname, of=of: c.search(cn, data=QG, limit=5, ranker=decay("age", "gauss", 0, 100), output_fields=of))
                run(f"sf {vname} {role} s rrf {of!r}", lambda cn=cname, of=of: c.search(cn, data=QG, limit=5, ranker=rrf_fn(), output_fields=of))
                run(f"sf {vname} {role} s boost {of!r}", lambda cn=cname, of=of: c.search(cn, data=QG, limit=5, ranker=FS(boost(2.0, "age > 1")), output_fields=of))
                run(f"sf {vname} {role} s plain {of!r}", lambda cn=cname, of=of: c.search(cn, data=QG, limit=5, output_fields=of))
                run(f"sf {vname} {role} h rrf {of!r}", H(cname, [AnnSearchRequest(data=QG, anns_field="v", param={}, limit=8)], RRFRanker(), limit=5, output_fields=of))
                run(f"sf {vname} {role} h decay {of!r}", H(cname, [AnnSearchRequest(data=QG, anns_field="v", param={}, limit=8)], decay("age", "gauss", 0, 100), limit=5, output_fields=of))
            for of in (None, ["age"], [vname]):
                run(f"sf {vname} {role} h boost {of!r}", H(cname, [AnnSearchRequest(data=QG, anns_field="v", param={}, limit=8)], FS(boost(2.0, "age > 1")), limit=5, output_fields=of))
                run(f"sf {vname} {role} h boost decay {of!r}", H(cname, [AnnSearchRequest(data=QG, anns_field="v", param={}, limit=8)], FS(boost(2.0, "age > 1"), decay("age", "gauss", 0, 100)), limit=5, output_fields=of))
            for gs in (1, 10):
                run(f"sf {vname} {role} s boost group gs{gs}", lambda cn=cname, gs=gs: c.search(cn, data=QG, limit=10, ranker=FS(boost(2.0, "age > 1")), group_by_field=vname, group_size=gs, output_fields=["age"]))
                run(f"sf {vname} {role} s plain group gs{gs}", lambda cn=cname, gs=gs: c.search(cn, data=QG, limit=10, group_by_field=vname, group_size=gs, output_fields=["age"]))
                run(f"sf {vname} {role} h boost group gs{gs}", H(cname, [AnnSearchRequest(data=QG, anns_field="v", param={}, limit=8)], FS(boost(2.0, "age > 1")), limit=10, group_by_field=vname, group_size=gs, output_fields=["age"]))
                run(f"sf {vname} {role} s rrf group gs{gs}", lambda cn=cname, gs=gs: c.search(cn, data=QG, limit=10, ranker=rrf_fn(), group_by_field=vname, group_size=gs, output_fields=["age"]))
                run(f"sf {vname} {role} s wtd group gs{gs}", lambda cn=cname, gs=gs: c.search(cn, data=QG, limit=10, ranker=wtd_fn([1.0]), group_by_field=vname, group_size=gs, output_fields=["age"]))
                run(f"sf {vname} {role} h decay group gs{gs}", H(cname, [AnnSearchRequest(data=QG, anns_field="v", param={}, limit=8)], decay("age", "gauss", 0, 100), limit=10, group_by_field=vname, group_size=gs, output_fields=["age"]))
                run(f"sf {vname} {role} s decay group gs{gs}", lambda cn=cname, gs=gs: c.search(cn, data=QG, limit=10, ranker=decay("age", "gauss", 0, 100), group_by_field=vname, group_size=gs, output_fields=["age"]))
                run(f"sf {vname} {role} h rrf group gs{gs}", H(cname, [AnnSearchRequest(data=QG, anns_field="v", param={}, limit=8)], RRFRanker(), limit=10, group_by_field=vname, group_size=gs, output_fields=["age"]))

    # 同名冲突的非数字覆盖值
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=True)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("age", DataType.INT32)
    for nm, rowv in (("str", "x"), ("list", [1]), ("dict", {"a": 1}), ("bool", True), ("null", None)):
        for key in ("$id", "$score", "$distance", "_decay_score"):
            cn = "rr_nv_%s_%s" % (nm, key.strip("$_"))
            c.create_collection(cn, schema=s, index_params=ip)
            c.insert(cn, [{"id": i, "v": [1.0, round(0.05 * i, 3)], "age": i, key: rowv} for i in range(6)])
            c.load_collection(cn)
            q = AnnSearchRequest(data=QG, anns_field="v", param={}, limit=5)
            run(f"nv {key} {nm} s decay", lambda cn=cn: c.search(cn, data=QG, limit=4, ranker=decay("age", "gauss", 0, 100), output_fields=["*"]))
            run(f"nv {key} {nm} s rrf", lambda cn=cn: c.search(cn, data=QG, limit=4, ranker=rrf_fn(), output_fields=["*"]))
            run(f"nv {key} {nm} s wtd", lambda cn=cn: c.search(cn, data=QG, limit=4, ranker=wtd_fn([1.0]), output_fields=["*"]))
            run(f"nv {key} {nm} s wtd norm", lambda cn=cn: c.search(cn, data=QG, limit=4, ranker=wtd_fn([1.0], True), output_fields=["*"]))
            run(f"nv {key} {nm} s boost", lambda cn=cn: c.search(cn, data=QG, limit=4, ranker=FS(boost(2.0)), output_fields=["*"]))
            run(f"nv {key} {nm} h rrf", H(cn, [q], RRFRanker(), limit=4, output_fields=["*"]))
            run(f"nv {key} {nm} h group", H(cn, [q], RRFRanker(), limit=4, group_by_field="age", group_size=1, output_fields=["*"]))

    # 多值动态键：组数才能看出“存在”
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=True)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
    s.add_field("age", DataType.INT32)
    c.create_collection("rr_cfm", schema=s, index_params=ip)
    c.insert("rr_cfm", [{"id": i, "v": [1.0, round(0.05 * i, 3)], "age": i, "$group_score": i % 3, "$distance": i % 3 + 0.5, "_decay_score": i % 3 + 10} for i in range(10)])
    c.load_collection("rr_cfm")
    QM = lambda: AnnSearchRequest(data=QG, anns_field="v", param={}, limit=8)
    for fld in ("$group_score", "$distance", "_decay_score"):
        for of in (["id"], ["*"], None, [], [fld]):
            for gs in (1, 10):
                run(f"cfm {fld} s decay {of!r} gs{gs}", lambda fld=fld, of=of, gs=gs: c.search("rr_cfm", data=QG, limit=10, ranker=decay("age", "gauss", 0, 100), group_by_field=fld, group_size=gs, output_fields=of))
                run(f"cfm {fld} s rrf {of!r} gs{gs}", lambda fld=fld, of=of, gs=gs: c.search("rr_cfm", data=QG, limit=10, ranker=rrf_fn(), group_by_field=fld, group_size=gs, output_fields=of))
                run(f"cfm {fld} s wtd {of!r} gs{gs}", lambda fld=fld, of=of, gs=gs: c.search("rr_cfm", data=QG, limit=10, ranker=wtd_fn([1.0]), group_by_field=fld, group_size=gs, output_fields=of))
                run(f"cfm {fld} h rrf {of!r} gs{gs}", H("rr_cfm", [QM()], RRFRanker(), limit=10, group_by_field=fld, group_size=gs, output_fields=of))
                run(f"cfm {fld} h wtd {of!r} gs{gs}", H("rr_cfm", [QM()], WeightedRanker(1.0), limit=10, group_by_field=fld, group_size=gs, output_fields=of))
                run(f"cfm {fld} h decay {of!r} gs{gs}", H("rr_cfm", [QM()], decay("age", "gauss", 0, 100), limit=10, group_by_field=fld, group_size=gs, output_fields=of))
                run(f"cfm {fld} h boost {of!r} gs{gs}", H("rr_cfm", [QM()], FS(boost(2.0, "age > 1")), limit=10, group_by_field=fld, group_size=gs, output_fields=of))
            run(f"cfm {fld} s boost {of!r}", lambda fld=fld, of=of: c.search("rr_cfm", data=QG, limit=10, ranker=FS(boost(2.0, "age > 1")), group_by_field=fld, group_size=2, output_fields=of))
            run(f"cfm {fld} s plain {of!r}", lambda fld=fld, of=of: c.search("rr_cfm", data=QG, limit=10, group_by_field=fld, group_size=2, output_fields=of))
    # 路由 boost 与动态同名
    for of in (["id"], ["*"], None, [], ["$id"], ["$score"], ["$distance"]):
        run(f"cf dyn h boost output {of!r}", CFH(FS(boost(2.0, "age > 1")), output_fields=of))
        run(f"cf dyn h boost decay output {of!r}", CFH(FS(boost(2.0, "age > 1"), decay("age", "gauss", 0, 100)), output_fields=of))
        run(f"cf dyn s boost group output {of!r}", CFP(FS(boost(2.0, "age > 1")), group_by_field="g", group_size=2, output_fields=of))
        run(f"cf dyn h boost group output {of!r}", CFH(FS(boost(2.0, "age > 1")), group_by_field="g", group_size=2, output_fields=of))
    # 同名 schema 字段的值类型：DOUBLE / VARCHAR（字段与主键）
    for vname in CF:
        for role, dt, kw in (("fieldD", DataType.DOUBLE, {}), ("fieldV", DataType.VARCHAR, {"max_length": 16}), ("pkV", DataType.VARCHAR, {"max_length": 16})):
            cname = f"rr_st_{CF.index(vname)}_{role}"
            sc = MilvusClient.create_schema(auto_id=False)
            if role == "pkV":
                sc.add_field(vname, dt, is_primary=True, **kw)
                sc.add_field("age", DataType.INT32)
            else:
                sc.add_field("id", DataType.INT64, is_primary=True)
                sc.add_field(vname, dt, **kw)
                sc.add_field("age", DataType.INT32)
            sc.add_field("v", DataType.FLOAT_VECTOR, dim=2)
            c.create_collection(cname, schema=sc, index_params=ip)
            data = []
            for i in range(10):
                row = {"v": [1.0, round(0.05 * i, 3)], "age": i}
                if role == "pkV":
                    row[vname] = "k%d" % i
                else:
                    row["id"] = i
                    row[vname] = (float(i % 3) if role == "fieldD" else "s%d" % (i % 3))
                data.append(row)
            c.insert(cname, data); c.load_collection(cname)
            q = lambda: AnnSearchRequest(data=QG, anns_field="v", param={}, limit=8)
            for of in (None, ["age"], [vname]):
                run(f"st {vname} {role} s decay {of!r}", lambda cn=cname, of=of: c.search(cn, data=QG, limit=5, ranker=decay("age", "gauss", 0, 100), output_fields=of))
                run(f"st {vname} {role} s rrf {of!r}", lambda cn=cname, of=of: c.search(cn, data=QG, limit=5, ranker=rrf_fn(), output_fields=of))
                run(f"st {vname} {role} s wtd {of!r}", lambda cn=cname, of=of: c.search(cn, data=QG, limit=5, ranker=wtd_fn([1.0]), output_fields=of))
                run(f"st {vname} {role} s boost {of!r}", lambda cn=cname, of=of: c.search(cn, data=QG, limit=5, ranker=FS(boost(2.0, "age > 1")), output_fields=of))
                run(f"st {vname} {role} h rrf {of!r}", H(cname, [q()], RRFRanker(), limit=5, output_fields=of))
                run(f"st {vname} {role} h boost {of!r}", H(cname, [q()], FS(boost(2.0, "age > 1")), limit=5, output_fields=of))
            for gs in (1, 10):
                run(f"st {vname} {role} s decay group gs{gs}", lambda cn=cname, gs=gs: c.search(cn, data=QG, limit=10, ranker=decay("age", "gauss", 0, 100), group_by_field=vname, group_size=gs, output_fields=["age"]))
                run(f"st {vname} {role} s rrf group gs{gs}", lambda cn=cname, gs=gs: c.search(cn, data=QG, limit=10, ranker=rrf_fn(), group_by_field=vname, group_size=gs, output_fields=["age"]))
                run(f"st {vname} {role} h rrf group gs{gs}", H(cname, [q()], RRFRanker(), limit=10, group_by_field=vname, group_size=gs, output_fields=["age"]))
                run(f"st {vname} {role} s boost group gs{gs}", lambda cn=cname, gs=gs: c.search(cn, data=QG, limit=10, ranker=FS(boost(2.0, "age > 1")), group_by_field=vname, group_size=gs, output_fields=["age"]))

    return results


def collect_s4(c):
    ip = c.prepare_index_params(); ip.add_index("v", index_type="FLAT", metric_type="COSINE")
    Q = [[1.0, 0.0]]
    BOOST = FunctionScore([Function("b", FunctionType.RERANK, [], params={"reranker": "boost", "weight": 2.0, "filter": "age > 1"})])
    TYPES = {
        "BOOL": (DataType.BOOL, {}, lambda i: bool(i % 2)),
        "INT8": (DataType.INT8, {}, lambda i: i % 3),
        "INT64": (DataType.INT64, {}, lambda i: 100 + i % 3),
        "VARCHAR_num": (DataType.VARCHAR, {"max_length": 16}, lambda i: str(i % 3)),
        "VARCHAR_float": (DataType.VARCHAR, {"max_length": 16}, lambda i: "%d.5" % (i % 3)),
        "VARCHAR_alpha": (DataType.VARCHAR, {"max_length": 16}, lambda i: "s%d" % (i % 3)),
        "FLOAT": (DataType.FLOAT, {}, lambda i: float(i % 3)),
        "DOUBLE": (DataType.DOUBLE, {}, lambda i: float(i % 3)),
        "JSON": (DataType.JSON, {}, lambda i: {"k": i % 3}),
        "ARRAY": (DataType.ARRAY, {"element_type": DataType.INT64, "max_capacity": 2}, lambda i: [i % 3]),
        "TIMESTAMPTZ": (DataType.TIMESTAMPTZ, {}, lambda i: "2025-01-0%dT00:00:00Z" % (1 + i % 3)),
    }
    out = {}
    conn = c._get_connection(); orig = conn._stub.Search; RAW = []
    def spy(req, *a, **k):
        r = orig(req, *a, **k); d = r.results; g = d.group_by_field_value; w = g.scalars.WhichOneof("data")
        RAW.append({"status": [r.status.code, r.status.reason], "ids": list(d.ids.int_id.data) or list(d.ids.str_id.data), "scores": list(d.scores),
                    "group_has": d.HasField("group_by_field_value"), "group_kind": w, "group_valid": list(g.valid_data),
                    "group_vals": group_values(g)})
        return r
    conn._stub.Search = spy
    n = 0
    for vname in ("$id", "$distance", "$score", "$group_score", "_decay_score"):
        for tname, (dt, kw, val) in TYPES.items():
            n += 1; cn = "c%d" % n
            s = MilvusClient.create_schema(auto_id=False)
            s.add_field("id", DataType.INT64, is_primary=True); s.add_field(vname, dt, **kw)
            s.add_field("age", DataType.INT32); s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
            c.create_collection(cn, schema=s, index_params=ip)
            c.insert(cn, [{"id": i, "v": [1.0, round(0.05 * i, 3)], "age": i, vname: val(i)} for i in range(9)]); c.load_collection(cn)
            for gs in (1, 10):
                RAW.clear(); key = f"{vname} {tname} s boost group gs{gs}"
                try:
                    c.search(cn, data=Q, limit=10, ranker=BOOST, group_by_field=vname, group_size=gs, output_fields=["age"]); out[key] = RAW[-1]
                except Exception as e:
                    out[key] = {"error": failure(e), "raw": list(RAW)}
    # VARCHAR 数字字符串：过滤全部命中 / 全不命中 / 部分命中
    def FSF(flt=None):
        p = {"reranker": "boost", "weight": 2.0}
        if flt: p["filter"] = flt
        return FunctionScore([Function("b", FunctionType.RERANK, [], params=p)])
    for vname in ("$id", "$distance"):
        for tname in ("VARCHAR_num", "VARCHAR_float", "INT64"):
            dt, kw, val = TYPES[tname]
            n += 1; cn = "c%d" % n
            s = MilvusClient.create_schema(auto_id=False)
            s.add_field("id", DataType.INT64, is_primary=True); s.add_field(vname, dt, **kw)
            s.add_field("age", DataType.INT32); s.add_field("v", DataType.FLOAT_VECTOR, dim=2)
            c.create_collection(cn, schema=s, index_params=ip)
            c.insert(cn, [{"id": i, "v": [1.0, round(0.05 * i, 3)], "age": i, vname: val(i)} for i in range(9)]); c.load_collection(cn)
            for fname, flt in (("all", None), ("none", "age > 99"), ("some", "age > 1")):
                for gname, kw2 in (("group", dict(group_by_field=vname, group_size=2)), ("nogroup", {}), ("output", dict(output_fields=[vname]))):
                    RAW.clear(); key = f"filter {vname} {tname} {fname} {gname}"
                    kw3 = dict(kw2); kw3.setdefault("output_fields", ["age"])
                    try:
                        c.search(cn, data=Q, limit=10, ranker=FSF(flt), **kw3); out[key] = RAW[-1]
                    except Exception as e:
                        out[key] = {"error": failure(e), "raw": list(RAW)}
    return out
