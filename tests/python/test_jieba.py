"""jieba 分词器（0036）：经 pymilvus 建表、写入与检索，固定用户可见的结果。

前半部分改写自参照实现的 jieba 用例。那些用例直接导入 3.x 的内部模块（INVENTORY.md 中「依赖内部模块」一类），
这里把断言改为经 gRPC 观察：BM25 输出字段存的是词项的 FNV-1a 计数，检查某个词项在或不在其中。
- 参照实现 tests/analyzer/test_analyzer.py（Apache-2.0）：`TestJiebaAnalyzer` 与 `TestFactory` 的 jieba 用例；
- 参照实现 tests/adapter/test_issue_19_20.py（Apache-2.0）：`TestJiebaAnalyzerKey`。

后半部分是本实现登记的差异（0036「与 3.x 的差异」）：用户词的两个上限，3.x 没有，不能做差分。
与 3.x 的逐项对照在 tests/differential/test_bm25.py。
"""

import json

import pytest
from milvus_lite.server_manager import ServerManager
from pymilvus import DataType, Function, FunctionType, MilvusClient
from pymilvus.exceptions import MilvusException


def term_id(term):
    h = 0x811C9DC5
    for b in term.encode("utf-8"):
        h = ((h ^ b) * 0x01000193) & 0xFFFFFFFF
    return h


@pytest.fixture
def client(tmp_path):
    manager = ServerManager()
    path = str(tmp_path / "j.db")
    c = MilvusClient(uri=manager.start_and_get_uri(path))
    try:
        yield c
    finally:
        c.close()
        manager.release_server(path)


def schema(analyzer_params):
    s = MilvusClient.create_schema(auto_id=False)
    s.add_field("id", DataType.INT64, is_primary=True)
    s.add_field("text", DataType.VARCHAR, max_length=500, enable_analyzer=True, analyzer_params=analyzer_params)
    s.add_field("sp", DataType.SPARSE_FLOAT_VECTOR)
    s.add_function(Function(name="bm", function_type=FunctionType.BM25, input_field_names=["text"], output_field_names=["sp"]))
    return s


def terms(client, analyzer_params, text):
    """建一个集合、写入一行，返回这一行的词项 ID 集合。"""
    name = f"c{len(client.list_collections())}"
    client.create_collection(name, schema=schema(analyzer_params))
    client.insert(name, [{"id": 1, "text": text}])
    [row] = client.query(name, "id == 1", output_fields=["sp"])
    return set(row["sp"])


def test_reference_jieba_assertions_through_grpc(client):
    # TestJiebaAnalyzer::test_basic_chinese：exact 模式切出“学生”。
    assert term_id("学生") in terms(client, {"tokenizer": {"type": "jieba", "mode": "exact"}}, "我是一个学生")
    # test_search_mode：search 模式至少一个词项；比 exact 细，含“中华”。
    assert term_id("中华") in terms(client, {"tokenizer": {"type": "jieba", "mode": "search"}}, "中华人民共和国")
    # test_stop_words：停用词不出现。
    got = terms(client, {"tokenizer": "jieba", "filter": [{"type": "stop", "stop_words": ["的", "是", "了"]}]},
                "今天的天气是很好的")
    assert got and not got & {term_id("的"), term_id("是")}
    # test_analyze_returns_ids：词项 ID 是 uint32。
    got = terms(client, {"tokenizer": "jieba"}, "机器学习算法")
    assert got and all(0 <= i < 2**32 for i in got)
    # test_user_dict：用户词作为整体出现。
    assert term_id("深度学习") in terms(client, {"tokenizer": {"type": "jieba", "dict": ["深度学习"]}},
                                     "深度学习是人工智能的子领域")
    # test_english_text：英文照常切出词项，转为小写。
    assert term_id("machine") in terms(client, {"tokenizer": "jieba"}, "Machine learning")
    # TestFactory::test_jieba_with_dict_and_stop_words。
    got = terms(client, {"tokenizer": {"type": "jieba", "mode": "search", "dict": ["自定义词"]},
                         "filter": [{"type": "stop", "stop_words": ["的"]}]}, "这是自定义词的测试")
    assert term_id("自定义词") in got and term_id("的") not in got
    # TestJiebaAnalyzerKey 与 TestFactory::test_jieba_string / test_jieba_dict_form：三种写法都选中 jieba。
    # standard 把整段汉字当作一个 `\w` 词项，jieba 把它切开。
    for params in ({"type": "jieba"}, {"tokenizer": "jieba"}, {"tokenizer": {"type": "jieba", "mode": "exact"}}):
        assert term_id("学生") in terms(client, params, "我是一个学生")
    assert terms(client, {"type": "standard"}, "我是一个学生") == {term_id("我是一个学生")}


def test_jieba_collection_search_and_reopen(tmp_path):
    manager = ServerManager()
    path = str(tmp_path / "r.db")
    c = MilvusClient(uri=manager.start_and_get_uri(path))
    c.create_collection("c", schema=schema({"tokenizer": {"type": "jieba", "dict": ["米尔维斯"]}}))
    c.insert("c", [{"id": 1, "text": "米尔维斯是向量数据库"}, {"id": 2, "text": "今天天气很好"}])
    c.flush("c")
    c.insert("c", [{"id": 3, "text": "米尔维斯和米尔"}])
    hits = lambda: [(h["id"], h["distance"]) for h in c.search("c", data=["米尔维斯"], anns_field="sp", limit=5)[0]]  # noqa: E731
    stored = lambda: {r["id"]: set(r["sp"]) for r in c.query("c", "id >= 0", output_fields=["sp"])}  # noqa: E731
    before = hits()
    assert sorted(i for i, _ in before) == [1, 3]
    # 用户词整体成词：没有用户词时“米尔维斯”切成“米尔 / 米尔维 / 斯”。
    assert term_id("米尔维斯") in stored()[1] and term_id("米尔维斯") in stored()[3]
    c.close()
    manager.release_server(path)
    c = MilvusClient(uri=manager.start_and_get_uri(path))
    try:
        assert hits() == before
        assert term_id("米尔维斯") in stored()[1]
    finally:
        c.close()
        manager.release_server(path)


def create_error(client, words):
    with pytest.raises(MilvusException) as e:
        client.create_collection("big", schema=schema(json.dumps({"tokenizer": {"type": "jieba", "dict": words}})))
    assert not client.has_collection("big")
    return e.value


def test_user_dictionary_limits_report_code_1(client):
    # 会进入 trie 的用户词码位总数上限 100 万（0036）：250001 个互不相同的四字词（前两个字按序号编码），
    # 共 1000004 个码位。
    words = [chr(0x4E00 + i % 20902) + chr(0x4E00 + i // 20902) + "米尔" for i in range(250001)]
    assert len(set(words)) == len(words)
    e = create_error(client, words)
    assert e.code == 1
    assert e.message == ("the jieba user dictionary is too large: words that can be matched exceed 1000000 "
                         "characters in total (limit of milvus-lite-rust)")
    # 恰好 100 万个码位可以建表。
    client.create_collection("ok", schema=schema({"tokenizer": {"type": "jieba", "dict": words[:250000]}}))
    # 码位按列表的每一项累加，重复项与默认词典里已有的词也计入（3.x 没有这个上限，登记为差异）。
    e = create_error(client, ["米尔维斯"] * 250001)
    assert e.code == 1 and "exceed 1000000 characters" in e.message
    # 空串让 `total` 约翻倍，几十个就超出 u64。
    e = create_error(client, [""] * 64)
    assert e.code == 1
    assert e.message.startswith("the jieba user dictionary is too large: ")
    assert e.message.endswith(" exceeds 18446744073709551615 (limit of milvus-lite-rust)")
