"""用参照或候选 Rust 生成目录，核验已发行旧包的读取、拒绝与文件保护边界。"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


def hashes(path):
    return {str(p.relative_to(path)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in sorted(path.rglob("*")) if p.is_file() and "collections" in p.relative_to(path).parts}


def seed(args):
    from run import REF
    actual = subprocess.check_output(["git", "-C", str(args.reference), "rev-parse", "HEAD"], text=True).strip()
    if actual != REF:
        raise ValueError("参照版本不符")
    subprocess.run(["git", "-C", str(args.reference), "diff", "--exit-code", "HEAD", "--", "milvus_lite"], check=True, stdout=subprocess.DEVNULL)
    sys.path.insert(0, str(args.reference.resolve()))
    sys.dont_write_bytecode = True
    from milvus_lite.db import MilvusLite
    from milvus_lite.schema.types import CollectionSchema, DataType, FieldSchema

    args.work_dir.mkdir(parents=True, exist_ok=False)
    db = MilvusLite(str(args.work_dir))
    for name, kwargs, value in (("legacy", {}, 7), ("nullable", {"nullable": True}, None),
                                ("defaults", {"default_value": 0}, None),
                                ("array", {"element_type": DataType.INT64, "max_capacity": 3}, [1, 2])):
        dtype = DataType.ARRAY if name == "array" else DataType.INT64
        schema = CollectionSchema([FieldSchema("id", DataType.INT64, is_primary=True),
                                   FieldSchema("vector", DataType.FLOAT_VECTOR, dim=2), FieldSchema("x", dtype, **kwargs)])
        col = db.create_collection(name, schema)
        col.create_index("vector", {"index_type": "FLAT", "metric_type": "L2"})
        col.insert([{"id": 1, "vector": [1.0, 0.0], "x": value}])
        if args.state == "segment":
            col.flush()
            col._wait_for_bg(timeout=30)
    if args.state == "wal":
        os._exit(0)
    db.close()


def seed_native(args):
    import importlib.metadata as metadata
    import milvus_lite._native as native
    from pymilvus import MilvusClient,DataType
    from milvus_lite.server_manager import ServerManager

    args.work_dir.mkdir(parents=True,exist_ok=False)
    manager=ServerManager();client=MilvusClient(manager.start_and_get_uri(str(args.work_dir)),dedicated=True)
    for name in args.collections:
        schema=client.create_schema(auto_id=False,enable_dynamic_field=False)
        schema.add_field('id',DataType.INT64,is_primary=True);schema.add_field('vector',DataType.FLOAT_VECTOR,dim=2)
        kwargs={'nullable':True} if name=='nullable' else {'default_value':0} if name=='defaults' else {'element_type':DataType.INT64,'max_capacity':3} if name=='array' else {}
        schema.add_field('x',DataType.ARRAY if name=='array' else DataType.INT64,**kwargs)
        indexes=client.prepare_index_params();indexes.add_index('vector',index_type='FLAT',metric_type='L2')
        client.create_collection(name,schema=schema,index_params=indexes)
        for i in range(4 if args.state=='compacted' and name!='legacy' else 1):
            client.insert(name,[{'id':i+1,'vector':[1.,0.],'x':7 if name=='legacy' else [1,2] if name=='array' else None}])
            if args.state!='wal':client.flush(name)
    identity={'distribution':metadata.version('milvus-lite-rust'),'pymilvus':metadata.version('pymilvus'),
              'native_sha256':hashlib.sha256(Path(native.__file__).read_bytes()).hexdigest(),'collections':args.collections,'state':args.state}
    args.work_dir.with_name(args.work_dir.name+'.writer.json').write_text(json.dumps(identity,indent=2)+'\n')
    if args.state=='wal':os._exit(0)
    client.close();manager.release_all()
    if args.state=='compacted':
        # 后续 create 可按 0010 取消未提交维护；先稳定目录，再独立重开触发无建表竞争的维护。
        manager=ServerManager();client=MilvusClient(manager.start_and_get_uri(str(args.work_dir)),dedicated=True)
        for name in args.collections:client.load_collection(name)
        client.close();manager.release_all()
        for name in args.collections:
            manifest=json.loads((args.work_dir/'collections'/name/'manifest.json').read_text())
            assert len(manifest['partitions']['_default']['data_files'])==1,name


def check(args):
    import importlib.metadata as metadata
    from milvus_lite.server_manager import ServerManager
    import milvus_lite._native as native
    from pymilvus import MilvusClient

    destination = args.work_dir / "copy.db"
    args.work_dir.mkdir(parents=True, exist_ok=False)
    shutil.copytree(args.seed, destination)
    before = hashes(destination)
    manager = ServerManager()
    client = MilvusClient(manager.start_and_get_uri(str(destination)))
    outcomes = {}
    try:
        for name in args.collections:
            try:
                client.load_collection(name)
                result = client.query(name, filter="id >= 0", output_fields=["id", "x"])
                outcomes[name] = {"ok": True, "rows": result}
            except Exception as error:
                code = getattr(error, "code", None)
                code = code() if callable(code) else code
                outcomes[name] = {"ok": False, "type": type(error).__name__, "code": getattr(code, "name", code), "error": str(error)}
        healthy_after = client.query("legacy", filter="id >= 0", output_fields=["id", "x"])
    finally:
        client.close()
        manager.release_all()
    after = hashes(destination)
    protected = lambda files: {name: value for name, value in files.items() if name.split("/")[1] != "legacy"}
    result = {"distribution": metadata.version("milvus-lite-rust"), "pymilvus": metadata.version("pymilvus"),
              "native_sha256": hashlib.sha256(Path(native.__file__).read_bytes()).hexdigest(),
              "outcomes": outcomes, "all_files_unchanged": before == after,
              "protected_files_unchanged": protected(before) == protected(after), "healthy_after": healthy_after}
    (args.work_dir / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    assert outcomes["legacy"]["ok"] and outcomes["legacy"]["rows"] == [{"id": 1, "x": 7}], result
    assert all(not outcomes[name]["ok"] and "unsupported attributes" in outcomes[name]["error"] for name in args.collections if name != "legacy"), result
    assert result["protected_files_unchanged"] and healthy_after == [{"id": 1, "x": 7}], result
    print(json.dumps(result))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("seed", "seed-native", "check"))
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--reference", type=Path)
    parser.add_argument("--state", choices=("wal", "segment", "compacted"))
    parser.add_argument("--seed", type=Path)
    parser.add_argument("--collections", nargs="+", choices=("legacy","nullable","defaults","array"), default=["legacy","nullable","defaults","array"])
    args = parser.parse_args()
    from run import outside
    args.work_dir = outside(args.work_dir)
    if args.action == "seed":
        if args.reference is None or args.state is None:
            parser.error("seed 需要 --reference 和 --state")
        if args.state=='compacted':parser.error('参照 seed 使用 wal/segment；compacted 用于 seed-native')
        seed(args)
    elif args.action=='seed-native':
        if args.state is None:parser.error('seed-native 需要 --state')
        seed_native(args)
    else:
        if args.seed is None:
            parser.error("check 需要 --seed")
        if "legacy" not in args.collections or len(set(args.collections))!=len(args.collections):
            parser.error("collections 必须包含 legacy 且不能重复")
        check(args)


if __name__ == "__main__":
    main()
