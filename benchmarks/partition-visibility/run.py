"""隔离组件资源诊断：同步采样 RSS，所有计时和计数原样保存在仓库外。"""

import argparse
import fcntl
import hashlib
import json
from pathlib import Path
import platform
import select
import shutil
import subprocess
import time


REPO = Path(__file__).resolve().parents[2]


def line(process):
    if not select.select([process.stdout], [], [], 120)[0]:
        raise TimeoutError('组件同步点超时')
    text = process.stdout.readline()
    if not text:
        raise RuntimeError('组件在同步点前退出')
    return json.loads(text)


def run(binary, root, parts, rows, overlap, kind, core=False):
    name = f'{parts}-{rows}-{overlap}-{kind}'
    with (root/(name+'.stderr')).open('w') as error:
        process = subprocess.Popen([str(binary), 'stress-core' if core else 'stress', str(parts), str(rows), overlap, kind],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=error, text=True)
        phases = []
        try:
            for expected in ['empty', 'loaded', 'queried']:
                event = line(process)
                assert event['phase'] == expected, event
                event['rss_kib'] = int(subprocess.check_output(['ps', '-o', 'rss=', '-p', str(process.pid)], text=True).strip())
                phases.append(event)
                process.stdin.write('continue\n'); process.stdin.flush()
            output, _ = process.communicate(timeout=30)
            assert process.returncode == 0 and not output, (process.returncode, output)
        finally:
            if process.poll() is None:
                process.kill(); process.wait()
    result = {'partitions': parts, 'rows': rows, 'overlap': overlap, 'kind': kind, 'phases': phases}
    (root/(name+'.json')).write_text(json.dumps(result, indent=2)+'\n')
    return result


def lock_seed(path):
    seed = path.resolve(strict=True)
    assert not seed.is_relative_to(REPO), '种子必须在仓库外'
    assert seed.is_dir() and not path.is_symlink(), '种子必须是真实目录'
    assert all(not p.is_symlink() for p in seed.rglob('*')), '种子不能包含符号链接'
    lock = (seed/'LOCK').open('rb')
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        for manifest_path in (seed/'collections').glob('*/manifest.json'):
            manifest = json.loads(manifest_path.read_text())
            assert (manifest_path.parent/'schema.json').is_file(), '种子缺少 schema'
            assert not list((manifest_path.parent/'wal').glob('*.arrow')), '种子必须已正常关闭并排空 WAL'
            for name, files in manifest['partitions'].items():
                assert name not in ('', '.', '..') and not any(c in name for c in ('/', chr(92), chr(0))), '不安全的种子分区名'
                for file in files['data_files'] + files['delta_files']:
                    target = manifest_path.parent/'partitions'/name/file
                    assert target.resolve().is_relative_to(seed) and target.is_file(), '种子引用不完整'
        assert list((seed/'collections').glob('*/manifest.json')), '种子没有 collection'
        return lock
    except BaseException:
        lock.close()
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--engine-seed', type=Path)
    parser.add_argument('--core', action='store_true')
    parser.add_argument('--bidirectional', action='store_true')
    args = parser.parse_args()
    if args.bidirectional and not args.core:parser.error('--bidirectional 必须同时选择 --core')
    root = args.work_dir.resolve()
    assert not root.is_relative_to(REPO), '输出必须在仓库外'
    seed_lock = lock_seed(args.engine_seed) if args.engine_seed else None
    root.mkdir(parents=True, exist_ok=False)
    binary = args.binary.resolve()
    metadata = {'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                'script_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                'core': args.core, 'bidirectional': args.bidirectional, 'platform': platform.platform(), 'python': platform.python_version()}
    (root/'metadata.json').write_text(json.dumps(metadata, indent=2)+'\n')
    if args.engine_seed:
        assert seed_lock is not None
        results=[]
        for repeat in range(3):
            directory=root/('reader-'+str(repeat)+'.db')
            shutil.copytree(args.engine_seed,directory,symlinks=True)
            with (root/('reader-'+str(repeat)+'.stderr')).open('w') as error:
                process=subprocess.Popen([str(binary),'open',str(directory)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=error,text=True)
                phases=[]
                try:
                    for expected in ['empty','columns','visibility','queried']:
                        event=line(process);assert event['phase']==expected,event
                        event['rss_kib']=int(subprocess.check_output(['ps','-o','rss=','-p',str(process.pid)],text=True).strip())
                        phases.append(event);process.stdin.write('continue\n');process.stdin.flush()
                    output,_=process.communicate(timeout=30);assert process.returncode==0 and not output,(process.returncode,output)
                finally:
                    if process.poll() is None:process.kill();process.wait()
            results.append(phases)
            (root/('reader-'+str(repeat)+'.json')).write_text(json.dumps(phases,indent=2)+'\n')
        (root/'results.json').write_text(json.dumps(results,indent=2)+'\n')
        seed_lock.close()
        print(json.dumps({'engine_rounds':len(results)}));return
    configs = [(p, 65536, overlap, kind) for p in [1, 16, 64, 256]
               for overlap in ['none', 'full', 'one'] for kind in ['int64', 'varchar']]
    configs += [(16, 1048576, overlap, 'int64') for overlap in ['none', 'full']]
    if args.bidirectional:
        configs += [(p,65536,'full-bidirectional',kind) for p in [1,16,64,256] for kind in ['int64','varchar']]
        configs += [(16,1048576,'full-bidirectional','int64')]
    start = time.monotonic()
    results = [run(binary, root, *config, core=args.core) for config in configs]
    (root/'results.json').write_text(json.dumps(results, indent=2)+'\n')
    print(json.dumps({'cases': len(results), 'seconds': time.monotonic()-start}))


if __name__ == '__main__':
    main()
