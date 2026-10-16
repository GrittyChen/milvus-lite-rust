"""JSON 写入基准不能把失败、少写行或 JSON 类型变化当作更快。"""

from pathlib import Path
import runpy
from types import SimpleNamespace

import pytest


HELPERS = runpy.run_path(str(Path(__file__).resolve().parents[2] / "benchmarks/json-write/run.py"))


def test_raw_insert_validation_checks_status_count_ids_and_success_positions():
    def response(*, code=0, count=2, ids=(0, 1), positions=(0, 1)):
        return SimpleNamespace(status=SimpleNamespace(code=code, error_code=0, reason=""),
                               insert_cnt=count, IDs=SimpleNamespace(int_id=SimpleNamespace(data=ids)),
                               succ_index=positions)

    check = HELPERS["check_insert"]
    check(response(), 2)
    for bad in [response(code=6), response(count=1), response(ids=(0, 0)), response(ids=(0,)),
                response(positions=(0,)), response(positions=(1, 0))]:
        with pytest.raises(RuntimeError):
            check(bad, 2)


def test_readback_validation_rejects_missing_rows_duplicate_ids_and_json_type_changes():
    check = HELPERS["check_rows"]
    expected = {0: {"active": True}, 1: {"text": "null"}}
    good = [{"id": pk, "j": value} for pk, value in expected.items()]
    check(good, expected)
    for bad in [good[:1], [good[0], good[0]],
                [{"id": 0, "j": {"active": 1}}, good[1]],
                [good[0], {"id": 1, "j": {"text": None}}]]:
        with pytest.raises(RuntimeError):
            check(bad, expected)


def test_payload_has_fixed_size_and_varying_record_identity():
    import json

    values = [HELPERS["payload"](row) for row in (0, 96, 97)]
    assert values[0] == values[2] and values[0] != values[1]
    assert all(2600 < len(json.dumps(value).encode()) < 2800 for value in values)
