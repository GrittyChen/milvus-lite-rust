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


def test_tested_output_passes_the_candidate_cross_check(tmp_path, capsys):
    """sums.py tested 的真实输出须能通过 candidate.py tested 的成对核对（0039 第 8 条），两者经 CLI 衔接。"""
    import candidate as c
    version = "1.2.0rc3"
    formal, fi, stages = tmp_path / "formal", tmp_path / "fi", tmp_path / "stages"
    formal.mkdir()
    stages.mkdir()
    sdist = formal / f"milvus_lite_rust-{version}.tar.gz"
    sdist.write_bytes(b"sdist")
    for scope in c.SCOPE_TAGS:
        name = c.wheel_name(version, scope)
        (formal / name).write_bytes(f"formal {scope}".encode())
        (fi / f"build-{scope}").mkdir(parents=True)
        (fi / f"build-{scope}" / name).write_bytes(f"fi {scope}".encode())
    sums.write(formal, tmp_path / "formal.sha256")
    sums.write(fi, tmp_path / "fi.sha256")
    for scope in c.SCOPE_TAGS:
        for part in c.TEST_PARTS:
            name = c.wheel_name(version, scope)
            sums.tested(stages / c.tested_name(scope, part), scope, formal / name, fi / f"build-{scope}" / name, sdist)
    argv = ["tested", "--dir", str(stages), "--formal-sums", str(tmp_path / "formal.sha256"),
            "--fi-sums", str(tmp_path / "fi.sha256"), "--version", version]
    assert c.main(argv) == 0
    assert "24 files" in capsys.readouterr().out
    # 某个 job 用错平台的 FI wheel：成对核对拒绝。
    scope = "macos-arm64"
    name = c.wheel_name(version, scope)
    other = fi / "build-macos-x86_64" / c.wheel_name(version, "macos-x86_64")
    sums.tested(stages / c.tested_name(scope, "fi"), scope, formal / name, other, sdist)
    assert c.main(argv) == 1


def test_wheel_tags_literal_matches_the_scope_map():
    import candidate as c
    assert c.WHEEL_TAGS == tuple(sorted(c.SCOPE_TAGS.values()))
