"""BM25（0031）：`bm25_cases` 的每个场景在本实现与 3.x 上各跑一遍，逐项比较；已登记的差异两边分别断言。

成功结果逐项相等且 repr 相同；错误比较 code、旧 error_code 与原文。
"""

import pytest
from pymilvus import DataType, Function, FunctionType, MilvusClient

import bm25_cases as cases
from bm25_cases import hits
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


def term_ids(terms):
    """3.x `analyzer/hash.py` 的 FNV-1a，按词项计数：BM25 输出字段在写入时存的就是它。"""
    out = {}
    for term in terms:
        h = 0x811C9DC5
        for b in term.encode("utf-8"):
            h = ((h ^ b) * 0x01000193) & 0xFFFFFFFF
        out[h] = out.get(h, 0.0) + 1.0
    return out


def jieba_schema(user_words=None):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    tokenizer = {"type": "jieba"} if user_words is None else {"type": "jieba", "dict": user_words}
    s.add_field("text", DataType.VARCHAR, max_length=500, enable_analyzer=True, analyzer_params={"tokenizer": tokenizer})
    s.add_field("sp", DataType.SPARSE_FLOAT_VECTOR)
    s.add_function(Function(name="bm", function_type=FunctionType.BM25, input_field_names=["text"], output_field_names=["sp"]))
    return s


# 不在词典里的新造词、它们的单字（字符串形状逐字符加词）与词典字符集之外的词：每项加一次（本实现）与每次构造
# 都再加一遍（3.x）的切分相同，带用户词的集合两边逐项比较。字符串 "米尔维斯" 逐字符加词与整词加词在 USER_DOCS
# 上的切分不同，所以这一组能区分两种语义。加一次与加多次在高频词上的差异由组件 fixture 覆盖（0036「实施前核实的结果」第 2 项）。
USER_WORDS = {"list": ["米尔维斯", "向量数据库", "米尔维斯"], "string": "米尔维斯", "object": {"云原生向量库": 1, "米尔维斯": None},
              "outside_charset": ["T.S.Eliot", "a.b", " ", "\n", "米尔维斯"]}
USER_DOCS = cases.JIEBA_DOCS + ["我爱米尔维斯", "米尔维斯向量数据库很快", "云原生向量库与 T.S.Eliot 和 a.b"]


def user_words_steps(c, rec):
    rec("query", lambda: c.query("u", "id >= 0", output_fields=["sp"]))
    rec("search", lambda: hits(c.search("u", data=["米尔维斯", "向量数据库", "云原生向量库", "a.b 米尔"], anns_field="sp",
                                        limit=4)))


@pytest.mark.parametrize("shape", sorted(USER_WORDS))
def test_jieba_user_words_are_per_collection(pair, shape):
    """0036 决策 C：本实现每个集合一份用户词，每项按 Python `add_word` 加一次；3.x 加进进程全局的词典，每次构造
    分词器再加一遍。每个 3.x 进程里只有一个带用户词的集合，两边逐项比较，重开后 3.x 在新进程里重新加词，仍相同。
    同一进程里没有用户词的集合，3.x 受前一个集合的用户词影响，两边分别断言。"""
    d = pair.open()
    d.step("create", lambda c: c.create_collection("u", schema=jieba_schema(USER_WORDS[shape])))
    d.step("insert", lambda c: c.insert("u", [{"id": j, "text": t} for j, t in enumerate(USER_DOCS)]))
    compare("user_words", collect(user_words_steps, d.rust), collect(user_words_steps, d.ref))
    if shape == "list":
        d.step("plain/create", lambda c: c.create_collection("plain", schema=jieba_schema()))
        d.step("plain/insert", lambda c: c.insert("plain", [{"id": 0, "text": "我爱米尔维斯"}]))
        for client, tokens in ((d.rust, ["我", "爱", "米尔", "米尔维", "斯"]),
                               (d.ref, ["我", "爱", "米尔", "维斯", "米尔维", "米尔维斯"])):
            assert list(client.query("plain", "id >= 0", output_fields=["sp"])) == [{"id": 0, "sp": term_ids(tokens)}]
    pair.close()
    d = pair.open()
    compare("user_words", collect(user_words_steps, d.rust), collect(user_words_steps, d.ref))
