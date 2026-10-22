"""源码身份生成与工作树核对的变异用例：软链、未跟踪输入、文件顺序、内容与模式变化都必须改变身份或被拒绝。"""

import json
import os
from pathlib import Path
import subprocess

import pytest

import source_identity as si


def git(repo, *args):
    return subprocess.run(["git", "-C", repo, "-c", "user.name=Test", "-c", "user.email=test@example.invalid", *args],
                          check=True, capture_output=True, text=True).stdout.strip()


@pytest.fixture
def repo(tmp_path):
    repo = tmp_path / "repo"
    (repo / "crates/core/src").mkdir(parents=True)
    (repo / "docs").mkdir()
    for name in si.ROOT_INPUTS:
        (repo / name).write_text(f"# {name}\n")
    (repo / "Cargo.toml").write_text('[workspace.package]\nversion = "1.2.0-rc.3"\n')
    (repo / "crates/core/src/lib.rs").write_text("pub fn f() {}\n")
    (repo / "crates/core/build.sh").write_text("#!/bin/sh\n")
    (repo / "crates/core/build.sh").chmod(0o755)
    os.symlink("src/lib.rs", repo / "crates/core/link.rs")
    (repo / "docs/a.md").write_text("a\n")
    subprocess.run(["git", "init", "-q", repo], check=True)
    git(repo, "add", "-A")
    git(repo, "commit", "-qm", "fixture")
    return repo


def generate(repo, out, version="1.2.0rc3"):
    return si.generate(repo, git(repo, "rev-parse", "HEAD"), version, out)


def test_manifest_rules(repo, tmp_path):
    identity = generate(repo, tmp_path / "id")
    source = json.loads((tmp_path / "id/source-inputs.json").read_text())
    product = json.loads((tmp_path / "id/product-inputs.json").read_text())
    assert identity["source_entries"] == len(source) == 9
    assert identity["symlinks"] == 1 and identity["regular_files"] == 8
    assert source["crates/core/link.rs"] == {"mode": "120000", "sha256": si.sha256(b"src/lib.rs")}
    assert source["crates/core/build.sh"]["mode"] == "100755"
    assert set(product) == set(si.ROOT_INPUTS) | {"crates/core/src/lib.rs", "crates/core/build.sh", "crates/core/link.rs"}
    assert "docs/a.md" not in product
    assert identity["product_manifest_sha256"] == si.sha256((tmp_path / "id/product-inputs.json").read_bytes())
    assert identity["archive_sha256"] == si.sha256((tmp_path / "id/source.tar").read_bytes())
    assert set(identity["tool_versions"]) == {"git", "python"} and identity["tool_versions"]["git"].startswith("git version ")
    assert si.check_worktree(repo, source) == 8


def test_manifest_hash_does_not_depend_on_order():
    entries = {"b": "2", "a": "1"}
    assert si.canonical(entries) == si.canonical(dict(reversed(list(entries.items()))))


@pytest.mark.parametrize("mutate", [
    lambda r: (r / "crates/core/src/lib.rs").write_text("pub fn g() {}\n"),
    lambda r: ((r / "crates/core/link.rs").unlink(), os.symlink("build.sh", r / "crates/core/link.rs")),
    lambda r: (r / "crates/core/src/new.rs").write_text("\n"),
    lambda r: (r / "rust-toolchain.toml").write_text("changed\n"),
])
def test_committed_changes_change_identity(repo, tmp_path, mutate):
    before = generate(repo, tmp_path / "before")
    mutate(repo)
    git(repo, "add", "-A")
    git(repo, "commit", "-qm", "change")
    after = generate(repo, tmp_path / "after")
    assert after["product_manifest_sha256"] != before["product_manifest_sha256"]
    assert after["source_manifest_sha256"] != before["source_manifest_sha256"]


def test_mode_change_is_recorded_in_source_manifest_only(repo, tmp_path):
    # 产品清单沿用 rc2 的格式只记内容，便于与以往候选比较；执行位由源树清单与工作树核对覆盖。
    before = generate(repo, tmp_path / "before")
    (repo / "crates/core/build.sh").chmod(0o644)
    git(repo, "commit", "-qam", "mode")
    after = generate(repo, tmp_path / "after")
    assert after["product_manifest_sha256"] == before["product_manifest_sha256"]
    assert after["source_manifest_sha256"] != before["source_manifest_sha256"]


def test_non_product_change_keeps_product_identity(repo, tmp_path):
    before = generate(repo, tmp_path / "before")
    (repo / "docs/a.md").write_text("b\n")
    git(repo, "commit", "-qam", "docs")
    after = generate(repo, tmp_path / "after")
    assert after["product_manifest_sha256"] == before["product_manifest_sha256"]
    assert after["source_manifest_sha256"] != before["source_manifest_sha256"]


@pytest.mark.parametrize("mutate, message", [
    (lambda r: (r / "crates/core/src/extra.rs").write_text("\n"), "untracked build input crates/core/src/extra.rs"),
    (lambda r: ((r / ".gitignore").write_text("*.so\n"), (r / "crates/core/x.so").write_text("")), "untracked build input crates/core/x.so"),
    (lambda r: os.symlink("src", r / "crates/core/dirlink"), "untracked build input crates/core/dirlink"),
    (lambda r: (r / "crates/core/src/lib.rs").write_text("changed\n"), "differs from frozen commit crates/core/src/lib.rs"),
    (lambda r: (r / "crates/core/build.sh").chmod(0o644), "differs from frozen commit crates/core/build.sh"),
    (lambda r: ((r / "crates/core/link.rs").unlink(), os.symlink("build.sh", r / "crates/core/link.rs")), "differs from frozen commit crates/core/link.rs"),
    (lambda r: (r / "Cargo.lock").unlink(), "missing build input Cargo.lock"),
])
def test_worktree_check_rejects(repo, tmp_path, mutate, message):
    generate(repo, tmp_path / "id")
    source = json.loads((tmp_path / "id/source-inputs.json").read_text())
    mutate(repo)
    with pytest.raises(si.IdentityError, match=message):
        si.check_worktree(repo, source)


def test_worktree_check_ignores_non_product_paths(repo, tmp_path):
    generate(repo, tmp_path / "id")
    source = json.loads((tmp_path / "id/source-inputs.json").read_text())
    (repo / "docs/untracked.md").write_text("\n")
    (repo / "target").mkdir()
    (repo / "target/out").write_text("\n")
    assert si.check_worktree(repo, source) == 8


def test_rejects_version_mismatch_and_short_sha(repo, tmp_path):
    with pytest.raises(si.IdentityError, match="does not match"):
        generate(repo, tmp_path / "a", version="1.2.0rc4")
    with pytest.raises(si.IdentityError, match="full SHA"):
        si.generate(repo, git(repo, "rev-parse", "--short", "HEAD"), "1.2.0rc3", tmp_path / "b")
    assert not (tmp_path / "a").exists() and not (tmp_path / "b").exists()


@pytest.mark.parametrize("cargo, python", [("0.3.0-rc.2", "0.3.0rc2"), ("1.0.0", "1.0.0"), ("2.1.3-b.1", "2.1.3b1")])
def test_cargo_version_mapping(cargo, python):
    assert si.cargo_version_to_python(cargo) == python


@pytest.mark.parametrize("cargo", ["0.3.0-rc2", "0.3.0-dev.1", "0.3"])
def test_cargo_version_mapping_rejects_unknown(cargo):
    with pytest.raises(si.IdentityError):
        si.cargo_version_to_python(cargo)


def test_cli_reports_failure(repo, tmp_path, capsys):
    assert si.main(["generate", "--repo", str(repo), "--commit", git(repo, "rev-parse", "HEAD"),
                    "--version", "9.9.9", "--out", str(tmp_path / "id")]) == 1
    assert "source identity failed" in capsys.readouterr().err
