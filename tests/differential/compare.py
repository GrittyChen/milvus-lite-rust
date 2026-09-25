"""把同一个操作分别发给本实现与 3.x，按 0004「决策」的比较口径判断结果是否一致。

每一步的结果归一成三种形式之一：
- ("ok", 值)：值转成只含 dict / list / 标量的结构；
- ("err", code, compatible_code, message)：pymilvus 的 MilvusException，compatible_code 是旧的 error_code；
- ("rpc", 状态码名, details)：gRPC 层的错误，例如 UNIMPLEMENTED。
错误逐字比较。成功结果默认逐项相等，并且 repr 也相同：== 分不出 1 与 1.0、True 与 1、-0.0 与 0.0，
也分不出 dict 的 key 顺序，而这些都是用户看得到的。检索结果用 hits() 按距离的允许误差比较。
"""

from __future__ import annotations

import enum
import math

import grpc
import numpy as np
from pymilvus.exceptions import MilvusException


def plain(x):
    if x is None or isinstance(x, (bool, int, float, str, bytes)):
        return x
    if isinstance(x, enum.Enum):
        return str(x)
    if isinstance(x, np.generic):
        return x.item()
    if isinstance(x, dict) or hasattr(x, "items"):
        return {k: plain(v) for k, v in x.items()}
    # protobuf 的 repeated 容器（例如 insert 返回的 ids）没有 __iter__，只能经 iter() 迭代。
    try:
        items = iter(x)
    except TypeError:
        raise TypeError(f"cannot normalize {type(x).__name__}") from None
    return [plain(v) for v in items]


def run(fn, client):
    try:
        return ("ok", plain(fn(client)))
    except MilvusException as e:
        return ("err", e.code, e.compatible_code, e.message)
    except grpc.RpcError as e:
        return ("rpc", e.code().name, e.details())


def same(a, b):
    return a == b and repr(a) == repr(b)


def exact(a, b):
    return None if same(a, b) else "results differ"


def tolerance(metric, distance, q, v):
    """0004「决策」的单个距离的允许误差。"""
    if metric == "L2":
        return max(1e-5 * abs(distance), 1e-6)
    if metric == "IP":
        return max(1e-5 * float(np.linalg.norm(q)) * float(np.linalg.norm(v)), 1e-6)
    if metric == "COSINE":
        return 1e-5
    raise ValueError(metric)


def pk_of(hit):
    """pymilvus 的检索结果以主键字段名为键，另有 distance 与 entity；返回 (字段名, 值)。"""
    (key,) = hit.keys() - {"distance", "entity"}
    return key, hit[key]


def hits(metric, queries, vectors, *, k, offset=0, round_decimal=None):
    """检索结果的比较（0004「决策」）。

    - 每个查询两边都返回 k 个结果：场景写错时两边会一起返回空结果，所以由调用方声明结果数；
    - 距离都是有限值：NaN 与 inf 让下面的误差、排序与配对判断失效（比较结果恒为假，L2 的误差随距离变成 inf）。
      M1 的场景不产生这类距离，需要比较它们时再单独定规则；
    - 本实现的结果按自己的距离排好序；
    - 两边都返回的主键（字段名、值与值的类型都相同）：距离之差不超过允许误差，entity 逐项相等。
      由这两条可推出 0004 的顺序规则：两个结果在两边先后相反时，3.x 的距离之差不超过两者的允许误差之和；
    - 只有一边返回的主键：两边的差集按距离排序后一一配对，每对是截断位置上的一次互换：
      - 配对双方与 3.x 中两者的距离之差不超过允许误差之和。本实现独有的 u 没有 3.x 的距离，用本实现的
        距离代替，它与 3.x 的值相差不超过 t_u，所以检查 |a_u − b_w| ≤ 2·t_u + t_w；
      - 3.x 独有的 w 落在截断位置：与 3.x 末个结果的距离之差不超过两者的允许误差之和；offset > 0 时
        前端也是截断位置，也可以与首个结果比较。
    带 round_decimal 时，单个距离的允许误差取 max(10^-round_decimal, 上述误差)；本实现的距离还须已经舍入，
    即再舍入一次不变。只比较两边之差时，多保留一位的偏差在 10^-round_decimal 以内，发现不了。
    pymilvus 在 round_decimal > 0 时会在客户端对服务端的值再 round 一次，所以经 pymilvus 只看得到
    round_decimal = 0 时服务端的舍入；服务端的舍入另由 tests/python 中手写请求的用例对照 3.x。
    vectors 是主键值到向量的映射，用于算 IP 的 ‖q‖·‖v‖。
    """

    def tol(qi, pk, d):
        t = tolerance(metric, d, queries[qi], vectors[pk])
        if round_decimal is None:
            return t
        # 舍入后恰好相差一个单位时，两个十进制小数的浮点差可能略大于 10^-round_decimal。
        return max(10.0 ** -round_decimal, t) * (1 + 1e-9)

    def key(hit):
        # 1、1.0 与 True 作为 dict 的键会被当成同一个，用 repr 区分。
        name, value = pk_of(hit)
        return name, repr(value)

    def check(a, b):
        if not (a[0] == b[0] == "ok"):
            return exact(a, b)
        ra, rb = a[1], b[1]
        if len(ra) != len(rb):
            return f"nq differs: {len(ra)} vs {len(rb)}"
        for qi, (ha, hb) in enumerate(zip(ra, rb)):
            if len(ha) != k or len(hb) != k:
                return f"query {qi}: expected {k} hits, got {len(ha)} vs {len(hb)}"
            if not all(math.isfinite(h["distance"]) for h in ha + hb):
                return f"query {qi}: non-finite distance"
            # 主键的字段名与值的类型在一个 collection 内是固定的；不先检查，名字或类型不同的结果
            # 会被当成截断位置上两个并列的不同主键而放过。
            if len({(pk_of(h)[0], type(pk_of(h)[1])) for h in ha + hb}) > 1:
                return f"query {qi}: primary key field names or value types differ"
            da = {key(h): h for h in ha}
            db = {key(h): h for h in hb}
            if len(da) != len(ha) or len(db) != len(hb):
                return f"query {qi}: duplicate ids"
            t = {}
            for pk, h in {**da, **db}.items():
                value = pk_of(h)[1]
                if value not in vectors:
                    return f"query {qi}: unknown pk {pk!r}"
                t[pk] = tol(qi, value, h["distance"])
            if round_decimal is not None:
                for pk, h in da.items():
                    d = h["distance"]
                    if round(d, round_decimal) != d:
                        return f"query {qi}: id {pk!r} distance {d!r} is not rounded to {round_decimal} digits"
            ds = [h["distance"] for h in ha]
            if ds != sorted(ds, reverse=metric != "L2"):
                return f"query {qi}: hits are not sorted by distance"
            for pk in da.keys() & db.keys():
                x, y = da[pk], db[pk]
                if not math.isclose(x["distance"], y["distance"], rel_tol=0, abs_tol=t[pk]):
                    return f"query {qi}: id {pk!r} distance {x['distance']!r} vs {y['distance']!r} (tolerance {t[pk]:.3g})"
                if not same(x.get("entity"), y.get("entity")):
                    return f"query {qi}: id {pk!r} entity {x.get('entity')!r} vs {y.get('entity')!r}"
            cuts = hb[-1:] + (hb[:1] if offset > 0 else [])
            for u, w in zip(sorted(da.keys() - db.keys(), key=lambda pk: da[pk]["distance"]),
                            sorted(db.keys() - da.keys(), key=lambda pk: db[pk]["distance"])):
                au, bw = da[u]["distance"], db[w]["distance"]
                if abs(au - bw) > 2 * t[u] + t[w]:
                    return f"query {qi}: {u!r} (only in rust) and {w!r} (only in 3.x) are not tied"
                if not any(abs(bw - c["distance"]) <= t[w] + t[key(c)] for c in cuts):
                    return f"query {qi}: {u!r} and {w!r} differ away from the cut"
        return None

    return check


class Diff:
    def __init__(self, rust, ref):
        self.rust, self.ref = rust, ref

    def step(self, name, fn, check=exact, error=False):
        """在两边执行 fn(client)，不一致时报告两边的结果；返回本实现的归一化结果。

        error 声明这一步应当失败：场景数据写错时两边会一起报错、之后一起返回空结果，比较照样一致，
        所以每一步都要声明预期的成败。
        """
        a, b = run(fn, self.rust), run(fn, self.ref)
        why = check(a, b)
        assert why is None, f"{name}: {why}\n  rust: {a!r}\n  3.x:  {b!r}"
        assert (b[0] != "ok") == error, f"{name}: expected {'an error' if error else 'success'}, 3.x gave {b!r}"
        return a
