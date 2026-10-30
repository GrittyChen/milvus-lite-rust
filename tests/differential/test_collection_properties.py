"""0023 A AlterCollection：合同「参照实测 / 集合属性」逐行对照 3.x（43d1257）。

raw gRPC 比较 code、error_code、文案与 DescribeCollection 的属性顺序；每次写入后两边的 `schema.json`
逐字节相同。`timezone` 与非法语法名字的预期差异单独断言。
"""

import json
from pathlib import Path

import grpc
from pymilvus.grpc_gen import common_pb2

from test_aliases import Both, call, schema
from test_reopen import pair  # noqa: F401  （fixture）
from test_scenarios import index


def kv(*items):
    return [common_pb2.KeyValuePair(key=k, value=v) for k, v in items]


def schema_files(pair, name):
    return [Path(root) / "collections" / name / "schema.json" for root in (pair.rust_path, pair.ref_path)]


def same_schema_bytes(pair, name):
    rust, ref = (path.read_bytes() for path in schema_files(pair, name))
    assert rust == ref, f"{name}\n  rust: {rust!r}\n  3.x:  {ref!r}"


def props(both, name):
    return both("DescribeCollection", collection_name=name)[-1]


def test_alter_collection_matches_3x(pair):
    d = pair.open()
    both = Both(pair)
    d.step("create c", lambda c: c.create_collection("c", schema=schema(), index_params=index("L2")))
    d.step("create plain", lambda c: c.create_collection("plain", schema=schema()))
    d.step("insert", lambda c: c.insert("c", [{"id": 1, "vector": [1.0, 0.0]}]))
    same_schema_bytes(pair, "c")

    # 进程内：新键追加、已有键原位替换；空 key 丢弃、同名 key 保留首次位置取最后的值。
    both("AlterCollection", collection_name="c", properties=kv(("zeta", "1"), ("alpha", "2"), ("", "x"), ("mid", "3"), ("zeta", "5")))
    assert props(both, "c") == [("zeta", "5"), ("alpha", "2"), ("mid", "3")]
    same_schema_bytes(pair, "c")
    both("AlterCollection", collection_name="c", properties=kv(("alpha", "中文"), ("键", "值")))
    # 先删后设：删除不存在的键忽略，被删又设置的键追加在末尾。
    both("AlterCollection", collection_name="c", properties=kv(("zeta", "9")), delete_keys=["zeta", "missing", ""])
    assert props(both, "c") == [("alpha", "中文"), ("mid", "3"), ("键", "值"), ("zeta", "9")]
    same_schema_bytes(pair, "c")
    both("AlterCollection", collection_name="c")
    both("AlterCollection", collection_name="c", delete_keys=["missing"])
    same_schema_bytes(pair, "c")

    # 经别名修改目标集合；不存在为 100/4，判定先于 timezone。
    both("CreateAlias", collection_name="c", alias="a")
    both("AlterCollection", collection_name="a", properties=kv(("via", "alias")))
    assert props(both, "c")[-1] == ("via", "alias")
    for properties in [kv(("k", "v")), kv(("timezone", "UTC"))]:
        result = both("AlterCollection", collection_name="nope", properties=properties)
        assert result[:3] == (100, 4, "collection 'nope' does not exist"), result
    same_schema_bytes(pair, "c")

    d.step("client alter", lambda c: c.alter_collection_properties("c", {"mmap.enabled": True, "collection.ttl.seconds": 5}))
    d.step("client drop", lambda c: c.drop_collection_properties("c", ["via", "missing"]))
    d.step("client describe", lambda c: c.describe_collection("c")["properties"])
    d.step("query", lambda c: c.query("c", filter="id >= 0", output_fields=["id"]))
    same_schema_bytes(pair, "c")
    in_process = props(both, "c")

    # 重开后从 schema.json 读回：3.x 排序写出，因此为键序。
    pair.close()
    pair.open()
    reopened = props(both, "c")
    assert reopened == sorted(in_process) != in_process
    both("GetLoadState", collection_name="c")

    # 未排序的文件（0.3.0rc3 写出或外部写入）：读回按文件顺序；无操作的 Alter 也规范重写。
    pair.close()
    data = json.loads(schema_files(pair, "c")[1].read_text())
    data["properties"] = dict(reversed(list(data["properties"].items())))
    unsorted = json.dumps(dict(reversed(list(data.items()))), indent=4).encode()
    for path in schema_files(pair, "c"):
        path.write_bytes(unsorted)
    pair.open()
    assert props(both, "c") == list(reversed(reopened))
    both("AlterCollection", collection_name="c")
    same_schema_bytes(pair, "c")
    assert schema_files(pair, "c")[0].read_bytes() != unsorted


def test_timezone_and_invalid_names_are_expected_differences(pair):
    d = pair.open()
    d.step("create", lambda c: c.create_collection("c", schema=schema()))
    rust_uri, ref_uri = pair.uris
    before = [path.read_bytes() for path in schema_files(pair, "c")]
    # 范围延后（合同「范围延后」，取舍 1）：本实现整请求 UNIMPLEMENTED、零写入；3.x 校验后保存。
    for properties, ref_status in [
        (kv(("timezone", "UTC")), (0, 0, "")),
        (kv(("keep", "1"), ("timezone", "Mars/Base")), (6, 6, "unknown timezone 'Mars/Base'")),
    ]:
        try:
            call(rust_uri, "AlterCollection", collection_name="c", properties=properties)
        except grpc.RpcError as error:
            assert error.code() == grpc.StatusCode.UNIMPLEMENTED
            assert error.details().startswith("MilvusLite does not support timezone property"), error.details()
        else:
            raise AssertionError("timezone must be UNIMPLEMENTED")
        assert call(ref_uri, "AlterCollection", collection_name="c", properties=properties)[:3] == ref_status
    assert schema_files(pair, "c")[0].read_bytes() == before[0]
    assert call(rust_uri, "DescribeCollection", collection_name="c")[-1] == []
    # 删除 `timezone` 键不受影响。
    assert call(rust_uri, "AlterCollection", collection_name="c", delete_keys=["timezone"])[:2] == (0, 0)

    # 非法语法的名字：3.x 按路径判定为不存在（100/4），本实现先做名字校验（1/0），与其他集合入口相同。
    for name in ["", "a/b", "..", "a" * 256]:
        assert call(rust_uri, "AlterCollection", collection_name=name, properties=kv(("k", "v")))[:2] == (1, 0)
        ref = call(ref_uri, "AlterCollection", collection_name=name, properties=kv(("k", "v")))
        assert ref[:3] == (100, 4, f"collection {name!r} does not exist"), ref
