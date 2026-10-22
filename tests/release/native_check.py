"""核对环境实际加载的原生模块与给定 wheel 中的一致，用于工作流自行组装的测试环境（差分、互通、FI 恢复）。"""

import argparse
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "packaging"))
from run_acceptance import installed_native, wheel_native  # noqa: E402


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--python", required=True)
    parser.add_argument("--wheel", type=Path, required=True)
    args = parser.parse_args(argv)
    expected, actual = wheel_native(args.wheel), installed_native(args.python, Path.cwd())
    if expected != actual:
        print(f"environment loads native module {actual}, wheel contains {expected}", file=sys.stderr)
        return 1
    print(f"native module matches wheel: {actual}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
