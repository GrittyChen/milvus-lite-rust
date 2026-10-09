"""正常停服排空后的来源顺序、分页、并列检索及更新，以 3.x 作证。"""

from pathlib import Path
import json

from test_reopen import pair, schema
from test_scenarios import index
from compare import hits as compare_hits
import pytest
from pymilvus import MilvusClient, DataType


@pytest.mark.parametrize("varchar", [False,True])
def test_compaction_order_after_normal_close_and_reopen(pair,varchar):
    pk=lambda value: f"é{value}" if varchar else value
    definition=schema()
    if varchar:
        definition=MilvusClient.create_schema(auto_id=False,enable_dynamic_field=True)
        definition.add_field("id",DataType.VARCHAR,is_primary=True,max_length=64)
        definition.add_field("vector",DataType.FLOAT_VECTOR,dim=2)
    diff=pair.open()
    diff.step("create",lambda c:c.create_collection("c",schema=definition,index_params=index("L2")))
    for batch in range(4):
        rows=[{"id":pk(value),"vector":[1.0,0.0],"tag":value} for value in range(batch*10,(batch+1)*10)]
        diff.step(f"insert_{batch}",lambda c:c.insert("c",rows))
        diff.step(f"flush_{batch}",lambda c:c.flush("c"))
    pair.close()
    for root in (pair.rust_path,pair.ref_path):
        manifest=json.loads((Path(root)/"collections/c/manifest.json").read_text())
        assert len(manifest["partitions"]["_default"]["data_files"])==1
    diff=pair.open()
    diff.step("load",lambda c:c.load_collection("c"))
    ordered=diff.step("ordered",lambda c:c.query("c",filter="",output_fields=["id","tag"],limit=40))[1]
    assert [r["id"] for r in ordered]==[pk(value) for value in range(40)]
    page=diff.step("page",lambda c:c.query("c",filter="",output_fields=["id"],limit=7,offset=3))[1]
    assert [r["id"] for r in page]==[pk(value) for value in range(3,10)]
    # 截断处并列按 0004 容忍互换；非截断结果另固定完整来源/物理行次序。
    diff.step("tied_search",lambda c:c.search("c",[[1.0,0.0]],limit=7,offset=3,search_params={"metric_type":"L2"}), compare_hits("L2",[[1.0,0.0]],{pk(value):[1.0,0.0] for value in range(40)},k=7,offset=3))
    full=diff.step("full_tied_search",lambda c:c.search("c",[[1.0,0.0]],limit=40,search_params={"metric_type":"L2"}))[1]
    assert [r["id"] for r in full[0]]==[pk(value) for value in range(40)]
    diff.step("upsert",lambda c:c.upsert("c",[{"id":pk(1),"vector":[1.0,0.0],"tag":99}]))
    diff.step("delete",lambda c:c.delete("c",ids=[pk(2)]))
    diff.step("flush_after",lambda c:c.flush("c"))
    pair.close()
    diff=pair.open()
    diff.step("load_after",lambda c:c.load_collection("c"))
    count=diff.step("count_after",lambda c:c.query("c",filter="",output_fields=["count(*)"]))[1]
    assert count==[{"count(*)":39}]
    ordered=diff.step("ordered_after",lambda c:c.query("c",filter="",output_fields=["id","tag"],limit=40))[1]
    assert [r["id"] for r in ordered]==[pk(value) for value in [0,*range(3,40),1]]
    assert ordered[-1]["tag"]==99


def test_compaction_rpc_completed_is_acknowledgement_not_a_wait(pair):
    diff=pair.open()
    diff.step("compact_missing",lambda c:c.compact("missing"))
    result=diff.step("state",lambda c:c.get_compaction_state(0))[1]
    assert result=="Completed"
