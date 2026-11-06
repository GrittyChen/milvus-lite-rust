"""0026 第 4 步：rerank_cases.py 的全部场景分别发给本实现与 3.x（43d1257），逐场景比较。

分类即 0026「测试矩阵」（规则同设计阶段的 classify.py）。通过差分的场景比较整条记录：SDK 命中与原始响应
（ids、scores、topks、top_k、group_by_field_value、fields_data、状态），浮点相对 / 绝对容差 1e-5，NaN 视为相等，
组值里的 `-0.0` 与 `0.0` 区分符号；部分命中时排序比较的报错，操作数顺序取决于 Timsort 的比较序列，只比较错误码
与固定的一段文案。登记差异与沿用已有差异的场景两边各自断言，见下面的表。
"""

import math

from pymilvus.grpc_gen import milvus_pb2

import rerank_cases as cases
from test_reopen import pair  # noqa: F401  （fixture）

UNIMPLEMENTED = "StatusCode.UNIMPLEMENTED"
MODEL = {"h model valid", "s model valid", "s l2 model", "fs provider only"}
MODEL_REF = [1, "Cohere API key is required. Pass api_key param or set COHERE_API_KEY environment variable."]
MODEL_RUST = [UNIMPLEMENTED, "MilvusLite does not support rerank model providers: not supported in this build of milvus-lite-rust yet"]
RANDOM = {"s boost random_score", "s boost random_score noseed", "s random_score valid", "route fs boost random"}
RANDOM_RUST = [UNIMPLEMENTED,
               "MilvusLite does not support random_score in boost functions: not supported in this build of milvus-lite-rust yet"]
FILTER_RANGE = {"s boost filter arithmetic", "s boost filter text_match", "dyn boost filter age + 1 > 5",
                "dyn boost filter text_match(name, 'n1')", "nodyn boost filter age + 1 > 5",
                "nodyn boost filter text_match(name, 'n1')"}
FILTER_RUST = [UNIMPLEMENTED,
               "MilvusLite does not support filter expressions: arithmetic, text_match, and geometry predicates are not supported"]
# 0004：本实现按严格的维度校验报错；3.x 的文案来自 numpy，不比较。
DIM = "h bad dim"
DIM_RUST = [1, "query vector has dim 2, but field 'v' expects dim 4"]
# 0024 已登记“不复制”的引擎分组缺陷（主键不叫 id 时按主键分组成单个空值组；字段叫 id / distance 时取错值）。
# 每项是 (3.x, 本实现)，各为 [ids, group_vals, group_valid] 或 {"error": ...}。
BUGS = {
    'pk field distance s boost gs1': ([["k4", "k5", "k6", "k8"], [0, 1, 0, 0], []],
                                      [["k4", "k5", "k6"], [0, 1, 0], [False, True, True]]),
    'pk field distance s boost gs5': ([["k4", "k5", "k7", "k9", "k11", "k1"], [0, 1, 1, 1, 1, 1], []],
                                      [["k4", "k8", "k0", "k5", "k7", "k9"], [0, 0, 0, 1, 1, 1], [False, False, False, True, True, True]]),
    'pk field distance s plain gs1': ([["k0", "k1", "k4", "k8"], [0, 1, 0, 0], []],
                                      [["k0", "k1", "k2"], [0, 1, 0], [False, True, True]]),
    'pk field distance s plain gs5': ([["k0", "k2", "k6", "k10", "k1", "k3", "k5", "k7", "k9", "k4", "k8"], [0, 0, 0, 0, 1, 1, 1, 1, 1, 0, 0], []],
                                      [["k0", "k4", "k8", "k1", "k3", "k5", "k7", "k9", "k2", "k6", "k10"], [0, 0, 0, 1, 1, 1, 1, 1, 0, 0, 0], [False, False, False, True, True, True, True, True, True, True, True]]),
    'pk field id s boost gs1': ({"error": [1, "invalid literal for int() with base 10: 'k6'"]},
                                [["k4", "k5", "k6"], [0, 1, 0], [True, True, False]]),
    'pk field id s boost gs5': ([["k4", "k8", "k10", "k2", "k5", "k7"], [0, 0, 0, 0, 1, 1], []],
                                [["k4", "k8", "k10", "k2", "k5", "k7"], [0, 0, 0, 0, 1, 1], []]),
    'pk field id s plain gs1': ({"error": [1, "invalid literal for int() with base 10: 'k0'"]},
                                [["k0", "k1", "k2"], [0, 1, 0], [False, True, True]]),
    'pk field id s plain gs5': ({"error": [1, "invalid literal for int() with base 10: 'k0'"]},
                                [["k0", "k3", "k6", "k9", "k1", "k5", "k7", "k11", "k2", "k4", "k8", "k10"], [0, 0, 0, 0, 1, 1, 1, 1, 0, 0, 0, 0], [False, False, False, False, True, True, True, True, True, True, True, True]]),
    'pk rr_pk s boost group pk': ([["k4", "k5"], ["", ""], [False, False]],
                                  [["k4", "k5", "k6", "k7"], ["k4", "k5", "k6", "k7"], []]),
    'pk rr_pk s plain group pk': ([["k0", "k1"], ["", ""], [False, False]],
                                  [["k0", "k1", "k2", "k3"], ["k0", "k1", "k2", "k3"], []]),
    'pk rr_pki s boost group pk': ([[4, 5], [0, 0], [False, False]],
                                   [[4, 5, 6, 7], [4, 5, 6, 7], []]),
    'pk rr_pki s plain group pk': ([[0, 1], [0, 0], [False, False]],
                                   [[0, 1, 2, 3], [0, 1, 2, 3], []]),
    'sf $distance pk s boost group gs1': ([[2], [0], [False]],
                                          [[2, 3, 4, 5, 6, 7, 8, 9, 0, 1], [2, 3, 4, 5, 6, 7, 8, 9, 0, 1], []]),
    'sf $distance pk s boost group gs10': ([[2, 3, 4, 5, 6, 7, 8, 9, 0, 1], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [False, False, False, False, False, False, False, False, False, False]],
                                           [[2, 3, 4, 5, 6, 7, 8, 9, 0, 1], [2, 3, 4, 5, 6, 7, 8, 9, 0, 1], []]),
    'sf $distance pk s plain group gs1': ([[0], [0], [False]],
                                          [[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], [0, 1, 2, 3, 4, 5, 6, 7, 8, 9], []]),
    'sf $distance pk s plain group gs10': ([[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [False, False, False, False, False, False, False, False, False, False]],
                                           [[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], [0, 1, 2, 3, 4, 5, 6, 7, 8, 9], []]),
    'sf $group_score pk s boost group gs1': ([[2], [0], [False]],
                                             [[2, 3, 4, 5, 6, 7, 8, 9, 0, 1], [2, 3, 4, 5, 6, 7, 8, 9, 0, 1], []]),
    'sf $group_score pk s boost group gs10': ([[2, 3, 4, 5, 6, 7, 8, 9, 0, 1], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [False, False, False, False, False, False, False, False, False, False]],
                                              [[2, 3, 4, 5, 6, 7, 8, 9, 0, 1], [2, 3, 4, 5, 6, 7, 8, 9, 0, 1], []]),
    'sf $group_score pk s plain group gs1': ([[0], [0], [False]],
                                             [[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], [0, 1, 2, 3, 4, 5, 6, 7, 8, 9], []]),
    'sf $group_score pk s plain group gs10': ([[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [False, False, False, False, False, False, False, False, False, False]],
                                              [[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], [0, 1, 2, 3, 4, 5, 6, 7, 8, 9], []]),
    'sf $id pk s boost group gs1': ([[2], [0], [False]],
                                    [[2, 3, 4, 5, 6, 7, 8, 9, 0, 1], [2, 3, 4, 5, 6, 7, 8, 9, 0, 1], []]),
    'sf $id pk s boost group gs10': ([[2, 3, 4, 5, 6, 7, 8, 9, 0, 1], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [False, False, False, False, False, False, False, False, False, False]],
                                     [[2, 3, 4, 5, 6, 7, 8, 9, 0, 1], [2, 3, 4, 5, 6, 7, 8, 9, 0, 1], []]),
    'sf $id pk s plain group gs1': ([[0], [0], [False]],
                                    [[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], [0, 1, 2, 3, 4, 5, 6, 7, 8, 9], []]),
    'sf $id pk s plain group gs10': ([[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [False, False, False, False, False, False, False, False, False, False]],
                                     [[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], [0, 1, 2, 3, 4, 5, 6, 7, 8, 9], []]),
    'sf $score pk s boost group gs1': ([[2], [0], [False]],
                                       [[2, 3, 4, 5, 6, 7, 8, 9, 0, 1], [2, 3, 4, 5, 6, 7, 8, 9, 0, 1], []]),
    'sf $score pk s boost group gs10': ([[2, 3, 4, 5, 6, 7, 8, 9, 0, 1], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [False, False, False, False, False, False, False, False, False, False]],
                                        [[2, 3, 4, 5, 6, 7, 8, 9, 0, 1], [2, 3, 4, 5, 6, 7, 8, 9, 0, 1], []]),
    'sf $score pk s plain group gs1': ([[0], [0], [False]],
                                       [[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], [0, 1, 2, 3, 4, 5, 6, 7, 8, 9], []]),
    'sf $score pk s plain group gs10': ([[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [False, False, False, False, False, False, False, False, False, False]],
                                        [[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], [0, 1, 2, 3, 4, 5, 6, 7, 8, 9], []]),
    'sf _decay_score pk s boost group gs1': ([[2], [0], [False]],
                                             [[2, 3, 4, 5, 6, 7, 8, 9, 0, 1], [2, 3, 4, 5, 6, 7, 8, 9, 0, 1], []]),
    'sf _decay_score pk s boost group gs10': ([[2, 3, 4, 5, 6, 7, 8, 9, 0, 1], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [False, False, False, False, False, False, False, False, False, False]],
                                              [[2, 3, 4, 5, 6, 7, 8, 9, 0, 1], [2, 3, 4, 5, 6, 7, 8, 9, 0, 1], []]),
    'sf _decay_score pk s plain group gs1': ([[0], [0], [False]],
                                             [[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], [0, 1, 2, 3, 4, 5, 6, 7, 8, 9], []]),
    'sf _decay_score pk s plain group gs10': ([[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [False, False, False, False, False, False, False, False, False, False]],
                                              [[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], [0, 1, 2, 3, 4, 5, 6, 7, 8, 9], []]),
    'st $distance pkV s boost group gs1': ([["k2"], [""], [False]],
                                           [["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], ["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], []]),
    'st $distance pkV s boost group gs10': ([["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], ["", "", "", "", "", "", "", "", "", ""], [False, False, False, False, False, False, False, False, False, False]],
                                            [["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], ["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], []]),
    'st $group_score pkV s boost group gs1': ([["k2"], [""], [False]],
                                              [["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], ["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], []]),
    'st $group_score pkV s boost group gs10': ([["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], ["", "", "", "", "", "", "", "", "", ""], [False, False, False, False, False, False, False, False, False, False]],
                                               [["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], ["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], []]),
    'st $id pkV s boost group gs1': ([["k2"], [""], [False]],
                                     [["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], ["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], []]),
    'st $id pkV s boost group gs10': ([["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], ["", "", "", "", "", "", "", "", "", ""], [False, False, False, False, False, False, False, False, False, False]],
                                      [["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], ["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], []]),
    'st $score pkV s boost group gs1': ([["k2"], [""], [False]],
                                        [["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], ["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], []]),
    'st $score pkV s boost group gs10': ([["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], ["", "", "", "", "", "", "", "", "", ""], [False, False, False, False, False, False, False, False, False, False]],
                                         [["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], ["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], []]),
    'st _decay_score pkV s boost group gs1': ([["k2"], [""], [False]],
                                              [["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], ["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], []]),
    'st _decay_score pkV s boost group gs10': ([["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], ["", "", "", "", "", "", "", "", "", ""], [False, False, False, False, False, False, False, False, False, False]],
                                               [["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], ["k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k0", "k1"], []]),}
# 请求没到服务端、两边是同一个客户端异常的场景：SDK 的 limit 校验；pymilvus 2.6 的 proto 没有 `function_chains`，
# 改写这个字段的场景在客户端就失败（3.0 线上它们进入服务端，按通过差分比较）。
SDK_ONLY = {"h rrf limit 0", "h rrf limit -1", "s boost limit 0"}
CHAINS = {"h function_chains rejected", "h function_chains + bad strategy", "h function_chains + missing collection",
          "s boost ranker + function_chains"}
if "function_chains" not in milvus_pb2.SearchRequest.DESCRIPTOR.fields_by_name:
    SDK_ONLY |= CHAINS
ORDER_ERROR = "not supported between instances of"


def same(a, b, signed=False):
    if isinstance(a, float) or isinstance(b, float):
        if isinstance(a, bool) or isinstance(b, bool) or not isinstance(a, (int, float)) or not isinstance(b, (int, float)):
            return False
        if math.isnan(a) or math.isnan(b):
            return math.isnan(a) and math.isnan(b)
        if signed and a == 0 and b == 0:
            return math.copysign(1, a) == math.copysign(1, b)
        return math.isclose(a, b, rel_tol=1e-5, abs_tol=1e-5)
    if isinstance(a, (list, tuple)) and isinstance(b, (list, tuple)):
        return len(a) == len(b) and all(same(x, y, signed) for x, y in zip(a, b))
    if isinstance(a, dict) and isinstance(b, dict):
        return a.keys() == b.keys() and all(same(a[k], b[k], signed or k == "group_vals") for k in a)
    return type(a) is type(b) and a == b


def normalized(outcome):
    """错误与原始响应状态里的排序报错都只留错误码与固定的一段文案。"""
    if "error" not in outcome or ORDER_ERROR not in str(outcome["error"][1]):
        return outcome
    raw = [dict(r, status=[r["status"][0], ORDER_ERROR]) if ORDER_ERROR in r["status"][1] else r
           for r in outcome.get("raw", [])]
    return dict(outcome, error=[outcome["error"][0], ORDER_ERROR], raw=raw)


def brief(outcome):
    if "error" in outcome:
        return {"error": outcome["error"]}
    raw = outcome["raw"][-1]
    return [raw["ids"], raw["group_vals"], raw["group_valid"]]


def category(name, outcome):
    if name in MODEL:
        return "model"
    if name in RANDOM:
        return "random_score"
    if name in FILTER_RANGE:
        return "filter"
    if name == DIM:
        return "0004"
    if name in BUGS:
        return "0024"
    if name.startswith("sf create") or name == "cf dynamic insert":
        return "setup"
    if not outcome["raw"]:
        return "sdk"
    return "pass"


# 原始响应里携带结果的键（`collect` 的记录格式）。
DATA = ("ids", "scores", "topks", "fields_data", "group_vals", "group_valid")


def failed(outcome, expected):
    """只有错误：没有命中；记下的原始响应（服务端以状态码报错时有一条）都是错误状态、不带数据。"""
    return (outcome.get("error") == expected and "hits" not in outcome
            and all(r["status"][0] != 0 and not any(r[k] for k in DATA) for r in outcome.get("raw", [])))


def check(name, rust, ref, kind):
    if kind == "model":
        ok = failed(ref, MODEL_REF) and failed(rust, MODEL_RUST)
    elif kind == "random_score":
        ok = "error" not in ref and failed(rust, RANDOM_RUST)
    elif kind == "filter":
        ok = "error" not in ref and failed(rust, FILTER_RUST)
    elif kind == "0004":
        ok = ref.get("error", [None])[0] == 1 and failed(ref, ref["error"]) and failed(rust, DIM_RUST)
    elif kind == "0024":
        ok = same([brief(ref), brief(rust)], list(BUGS[name]), signed=True)
    else:
        ok = same(normalized(rust), normalized(ref))
    return ok


def compare(rust_results, ref_results, expected, classify=category):
    assert rust_results.keys() == ref_results.keys()
    counts, failures, sdk = {}, [], set()
    for name in ref_results:
        kind = classify(name, ref_results[name])
        ok = check(name, rust_results[name], ref_results[name], kind)
        counts[kind] = counts.get(kind, 0) + 1
        if kind == "sdk":
            sdk.add(name)
        if not ok:
            failures.append(f"{name} [{kind}]\n rust {str(rust_results[name])[:600]}\n ref  {str(ref_results[name])[:600]}")
    assert not failures, f"{len(failures)} / {len(ref_results)}:\n" + "\n".join(failures[:20])
    assert counts == expected
    return sdk


def test_rerank_probe_scenarios_match_3x(pair, monkeypatch):
    # model 场景的 3.x 一侧依赖“缺 key”的报错；有 key 时会调用付费的外部服务。3.x 服务进程在 open() 时继承环境。
    monkeypatch.delenv("COHERE_API_KEY", raising=False)
    d = pair.open()
    sdk = compare(cases.collect(d.rust), cases.collect(d.ref),
                  {"pass": 2752 - len(SDK_ONLY), "setup": 13, "sdk": len(SDK_ONLY), "model": 4, "random_score": 4,
                   "filter": 6, "0004": 1, "0024": 42})
    assert sdk == SDK_ONLY


def test_engine_group_same_named_field_types_match_3x(pair):
    """Search + boost 的引擎分组：字段名为虚拟列名时各类型的组键与组值，以及 VARCHAR 数字字符串的过滤。"""
    d = pair.open()
    compare(cases.collect_s4(d.rust), cases.collect_s4(d.ref), {"pass": 164},
            classify=lambda name, outcome: "pass")
