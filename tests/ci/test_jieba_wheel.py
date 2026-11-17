"""参照环境的 jieba 受控构建（0036 决策 G）只接受固定哈希的 sdist，测试 job 只接受本次构建的 wheel 与固定的词典。"""

import hashlib
import json
from pathlib import Path
import sys

import pytest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / ".github" / "ci"))

import jieba_wheel as jw  # noqa: E402


def test_pins_match_the_vendored_dictionary():
    data = ROOT / "crates" / "core" / "data" / "jieba"
    assert hashlib.sha256((data / "dict.txt").read_bytes()).hexdigest() == jw.DICT_SHA256
    readme = (data / "README.md").read_text(encoding="utf-8")
    assert jw.SDIST_SHA256 in readme and jw.DICT_SHA256 in readme


def test_rejects_unpinned_sdist(tmp_path):
    sdist = tmp_path / "jieba-0.42.1.tar.gz"
    sdist.write_bytes(b"not the pinned archive")
    with pytest.raises(SystemExit, match="not the pinned"):
        jw.build(sdist, "uv", "python3.12", tmp_path / "out")
    assert not (tmp_path / "out").exists()


def fake_python(tmp_path, version, dictionary):
    package = tmp_path / "jieba"
    package.mkdir(parents=True)
    (package / "dict.txt").write_bytes(dictionary)
    script = tmp_path / "python"
    script.write_text(f"#!/bin/sh\necho '{json.dumps({'version': version, 'file': str(package / '__init__.py')})}'\n")
    script.chmod(0o755)
    return str(script)


def test_check_installed(tmp_path):
    record = {"wheel": {"name": jw.WHEEL, "dict_sha256": hashlib.sha256(b"dict").hexdigest()}}
    assert jw.check_installed(record, fake_python(tmp_path / "ok", "0.42.1", b"dict"))["version"] == "0.42.1"
    for i, (version, dictionary) in enumerate([("0.42", b"dict"), ("0.42.1", b"other")]):
        with pytest.raises(SystemExit, match="not the controlled build"):
            jw.check_installed(record, fake_python(tmp_path / str(i), version, dictionary))


def test_check_installed_rejects_another_wheel(tmp_path):
    wheel = tmp_path / jw.WHEEL
    wheel.write_bytes(b"rebuilt")
    record = tmp_path / "record.json"
    record.write_text(json.dumps({"wheel": {"sha256": hashlib.sha256(b"built").hexdigest()}}))
    with pytest.raises(SystemExit, match="not the wheel built by the jieba job"):
        jw.main(["check-installed", "--record", str(record), "--wheel", str(wheel), "--python", sys.executable])


def test_pinned_setuptools(tmp_path):
    assert jw.pinned_setuptools() == "84.0.0"
    constraints = tmp_path / "build.txt"
    constraints.write_text("hatchling==1.0\n")
    with pytest.raises(SystemExit, match="must pin setuptools exactly once"):
        jw.pinned_setuptools(constraints)


def fake_sdist(tmp_path, monkeypatch):
    import io
    import tarfile
    sdist = tmp_path / "jieba-0.42.1.tar.gz"
    with tarfile.open(sdist, "w:gz") as bundle:
        member = tarfile.TarInfo("jieba-0.42.1/test/test.py")
        member.size = len(b"cuttest('x')\n")
        bundle.addfile(member, io.BytesIO(b"cuttest('x')\n"))
    monkeypatch.setattr(jw, "SDIST_SHA256", hashlib.sha256(sdist.read_bytes()).hexdigest())
    return sdist


def fake_uv(tmp_path, generator, dictionary):
    """写出一个假的 `uv build`：在 `-o` 目录放一个只含 WHEEL 元数据与词典的 wheel。"""
    script = tmp_path / "uv"
    script.write_text(f"""#!{sys.executable}
import sys, zipfile
from pathlib import Path
out = Path(sys.argv[sys.argv.index("-o") + 1]); out.mkdir(parents=True)
with zipfile.ZipFile(out / "{jw.WHEEL}", "w") as z:
    z.writestr("jieba-0.42.1.dist-info/WHEEL", "Wheel-Version: 1.0\\nGenerator: {generator}\\n")
    z.writestr("jieba/dict.txt", {dictionary!r})
""")
    script.chmod(0o755)
    return str(script)


def test_build_records_the_wheel_and_extracts_test_py(tmp_path, monkeypatch):
    sdist = fake_sdist(tmp_path, monkeypatch)
    monkeypatch.setattr(jw, "DICT_SHA256", hashlib.sha256(b"dict").hexdigest())
    record = jw.build(sdist, fake_uv(tmp_path, "setuptools (84.0.0)", b"dict"), "python3.12", tmp_path / "out")
    assert sorted(p.name for p in (tmp_path / "out").iterdir()) == [jw.WHEEL, "test.py"]
    assert (tmp_path / "out" / "test.py").read_bytes() == b"cuttest('x')\n"
    assert record["wheel"]["generator"] == ["Generator: setuptools (84.0.0)"]
    assert record["wheel"]["sha256"] == hashlib.sha256((tmp_path / "out" / jw.WHEEL).read_bytes()).hexdigest()


@pytest.mark.parametrize("generator, dictionary, message", [
    ("setuptools (84.2.1)", b"dict", "not the pinned setuptools"),
    ("setuptools (84.0.0)", b"other", "not the pinned dictionary"),
])
def test_build_rejects_unpinned_backend_or_dictionary(tmp_path, monkeypatch, generator, dictionary, message):
    sdist = fake_sdist(tmp_path, monkeypatch)
    monkeypatch.setattr(jw, "DICT_SHA256", hashlib.sha256(b"dict").hexdigest())
    with pytest.raises(SystemExit, match=message):
        jw.build(sdist, fake_uv(tmp_path, generator, dictionary), "python3.12", tmp_path / "out")


def test_check_installed_reports_import_errors(tmp_path):
    script = tmp_path / "python"
    script.write_text("#!/bin/sh\necho 'ModuleNotFoundError: jieba' >&2\nexit 1\n")
    script.chmod(0o755)
    with pytest.raises(SystemExit, match="(?s)cannot import jieba.*ModuleNotFoundError"):
        jw.check_installed({"wheel": {"name": jw.WHEEL, "dict_sha256": "x"}}, str(script))
