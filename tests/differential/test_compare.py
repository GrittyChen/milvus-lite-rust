"""比较器自身的用例：构造两边的结果，确认 0004 的比较口径既不漏报也不误报。"""

import numpy as np
import pytest

from compare import Diff, exact, hits


def result(*queries, name="id"):
    """每个查询是 (主键, 距离) 的列表，entity 只含主键。"""
    return ("ok", [[{name: pk, "distance": d, "entity": {name: pk}} for pk, d in q] for q in queries])


def check(metric, a, b, q=((1.0, 0.0),), vectors=None, k=None, **kw):
    """k 默认取 3.x 一侧的结果数。"""
    vectors = vectors or {pk: (1.0, 0.0) for pk in range(10)}
    if k is None:
        k = len(b[1][0]) if b[0] == "ok" else 0
    return hits(metric, [list(x) for x in q], vectors, k=k, **kw)(a, b)


def test_identical_and_within_tolerance():
    b = result([(1, 0.5), (2, 0.8)])
    assert check("L2", b, b) is None
    assert check("L2", result([(1, 0.5 + 4e-6), (2, 0.8)]), b) is None
    assert check("L2", result([(1, 0.5 + 6e-6), (2, 0.8)]), b) is not None


def test_ip_tolerance_scales_with_norms_not_with_the_distance():
    # 正负项相消：距离接近 0，而 ‖q‖·‖v‖ = 10，允许误差是 1e-4。
    vectors = {1: (10.0, 0.0), 2: (0.0, 10.0)}
    b = result([(1, 1e-7), (2, -1e-7)])
    assert check("IP", result([(1, 9e-5), (2, -1e-7)]), b, vectors=vectors) is None
    assert check("IP", result([(1, 2e-4), (2, -1e-7)]), b, vectors=vectors) is not None


def test_cosine_tolerance_is_absolute():
    b = result([(1, 1e-3)])
    assert check("COSINE", result([(1, 1e-3 + 9e-6)]), b) is None
    assert check("COSINE", result([(1, 1e-3 + 2e-5)]), b) is not None


def test_order_may_differ_only_within_tolerance():
    b = result([(1, 0.5), (2, 0.5 + 5e-6), (3, 0.9)])
    assert check("L2", result([(2, 0.5), (1, 0.5 + 4e-6), (3, 0.9)]), b) is None
    # 先后相反而超出误差时，必有一边的距离超出误差或本实现的结果没有排好序。
    assert check("L2", result([(2, 0.5), (1, 0.5 + 6e-6), (3, 0.9)]), b) is not None
    assert check("L2", result([(2, 0.5 + 5e-6), (1, 0.5), (3, 0.9)]), b) is not None


def test_one_sided_ids_only_as_ties_at_the_cut():
    b = result([(1, 0.1), (2, 0.5)])
    assert check("L2", result([(1, 0.1), (3, 0.5 + 5e-6)]), b) is None
    assert check("L2", result([(1, 0.1), (3, 0.7)]), b) is not None
    # 尾部并列互换。
    b = result([(1, 1.0), (2, 4.0), (3, 4.0)])
    assert check("L2", result([(1, 1.0), (2, 4.0), (4, 4.0)]), b) is None
    # 丢掉一个并列项、补进另一个不与它并列的结果。
    b = result([(1, 1.0), (2, 1.0), (3, 4.0)])
    assert check("L2", result([(1, 1.0), (3, 4.0), (4, 4.0)]), b) is not None
    # 前端三项并列、尾部与第 5 项并列：丢掉一个前端并列项、补进尾部的并列项。
    b = result([(1, 1.0), (2, 1.0), (3, 1.0), (4, 2.0)])
    assert check("L2", result([(2, 1.0), (3, 1.0), (4, 2.0), (5, 2.0)]), b) is not None


def test_front_is_a_cut_only_with_offset():
    b = result([(2, 1.0), (3, 2.0)])
    a = result([(1, 1.0), (3, 2.0)])
    assert check("L2", a, b, offset=1) is None
    assert check("L2", a, b) is not None


def test_hit_count_entity_and_errors():
    b = result([(1, 0.1), (2, 0.5)])
    assert check("L2", result([(1, 0.1)]), b) is not None
    # 两边一起返回空结果（例如场景写错）时，结果数与声明的不符。
    assert check("L2", result([]), result([]), k=2) is not None
    assert check("L2", result([]), result([]), k=0) is None
    a = ("ok", [[{"id": 1, "distance": 0.1, "entity": {"id": 1, "t": "x"}}, {"id": 2, "distance": 0.5, "entity": {"id": 2}}]])
    assert check("L2", a, b) is not None
    assert check("L2", ("err", 1, 0, "x"), ("err", 1, 0, "x")) is None
    assert check("L2", ("err", 1, 0, "x"), ("err", 1, 0, "y")) is not None
    assert check("L2", ("err", 1, 0, "x"), ("err", 1, 1, "x")) is not None
    assert check("L2", ("err", 1, 0, "x"), b) is not None


def test_types_and_key_order_matter():
    for a, b in [({"a": 1}, {"a": 1.0}), ([True], [1]), ([-0.0], [0.0]), ({"a": 1, "b": 2}, {"b": 2, "a": 1})]:
        assert exact(("ok", a), ("ok", b)) is not None
    a = ("ok", [[{"id": 1, "distance": 0.1, "entity": {"id": 1, "x": 1}}]])
    b = ("ok", [[{"id": 1, "distance": 0.1, "entity": {"id": 1, "x": 1.0}}]])
    assert check("L2", a, b) is not None


def test_primary_key_name_and_type_matter():
    b = result([(1, 0.1)])
    assert check("L2", result([(1, 0.1)], name="pk"), b) is not None
    a = ("ok", [[{"id": True, "distance": 0.1, "entity": {"id": 1}}]])
    assert check("L2", a, b) is not None


def test_unknown_primary_key_is_reported():
    assert "unknown pk" in check("L2", result([(99, 0.1)]), result([(99, 0.1)]))


@pytest.mark.parametrize("metric", ["L2", "IP", "COSINE"])
@pytest.mark.parametrize("bad", [float("nan"), float("inf"), float("-inf")])
def test_non_finite_distances_are_rejected(metric, bad):
    good = [(1, 0.1), (2, 0.5)] if metric == "L2" else [(1, 0.5), (2, 0.1)]
    assert check(metric, result(good), result(good)) is None
    assert check(metric, result([good[0], (2, bad)]), result(good)) is not None
    assert check(metric, result([good[0], (3, bad)]), result(good)) is not None
    assert check(metric, result(good), result([good[0], (3, bad)])) is not None
    assert check(metric, result([good[0], (2, bad)]), result([good[0], (2, bad)])) is not None


def test_own_hits_must_be_sorted():
    b = result([(1, 0.1), (2, 0.5)])
    assert check("L2", result([(3, 0.5), (1, 0.1)]), b) is not None
    assert check("IP", result([(1, 0.1), (2, 0.5)]), result([(2, 0.5), (1, 0.1)])) is not None
    # 含 NaN 的列表与排好序的自己比较时，list 的 == 先比对象身份，会判为相等。
    nan = float("nan")
    assert check("L2", result([(1, nan), (2, 0.5), (3, nan)]), result([(1, 0.1), (2, 0.5), (3, 0.9)])) is not None


def f32(x):
    return float(np.float32(x))


def test_round_decimal_widens_the_tolerance():
    # pymilvus 已在客户端按 round_decimal 舍入，拿到的是十进制小数的 float64 值。
    b = result([(1, 0.12)])
    assert check("L2", result([(1, 0.13)]), b, round_decimal=2) is None
    assert check("L2", result([(1, 0.14)]), b, round_decimal=2) is not None


def test_round_decimal_requires_rounded_values():
    b = result([(1, 0.0), (2, 3.0)])
    assert check("L2", b, b, round_decimal=0) is None
    assert check("L2", result([(1, f32(0.1)), (2, 3.0)]), b, round_decimal=0) is not None
    b = result([(1, 0.12)])
    assert check("L2", b, b, round_decimal=2) is None
    assert check("L2", result([(1, f32(0.123))]), b, round_decimal=2) is not None


def test_step_requires_the_declared_outcome():
    diff = Diff(rust=object(), ref=object())

    def fail(_):
        raise ValueError("boom")

    with pytest.raises(ValueError):
        diff.step("raises", fail)
    assert diff.step("ok", lambda c: 1, exact) == ("ok", 1)
    with pytest.raises(AssertionError, match="expected an error"):
        diff.step("ok_but_error_expected", lambda c: 1, error=True)
