"""修改工作树的基准脚本不得改变两份历史测量的自证输入。"""
from pathlib import Path
import shutil
import subprocess

from test_m2_benchmark import assert_m2_historical_fingerprint
from test_s1_benchmark import assert_s1_historical_fingerprint


def test_historical_inputs_survive_changed_removed_and_added_working_scripts(tmp_path):
    repo = Path(__file__).resolve().parents[2]
    checkout = tmp_path / "checkout"
    # 无 checkout 的本地副本共享历史对象，不复制用户的工作树或访问远端。
    subprocess.run(["git", "clone", "--quiet", "--shared", "--no-checkout", str(repo), str(checkout)],
                   check=True, capture_output=True, text=True)
    baseline = checkout / "benchmarks/baseline"
    baseline.mkdir(parents=True)
    (baseline / "bench.py").write_text("changed working copy\n")
    (baseline / "new_normal_script.py").write_text("new working script\n")
    assert not (baseline / "run.sh").exists()
    shutil.copy2(repo / "benchmarks/baseline/measured-20261008.patch", baseline)
    s1 = checkout / "benchmarks/s1"
    s1.mkdir()
    for name in ["run.py", "measured-20261008.patch"]:
        shutil.copy2(repo / "benchmarks/s1" / name, s1 / name)
    assert_m2_historical_fingerprint(checkout, tmp_path / "restored-m2")
    assert_s1_historical_fingerprint(checkout, tmp_path / "restored-s1")
    assert (baseline / "bench.py").read_text() == "changed working copy\n"
    assert (baseline / "new_normal_script.py").read_text() == "new working script\n"
    assert not (baseline / "run.sh").exists()
