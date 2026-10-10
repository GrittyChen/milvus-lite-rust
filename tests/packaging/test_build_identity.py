"""根配置的未跟踪改动也必须区分候选构建。"""

import os
from pathlib import Path
import shutil
import subprocess


def test_untracked_root_packaging_inputs_change_build_identity(tmp_path):
    repo, work, commands = tmp_path / "repo", tmp_path / "work", tmp_path / "commands"
    (repo / "benchmarks/baseline").mkdir(parents=True)
    shutil.copy2(Path(__file__).resolve().parents[2] / "benchmarks/baseline/setup_rs.sh", repo / "benchmarks/baseline/setup_rs.sh")
    (repo / ".venv/bin").mkdir(parents=True)
    maturin = repo / ".venv/bin/maturin"
    maturin.write_text('#!/bin/bash\nwhile [ "$1" != "-o" ]; do shift; done\nmkdir -p "$2"\ntouch "$2/milvus_lite_rust-fake.whl"\n')
    maturin.chmod(0o755)
    commands.mkdir()
    uv = commands / "uv"
    uv.write_text('#!/bin/bash\nif [ "$1" = "venv" ]; then\n  path="${@: -1}"\n  mkdir -p "$path/bin"\n  printf "#!/bin/bash\\nexit 0\\n" > "$path/bin/python"\n  chmod +x "$path/bin/python"\nfi\n')
    uv.chmod(0o755)
    subprocess.run(["git", "init", "-q", repo], check=True)
    subprocess.run(["git", "-C", repo, "add", "benchmarks"], check=True)
    subprocess.run(["git", "-C", repo, "-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm", "fixture"], check=True)
    env = {**os.environ, "PATH": str(commands) + os.pathsep + os.environ["PATH"]}
    def identity():
        subprocess.run(["bash", repo / "benchmarks/baseline/setup_rs.sh", work], env=env, check=True, capture_output=True)
        return (work / "rs/BUILD_COMMIT").read_text()
    (repo / "pyproject.toml").write_text('requires-python = ">=3.10"\n')
    first = identity()
    (repo / "pyproject.toml").write_text('requires-python = ">=3.11"\n')
    second = identity()
    (repo / "rust-toolchain.toml").write_text('channel = "1.98.1"\n')
    third = identity()
    assert len({first, second, third}) == 3
