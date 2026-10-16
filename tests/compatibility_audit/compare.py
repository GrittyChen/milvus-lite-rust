"""按原用例身份对照受控干预，不把干预后的通过计入兼容率。"""

import argparse
from collections import Counter
import json
from pathlib import Path


def compare(baseline, reference, current):
    if reference["mode"] != current["mode"] or set(reference["cases"]) != set(current["cases"]):
        raise ValueError("配对模式或用例集合不一致")
    if reference["backend"] != "reference" or current["backend"] != "rust":
        raise ValueError("配对后端不一致")
    if baseline["mode"] != "original" or not set(current["cases"]) <= set(baseline["cases"]):
        raise ValueError("缺少未干预基线")
    cases = {}
    for node, after in current["cases"].items():
        before, control = baseline["cases"][node], reference["cases"][node]
        changed = after["intervened"] or control["intervened"]
        if not changed:
            category = "no_intervention"
        elif control["status"] != "passed":
            category = "reference_no_longer_passes"
        elif after["status"] == "passed":
            category = "paired_pass_diagnostic_only"
        elif after["reason"].startswith("UNIMPLEMENTED:"):
            category = "remaining_unimplemented"
        else:
            category = "other_failure_needs_inspection"
        cases[node] = {"category": category, "before": before["reason"], "reference": control["reason"],
                       "current": after["reason"], "fields": before["fields"],
                       "explicit_null_fields": control["explicit_null_fields"]}
    return {"mode": current["mode"], "formal": False, "categories": dict(Counter(v["category"] for v in cases.values())), "cases": cases}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("baseline", "reference", "current", "output"):
        parser.add_argument("--" + name, required=True, type=Path)
    args = parser.parse_args()
    from run import outside
    args.output = outside(args.output)
    result = compare(*[json.loads(getattr(args, name).read_text()) for name in ("baseline", "reference", "current")])
    with args.output.open("x") as out:
        json.dump(result, out, ensure_ascii=False, indent=2)
        out.write("\n")
    print(json.dumps(result["categories"], ensure_ascii=False))


if __name__ == "__main__":
    main()
