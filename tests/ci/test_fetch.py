"""内容由 sha256 固定的下载与 uv 固定清单（0021 第 11 条传输层有限重试）的反证。"""

import hashlib
from pathlib import Path
import re
import subprocess
import sys

import pytest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / ".github" / "ci"))

import fetch  # noqa: E402

LIST = ROOT / ".github" / "ci" / "requirements" / "uv-wheels.txt"
CONTENT = b"fixed bytes"
DIGEST = hashlib.sha256(CONTENT).hexdigest()


class Curl:
    """按顺序返回预设结果：None 表示写出 CONTENT 并成功，字符串表示以该 stderr 失败。"""

    def __init__(self, *outcomes, content=CONTENT):
        self.outcomes = list(outcomes)
        self.content = content
        self.calls = 0

    def __call__(self, command, **_):
        self.calls += 1
        outcome = self.outcomes.pop(0)
        if outcome is None:
            Path(command[command.index("-o") + 1]).write_bytes(self.content)
            return subprocess.CompletedProcess(command, 0, "", "")
        return subprocess.CompletedProcess(command, 56, "", outcome)


def test_retries_transport_failures_then_verifies(tmp_path, capsys):
    curl, slept = Curl("curl: (6) Could not resolve host", "curl: (18) transfer closed", None), []
    out = fetch.download("https://example.invalid/f.whl", DIGEST, tmp_path / "d" / "f.whl", run=curl, sleep=slept.append)
    assert out.read_bytes() == CONTENT and curl.calls == 3 and slept == [5, 15]
    notices = [line for line in capsys.readouterr().err.splitlines() if "transport-retry" in line]
    assert len(notices) == 2 and "attempt 1" in notices[0] and "Could not resolve host" in notices[0]


def test_gives_up_after_the_limit(tmp_path):
    curl, slept = Curl(*["curl: (7) Failed to connect"] * 4), []
    with pytest.raises(fetch.FetchError, match="after 4 attempts"):
        fetch.download("https://example.invalid/f.whl", DIGEST, tmp_path / "f.whl", run=curl, sleep=slept.append)
    assert curl.calls == 4 and slept == [5, 15, 45]
    assert not (tmp_path / "f.whl").exists()


def test_hash_mismatch_fails_without_retry(tmp_path):
    curl, slept = Curl(None, None, content=b"other bytes"), []
    with pytest.raises(fetch.FetchError, match="expected"):
        fetch.download("https://example.invalid/f.whl", DIGEST, tmp_path / "f.whl", run=curl, sleep=slept.append)
    assert curl.calls == 1 and slept == []
    assert list(tmp_path.iterdir()) == []


def write_list(path, lines):
    path.write_text("# comment\n" + "".join(line + "\n" for line in lines))
    return path


def entry(key, version="1.2.3", digest="a" * 64):
    name = f"uv-{version}-py3-none-{key}.whl"
    return f"{key} {name} https://files.example/x/{name} {digest}"


KEYS = ["linux-x86_64", "linux-aarch64", "macos-arm64", "macos-x86_64"]


def test_uv_entry_picks_the_platform(tmp_path):
    path = write_list(tmp_path / "uv.txt", [entry(key) for key in KEYS])
    assert fetch.uv_entry(path, "1.2.3", "Linux", "aarch64")[0] == "uv-1.2.3-py3-none-linux-aarch64.whl"
    assert fetch.uv_entry(path, "1.2.3", "Darwin", "x86_64")[0] == "uv-1.2.3-py3-none-macos-x86_64.whl"
    with pytest.raises(fetch.FetchError, match="no uv wheel"):
        fetch.uv_entry(path, "1.2.3", "Windows", "AMD64")


@pytest.mark.parametrize("lines, message", [
    ([entry(key) for key in KEYS[:3]], "platforms"),
    ([entry(key) for key in KEYS] + [entry(KEYS[0])], "one line per platform"),
    ([entry(key) for key in KEYS[1:]] + [entry(KEYS[0], digest="a" * 63)], "64 hex"),
    ([entry(key) for key in KEYS[1:]] + [entry(KEYS[0], digest="g" * 64)], "64 hex"),
    ([entry(key) for key in KEYS[1:]] + [entry(KEYS[0]).replace("https://files.example/x/", "https://files.example/y")], "url"),
])
def test_uv_list_rejects_malformed(tmp_path, lines, message):
    with pytest.raises(fetch.FetchError, match=message):
        fetch.uv_wheels(write_list(tmp_path / "uv.txt", lines))


def test_uv_entry_rejects_other_version(tmp_path):
    path = write_list(tmp_path / "uv.txt", [entry(key) for key in KEYS[1:]] + [entry(KEYS[0], version="9.9.9")])
    with pytest.raises(fetch.FetchError, match="is not uv 1.2.3"):
        fetch.uv_entry(path, "1.2.3", "Linux", "aarch64")


def test_repository_list_matches_workflow_versions():
    entries = fetch.uv_wheels(LIST)
    versions = set()
    for workflow in ("ci.yml", "compare.yml", "release.yml"):
        found = re.findall(r'^\s*UV_VERSION:\s*"?([0-9][^"\s]*)"?\s*$',
                           (ROOT / ".github" / "workflows" / workflow).read_text(), re.M)
        assert len(found) == 1, workflow
        versions.add(found[0])
    assert len(versions) == 1
    version = versions.pop()
    assert all(filename.startswith(f"uv-{version}-py3-none-") for filename, _, _ in entries.values())
    assert all(url.startswith("https://files.pythonhosted.org/packages/") for _, url, _ in entries.values())
