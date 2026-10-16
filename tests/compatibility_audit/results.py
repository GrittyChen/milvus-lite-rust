"""严格解析 JUnit；诊断干预结果不计为正式兼容性通过。"""

from collections import Counter
import json
from pathlib import Path
import re
import xml.etree.ElementTree as ET


def key(nodeid):
    path, *parts = nodeid.split("::")
    return path[:-3].replace("/", ".") + ("." + ".".join(parts[:-1]) if len(parts) > 1 else "") + "::" + parts[-1]


def load(path):
    records = {}
    for case in ET.parse(path).getroot().iter("testcase"):
        identity = case.get("classname", "") + "::" + case.get("name", "")
        if identity in records:
            raise ValueError(f"重复用例：{identity}")
        failures = [child for child in case if child.tag in ("failure", "error")]
        skipped = case.find("skipped")
        status = failures[0].tag if failures else "skipped" if skipped is not None else "passed"
        text = "\n".join((child.get("message") or "") + "\n" + (child.text or "") for child in failures)
        if skipped is not None:
            text += (skipped.get("message") or "") + "\n" + (skipped.text or "")
        match = re.search(r"MilvusLite does not support ([^:\n]+?)(?::|\n|$)", text)
        reason = "UNIMPLEMENTED: " + match.group(1).strip() if match else status
        records[identity] = {"status": status, "reason": reason, "text": text}
    return records


def verify(records, nodes):
    expected = {key(node) for node in nodes}
    if len(expected) != len(nodes) or set(records) != expected:
        raise ValueError(f"JUnit 集合改变：expected={len(nodes)}, actual={len(records)}, missing={expected-set(records)}, extra={set(records)-expected}")


def summarize(run_dir):
    run_dir = Path(run_dir)
    meta = json.loads((run_dir / "run.json").read_text())
    nodes = json.loads((run_dir / "selection.json").read_text())
    records = load(run_dir / "result.xml")
    verify(records, nodes)
    events = [json.loads(line) for line in (run_dir / "events.jsonl").read_text().splitlines()]
    by_node = {}
    for node in nodes:
        own = [event for event in events if event["nodeid"] == node]
        declarations = list({json.dumps(field, sort_keys=True): field for event in own if event["event"] in ("create", "use") for field in event["fields"]}.values())
        null_fields = {name for event in own if event["event"] in ("insert", "upsert") for row in event["rows"] for name in row["null_fields"]}
        entry = records[key(node)]
        by_node[node] = {**entry, "fields": declarations, "explicit_null_fields": sorted(null_fields),
                         "intervened": any(field.get("changed") for field in declarations)}
    return {
        "mode": meta["mode"], "backend": meta["backend"], "identity": meta["identity"],
        "formal": meta["mode"] == "original", "count": len(records),
        "outcomes": dict(Counter(x["status"] for x in records.values())),
        "reasons": dict(Counter(x["reason"] for x in records.values())), "cases": by_node,
    }
