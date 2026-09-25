# 复制自 milvus-lite v3.2.1（43d1257）的 tests/adapter/test_crud_alignment_milvus.py，Apache-2.0 许可。
# 只保留 M1 范围内的用例（逐条状态见 INVENTORY.md），并删去随之不再使用的 import、helper 与分节标题。
"""CRUD alignment tests adapted from upstream Milvus test suite.

Each test is a self-contained adaptation of a specific Milvus pymilvus test
method. The original test name and file are cited in the docstring. Data is
reduced to 10-20 rows with dim=4 for speed. All tests use BRUTE_FORCE index
with COSINE metric and the ``milvus_client`` fixture from conftest.py.

Adapted from:
  - milvus/tests/python_client/milvus_client/test_milvus_client_delete.py
  - milvus/tests/python_client/milvus_client/test_milvus_client_query.py
  - milvus/tests/python_client/milvus_client_v2/test_milvus_client_search_v2.py
"""

import math

import numpy as np
import pytest
from pymilvus import DataType, MilvusClient

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

DIM = 4
NB = 20


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _rng():
    return np.random.default_rng(seed=19530)


def _create_and_insert(client, name, nb=NB, *, extra_fields=True):
    """Create a collection with id/vec/float_field/varchar_field, insert nb
    rows, and return the row dicts.

    Does NOT create index or load -- caller decides.
    """
    schema = MilvusClient.create_schema()
    schema.add_field("id", DataType.INT64, is_primary=True)
    schema.add_field("vec", DataType.FLOAT_VECTOR, dim=DIM)
    if extra_fields:
        schema.add_field("float_field", DataType.FLOAT)
        schema.add_field("varchar_field", DataType.VARCHAR, max_length=64)
    client.create_collection(name, schema=schema)

    rng = _rng()
    rows = [
        {
            "id": i,
            "vec": list(rng.random(DIM).astype(float)),
            **(
                {
                    "float_field": float(i),
                    "varchar_field": str(i),
                }
                if extra_fields
                else {}
            ),
        }
        for i in range(nb)
    ]
    client.insert(name, rows)
    return rows


def _index_and_load(client, name):
    """Create a BRUTE_FORCE/COSINE index and load the collection."""
    idx = client.prepare_index_params()
    idx.add_index(
        field_name="vec",
        index_type="BRUTE_FORCE",
        metric_type="COSINE",
        params={},
    )
    client.create_index(name, idx)
    client.load_collection(name)


# ===================================================================
# 4. Get with output_fields
#    Source: test_milvus_client_query.py  lines 4100-4126
#           test_milvus_client_get_output_fields
# ===================================================================


def test_get_output_fields(milvus_client):
    """Adapted from test_milvus_client_get_output_fields
    (test_milvus_client_query.py:4100).

    Validates that get(ids, output_fields=[...]) returns only the
    requested fields (plus the primary key), and that get with a single id
    returns the same result as get with a list containing that id.
    """
    name = "get_of"
    _create_and_insert(milvus_client, name)
    _index_and_load(milvus_client, name)

    output_fields = ["id", "vec", "float_field", "varchar_field"]

    # get by list of ids
    rows_list = milvus_client.get(name, ids=[0], output_fields=output_fields)
    assert len(rows_list) == 1
    assert len(rows_list[0]) == len(output_fields)

    # get by scalar id -- should return identical data
    rows_scalar = milvus_client.get(name, ids=0, output_fields=output_fields)
    assert len(rows_scalar) == 1
    assert dict(rows_list[0]) == dict(rows_scalar[0])

    # verify each requested field is present
    for field in output_fields:
        assert field in rows_list[0]

    milvus_client.drop_collection(name)


# ===================================================================
# 6. Search round_decimal
#    Source: test_milvus_client_search_v2.py  lines 291-329
#           test_search_round_decimal
# ===================================================================


@pytest.mark.parametrize("round_decimal", [0, 1, 2, 3, 4, 5, 6])
def test_search_round_decimal(milvus_client, round_decimal):
    """Adapted from test_search_round_decimal
    (test_milvus_client_search_v2.py:291).

    Validates that search(round_decimal=N) rounds distance values to N
    decimal places. Compares distances from a rounded search against an
    unrounded search and verifies they match within tolerance.
    """
    name = f"rd_{round_decimal}"
    nb = 10
    _create_and_insert(milvus_client, name, nb=nb)
    _index_and_load(milvus_client, name)

    limit = 5
    rng = _rng()
    query_vec = [list(rng.random(DIM).astype(float))]

    # Unrounded reference search
    res_raw = milvus_client.search(
        name,
        data=query_vec,
        limit=limit,
        search_params={"metric_type": "COSINE"},
        output_fields=["id"],
    )

    # Rounded search
    res_round = milvus_client.search(
        name,
        data=query_vec,
        limit=limit,
        search_params={"metric_type": "COSINE"},
        round_decimal=round_decimal,
        output_fields=["id"],
    )

    # Build id->distance map from raw results
    dist_map = {hit["id"]: hit["distance"] for hit in res_raw[0]}

    abs_tol = pow(10, 1 - round_decimal)
    matched_count = 0
    for hit in res_round[0]:
        pk = hit["id"]
        if pk in dist_map:
            expected = round(dist_map[pk], round_decimal)
            actual = hit["distance"]
            assert math.isclose(actual, expected, rel_tol=0, abs_tol=abs_tol), (
                f"round_decimal={round_decimal}: id={pk} "
                f"expected={expected}, actual={actual}"
            )
            matched_count += 1

    assert matched_count > 0, (
        "no matching PKs found between rounded and unrounded results"
    )

    milvus_client.drop_collection(name)
