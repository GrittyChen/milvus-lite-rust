"""0017 产品目录边界：只接受仓库外的新目录，不运行参照的危险路径。"""
import argparse
import hashlib
import importlib.metadata as metadata
import json
from pathlib import Path

from run import outside


def hashes(root):
    return {str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest()
            for p in root.rglob('*') if p.is_file() and not p.is_symlink() and p.name!='LOCK'}


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--work-dir',type=Path,required=True);a=p.parse_args()
    root=outside(a.work_dir);root.mkdir(parents=True,exist_ok=False)
    import grpc
    from pymilvus import MilvusClient
    from pymilvus.grpc_gen import milvus_pb2 as rpc,milvus_pb2_grpc
    from milvus_lite.server_manager import ServerManager
    import milvus_lite._native as native
    manager=ServerManager();path=root/'probe.db';uri=manager.start_and_get_uri(str(path));assert uri
    client=MilvusClient(uri,dedicated=True);client.create_collection('c',dimension=2);client.create_collection('healthy',dimension=2)
    client.insert('healthy',[{'id':99,'vector':[1.,0.]}]);client.flush('healthy')
    records=[];identities={};parts=path/'collections/c/partitions'
    try:
        with grpc.insecure_channel(uri.removeprefix('http://')) as channel:
            stub=milvus_pb2_grpc.MilvusServiceStub(channel)
            for first,second in [('p1','P1'),('é','e\u0301')]:
                client.create_partition('c',first);alias=(parts/second).exists()
                before=hashes(path)
                response=stub.CreatePartition(rpc.CreatePartitionRequest(collection_name='c',partition_name=second))
                assert response.code==(1 if alias else 0),response
                if alias:assert hashes(path)==before and 'same directory' in response.reason
                for name in [first,second]:
                    stat=(parts/name).stat();identities[name]=[stat.st_dev,stat.st_ino]
                assert (identities[first]==identities[second])==alias
                records.append({'first':first,'second':second,'alias':alias,'code':response.code})
            for name in ['', '.', '..', '_all', 'a/b', 'a\\b', 'a\0b', str(root/'outside')]:
                before=hashes(path)
                response=stub.CreatePartition(rpc.CreatePartitionRequest(collection_name='c',partition_name=name))
                assert (response.code,response.error_code,response.reason)==(6,6,'invalid partition name: '+repr(name))
                assert hashes(path)==before
                records.append({'unsafe_name':name.replace(str(root),'<work>'),'code':response.code,'files_unchanged':True})
            before=hashes(path);long_name='x'*256
            response=stub.CreatePartition(rpc.CreatePartitionRequest(collection_name='c',partition_name=long_name))
            assert response.code==1 and hashes(path)==before
            assert not stub.HasPartition(rpc.HasPartitionRequest(collection_name='c',partition_name=long_name)).value
            records.append({'overlong_name_io_error':True,'not_registered':True,'files_unchanged':True})
            target=root/'link-target';target.mkdir();canary=target/'canary';canary.write_bytes(b'owned-outside')
            link=parts/'link';link.symlink_to(target,target_is_directory=True)
            before=hashes(target)
            response=stub.CreatePartition(rpc.CreatePartitionRequest(collection_name='c',partition_name='link'))
            assert response.code==1 and hashes(target)==before
            link.unlink()
            records.append({'directory_link_rejected':True,'outside_files_unchanged':True})
    finally:client.close();manager.release_all()
    # 启动前已有的危险 manifest 必须在清理前拒绝；同库健康 collection 继续读取。
    manifest=path/'collections/c/manifest.json';value=json.loads(manifest.read_text());value['partitions']['..']={'data_files':[],'delta_files':[]};manifest.write_text(json.dumps(value))
    protected=hashes(path/'collections/c');uri=manager.start_and_get_uri(str(path));assert uri;client=MilvusClient(uri,dedicated=True)
    try:
        try:client.load_collection('c')
        except Exception as error:assert 'unsafe partition name' in str(error),error
        else:raise AssertionError('危险的存量 manifest 被接受')
        client.load_collection('healthy');assert list(client.query('healthy',filter='',output_fields=['id']))==[{'id':99}]
    finally:client.close();manager.release_all()
    assert hashes(path/'collections/c')==protected
    records.append({'unsafe_existing_collection_rejected':True,'protected_files_unchanged':True,'healthy_after':True})
    result={'native_sha256':hashlib.sha256(Path(native.__file__).read_bytes()).hexdigest(),
            'script_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),'pymilvus':metadata.version('pymilvus'),
            'directory_identities':identities,'checks':records}
    (root/'result.json').write_text(json.dumps(result,ensure_ascii=False,indent=2)+'\n');print(json.dumps(result,ensure_ascii=False))


if __name__=='__main__':main()
