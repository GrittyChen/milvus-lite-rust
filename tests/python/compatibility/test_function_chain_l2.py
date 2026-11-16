"""Upstream L2 Function Chain compatibility tests through MilvusClient."""

# 复制自 milvus-lite v3.2.1（43d1257）的 tests/compatibility/test_function_chain_l2.py（`1320d26` 未改动），
# Apache-2.0 许可；按 0035「参照用例」改后复制，逐条状态见 tests/python/adapter/INVENTORY.md：
# - 起服务的 `server` fixture 换成本包的 server_manager（上游经 `start_server_in_thread` 起 3.x 服务）。
# - 上游在 pymilvus 没有 Function Chain API 时整模块跳过；本仓两条 SDK 线都运行、不允许跳过，所以去掉这两处
#   `pytest.skip`，链改用 tests/python/chainwire.py 里与 SDK 同名的写法构造。
# - `client.search(..., function_chains=chain)` 改为在 `with sending(client, chain):` 里发出同一个 search。
# - 不复制 test_search_rejects_l2_function_chain_with_function_score：它断言的是 pymilvus 3.0 客户端在发出
#   请求前抛出的 ParamError，服务端的冲突由 adapter/test_function_chain.py 的
#   test_raw_search_rejects_function_score_function_chain_conflict 覆盖。

from __future__ import annotations

import os
import shutil
import sys
import tempfile
import uuid
from pathlib import Path

import pytest

from pymilvus import (
    DataType,
    MilvusClient,
)
from pymilvus.exceptions import MilvusException

from milvus_lite.server_manager import ServerManager

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from chainwire import FunctionChain, FunctionChainStage, col, fn, sending  # noqa: E402


DIM = 2
VECTOR_FIELD = "vector"
SCALAR_FIELD = "ts"


@pytest.fixture(scope="module")
def server():
    data_dir = tempfile.mkdtemp(prefix="function_chain_l2_")
    manager = ServerManager()
    uri = manager.start_and_get_uri(os.path.join(data_dir, "function_chain_l2.db"))
    assert uri is not None, "failed to start the in-process server"
    try:
        yield int(uri.rsplit(":", 1)[1])
    finally:
        try:
            manager.release_all()
        finally:
            shutil.rmtree(data_dir, ignore_errors=True)


@pytest.fixture
def client(server):
    milvus_client = MilvusClient(uri=f"http://127.0.0.1:{server}")
    try:
        yield milvus_client
    finally:
        try:
            for name in milvus_client.list_collections():
                milvus_client.drop_collection(name)
        finally:
            milvus_client.close()


def _collection_name(prefix: str) -> str:
    return f"{prefix}_{uuid.uuid4().hex[:10]}"


def _create_function_chain_collection(client: MilvusClient) -> str:
    name = _collection_name("function_chain_l2")
    schema = client.create_schema(
        auto_id=False,
        enable_dynamic_field=False,
    )
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field(SCALAR_FIELD, DataType.INT64)
    schema.add_field(VECTOR_FIELD, DataType.FLOAT_VECTOR, dim=DIM)

    index_params = client.prepare_index_params()
    index_params.add_index(
        field_name=VECTOR_FIELD,
        index_type="BRUTE_FORCE",
        metric_type="L2",
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
            {"id": 1, SCALAR_FIELD: 10, VECTOR_FIELD: [0.0, 0.0]},
            {"id": 2, SCALAR_FIELD: 20, VECTOR_FIELD: [0.01, 0.0]},
            {"id": 3, SCALAR_FIELD: 30, VECTOR_FIELD: [0.02, 0.0]},
        ],
    )
    client.load_collection(name)
    return name


def _score_plus_ts_chain():
    return (
        FunctionChain(FunctionChainStage.L2_RERANK, name="score_plus_ts")
        .map(
            "$score",
            fn.num_combine(col("$score"), col(SCALAR_FIELD), mode="sum"),
        )
        .sort(col("$score"), desc=True, tie_break_col=col("$id"))
    )


def _hit_field(hit: dict, field: str):
    if field in hit:
        return hit[field]
    return hit.get("entity", {}).get(field)


def _assert_field_not_returned(hit: dict, field: str) -> None:
    assert field not in hit
    assert field not in hit.get("entity", {})


def _assert_search_error(
    client: MilvusClient,
    collection_name: str,
    function_chains,
    message: str,
    **kwargs,
) -> None:
    with pytest.raises(MilvusException) as exc_info, sending(client, function_chains):
        client.search(
            collection_name=collection_name,
            data=[[0.0, 0.0]],
            anns_field=VECTOR_FIELD,
            search_params={"metric_type": "L2", "params": {}},
            limit=3,
            **kwargs,
        )

    normalized_expected = message.replace("'", '"')
    normalized_actual = str(exc_info.value).replace("'", '"')
    assert normalized_expected in normalized_actual


def test_search_with_l2_function_chain_sdk_reranks_by_scalar_field(client):
    collection_name = _create_function_chain_collection(client)

    with sending(client, _score_plus_ts_chain()):
        result = client.search(
            collection_name=collection_name,
            data=[[0.0, 0.0]],
            anns_field=VECTOR_FIELD,
            search_params={"metric_type": "L2", "params": {}},
            limit=3,
            output_fields=[SCALAR_FIELD],
        )

    assert [hit["id"] for hit in result[0]] == [3, 2, 1]
    assert [_hit_field(hit, SCALAR_FIELD) for hit in result[0]] == [30, 20, 10]


def test_search_with_l2_function_chain_sdk_uses_hidden_input_field(client):
    collection_name = _create_function_chain_collection(client)

    with sending(client, _score_plus_ts_chain()):
        result = client.search(
            collection_name=collection_name,
            data=[[0.0, 0.0]],
            anns_field=VECTOR_FIELD,
            search_params={"metric_type": "L2", "params": {}},
            limit=3,
            output_fields=["id"],
        )

    assert [hit["id"] for hit in result[0]] == [3, 2, 1]
    for hit in result[0]:
        _assert_field_not_returned(hit, SCALAR_FIELD)


def test_search_with_l2_function_chain_sdk_temp_column_not_returned(client):
    collection_name = _create_function_chain_collection(client)
    chain = (
        FunctionChain(FunctionChainStage.L2_RERANK, name="l2_temp_score")
        .map(
            "tmp_score",
            fn.num_combine(col("$score"), col(SCALAR_FIELD), mode="sum"),
        )
        .map(
            "$score",
            fn.num_combine(col("tmp_score"), col("$score"), mode="sum"),
        )
        .sort(col("$score"), desc=True, tie_break_col=col("$id"))
    )

    with sending(client, chain):
        result = client.search(
            collection_name=collection_name,
            data=[[0.0, 0.0]],
            anns_field=VECTOR_FIELD,
            search_params={"metric_type": "L2", "params": {}},
            limit=3,
            output_fields=[SCALAR_FIELD],
        )

    assert [hit["id"] for hit in result[0]] == [3, 2, 1]
    assert [_hit_field(hit, SCALAR_FIELD) for hit in result[0]] == [30, 20, 10]
    for hit in result[0]:
        _assert_field_not_returned(hit, "tmp_score")


def test_search_with_l2_function_chain_sdk_limit_op(client):
    collection_name = _create_function_chain_collection(client)
    chain = FunctionChain(
        FunctionChainStage.L2_RERANK,
        name="l2_limit",
    ).limit(2)

    with sending(client, chain):
        result = client.search(
            collection_name=collection_name,
            data=[[0.0, 0.0]],
            anns_field=VECTOR_FIELD,
            search_params={"metric_type": "L2", "params": {}},
            limit=3,
        )

    assert len(result[0]) == 2


def test_search_rejects_l2_function_chain_write_readonly_system_column(client):
    collection_name = _create_function_chain_collection(client)
    chain = FunctionChain(
        FunctionChainStage.L2_RERANK,
        name="bad_l2_write_id",
    ).map(
        "$id",
        fn.num_combine(col("$score"), col(SCALAR_FIELD), mode="sum"),
    )

    _assert_search_error(
        client,
        collection_name,
        chain,
        'system output "$id" is not writable',
    )


def test_search_rejects_l2_function_chain_reserved_temp_output(client):
    collection_name = _create_function_chain_collection(client)
    chain = FunctionChain(
        FunctionChainStage.L2_RERANK,
        name="bad_l2_reserved_temp_output",
    ).map(
        "$tmp_score",
        fn.num_combine(col("$score"), col(SCALAR_FIELD), mode="sum"),
    )

    _assert_search_error(
        client,
        collection_name,
        chain,
        'system output "$tmp_score" is not writable',
    )


def test_search_rejects_l2_function_chain_read_internal_system_input(client):
    collection_name = _create_function_chain_collection(client)
    chain = FunctionChain(
        FunctionChainStage.L2_RERANK,
        name="bad_l2_seg_offset_input",
    ).map(
        "$score",
        fn.num_combine(col("$seg_offset"), col("$score"), mode="sum"),
    )

    _assert_search_error(
        client,
        collection_name,
        chain,
        'system input "$seg_offset" is not supported',
    )


def test_search_rejects_l2_function_chain_read_unknown_system_input(client):
    collection_name = _create_function_chain_collection(client)
    chain = FunctionChain(
        FunctionChainStage.L2_RERANK,
        name="bad_l2_unknown_system_input",
    ).map(
        "$score",
        fn.num_combine(col("$tmp_score"), col("$score"), mode="sum"),
    )

    _assert_search_error(
        client,
        collection_name,
        chain,
        'system input "$tmp_score" is not supported',
    )


def test_search_rejects_l2_function_chain_with_order_by(client):
    collection_name = _create_function_chain_collection(client)

    _assert_search_error(
        client,
        collection_name,
        _score_plus_ts_chain(),
        "order_by and function rerank cannot be used together",
        order_by_fields=[{"field": SCALAR_FIELD, "order": "asc"}],
    )
