"""测试驱动进程的 grpcio fork 处理（tests/grpc_fork.py）：pytest 进程按是否安装 grpcio 走对应的合法路径，变量不留在
环境中、exec 的子进程看不到；grpc 已先导入时失败而不是假装生效；未安装与安装损坏分开处理；调用前的原值原样恢复。"""

import importlib.util
import os
from pathlib import Path
import subprocess
import sys

TESTS = Path(__file__).resolve().parents[1]
INSTALLED = importlib.util.find_spec("grpc") is not None


def test_pytest_process_state_matches_installation_and_does_not_leak():
    # 根 conftest 已执行；本仓开发环境装有 grpcio，CI 走完整初始化路径，只装 pytest 的工具环境走未安装路径。
    import grpc_fork
    assert grpc_fork._state is INSTALLED
    assert ("grpc" in sys.modules) is INSTALLED
    assert grpc_fork.VARIABLE not in os.environ
    out = subprocess.run([sys.executable, "-c", f"import os; print(os.environ.get({grpc_fork.VARIABLE!r}))"],
                         capture_output=True, text=True, check=True).stdout.strip()
    assert out == "None"


def child(code, *paths, site=False, **env):
    """在新解释器中运行；site 为假时不加载 site-packages，即 grpcio 未安装。"""
    environment = {k: v for k, v in os.environ.items() if k != "GRPC_ENABLE_FORK_SUPPORT"}
    environment.update(env)
    prelude = f"import sys; sys.path[:0] = {[str(TESTS), *map(str, paths)]!r}\n"
    flags = [] if site else ["-S"]
    return subprocess.run([sys.executable, *flags, "-c", prelude + code], capture_output=True, text=True, env=environment)


def test_absent_broken_preimported_and_restored_paths(tmp_path):
    absent = child("import os, grpc_fork\nassert grpc_fork.disable_fork_handlers(required=False) is False\n"
                   "print(os.environ['GRPC_ENABLE_FORK_SUPPORT'])", GRPC_ENABLE_FORK_SUPPORT="1")
    assert absent.returncode == 0 and absent.stdout.strip() == "1", absent.stderr
    needed = child("import grpc_fork\ngrpc_fork.disable_fork_handlers()")
    assert needed.returncode != 0 and "grpcio is not installed" in needed.stderr

    # 顶层包存在但包内模块缺失：即使 required 为假也原样失败，变量恢复原状。
    broken = tmp_path / "broken" / "grpc"
    broken.mkdir(parents=True)
    (broken / "__init__.py").write_text("import grpc._cython_missing\n")
    failed = child("import os, grpc_fork\ntry:\n    grpc_fork.disable_fork_handlers(required=False)\n"
                   "except ModuleNotFoundError as e:\n    print('raised', e.name, os.environ.get('GRPC_ENABLE_FORK_SUPPORT'))\n",
                   broken.parent)
    assert failed.returncode == 0 and failed.stdout.strip() == "raised grpc._cython_missing None", failed.stderr

    late = child("import sys, grpc_fork\nsys.modules['grpc'] = object()\ngrpc_fork.disable_fork_handlers(required=False)")
    assert late.returncode != 0 and "can no longer change this process" in late.stderr

    if INSTALLED:
        kept = child("import os, grpc_fork\nassert grpc_fork.disable_fork_handlers() is True\n"
                     "assert grpc_fork.disable_fork_handlers() is True\nimport sys; assert 'grpc' in sys.modules\n"
                     "print(os.environ['GRPC_ENABLE_FORK_SUPPORT'])", site=True, GRPC_ENABLE_FORK_SUPPORT="1")
        assert kept.returncode == 0 and kept.stdout.strip() == "1", kept.stderr
