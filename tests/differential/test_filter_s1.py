"""S1 的真实入口、失败优先级与重开顺序，以 v3.2.1 / 43d1257 作证。"""
import json
import struct
from pathlib import Path

import grpc
import pytest
from pymilvus import DataType, MilvusClient
from pymilvus.grpc_gen import common_pb2, milvus_pb2, milvus_pb2_grpc

from compare import hits, run
from test_reopen import pair  # 每次关闭服务，确保重开确实恢复同一个目录。
from test_scenarios import index

def test_iso_diagnostics_cover_clock_shapes_offsets_and_tail_bytes(diff):
    name="iso_diagnostics"
    diff.step("create",lambda c:c.create_collection(name,schema=schema(),index_params=index("L2")))
    diff.step("insert",lambda c:c.insert(name,[row(1,1)]))
    invalid=["ISO '2025-01-01T00:05:00Z08:30'", "ISO '2025-01-01T00:00:08:30'",
             "ISO '2025-01-01T00:00:08:30@Z'", "ISO '2025-01-01T000008:30Z'",
             "ISO '20250101T00400000'", "ISO '20250101404000008'",
             "ISO '0001-01-01T00:00:00.3+00:00:01.4'", "ISO '2025-01-01T00+08Z\x00'"]
    for expr in invalid:
        for api in ["query","search","delete"]:
            def call(c):
                if api=="query":return c.query(name,filter=expr)
                if api=="search":return c.search(name,data=[[1,0]],filter=expr,limit=2)
                return c.delete(name,filter=expr)
            result=diff.step(f"{api}_{expr}",call,error=True)
            assert result[:3]==("err",6,6)
    for expr in ["ISO '2025-01-01T00:00:08:30Z'", "ISO '2025-01-01T00:00:08:30+08:30'",
                 "ISO '2025-01-01T00:00:08:123456789+08:30:08:30'",
                 "ISO '2025-01-01T00200400Z'", "ISO '2025-01-01T002004001Z'",
                 "ISO '2025-01-01T12345678901234Z'", "ISO '2025-01-01T00200400+08'",
                 "ISO '0001-01-01T00:00:00.3+00:00:00.4'", "ISO '2025-01-01T00Z\x00+08'"]:
        assert run(lambda c:c.query(name,filter=expr),diff.rust)[:2]==("rpc","UNIMPLEMENTED")
        reference=run(lambda c:c.query(name,filter=expr),diff.ref)
        assert reference[:3]==("err",6,6)
        assert reference[3].startswith("top-level filter expression must evaluate to bool, got timestamptz")

def test_primary_key_membership_casts_and_signed_zero_match_reference_before_and_after_reopen(pair):
    diff=pair.open()
    healthy=[-1,0,1,2]
    wide=[0,9007199254740993,-9223372036854775808,9223372036854775807]
    for name,ids in [("in_small",healthy),("in_wide",wide)]:
        diff.step(name+"_create",lambda c:c.create_collection(name,schema=schema(),index_params=index("L2")))
        diff.step(name+"_insert",lambda c:c.insert(name,[row(pk,1) for pk in ids]))
    small_cases=[("[-0.0]",[0]),("[-0.0, 1]",[0,1]),("[-0.0, 1.5]",[]),
                 ("[0.0, 1.5]",[0]),("[-0.0, -1.0]",[-1,0]),("[-0.0, 1e999]",[]),
                 ("[-0.0, 0.0, 1.5]",[0]),("[]",[])]
    wide_cases=[("[-0.0]",[0]),("[-0.0, 1]",[0]),("[-0.0, 9007199254740992.0]",[0]),
                ("[-0.0, 9223372036854775808.0]",[0,9223372036854775807]),
                ("[-0.0, -9223372036854775808.0]",[0,-9223372036854775808]),("[]",[])]
    def check(stage):
        for name,ids,cases in [("in_small",healthy,small_cases),("in_wide",wide,wide_cases)]:
            for values,expected in cases:
                expr="id in "+values
                assert diff.step(f"{stage}_{name}_{expr}",lambda c:c.query(name,filter=expr,output_fields=["id"])) == ("ok",[{"id":pk} for pk in expected])
                assert diff.step(f"{stage}_{name}_count_{expr}",lambda c:c.query(name,filter=expr,output_fields=["count(*)"])) == ("ok",[{"count(*)":len(expected)}])
                diff.step(f"{stage}_{name}_search_{expr}",lambda c:c.search(name,data=[[1,0]],filter=expr,limit=10,search_params={"metric_type":"L2"}),
                          check=hits("L2",[[1,0]],{pk:[1,0] for pk in ids},k=len(expected)))
        for expr in ["id in [-0.0,1.5]","id in [-0.0,1e999]","id in [-0.0,1e20]",
                     "id in [9007199254740993,1.5]"]:
            diff.step(stage+"_wide_conversion_"+expr,lambda c:c.query("in_wide",filter=expr),error=True)
        assert diff.step(stage+"_comparison_zero",lambda c:c.query("in_small",filter="id == -0.0",output_fields=["id"])) == ("ok",[{"id":0}])
    check("active")
    for name in ["in_small","in_wide"]:diff.step(name+"_flush",lambda c:c.flush(name))
    check("committed")
    pair.close();diff=pair.open()
    for name in ["in_small","in_wide"]:diff.step(name+"_load",lambda c:c.load_collection(name))
    check("reopened")
    diff.step("delete_negative_zero_nonintegral",lambda c:c.delete("in_small",filter="id in [-0.0,1.5]"))
    assert diff.step("unchanged_count",lambda c:c.query("in_small",output_fields=["count(*)"])) == ("ok",[{"count(*)":4}])
    assert diff.step("delete_integer_cast",lambda c:c.delete("in_small",filter="id in [-0.0,1]")) == ("ok",[0,1])
    assert diff.step("remaining",lambda c:c.query("in_small",filter="",output_fields=["id"])) == ("ok",[{"id":-1},{"id":2}])

def schema(varchar=False):
    s = MilvusClient.create_schema(auto_id=False, enable_dynamic_field=False)
    s.add_field("id", DataType.VARCHAR if varchar else DataType.INT64, is_primary=True, **({"max_length": 100} if varchar else {}))
    s.add_field("vector", DataType.FLOAT_VECTOR, dim=2)
    for name, dtype in [("i8", DataType.INT8), ("i16", DataType.INT16), ("i32", DataType.INT32),
                        ("age", DataType.INT64), ("f", DataType.FLOAT), ("d", DataType.DOUBLE),
                        ("title", DataType.VARCHAR), ("active", DataType.BOOL)]:
        s.add_field(name, dtype, **({"max_length": 100} if dtype == DataType.VARCHAR else {}))
    return s

def row(pk, age, varchar=False):
    return {"id": str(pk) if varchar else pk, "vector": [1.0, 0.0],
            "i8": age, "i16": age, "i32": age, "age": age, "f": age / 4,
            "d": age / 4, "title": f"三{age}", "active": age % 2 == 0}


def test_column_kernels_keep_word_boundary_visibility_pagination_and_late_errors(pair):
    diff = pair.open()
    name = "s1_words"
    diff.step("create", lambda c: c.create_collection(name, schema=schema(), index_params=index("L2")))
    diff.step("first_word", lambda c: c.insert(name, [row(i, i % 11) for i in range(65)]))
    diff.step("flush", lambda c: c.flush(name))
    diff.step("tail_words", lambda c: c.insert(name, [row(i, i % 11) for i in range(65, 130)]))
    diff.step("overwrite", lambda c: c.upsert(name, [row(64, 7), row(65, 8)]))
    diff.step("delete_edges", lambda c: c.delete(name, ids=[0, 63, 66, 127, 129]))

    model = {i: row(i, i % 11) for i in range(130)}
    model.update({64: row(64, 7), 65: row(65, 8)})
    for pk in [0, 63, 66, 127, 129]:
        del model[pk]
    candidates = {i: r["vector"] for i, r in model.items() if r["age"] < 5 and r["active"]}

    def check(stage):
        for expr in ["age >= 0", "age < 5 and active == true", "5 <= age and title != '三8'",
                     "f >= 0.0 and d < 2.5", "id < 0 and age >= 0"]:
            diff.step(stage + expr + "_count", lambda c: c.query(name, filter=expr, output_fields=["count(*)"]))
            for offset, limit in [(0, 1), (1, 63), (63, 65), (64, 130)]:
                diff.step(f"{stage}_{expr}_{offset}_{limit}",
                          lambda c: c.query(name, filter=expr, output_fields=["id", "title"],
                                            offset=offset, limit=limit))
        diff.step(stage + "_search", lambda c: c.search(name, data=[[1, 0]], limit=130,
                  filter="age < 5 and active == true"),
                  check=hits("L2", [[1, 0]], candidates, k=len(candidates)))

    check("active")
    diff.step("flush_all", lambda c: c.flush(name))
    pair.close()
    diff = pair.open()
    diff.step("load", lambda c: c.load_collection(name))
    check("reopened")
    wide = row(130, 1)
    wide["age"] = 9007199254740993
    diff.step("late_wide", lambda c: c.insert(name, [wide]))
    before_failed_delete = diff.step("rows_before_failed_delete", lambda c: c.query(name, output_fields=["id", "age", "title"]))
    for expr in ["id < 0 and age > 1.0", "id >= 0 and age > 1.0"]:
        # 返回上限和已经归零的掩码都不能跳过末尾活行的转换错误。
        diff.step(expr + "_limit", lambda c: c.query(name, filter=expr, output_fields=["id"], limit=1), error=True)
        diff.step(expr + "_count", lambda c: c.query(name, filter=expr, output_fields=["count(*)"]), error=True)
        diff.step(expr + "_delete", lambda c: c.delete(name, filter=expr), error=True)
        assert diff.step(expr + "_delete_preserves_count", lambda c: c.query(name, output_fields=["count(*)"])) == ("ok", [{"count(*)": len(model) + 1}])
        assert diff.step(expr + "_delete_preserves_rows", lambda c: c.query(name, output_fields=["id", "age", "title"])) == before_failed_delete

@pytest.mark.parametrize("varchar", [False, True])
def test_scalar_comparisons_and_conjunctions_across_query_count_search_and_delete(diff, varchar):
    name = "s1_varchar" if varchar else "s1_int"
    diff.step("create", lambda c: c.create_collection(name, schema=schema(varchar), index_params=index("L2")))
    rows = [row(i, i, varchar) for i in range(1, 6)]
    diff.step("insert", lambda c: c.insert(name, rows))
    operators = {"==": lambda a,b:a == b, "!=": lambda a,b:a != b, "<": lambda a,b:a < b,
                 "<=": lambda a,b:a <= b, ">": lambda a,b:a > b, ">=": lambda a,b:a >= b}
    expressions = []
    for field in ["i8", "i16", "i32", "age", "f", "d", "title", "active"]:
        literal = "'三3'" if field == "title" else "true" if field == "active" else "0.75" if field in {"f", "d"} else "3"
        value = "三3" if field == "title" else True if field == "active" else 0.75 if field in {"f", "d"} else 3
        for op, predicate in operators.items():
            expressions.append((f"{field} {op} {literal}", [r["id"] for r in rows if predicate(r[field], value)]))
            expressions.append((f"{literal} {op} {field}", [r["id"] for r in rows if predicate(value, r[field])]))
    expressions += [("(age >= 2) && (age < 5) and active == true", [str(2) if varchar else 2, str(4) if varchar else 4]),
                    ("age > - 1 and age < 1e2", [r["id"] for r in rows]),
                    ("age >= １２", []), ("title == '三\\n'", [])]
    for n, (expr, ids) in enumerate(expressions):
        got = diff.step(f"query_{n}", lambda c: c.query(name, filter=expr, output_fields=["id"]))
        assert got == ("ok", [{"id": pk} for pk in ids])
        assert diff.step(f"count_{n}", lambda c: c.query(name, filter=expr, output_fields=["count(*)"])) == ("ok", [{"count(*)": len(ids)}])
        diff.step(f"search_{n}", lambda c: c.search(name, data=[[1, 0]], filter=expr, limit=5,
            search_params={"metric_type": "L2"}, output_fields=["age"]),
            check=hits("L2", [[1,0]], {r["id"]: r["vector"] for r in rows}, k=len(ids)))
    pk_list = "['3', '1', '3']" if varchar else "[3, 1, 3]"
    diff.step("fast_query", lambda c: c.query(name, filter=f"id in {pk_list}", limit=1, offset=1, output_fields=["id"]))
    assert diff.step("in_count", lambda c: c.query(name, filter=f"id in {pk_list}", output_fields=["count(*)"])) == ("ok", [{"count(*)": 2}])
    diff.step("in_search", lambda c: c.search(name, data=[[1,0]], filter=f"id in {pk_list}", limit=5,
        search_params={"metric_type": "L2"}), check=hits("L2",[[1,0]],{r["id"]:r["vector"] for r in rows},k=2))
    diff.step("release", lambda c: c.release_collection(name))
    assert diff.step("filtered_delete", lambda c: c.delete(name, filter="age >= 2 and active == true")) == ("ok", [str(i) if varchar else i for i in [2,4]])
    diff.step("delete_preserves_released_state", lambda c: c.get_load_state(name))
    diff.step("load", lambda c: c.load_collection(name))
    assert diff.step("remaining", lambda c: c.query(name, filter="", output_fields=["id"])) == ("ok", [{"id": str(i) if varchar else i} for i in [1,3,5]])
    diff.step("flush", lambda c: c.flush(name))

def test_filter_errors_and_full_syntax_before_unsupported(diff):
    name = "s1_errors"
    diff.step("create", lambda c: c.create_collection(name, schema=schema(), index_params=index("L2")))
    diff.step("insert", lambda c: c.insert(name, [row(1,1)]))
    expressions = ["age = 1", "age <> 1", "age & true", "age | true", "age == .5", "age == 1.",
        "age == 1e+", "age == tRuE", "age == 'a\\q'", "age in [bad]", "(age > 1", "age == 1 #c",
        "age == '1'", "title > 1", "active == 1", "ag >= 1", "_seq == 1", "age < 1 < 2",
        "age > 0 and 1", "age and missing > 1", "age == 'x' and missing > 1",
        "age > 0 and title and missing > 1", "id in [true]", "age", "age == 9223372036854775808"]
    for n, expr in enumerate(expressions):
        for api in ["query", "search", "delete"]:
            def call(c, api=api):
                if api == "query": return c.query(name, filter=expr, output_fields=["id"])
                if api == "search": return c.search(name, data=[[1,0]], filter=expr, limit=2)
                return c.delete(name, filter=expr)
            diff.step(f"{api}_{n}", call, error=True)
    # 范围外构造的语法错误仍须对齐；合法语法则显式拒绝，不能静默去掉谓词。
    invalid = ["age not in (1)", "$meta[0]", "text_match(1, 'x')", "array_length()",
               "json_contains(age,)", "exists 1", "1 is null", "age like 1", "unknown_fn(age)",
               "ISO '2025-01-01'", "INTERVAL 'P'", "age[ -1 ] == 1"]
    for n, expr in enumerate(invalid):
        diff.step(f"out_of_scope_syntax_{n}", lambda c: c.query(name, filter=expr), error=True)
    diff.step("bad_ids_parser",lambda c:c.delete(name,ids=["bad"]),error=True)
    for vectors,metric in [([[1,0]],"bad"),([[1]],"L2")]:
        diff.step("filter_before_search_parameters",lambda c:c.search(name,data=vectors,filter="age = 1",search_params={"metric_type":metric},limit=2),error=True)
    for expr in ["age > 1 or age < 0", "not age > 1", "age in [1]", "age + 1 > 0", "true and missing>1", "active and missing>1",
                 "title like '三%'", "age is null", "$meta['x'] == 1", "text_match(title, 'x')",
                 "array_length(age) > 0", "ISO '2025-01-01T00:00:00Z' == ISO '2025-01-01T00:00:00Z'"]:
        assert run(lambda c: c.query(name, filter=expr), diff.rust)[:2] == ("rpc", "UNIMPLEMENTED"), expr

def test_dynamic_collections_keep_reserved_field_errors_and_reject_dynamic_filter_evaluation(diff):
    s=MilvusClient.create_schema(auto_id=False,enable_dynamic_field=True)
    s.add_field("id",DataType.INT64,is_primary=True)
    s.add_field("vector",DataType.FLOAT_VECTOR,dim=2)
    name="s1_dynamic"
    diff.step("create",lambda c:c.create_collection(name,schema=s,index_params=index("L2")))
    diff.step("insert",lambda c:c.insert(name,[{"id":1,"vector":[1.0,0.0],"color":"red"}]))
    for expr in ["_seq > 0","_partition == '_default'"]:
        diff.step(expr,lambda c:c.query(name,filter=expr),error=True)
    assert run(lambda c:c.query(name,filter="color == 'red'"),diff.rust)[:2]==("rpc","UNIMPLEMENTED")
    assert diff.step("schema_field",lambda c:c.query(name,filter="id > 0",output_fields=["id"])) == ("ok",[{"id":1}])

def test_reopen_filter_query_limit_order_and_drop_recreate(pair):
    diff = pair.open()
    name = "s1_order"
    diff.step("create", lambda c: c.create_collection(name, schema=schema(), index_params=index("L2")))
    diff.step("insert", lambda c: c.insert(name, [row(3,3),row(1,1),row(2,2)]))
    diff.step("overwrite", lambda c: c.insert(name, [row(3,4)]))
    diff.step("delete", lambda c: c.delete(name, ids=[1]))
    diff.step("reinsert", lambda c: c.insert(name, [row(1,5)]))
    def check(label, order):
        for offset in [0,1,2,4]:
            result=diff.step(f"{label}_offset_{offset}", lambda c: c.query(name, filter="age >= 0", limit=2, offset=offset, output_fields=["id"]))
            assert result == ("ok", [{"id":pk} for pk in order[offset:offset+2]])
        for client,path in [(diff.rust,Path(pair.rust_path)),(diff.ref,Path(pair.ref_path))]:
            manifest=json.loads((path/"collections"/name/"manifest.json").read_text())
            assert len(manifest["partitions"]["_default"]["data_files"])<4
    check("active",[3,2,1])
    diff.step("flush",lambda c:c.flush(name))
    diff.step("upsert",lambda c:c.upsert(name,[row(2,6)]))
    check("mixed",[3,1,2])
    pair.close()
    diff=pair.open()
    assert diff.step("unloaded",lambda c:c.get_load_state(name))[1]["state"].name=="NotLoad"
    diff.step("load",lambda c:c.load_collection(name))
    check("reopened",[3,1,2])
    diff.step("filtered_delete",lambda c:c.delete(name,filter="age >= 5"))
    diff.step("drop",lambda c:c.drop_collection(name))
    diff.step("recreate",lambda c:c.create_collection(name,schema=schema(),index_params=index("L2")))
    diff.step("insert_new",lambda c:c.insert(name,[row(9,9)]))
    assert diff.step("new_rows",lambda c:c.query(name,filter="age > 0",output_fields=["id"])) == ("ok",[{"id":9}])

def test_raw_query_pagination_and_filtered_delete_count(pair):
    diff=pair.open();name="s1_raw"
    diff.step("create",lambda c:c.create_collection(name,schema=schema(),index_params=index("L2")))
    diff.step("insert",lambda c:c.insert(name,[row(i,i) for i in range(1,6)]))
    for limit in [None,"0","-1","1","2","bad"]:
        for offset in ["0","1","-1","-10","10","bad"]:
            params=[common_pb2.KeyValuePair(key="offset",value=offset)]
            if limit is not None:params.append(common_pb2.KeyValuePair(key="limit",value=limit))
            results=[]
            for uri in pair.uris:
                with grpc.insecure_channel(uri.removeprefix("http://"),options=[("grpc.enable_http_proxy",0)]) as channel:
                    result=milvus_pb2_grpc.MilvusServiceStub(channel).Query(milvus_pb2.QueryRequest(
                        collection_name=name,expr="age>=0",output_fields=["id"],query_params=params))
                    assert result.status.code==0
                    results.append(list(result.fields_data[0].scalars.long_data.data))
            assert results[0]==results[1],(limit,offset,results)
    for uri in pair.uris:
        with grpc.insecure_channel(uri.removeprefix("http://"),options=[("grpc.enable_http_proxy",0)]) as channel:
            result=milvus_pb2_grpc.MilvusServiceStub(channel).Delete(milvus_pb2.DeleteRequest(collection_name=name,expr="age>=3"))
            assert (result.status.code,result.delete_cnt,list(result.IDs.int_id.data))==(0,3,[3,4,5])
    assert diff.step("count",lambda c:c.query(name,filter="age>=0",output_fields=["count(*)"])) == ("ok",[{"count(*)":2}])

def test_arrow_numeric_conversion_boundaries_and_live_rows(diff):
    name="s1_bounds"
    diff.step("create",lambda c:c.create_collection(name,schema=schema(),index_params=index("L2")))
    diff.step("insert",lambda c:c.insert(name,[row(1,1)]))
    for expr in ["f==16777217","f== -16777217","d==9007199254740993", "id in [9007199254740993,1.5]", "age>1e999"]:
        diff.step(expr,lambda c:c.query(name,filter=expr,output_fields=["id"]),error=expr!="age>1e999")
    wide=row(2,2);wide["age"]=9007199254740993
    diff.step("insert_wide",lambda c:c.insert(name,[wide]))
    diff.step("live_wide",lambda c:c.query(name,filter="age>1.0",output_fields=["id"]),error=True)
    # 同代覆盖后 MemTable 不再包含宽整数旧行，两侧都成功；committed 旧行的差异由 core 用例固定。
    diff.step("overwrite_wide",lambda c:c.insert(name,[row(2,2)]))
    assert diff.step("after_overwrite",lambda c:c.query(name,filter="age>1.0",output_fields=["id"])) == ("ok",[{"id":2}])

def test_empty_memtable_and_dead_committed_sources_have_the_registered_evaluation_boundary(diff):
    name="s1_dead_sources"
    diff.step("create",lambda c:c.create_collection(name,schema=schema(),index_params=index("L2")))
    diff.step("insert",lambda c:c.insert(name,[row(1,1)]))
    diff.step("delete",lambda c:c.delete(name,ids=[1]))
    assert diff.step("empty_active",lambda c:c.query(name,filter="age>9223372036854775808",output_fields=["count(*)"])) == ("ok",[{"count(*)":0}])
    diff.step("empty_schema_error",lambda c:c.query(name,filter="age == 'bad'",output_fields=["count(*)"]),error=True)
    wide=row(2,2);wide["age"]=9007199254740993
    diff.step("wide",lambda c:c.insert(name,[wide]))
    diff.step("flush",lambda c:c.flush(name))
    diff.step("delete_wide",lambda c:c.delete(name,ids=[2]))
    # 3.x 对物理 segment 求值，Rust 只对有效行求值；旧数据随 compaction 消失时参照结果会改变。
    for expr in ["age>1.0","age>9223372036854775808","f==16777217"]:
        operation=lambda c:c.query(name,filter=expr,output_fields=["count(*)"])
        assert run(operation,diff.rust)==("ok",[{"count(*)":0}])
        assert run(operation,diff.ref)[:3]==("err",1,0)

def test_raw_search_compiles_before_zero_topk_and_preserves_strict_parameter_checks(pair):
    diff=pair.open();name="s1_search_params"
    diff.step("create",lambda c:c.create_collection(name,schema=schema(),index_params=index("L2")))
    wide=row(1,1);wide["age"]=9007199254740993
    diff.step("insert",lambda c:c.insert(name,[wide]))
    def request(expr, topk="0", metric="L2", vector=(1.0,0.0)):
        placeholder=common_pb2.PlaceholderGroup(placeholders=[common_pb2.PlaceholderValue(
            tag="$0",type=101,values=[struct.pack("<"+"f"*len(vector),*vector)])])
        return milvus_pb2.SearchRequest(collection_name=name,dsl=expr,placeholder_group=placeholder.SerializeToString(),
            search_params=[common_pb2.KeyValuePair(key=k,value=v) for k,v in [("anns_field","vector"),("metric_type",metric),("topk",topk)]])
    def both(req):
        replies=[]
        for uri in pair.uris:
            with grpc.insecure_channel(uri.removeprefix("http://"),options=[("grpc.enable_http_proxy",0)]) as channel:
                replies.append(milvus_pb2_grpc.MilvusServiceStub(channel).Search(req))
        return replies
    for reply in both(request("age>1.0")):
        assert reply.status.code==0 and list(reply.results.topks)==[0]
    for reply in both(request("age == 'bad'")):
        assert (reply.status.code,reply.status.error_code)==(6,6)
    # 沿用本仓 M1 的参数严检：即使过滤后无候选，也不接受错误维度或未知度量。
    for metric,vector in [("bad",(1.0,0.0)),("L2",(1.0,))]:
        rust,reference=both(request("age<0",topk="2",metric=metric,vector=vector))
        assert (rust.status.code,rust.status.error_code)==(1,0)
        assert reference.status.code==0 and list(reference.results.topks)==[0]
