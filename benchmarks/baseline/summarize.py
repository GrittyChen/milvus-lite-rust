"""把 run.sh 产生的 jsonl 汇总成 markdown 表（延迟取 p50）。用法: python summarize.py <out.jsonl>"""
import json
import sys

BUILD = ["insert_rows_per_s", "create_index_s", "search_nq1_top10", "search_nq100_top10", "search_nq1_filter10pct",
         "search_nq1_filter1pct_str", "query_filter_limit100", "query_count", "get_100_ids", "recall@10",
         "delete_1000_ids_s", "upsert_1000_s", "rss_mb_after_search"]
REOPEN = ["connect_s", "reopen_load_s", "reopen_total_to_ready_s", "first_search_ms", "search_nq1_top10", "rss_mb_after_reopen"]

M1 = [("insert_rows_per_s",), ("search_nq1_top10",), ("search_nq100_top10",), ("query_count_empty_expr",),
      ("get_100_ids",), ("recall@10",)] + [
    (f"concurrency_{kind}", f"k{k}", stat) for kind in ("threads", "processes") for k in (1, 2, 4, 8)
    for stat in ("qps", "p99_ms")]

rows, m1 = {}, {}
for line in open(sys.argv[1]):
    if line.strip():
        r = json.loads(line)
        key = (r.get("ver"), r.get("index"), r.get("mode"), r.get("N"), r.get("dim"))
        if r.get("phase") == "m1":
            # m1 按轮次重复运行，同一配置的各轮一起汇总；Rust 实现的不同构建分开汇总
            m1.setdefault(key + (r.get("versions", {}).get("build_commit"),), []).append(r)
        else:
            # 同一配置多次追加时以最后一次为准
            rows[key + (r.get("phase"),)] = r
rows = list(rows.values())


def val(v):
    return v["p50_ms"] if isinstance(v, dict) else v


def pick(r, path):
    v = r
    for p in path:
        v = v.get(p) if isinstance(v, dict) else None
    return val(v) if v is not None else None


def spread(xs):
    """中位数（最小–最大），轮次为 1 时只给数值。"""
    xs = sorted(x for x in xs if x is not None)
    if not xs:
        return "—"
    mid = xs[len(xs) // 2] if len(xs) % 2 else (xs[len(xs) // 2 - 1] + xs[len(xs) // 2]) / 2
    fmt = lambda x: f"{x:.4g}"
    return fmt(mid) if len(xs) == 1 else f"{fmt(mid)} ({fmt(xs[0])}–{fmt(xs[-1])})"


if m1:
    configs = sorted(m1, key=lambda k: (k[3], k[0], k[5] or ""))
    print("\n### m1（中位数，括号内为各轮的最小–最大）\n")
    print("| 指标 | " + " | ".join(f"{v}{'@' + b if b else ''} {i} {m} N={n} ×{len(m1[(v, i, m, n, d, b)])}"
                                   for v, i, m, n, d, b in configs) + " |")
    print("| --- |" + " --- |" * len(configs))
    for path in M1:
        print(f"| {' '.join(path)} | " + " | ".join(spread(pick(r, path) for r in m1[k]) for k in configs) + " |")

for phase, keys in (("build", BUILD), ("reopen", REOPEN)):
    rs = [r for r in rows if r.get("phase") == phase]
    if not rs:
        continue
    print(f"\n### {phase}\n")
    print("| 指标 | " + " | ".join(f"{r['ver']} {r['index']} {r['mode']} N={r['N']}" for r in rs) + " |")
    print("| --- |" + " --- |" * len(rs))
    for k in keys:
        print(f"| {k} | " + " | ".join(str(val(r.get(k, "—"))) for r in rs) + " |")
