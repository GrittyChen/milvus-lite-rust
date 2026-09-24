"""把 run.sh 产生的 jsonl 汇总成 markdown 表（延迟取 p50）。用法: python summarize.py <out.jsonl>"""
import json
import sys

BUILD = ["insert_rows_per_s", "create_index_s", "search_nq1_top10", "search_nq100_top10", "search_nq1_filter10pct",
         "search_nq1_filter1pct_str", "query_filter_limit100", "query_count", "get_100_ids", "recall@10",
         "delete_1000_ids_s", "upsert_1000_s", "rss_mb_after_search"]
REOPEN = ["connect_s", "reopen_load_s", "reopen_total_to_ready_s", "first_search_ms", "search_nq1_top10", "rss_mb_after_reopen"]

rows = {}
for line in open(sys.argv[1]):
    if line.strip():
        r = json.loads(line)
        # 同一配置多次追加时以最后一次为准
        rows[(r.get("ver"), r.get("index"), r.get("mode"), r.get("N"), r.get("dim"), r.get("phase"))] = r
rows = list(rows.values())


def val(v):
    return v["p50_ms"] if isinstance(v, dict) else v


for phase, keys in (("build", BUILD), ("reopen", REOPEN)):
    rs = [r for r in rows if r.get("phase") == phase]
    if not rs:
        continue
    print(f"\n### {phase}\n")
    print("| 指标 | " + " | ".join(f"{r['ver']} {r['index']} {r['mode']} N={r['N']}" for r in rs) + " |")
    print("| --- |" + " --- |" * len(rs))
    for k in keys:
        print(f"| {k} | " + " | ".join(str(val(r.get(k, "—"))) for r in rs) + " |")
