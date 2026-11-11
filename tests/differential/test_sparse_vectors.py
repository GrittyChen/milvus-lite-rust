"""稀疏向量（0030）：`sparse_cases` 的每个场景在本实现与 3.x 上各跑一遍，逐项比较；已登记的差异两边分别断言。

成功结果逐项相等且 repr 相同（分数不取整，排序与 float32 位模式都要一致）；错误比较 code、旧 error_code 与原文。
"""

import math

import grpc
import pytest
from pymilvus.exceptions import MilvusException

import sparse_cases as cases
from compare import plain
from test_reopen import pair  # noqa: F401  （fixture）


def outcome(fn):
    try:
        return ("ok", plain(fn()))
    except MilvusException as e:
        return ("err", e.code, e.compatible_code, e.message)
    except grpc.RpcError as e:
        return ("rpc", e.code().name, e.details())


def collect(scenario, client):
    out = {}

    def rec(name, fn):
        assert name not in out, name
        out[name] = outcome(fn)
    scenario(client, rec)
    return out


def close(a, b):
    """0004「决策」的距离误差（相对 1e-5、绝对 1e-6）；非浮点逐字比较。"""
    if isinstance(a, float) and isinstance(b, float):
        return math.isclose(a, b, rel_tol=1e-5, abs_tol=1e-6)
    if isinstance(a, (list, tuple)) and isinstance(b, (list, tuple)):
        return type(a) is type(b) and len(a) == len(b) and all(close(x, y) for x, y in zip(a, b))
    if isinstance(a, dict) and isinstance(b, dict):
        return list(a) == list(b) and all(close(a[k], b[k]) for k in a)
    return repr(a) == repr(b)


def compare(scenario, rust, ref):
    assert rust.keys() == ref.keys()
    registered = {step for name, step in cases.REGISTERED if name == scenario}
    assert registered <= rust.keys(), f"registered steps not exercised: {registered - rust.keys()}"
    failures = []
    for step in rust:
        if step in registered:
            expected = cases.REGISTERED[(scenario, step)]
            for side, got in (("rust", rust[step]), ("ref", ref[step])):
                want = ("ok", plain(expected[side][1])) if expected[side][0] == "ok" else expected[side]
                if repr(got) != repr(want):
                    failures.append(f"{step} [{side}, registered]: {got!r} != {want!r}")
        elif cases.dense(step):
            if not close(rust[step], ref[step]):
                failures.append(f"{step} (dense tolerance):\n  rust: {rust[step]!r}\n  3.x:  {ref[step]!r}")
        elif repr(rust[step]) != repr(ref[step]):
            failures.append(f"{step}:\n  rust: {rust[step]!r}\n  3.x:  {ref[step]!r}")
    assert not failures, "\n".join(failures)


@pytest.mark.parametrize("name", sorted(cases.SCENARIOS))
def test_sparse_scenarios_match_3x(diff, name):
    scenario = cases.SCENARIOS[name]
    compare(name, collect(scenario, diff.rust), collect(scenario, diff.ref))


def test_rows_and_indexes_survive_reopen_like_3x(pair):
    d = pair.open()
    compare("writes_reads_indexes_and_search",
            collect(cases.writes_reads_indexes_and_search, d.rust),
            collect(cases.writes_reads_indexes_and_search, d.ref))
    pair.close()
    d = pair.open()
    compare("reopened", collect(cases.reopened, d.rust), collect(cases.reopened, d.ref))
