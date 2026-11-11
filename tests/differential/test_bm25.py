"""BM25（0031）：`bm25_cases` 的每个场景在本实现与 3.x 上各跑一遍，逐项比较；已登记的差异两边分别断言。

成功结果逐项相等且 repr 相同；错误比较 code、旧 error_code 与原文。
"""

import pytest

import bm25_cases as cases
from test_reopen import pair  # noqa: F401  （fixture）
from test_sparse_vectors import collect


def compare(scenario, rust, ref):
    assert rust.keys() == ref.keys()
    registered = {step for name, step in cases.REGISTERED if name == scenario}
    assert registered <= rust.keys(), f"registered steps not exercised: {registered - rust.keys()}"
    failures = []
    for step in rust:
        if step in registered:
            expected = cases.REGISTERED[(scenario, step)]
            for side, got in (("rust", rust[step]), ("ref", ref[step])):
                if repr(got) != repr(expected[side]):
                    failures.append(f"{step} [{side}, registered]: {got!r} != {expected[side]!r}")
        elif repr(rust[step]) != repr(ref[step]):
            failures.append(f"{step}:\n  rust: {rust[step]!r}\n  3.x:  {ref[step]!r}")
    assert not failures, "\n".join(failures)


@pytest.mark.parametrize("name", sorted(cases.SCENARIOS))
def test_bm25_scenarios_match_3x(diff, name):
    scenario = cases.SCENARIOS[name]
    compare(name, collect(scenario, diff.rust), collect(scenario, diff.ref))


def test_functions_and_generated_rows_survive_reopen_like_3x(pair):
    d = pair.open()
    compare("before_reopen", collect(cases.before_reopen, d.rust), collect(cases.before_reopen, d.ref))
    pair.close()
    d = pair.open()
    compare("reopened", collect(cases.reopened, d.rust), collect(cases.reopened, d.ref))
