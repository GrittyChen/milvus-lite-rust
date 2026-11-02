"""0024 分组检索：同一批场景分别发给本实现与 3.x（43d1257），逐项比较。

比较命中的主键、距离（相对与绝对容差均为 1e-5，按位置逐个比较；度量相关的 0004 误差与并列顺序规则由现有检索差分负责，分组复用同一套 FLAT 距离计算）、实体、`topks`、`top_k` 与响应里的 `group_by_field_value`
（字段名、类型、值、有效位）。并列距离落在分组边界的场景只比较组级不变量；3.x 回退取值缺陷的场景
（`BUGS`）两边的输出都单独断言，原因见 0024。
"""

import math

import pytest
from pymilvus.exceptions import MilvusException

import group_by_cases as cases
from test_reopen import pair  # noqa: F401  （fixture）


def install_spy(client):
    """记录每次 Search 的原始响应；MilvusClient 只暴露转换后的命中，看不到 group_by_field_value。"""
    connection = client._get_connection()
    raw = []
    original = connection._stub.Search

    def spy(request, *args, **kwargs):
        reply = original(request, *args, **kwargs)
        data, group = reply.results, reply.results.group_by_field_value
        raw.append({
            "status": [reply.status.code, reply.status.reason[:120]],
            "top_k": data.top_k,
            "nq": data.num_queries,
            "topks": list(data.topks),
            "ids": list(data.ids.int_id.data) or list(data.ids.str_id.data),
            "output_fields": list(data.output_fields),
            "fields": [f.field_name for f in data.fields_data],
            "group_has": data.HasField("group_by_field_value"),
            "group_name": group.field_name,
            "group_type": group.type,
            "group_vals": (list(group.scalars.long_data.data) or list(group.scalars.string_data.data)
                           or list(group.scalars.int_data.data) or list(group.scalars.bool_data.data)),
            "group_valid": list(group.valid_data),
        })
        return reply

    connection._stub.Search = spy
    return raw


def hit_key(hit):
    (key,) = hit.keys() - {"distance", "entity"}
    return hit[key]


def outcome(client, raw, fn):
    raw.clear()
    try:
        result = fn(client)
    except MilvusException as e:
        return {"error": [e.code, str(e.message)[:200]], "raw": list(raw)}
    return {
        "hits": [[{"id": hit_key(h) if "entity" in h and "distance" in h and len(h) == 3 else h.get("id"),
                   "d": h["distance"], "e": dict(h["entity"])} for h in query] for query in result],
        "raw": list(raw),
    }


def close(a, b):
    return math.isclose(a, b, rel_tol=1e-5, abs_tol=1e-5)


def compare(name, rust, ref):
    if ("error" in rust) != ("error" in ref):
        return f"error mismatch\n rust {rust}\n ref  {ref}"
    if "error" in ref:
        return None if rust["error"] == ref["error"] else f"errors differ\n rust {rust['error']}\n ref  {ref['error']}"
    if len(rust["hits"]) != len(ref["hits"]):
        return "nq differs"
    for qi, (a, b) in enumerate(zip(rust["hits"], ref["hits"])):
        if len(a) != len(b):
            return f"query {qi}: {len(a)} vs {len(b)} hits"
        for x, y in zip(a, b):
            if x["id"] != y["id"] or x["e"] != y["e"] or not close(x["d"], y["d"]):
                return f"query {qi}: {x} vs {y}"
    return None if rust["raw"] == ref["raw"] else f"raw differs\n rust {rust['raw']}\n ref  {ref['raw']}"


def group_invariants(result):
    """每个查询的组集合与每组的主键集合，不看组内与组间的并列次序。"""
    out = []
    for raw in result["raw"]:
        groups = {}
        cursor = 0
        for k in raw["topks"]:
            ids = raw["ids"][cursor:cursor + k]
            vals = raw["group_vals"][cursor:cursor + k]
            valid = raw["group_valid"][cursor:cursor + k] or [True] * k
            for pk, v, ok in zip(ids, vals, valid):
                groups.setdefault((v if ok else None), []).append(pk)
            cursor += k
        out.append({g: sorted(ids) for g, ids in groups.items()})
    return out


def test_group_by_matches_3x_scenarios(pair):
    d = pair.open()
    rust_raw, ref_raw = install_spy(d.rust), install_spy(d.ref)
    failures, seen = [], set()
    for action in cases.ACTIONS:
        if action[0] == "setup":
            for client in (d.rust, d.ref):
                action[1](client)
            continue
        _, name, fn = action
        rust = outcome(d.rust, rust_raw, fn)
        ref = outcome(d.ref, ref_raw, fn)
        seen.add(name)
        if name in cases.BUGS:
            expected = cases.BUGS[name]
            for side, result in (("rust", rust), ("ref", ref)):
                raw = result["raw"][0]
                got = {k: raw[k] for k in ("ids", "group_vals", "group_valid")}
                if got != expected[side]:
                    failures.append(f"{name} [{side}]: {got} != {expected[side]}")
        elif name in cases.TIES:
            if "error" in rust or "error" in ref or group_invariants(rust) != group_invariants(ref):
                failures.append(f"{name}: {group_invariants(rust)} vs {group_invariants(ref)}")
            elif [r["topks"] for r in rust["raw"]] != [r["topks"] for r in ref["raw"]]:
                failures.append(f"{name}: topks differ")
        else:
            message = compare(name, rust, ref)
            if message:
                failures.append(f"{name}: {message}")
    assert seen >= set(cases.BUGS) | cases.TIES
    assert not failures, "\n".join(failures)
