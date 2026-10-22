"""工作流内部交接的摘要清单：文件多出、缺失、改动或清单格式异常都拒绝。"""

import pytest

import sums


@pytest.fixture
def tree(tmp_path):
    root = tmp_path / "dist"
    (root / "formal").mkdir(parents=True)
    (root / "formal/a.whl").write_bytes(b"a")
    (root / "fi").mkdir()
    (root / "fi/a.whl").write_bytes(b"fi")
    return root


def test_write_and_check(tree, capsys):
    sums.write(tree, tree / "build.sha256")
    text = (tree / "build.sha256").read_text()
    assert "  fi/a.whl\n" in text and "  formal/a.whl\n" in text and "build.sha256" not in text
    sums.check(tree / "build.sha256", tree)
    assert "2 files match" in capsys.readouterr().out


@pytest.mark.parametrize("mutate, message", [
    (lambda t: (t / "formal/a.whl").write_bytes(b"changed"), "checksum mismatch"),
    (lambda t: (t / "formal/extra.whl").write_bytes(b""), "extra"),
    (lambda t: (t / "fi/a.whl").unlink(), "missing"),
    (lambda t: (t / "build.sha256").write_text("x  ../escape\n"), "malformed"),
])
def test_check_rejects(tree, mutate, message):
    sums.write(tree, tree / "build.sha256")
    mutate(tree)
    with pytest.raises(SystemExit, match=message):
        sums.check(tree / "build.sha256", tree)
