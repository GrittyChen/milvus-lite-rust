"""从已发布冻结提交构建隔离组件诊断，不修改产品 checkout。"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile

FROZEN = '27aabf26d9fbf00a51f489d723873a0550b5557c'
ARCHIVE_SHA256 = 'ff82b6074910c9e9a20b7b22339808c876e1ac89bbbd827c7868000f41ea560b'


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--work-dir', required=True, type=Path)
    a = p.parse_args()
    repo = Path(__file__).resolve().parents[3]
    work = a.work_dir
    if (work.exists() or work.is_symlink() or not work.parent.is_dir()
            or work.resolve().is_relative_to(repo)):
        p.error('work-dir 必须是仓库外不存在的路径，且父目录存在')
    if sys.version_info < (3, 12):
        p.error('需要 Python 3.12 或以上')
    work.mkdir()
    archive = work / 'source.tar'
    with archive.open('wb') as out:
        subprocess.run(['git', '-C', str(repo), 'archive', FROZEN], stdout=out, check=True)
    if hashlib.sha256(archive.read_bytes()).hexdigest() != ARCHIVE_SHA256:
        raise RuntimeError('冻结归档身份不匹配')
    src = work / 'src'
    src.mkdir()
    with tarfile.open(archive) as t:
        t.extractall(src, filter='data')
    here = Path(__file__).resolve().parent
    for target, addition in [('crates/core/src/collection.rs', 'collection.rs'),
                             ('crates/core/src/storage/arrow_data.rs', 'arrow_data.rs')]:
        dst = src / target
        dst.write_bytes(dst.read_bytes() + (here / addition).read_bytes())
    (src / 'crates/core/examples/json_write_diagnostic.rs').write_bytes((here / 'component.rs').read_bytes())
    env = dict(os.environ, CARGO_TARGET_DIR=str(work.resolve() / 'target'),
               CARGO_BUILD_JOBS='4', CARGO_INCREMENTAL='0')
    for key in ['RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS']:
        env.pop(key, None)
    cmd = ['cargo', 'build', '--offline', '--locked', '--release', '--manifest-path',
           str(src.resolve() / 'Cargo.toml'), '-p', 'milvus-lite-core', '--example', 'json_write_diagnostic']
    with (work / 'build.log').open('w') as log:
        subprocess.run(cmd, cwd=src, env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
    exe = work / 'target/release/examples/json_write_diagnostic'
    (work / 'build-result.json').write_text(json.dumps({
        'passed': True, 'frozen_commit': FROZEN, 'archive_sha256': ARCHIVE_SHA256,
        'argv': cmd, 'executable_sha256': hashlib.sha256(exe.read_bytes()).hexdigest(),
        'diagnostic_sources': {n: hashlib.sha256((here / n).read_bytes()).hexdigest()
                               for n in ['collection.rs', 'arrow_data.rs', 'component.rs']},
    }, indent=2) + '\n')
    print(exe)


if __name__ == '__main__':
    main()
