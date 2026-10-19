"""0017 数据编码原型：比较删除分区与保留全局 delta；不模拟产品提交/故障协议。"""

import argparse
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import shutil
import subprocess

from partition_truth import flush, make, open_db, row
from run import REF, outside


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--reference', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    args = parser.parse_args()
    reference = args.reference.resolve()
    root = outside(args.work_dir)
    assert not root.is_relative_to(reference)
    assert subprocess.check_output(['git', '-C', str(reference), 'rev-parse', 'HEAD'], text=True).strip() == REF
    import milvus_lite
    import pyarrow
    from milvus_lite.storage.manifest import Manifest
    assert Path(milvus_lite.__file__).resolve().is_relative_to(reference)
    assert pyarrow.__version__ in {'15.0.0', '25.0.1'}
    root.mkdir(parents=True, exist_ok=False)
    seed = root/'seed.db'
    db, collection = make(seed)
    collection.insert([row(2, 'only-p2')], partition_name='p2')
    flush(collection)
    assert collection.delete([2], partition_name='p1') == 1
    flush(collection)
    assert collection.query(output_fields=['id']) == []
    db.close()
    result = {}
    for mode in ['control', 'anchor']:
        path = root/(mode+'.db')
        shutil.copytree(seed, path)
        copied = []
        if mode == 'control':
            db = open_db(path)
            collection = db.get_collection('c')
            collection.drop_partition('p1')
            db.close()
        else:
            base = path/'collections/c'
            manifest = Manifest.load(str(base))
            seq = manifest.current_seq
            for relative in manifest.get_delta_files('p1'):
                source = base/'partitions/p1'/relative
                target = base/'partitions/_default'/relative
                target.parent.mkdir(parents=True, exist_ok=True)
                assert not target.exists()
                with target.open('xb') as output:
                    output.write(source.read_bytes())
                    output.flush()
                    os.fsync(output.fileno())
                assert source.read_bytes() == target.read_bytes()
                copied.append({'file': relative, 'sha256': hashlib.sha256(target.read_bytes()).hexdigest()})
                manifest.add_delta_file('_default', relative)
            assert copied
            manifest.remove_partition('p1')
            assert manifest.current_seq == seq
            manifest.save()
            # 目录是本原型创建的副本；物理删去原 delta，避免读取残留掩盖缺陷。
            shutil.rmtree(base/'partitions/p1')
        db = open_db(path)
        collection = db.get_collection('c')
        ids = lambda: sorted(r['id'] for r in collection.query(output_fields=['id']))
        expected = [2] if mode == 'control' else []
        assert ids() == expected
        before = ids()
        for pk in [10, 11, 12]:
            collection.insert([row(pk, 'extra')], partition_name='p2')
            flush(collection)
        assert len(collection._manifest.get_data_files('p2')) == 1
        assert ids() == expected + [10, 11, 12]
        db.close()
        db = open_db(path)
        collection = db.get_collection('c')
        assert ids() == expected + [10, 11, 12]
        result[mode] = {'after_reopen': before, 'after_actual_compaction_and_reopen': ids(),
                        'p2_data_files': len(collection._manifest.get_data_files('p2')), 'copied': copied}
        db.close()
    report = {'reference': REF, 'arrow': pyarrow.__version__, 'pymilvus': importlib.metadata.version('pymilvus'),
              'script_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), 'cases': result}
    (root/'result.json').write_text(json.dumps(report, indent=2)+'\n')
    print(json.dumps(report))


if __name__ == '__main__':
    main()
