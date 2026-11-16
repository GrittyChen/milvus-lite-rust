# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_function_chain.py（`1320d26` 未改动），Apache-2.0 许可。
# 逐条状态见 INVENTORY.md。0035 起公共 function chain 的用例按「改后复制」纳入（0035「参照用例」）：
# - 链的构造：`FunctionChain` / `FunctionChainStage` / `col` / `fn` 换成 tests/python/chainwire.py 里的同名写法，
#   产出与 pymilvus 3.0 `to_proto()` 相同的线格式字节（pymilvus 2.6 没有这些类）；用例体不变。
# - 链的发送：`milvus_client.search(..., function_chains=chain)` 改为在 `with sending(milvus_client, chain):` 里
#   发出同一个 search；原始请求的 `request.function_chains.append(chain.to_proto())` 改为 `attach(request, chain)`
#   （HybridSearchRequest 用 15 号字段）。断言不变。
# - 依赖参照内部对象的四条，替换点在各自函数里注明：uses_primary_field_name_as_id_alias 去掉对
#   `Collection.search` 入参的观察；两条 before_ann 改为在未加载的 collection 上发同一请求（报 6 而不是 101）；
#   primary_name_conflicting 改为经公开接口建主键名为 `$score` 的 collection。
# - 不复制：test_public_chain_keeps_requested_candidate_budget 与
#   test_parse_search_request_accepts_legacy_shape_without_new_messages（只观察参照内部，见 INVENTORY.md）。
import json
import sys
import uuid
from pathlib import Path

import grpc
import pytest

from pymilvus import DataType, MilvusClient
from pymilvus.client.prepare import Prepare
from pymilvus.grpc_gen import milvus_pb2, milvus_pb2_grpc

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from chainwire import FunctionChain, FunctionChainStage, attach, col, fn, sending  # noqa: E402


def _collection_name(prefix: str) -> str:
    return f"{prefix}_{uuid.uuid4().hex[:10]}"


def _create_collection(client: MilvusClient, name: str, metric: str = "IP"):
    schema = client.create_schema(
        auto_id=False,
        enable_dynamic_field=False,
    )
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    schema.add_field("popularity", DataType.FLOAT)
    schema.add_field("title", DataType.VARCHAR, max_length=128)
    schema.add_field("category", DataType.VARCHAR, max_length=32)

    index_params = client.prepare_index_params()
    index_params.add_index(
        field_name="vector",
        index_type="BRUTE_FORCE",
        metric_type=metric,
        params={},
    )
    client.create_collection(
        name,
        schema=schema,
        index_params=index_params,
    )
    client.insert(
        name,
        [
            {
                "id": 1,
                "vector": [0.8, 0.6],
                "popularity": 1.0,
                "title": "one",
                "category": "a",
            },
            {
                "id": 2,
                "vector": [1.0, 0.0],
                "popularity": 10.0,
                "title": "two",
                "category": "a",
            },
            {
                "id": 3,
                "vector": [0.0, 1.0],
                "popularity": 5.0,
                "title": "three",
                "category": "b",
            },
            {
                "id": 4,
                "vector": [-1.0, 0.0],
                "popularity": 2.0,
                "title": "four",
                "category": "c",
            },
        ],
    )
    client.load_collection(name)


def _create_dynamic_collection(client: MilvusClient, name: str):
    schema = client.create_schema(
        auto_id=False,
        enable_dynamic_field=True,
    )
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    schema.add_field("popularity", DataType.FLOAT)
    schema.add_field("title", DataType.VARCHAR, max_length=128)

    index_params = client.prepare_index_params()
    index_params.add_index(
        field_name="vector",
        index_type="BRUTE_FORCE",
        metric_type="IP",
        params={},
    )
    client.create_collection(
        name,
        schema=schema,
        index_params=index_params,
    )
    client.insert(
        name,
        [
            {
                "id": 1,
                "vector": [0.8, 0.6],
                "popularity": 1.0,
                "title": "one",
                "dynamic_tag": "first",
            },
            {
                "id": 2,
                "vector": [1.0, 0.0],
                "popularity": 10.0,
                "title": "two",
                "dynamic_tag": "second",
            },
        ],
    )
    client.load_collection(name)


def _search_request(
    name: str,
    *,
    metric: str = "IP",
    limit: int = 3,
    data=None,
    output_fields=None,
    **kwargs,
):
    return Prepare.search_requests_with_expr(
        collection_name=name,
        anns_field="vector",
        param={"metric_type": metric, "params": {}},
        limit=limit,
        data=data or [[1.0, 0.0]],
        output_fields=output_fields,
        **kwargs,
    )


def _raw_stub(grpc_server):
    port, _db = grpc_server
    channel = grpc.insecure_channel(f"127.0.0.1:{port}")
    return channel, milvus_pb2_grpc.MilvusServiceStub(channel)


def _score_plus_popularity_chain(*, sort: bool = True):
    chain = FunctionChain(FunctionChainStage.L2_RERANK).map(
        "tmp_score",
        fn.num_combine(col("$score"), col("popularity"), mode="sum"),
    ).map(
        "$score",
        fn.round_decimal(col("tmp_score"), decimal=2),
    )
    if sort:
        chain.sort(col("$score"), desc=True, tie_break_col=col("$id"))
    return chain


def _dynamic_field_collision_chain():
    return (
        FunctionChain(FunctionChainStage.L2_RERANK)
        .map(
            "dynamic_tag",
            fn.num_combine(col("$score"), col("popularity"), mode="sum"),
        )
        .map(
            "$score",
            fn.round_decimal(col("dynamic_tag"), decimal=2),
        )
        .sort(col("$score"), desc=True, tie_break_col=col("$id"))
    )


def _primary_key_score_chain():
    return (
        FunctionChain(FunctionChainStage.L2_RERANK)
        .map(
            "$score",
            fn.num_combine(col("$score"), col("id"), mode="sum"),
        )
        .sort(col("$score"), desc=True, tie_break_col=col("$id"))
    )


def test_search_function_chain_reranks_by_hidden_field_and_serializes_score(
    milvus_client,
):
    name = _collection_name("function_chain_hidden")
    _create_collection(milvus_client, name)

    with sending(milvus_client, _score_plus_popularity_chain()):
        result = milvus_client.search(
            collection_name=name,
            data=[[1.0, 0.0]],
            anns_field="vector",
            search_params={"metric_type": "IP", "params": {}},
            limit=3,
            output_fields=["title"],
        )

    assert [hit["id"] for hit in result[0]] == [2, 3, 1]
    assert [hit["distance"] for hit in result[0]] == pytest.approx(
        [11.0, 5.0, 1.8]
    )
    for hit in result[0]:
        assert hit["entity"]["title"] in {"one", "two", "three"}
        assert "popularity" not in hit["entity"]
        assert "tmp_score" not in hit["entity"]


def test_search_function_chain_preserves_explicit_dynamic_output_field(
    milvus_client,
):
    name = _collection_name("function_chain_dynamic_explicit")
    _create_dynamic_collection(milvus_client, name)

    with sending(milvus_client, _score_plus_popularity_chain()):
        result = milvus_client.search(
            collection_name=name,
            data=[[1.0, 0.0]],
            anns_field="vector",
            search_params={"metric_type": "IP", "params": {}},
            limit=2,
            output_fields=["dynamic_tag"],
        )

    assert [hit["entity"]["dynamic_tag"] for hit in result[0]] == [
        "second",
        "first",
    ]
    assert all("tmp_score" not in hit["entity"] for hit in result[0])


def test_search_function_chain_preserves_default_dynamic_output_fields(
    milvus_client,
):
    name = _collection_name("function_chain_dynamic_default")
    _create_dynamic_collection(milvus_client, name)

    with sending(milvus_client, _score_plus_popularity_chain()):
        result = milvus_client.search(
            collection_name=name,
            data=[[1.0, 0.0]],
            anns_field="vector",
            search_params={"metric_type": "IP", "params": {}},
            limit=2,
        )

    assert [hit["entity"]["dynamic_tag"] for hit in result[0]] == [
        "second",
        "first",
    ]
    assert all("tmp_score" not in hit["entity"] for hit in result[0])


def test_search_function_chain_preserves_explicit_dynamic_field_on_temp_collision(
    milvus_client,
):
    name = _collection_name("function_chain_dynamic_collision_explicit")
    _create_dynamic_collection(milvus_client, name)

    with sending(milvus_client, _dynamic_field_collision_chain()):
        result = milvus_client.search(
            collection_name=name,
            data=[[1.0, 0.0]],
            anns_field="vector",
            search_params={"metric_type": "IP", "params": {}},
            limit=2,
            output_fields=["dynamic_tag"],
        )

    assert [hit["entity"]["dynamic_tag"] for hit in result[0]] == [
        "second",
        "first",
    ]


def test_search_function_chain_preserves_default_dynamic_field_on_temp_collision(
    milvus_client,
):
    name = _collection_name("function_chain_dynamic_collision_default")
    _create_dynamic_collection(milvus_client, name)

    with sending(milvus_client, _dynamic_field_collision_chain()):
        result = milvus_client.search(
            collection_name=name,
            data=[[1.0, 0.0]],
            anns_field="vector",
            search_params={"metric_type": "IP", "params": {}},
            limit=2,
        )

    assert [hit["entity"]["dynamic_tag"] for hit in result[0]] == [
        "second",
        "first",
    ]


def test_search_function_chain_uses_primary_field_name_as_id_alias(
    milvus_client,
    grpc_server,
):
    # 替换点：参照用 monkeypatch 记录 `Collection.search` 收到的 output_fields（断言为 ["title"]），
    # 本实现没有对应对象；保留结果、分数与响应 output_fields == ["title"] 的断言。
    name = _collection_name("function_chain_primary_alias")
    _create_collection(milvus_client, name)

    with sending(milvus_client, _primary_key_score_chain()):
        result = milvus_client.search(
            collection_name=name,
            data=[[1.0, 0.0]],
            anns_field="vector",
            search_params={"metric_type": "IP", "params": {}},
            limit=3,
            output_fields=["title"],
        )

    assert [hit["id"] for hit in result[0]] == [2, 3, 1]
    assert [hit["distance"] for hit in result[0]] == pytest.approx(
        [3.0, 3.0, 1.8]
    )

    request = _search_request(name, output_fields=["title"])
    attach(request, _primary_key_score_chain())
    channel, stub = _raw_stub(grpc_server)
    try:
        response = stub.Search(request)
    finally:
        channel.close()

    assert response.status.code == 0
    assert list(response.results.output_fields) == ["title"]


def test_search_function_chain_limit_changes_each_query_chunk(milvus_client):
    name = _collection_name("function_chain_limit")
    _create_collection(milvus_client, name)
    chain = _score_plus_popularity_chain().limit(2)

    with sending(milvus_client, chain):
        result = milvus_client.search(
            collection_name=name,
            data=[[1.0, 0.0], [0.0, 1.0]],
            anns_field="vector",
            search_params={"metric_type": "IP", "params": {}},
            limit=3,
        )

    assert len(result) == 2
    assert [len(chunk) for chunk in result] == [2, 2]
    assert [hit["id"] for hit in result[0]] == [2, 3]
    assert [hit["id"] for hit in result[1]] == [2, 3]
    assert {"vector", "popularity", "title", "category"}.issubset(
        result[0][0]["entity"]
    )
    assert "tmp_score" not in result[0][0]["entity"]


def test_search_function_chain_keeps_query_chunks_independent(milvus_client):
    name = _collection_name("function_chain_queries")
    _create_collection(milvus_client, name)
    chain = (
        FunctionChain(FunctionChainStage.L2_RERANK)
        .map("$score", fn.round_decimal(col("$score"), decimal=2))
        .sort(col("$score"), desc=True)
        .limit(2)
    )

    with sending(milvus_client, chain):
        result = milvus_client.search(
            collection_name=name,
            data=[[1.0, 0.0], [0.0, 1.0]],
            anns_field="vector",
            search_params={"metric_type": "IP", "params": {}},
            limit=3,
        )

    assert [[hit["id"] for hit in chunk] for chunk in result] == [
        [2, 1],
        [3, 1],
    ]


def test_search_function_chain_without_sort_preserves_ann_order(milvus_client):
    name = _collection_name("function_chain_no_sort")
    _create_collection(milvus_client, name)

    with sending(milvus_client, _score_plus_popularity_chain(sort=False)):
        result = milvus_client.search(
            collection_name=name,
            data=[[1.0, 0.0]],
            anns_field="vector",
            search_params={"metric_type": "IP", "params": {}},
            limit=3,
        )

    assert [hit["id"] for hit in result[0]] == [2, 1, 3]
    assert [hit["distance"] for hit in result[0]] == pytest.approx(
        [11.0, 1.8, 5.0]
    )


def test_search_function_chain_explicit_id_tie_break(milvus_client):
    name = _collection_name("function_chain_tie")
    _create_collection(milvus_client, name)
    chain = (
        FunctionChain(FunctionChainStage.L2_RERANK)
        .map("$score", fn.round_decimal(col("$score"), decimal=0))
        .sort(col("$score"), desc=True, tie_break_col=col("$id"))
    )

    with sending(milvus_client, chain):
        result = milvus_client.search(
            collection_name=name,
            data=[[1.0, 0.0]],
            anns_field="vector",
            search_params={"metric_type": "IP", "params": {}},
            limit=3,
        )

    assert [hit["id"] for hit in result[0]] == [1, 2, 3]
    assert [hit["distance"] for hit in result[0]] == [1.0, 1.0, 0.0]


@pytest.mark.parametrize(
    ("metric", "expected_ids", "expected_scores"),
    [
        ("IP", [2, 1, 3], [2.0, 0.9, 0.5]),
        ("COSINE", [2, 1, 3], [2.0, 0.9, 0.5]),
        ("L2", [2, 1, 3], [1.0, 0.5, 2.5]),
    ],
)
def test_search_function_chain_rewrites_metric_scores(
    milvus_client,
    metric,
    expected_ids,
    expected_scores,
):
    name = _collection_name(f"function_chain_{metric.lower()}")
    _create_collection(milvus_client, name, metric=metric)
    chain = FunctionChain(FunctionChainStage.L2_RERANK).map(
        "$score",
        fn.num_combine(
            col("$score"),
            col("popularity"),
            mode="weighted",
            weights=[1.0, 0.1],
        ),
    )

    with sending(milvus_client, chain):
        result = milvus_client.search(
            collection_name=name,
            data=[[1.0, 0.0]],
            anns_field="vector",
            search_params={"metric_type": metric, "params": {}},
            limit=3,
        )

    assert [hit["id"] for hit in result[0]] == expected_ids
    assert [hit["distance"] for hit in result[0]] == pytest.approx(
        expected_scores
    )


def test_public_chain_preserves_group_by_response_metadata(
    milvus_client,
    grpc_server,
):
    name = _collection_name("function_chain_group")
    _create_collection(milvus_client, name)
    request = _search_request(
        name,
        output_fields=["title"],
        group_by_field="category",
        group_size=1,
    )
    attach(request, _score_plus_popularity_chain())
    channel, stub = _raw_stub(grpc_server)

    try:
        response = stub.Search(request)
    finally:
        channel.close()

    assert response.status.code == 0
    assert response.results.group_by_field_value.field_name == "category"
    assert len(response.results.group_by_field_value.scalars.string_data.data) == 3
    assert list(response.results.output_fields) == ["title"]


def test_raw_search_rejects_function_score_function_chain_conflict(
    milvus_client,
    grpc_server,
):
    name = _collection_name("function_chain_conflict")
    _create_collection(milvus_client, name)
    request = _search_request(name)
    attach(request, _score_plus_popularity_chain())
    request.function_score.SetInParent()
    channel, stub = _raw_stub(grpc_server)

    try:
        response = stub.Search(request)
    finally:
        channel.close()

    assert response.status.error_code == 6
    assert response.status.code == 6
    assert response.status.reason == (
        "function_score and function_chains cannot be used together"
    )


def test_raw_search_rejects_order_by_function_chain_conflict_before_ann(
    milvus_client,
    grpc_server,
):
    # 替换点：参照用 monkeypatch 让 `Collection.search` 直接失败来证明冲突检查先于检索；这里改为在未加载的
    # collection 上发同一请求，冲突检查晚于检索时会先报未加载（101）。
    name = _collection_name("function_chain_order_by_conflict")
    _create_collection(milvus_client, name)
    milvus_client.release_collection(name)

    request = _search_request(name)
    attach(request, _score_plus_popularity_chain())
    request.search_params.add(
        key="order_by_fields",
        value=json.dumps("popularity:asc"),
    )
    channel, stub = _raw_stub(grpc_server)

    try:
        response = stub.Search(request)
    finally:
        channel.close()

    assert response.status.error_code == 6
    assert response.status.code == 6
    assert response.status.reason == (
        "order_by and function rerank cannot be used together"
    )


def test_raw_search_rejects_unknown_rerank_provider_as_illegal_argument(
    milvus_client,
    grpc_server,
):
    name = _collection_name("function_chain_unknown_provider")
    _create_collection(milvus_client, name)
    chain = FunctionChain(FunctionChainStage.L2_RERANK).map(
        "$score",
        fn.rerank_model(
            col("title"),
            queries=["query"],
            provider="unknown-provider",
        ),
    )
    request = _search_request(name, output_fields=["title"])
    attach(request, chain)
    channel, stub = _raw_stub(grpc_server)

    try:
        response = stub.Search(request)
    finally:
        channel.close()

    assert response.status.error_code == 6
    assert response.status.code == 6
    assert "Unknown rerank provider" in response.status.reason


def test_raw_search_rejects_request_rerank_endpoint_as_illegal_argument(
    milvus_client,
    grpc_server,
):
    name = _collection_name("function_chain_request_endpoint")
    _create_collection(milvus_client, name)
    chain = FunctionChain(FunctionChainStage.L2_RERANK).map(
        "$score",
        fn.rerank_model(
            col("title"),
            queries=["query"],
            provider="cohere",
            **{"Base_Url": "https://attacker.example"},
        ),
    )
    request = _search_request(name, output_fields=["title"])
    attach(request, chain)
    channel, stub = _raw_stub(grpc_server)

    try:
        response = stub.Search(request)
    finally:
        channel.close()

    assert response.status.error_code == 6
    assert response.status.code == 6
    assert "endpoint" in response.status.reason


def test_raw_search_rejects_unsupported_rerank_param_before_ann(
    milvus_client,
    grpc_server,
    monkeypatch,
):
    # 替换点：同 test_raw_search_rejects_order_by_function_chain_conflict_before_ann，在未加载的 collection 上
    # 发同一请求，代替让 `Collection.search` 直接失败的 monkeypatch。
    monkeypatch.setenv("COHERE_API_KEY", "test-key")
    name = _collection_name("function_chain_unsupported_rerank_param")
    _create_collection(milvus_client, name)
    milvus_client.release_collection(name)

    chain = FunctionChain(FunctionChainStage.L2_RERANK).map(
        "$score",
        fn.rerank_model(
            col("title"),
            queries=["query"],
            provider="cohere",
            typo="value",
        ),
    )
    request = _search_request(name, output_fields=["title"])
    attach(request, chain)
    channel, stub = _raw_stub(grpc_server)

    try:
        response = stub.Search(request)
    finally:
        channel.close()

    assert response.status.error_code == 6
    assert response.status.code == 6
    assert "unsupported rerank_model parameter 'typo'" in response.status.reason


def test_raw_search_rejects_primary_name_conflicting_with_system_column(
    milvus_client,
    grpc_server,
):
    # 替换点：参照先建普通 collection，再用 monkeypatch 把它的 schema 换成主键名为 `$score` 的 schema，并让
    # `Collection.search` 直接失败；这里经公开接口直接建这样的 collection（3.x 与本实现都允许），字段与参照
    # 替换后的 schema 相同。
    name = _collection_name("function_chain_system_pk")
    schema = milvus_client.create_schema(auto_id=False, enable_dynamic_field=False)
    schema.add_field("$score", DataType.INT64, is_primary=True)
    schema.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    schema.add_field("title", DataType.VARCHAR, max_length=128)
    index_params = milvus_client.prepare_index_params()
    index_params.add_index(field_name="vector", index_type="BRUTE_FORCE", metric_type="IP", params={})
    milvus_client.create_collection(name, schema=schema, index_params=index_params)
    milvus_client.load_collection(name)

    request = _search_request(name, output_fields=["title"])
    attach(request, FunctionChain(FunctionChainStage.L2_RERANK).limit(1))
    channel, stub = _raw_stub(grpc_server)

    try:
        response = stub.Search(request)
    finally:
        channel.close()

    assert response.status.error_code == 6
    assert response.status.code == 6
    assert response.status.reason == (
        "function chain primary key field '$score' conflicts with "
        "reserved system column '$score'"
    )


def test_raw_hybrid_search_rejects_function_chains(grpc_server):
    request = milvus_pb2.HybridSearchRequest(collection_name="unused")
    attach(request, FunctionChain(FunctionChainStage.L2_RERANK).limit(1), field=15)
    channel, stub = _raw_stub(grpc_server)

    try:
        response = stub.HybridSearch(request)
    finally:
        channel.close()

    assert response.status.error_code == 6
    assert response.status.code == 6
    assert response.status.reason == (
        "function_chains is not supported for hybrid search yet"
    )


def test_search_without_function_chain_preserves_legacy_behavior(milvus_client):
    name = _collection_name("function_chain_legacy")
    _create_collection(milvus_client, name)

    result = milvus_client.search(
        collection_name=name,
        data=[[1.0, 0.0]],
        anns_field="vector",
        search_params={"metric_type": "IP", "params": {}},
        limit=3,
        output_fields=["title"],
    )

    assert [hit["id"] for hit in result[0]] == [2, 1, 3]
    assert [hit["distance"] for hit in result[0]] == pytest.approx(
        [1.0, 0.8, 0.0]
    )
    assert [hit["entity"]["title"] for hit in result[0]] == [
        "two",
        "one",
        "three",
    ]
