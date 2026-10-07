"""已打开目录被外部移走后的 drop 生命周期，以真实 3.x / 43d1257 作证。"""
import shutil
from pathlib import Path
import pytest
from compare import run
from test_reopen import pair, schema
from test_scenarios import index

@pytest.mark.parametrize("move", [False, True])
def test_drop_and_recreate_an_open_collection_whose_directory_disappeared(pair, move):
    diff=pair.open()
    name="displaced"
    diff.step("create",lambda c:c.create_collection(name,schema=schema(),index_params=index("L2")))
    diff.step("insert",lambda c:c.insert(name,[{"id":1,"vector":[1.0,0.0]}]))
    diff.step("flush",lambda c:c.flush(name))
    for path in [Path(pair.rust_path),Path(pair.ref_path)]:
        collection=path/"collections"/name
        if move: collection.rename(path/"moved")
        else: shutil.rmtree(collection)
    diff.step("drop",lambda c:c.drop_collection(name))
    # 外部改变已打开目录属于显式登记的边界：Rust drop 立即撤销旧句柄，3.x 到 create 才替换它。
    assert run(lambda c:c.query(name,output_fields=["count(*)"]),diff.rust)[:3] == ("err",100,4)
    assert run(lambda c:c.query(name,output_fields=["count(*)"]),diff.ref) == ("ok",[{"count(*)":1}])
    diff.step("recreate",lambda c:c.create_collection(name,schema=schema(),index_params=index("L2")))
    assert diff.step("new_count",lambda c:c.query(name,output_fields=["count(*)"])) == ("ok",[{"count(*)":0}])
    diff.step("new_insert",lambda c:c.insert(name,[{"id":9,"vector":[1.0,0.0]}]))
    pair.close()
    diff=pair.open()
    diff.step("load",lambda c:c.load_collection(name))
    assert diff.step("new_get",lambda c:c.get(name,ids=[1,9],output_fields=["id"])) == ("ok",[{"id":9}])
