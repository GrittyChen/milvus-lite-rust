"""0017 A 的原生目录回退预检：旧读者拒绝非默认分区，Drop 后读回默认分区。"""
import argparse
import hashlib
import importlib.metadata
import json
from pathlib import Path
import shutil
import sys

from run import outside


def hashes(path):
    return {str(p.relative_to(path)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in sorted(path.rglob('*')) if p.is_file() and p.name != 'LOCK'}


def seed(args):
    import os
    from pymilvus import MilvusClient
    from milvus_lite.server_manager import server_manager_instance as manager
    import milvus_lite._native as native
    path=str(args.work_dir)
    args.work_dir.parent.mkdir(parents=True, exist_ok=True)
    if args.work_dir.exists(): raise ValueError('工作目录已存在')
    client=MilvusClient(path,dedicated=True)
    for name in ['healthy','partitioned','dropped']:
        client.create_collection(name,dimension=2,metric_type='L2')
        if name!='healthy':client.create_partition(name,'p')
        client.insert(name,[{'id':1,'vector':[1.,0.]}])
        if name!='healthy':client.insert(name,[{'id':2,'vector':[2.,0.]}],partition_name='p')
        if name=='dropped':
            client.flush(name);client.delete(name,ids=[1],partition_name='p');client.flush(name)
            client.drop_partition(name,'p')
        elif args.state!='wal':client.flush(name)
    identity={'state':args.state,'pymilvus':importlib.metadata.version('pymilvus'),
              'native_sha256':hashlib.sha256(Path(native.__file__).read_bytes()).hexdigest(),
              'script_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    args.work_dir.with_name(args.work_dir.name+'.writer.json').write_text(json.dumps(identity,indent=2)+'\n')
    if args.state=='wal':os._exit(0)
    client.close();manager.release_server(path)


def check(args):
    from pymilvus import MilvusClient
    from milvus_lite.server_manager import ServerManager
    import milvus_lite._native as native
    if any(p.is_symlink() for p in args.seed.rglob('*')):raise ValueError('种子包含符号链接')
    args.work_dir.mkdir(parents=True,exist_ok=False)
    target=args.work_dir/'copy.db';shutil.copytree(args.seed,target)
    before=hashes(target);manager=ServerManager();client=MilvusClient(manager.start_and_get_uri(str(target)),dedicated=True)
    try:
        client.load_collection('healthy')
        assert client.query('healthy',filter='id>=0',output_fields=['id'])==[{'id':1}]
        try:client.load_collection('partitioned')
        except Exception as error:
            rejection=str(error)
            assert 'non-default partitions' in rejection,rejection
        else:raise AssertionError('旧读者错误接受分区目录')
        client.load_collection('dropped')
        assert client.query('dropped',filter='id>=0',output_fields=['id'])==[]
        assert client.query('healthy',filter='id>=0',output_fields=['id'])==[{'id':1}]
    finally:client.close();manager.release_all()
    after=hashes(target)
    protected=lambda files:{p:h for p,h in files.items() if p.startswith('collections/partitioned/')}
    assert protected(before)==protected(after),'旧读者修改了受保护目录'
    result={'pymilvus':importlib.metadata.version('pymilvus'),
            'native_sha256':hashlib.sha256(Path(native.__file__).read_bytes()).hexdigest(),
            'rejection':rejection,'protected_files_unchanged':True,'dropped_default_readable':True}
    (args.work_dir/'result.json').write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result))


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('action',choices=['seed','check'])
    p.add_argument('--work-dir',type=Path,required=True);p.add_argument('--seed',type=Path)
    p.add_argument('--state',choices=['wal','segment'],default='segment');a=p.parse_args();a.work_dir=outside(a.work_dir)
    if a.action=='seed':seed(a)
    else:
        if a.seed is None:p.error('check 需要 --seed')
        a.seed=outside(a.seed);check(a)


if __name__=='__main__':main()
