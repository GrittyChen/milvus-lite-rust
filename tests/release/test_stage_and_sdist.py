"""阶段文件只记录符合矩阵的通过阶段；sdist 检查拒绝多余目录、缺失或改动的构建输入，以及非预期原因的失败。"""

import hashlib
import io
import json
import tarfile

import pytest

import candidate as c
import scorecard
import sdist_checks as sc
import stage

ENV = {k: "v" for k in scorecard.environment_keys("linux-x86_64")}


def test_write_stage_and_merge(tmp_path):
    stage.write(tmp_path, "linux-x86_64", "interop", {"collected": 211, "executed": 211, "group": 0}, ENV)
    stage.write(tmp_path, "sdist", "build-root", {}, ENV)
    merged = c.merge_stages(sorted(tmp_path.iterdir()))
    assert merged["linux-x86_64"]["interop"]["counts"]["collected"] == 211
    assert merged["sdist"]["build-root"] == {"returncode": 0, "counts": {}, "environment": ENV}


@pytest.mark.parametrize("scope, name, counts, message", [
    ("linux-x86_64", "interop", {"collected": 210, "executed": 210, "group": 0}, "do not match"),
    ("linux-x86_64", "interop", {"collected": 211, "executed": 211}, "do not match"),
    ("macos-arm64", "pid1", {}, "unknown stage"),
    ("nowhere", "x", {}, "unknown stage"),
])
def test_write_stage_rejects(tmp_path, scope, name, counts, message):
    with pytest.raises(c.CandidateError, match=message):
        stage.write(tmp_path, scope, name, counts, ENV)
    assert not any(tmp_path.iterdir())


def test_write_stage_never_overwrites(tmp_path):
    stage.write(tmp_path, "sdist", "build-root", {}, ENV)
    with pytest.raises(FileExistsError):
        stage.write(tmp_path, "sdist", "build-root", {}, ENV)


def test_environment_has_matrix_keys():
    for scope in ("linux-x86_64", "macos-arm64", "sdist"):
        assert set(stage.environment(scope)) == set(scorecard.environment_keys(scope))


def test_summary_counts(tmp_path):
    path = tmp_path / "summary.json"
    path.write_text(json.dumps({"passed": True, "counts": {"collected": 5, "executed": 5, "group": 0}, "problems": []}))
    assert stage.summary_counts(path) == {"collected": 5, "executed": 5, "group": 0}
    path.write_text(json.dumps({"passed": False, "counts": {"collected": 5, "executed": 4, "group": 0}, "problems": ["x"]}))
    with pytest.raises(c.CandidateError, match="did not pass"):
        stage.summary_counts(path)


def make_sdist(tmp_path, files, top="milvus_lite_rust-1.0.0"):
    path = tmp_path / "s.tar.gz"
    with tarfile.open(path, "w:gz") as bundle:
        for name, data in files.items():
            info = tarfile.TarInfo(f"{top}/{name}" if top else name)
            info.size = len(data)
            bundle.addfile(info, io.BytesIO(data))
    return path


PRODUCT = {"Cargo.toml": b"cargo", "crates/a.rs": b"a", "clippy.toml": b"c", "rust-toolchain.toml": b"t"}


def digests(files):
    return {name: hashlib.sha256(data).hexdigest() for name, data in files.items()}


def test_unpack_and_check_inputs(tmp_path):
    files = {k: v for k, v in PRODUCT.items() if k not in sc.EXCLUDED_FROM_SDIST} | {"README.md": b"r"}
    source = sc.unpack(make_sdist(tmp_path, files), "1.0.0", tmp_path)
    assert sc.check_inputs(source, digests(PRODUCT)) == 2


@pytest.mark.parametrize("files, message", [
    ({"Cargo.toml": b"cargo"}, "differs from frozen commit: crates/a.rs"),
    ({"Cargo.toml": b"cargo", "crates/a.rs": b"changed"}, "differs from frozen commit: crates/a.rs"),
    ({"Cargo.toml": b"cargo", "crates/a.rs": b"a", "rust-toolchain.toml": b"t"}, "must not contain rust-toolchain.toml"),
])
def test_check_inputs_rejects(tmp_path, files, message):
    source = sc.unpack(make_sdist(tmp_path, files), "1.0.0", tmp_path)
    with pytest.raises(SystemExit, match=message):
        sc.check_inputs(source, digests(PRODUCT))


@pytest.mark.parametrize("top", ["milvus_lite_rust-1.0.1", None])
def test_unpack_rejects_unexpected_layout(tmp_path, top):
    with pytest.raises(SystemExit, match="single milvus_lite_rust-1.0.0"):
        sc.unpack(make_sdist(tmp_path, {"Cargo.toml": b"x"}, top=top), "1.0.0", tmp_path)


def test_runner_requires_specific_rejection(tmp_path):
    run = sc.Runner(tmp_path, {"PATH": "/usr/bin:/bin"})
    run("ok", ["true"], tmp_path)
    with pytest.raises(SystemExit, match="failed"):
        run("bad", ["false"], tmp_path)
    reason = sc.REJECTIONS["reject-pypy"]
    run("rejected", ["sh", "-c", f"echo 'milvus-lite-rust build rejected: {reason}'; exit 101"], tmp_path, expect_failure=reason)
    for name, script in [("other-error", "echo 'error[E0425]'; exit 101"),
                         ("wrong-reason", f"echo 'milvus-lite-rust build rejected: {sc.REJECTIONS['reject-windows']}'; exit 101"),
                         ("succeeded", f"echo 'milvus-lite-rust build rejected: {reason}'; exit 0")]:
        with pytest.raises(SystemExit, match="was not rejected"):
            run(name, ["sh", "-c", script], tmp_path, expect_failure=reason)


def test_runner_drops_unset_variables(tmp_path):
    run = sc.Runner(tmp_path, {"PATH": "/usr/bin:/bin", "PYO3_PYTHON": "x"})
    assert "PYO3_PYTHON" not in run("env", ["env"], tmp_path, {"PYO3_PYTHON": None})


def test_cargo_counts(tmp_path):
    log = tmp_path / "cargo.log"
    # crash_recovery 形态：两个子进程的汇总先于所属目标的汇总出现，只计目标自己的一行。
    log.write_text("     Running unittests src/lib.rs (target/debug/deps/core-1)\n"
                   "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured\nnoise\n"
                   "     Running tests/crash_recovery.rs (target/debug/deps/crash_recovery-2)\n"
                   "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 6 filtered out\n"
                   "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 6 filtered out\n"
                   "test result: ok. 4 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\n"
                   "   Doc-tests core\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured\n")
    assert stage.cargo_counts(log) == {"passed": 7, "failed": 0, "ignored": 1}
    with pytest.raises(c.CandidateError, match="ignored"):
        stage.write(tmp_path / "s", "linux-x86_64", "rust-default", stage.cargo_counts(log), ENV)
    log.write_text("     Running tests/a.rs (x)\ntest result: FAILED. 3 passed; 1 failed; 0 ignored;\n")
    with pytest.raises(c.CandidateError, match="failing"):
        stage.cargo_counts(log)
    log.write_text("     Running tests/a.rs (x)\ntest result: ok. 3 passed; 0 failed; 0 ignored;\n"
                   "     Running tests/b.rs (y)\nthread panicked\n")
    with pytest.raises(c.CandidateError, match="without a summary: tests/b.rs"):
        stage.cargo_counts(log)
    for text in ("no summary\n", "test result: ok. 3 passed; 0 failed; 0 ignored;\n"):
        log.write_text(text)
        with pytest.raises(c.CandidateError, match="no cargo test summary"):
            stage.cargo_counts(log)


def test_acceptance_counts(tmp_path):
    out = tmp_path / "out.json"
    out.write_text("noise\n" + json.dumps({"passed": True, "suite": {"collected": 473, "executed": 473, "group": 0}}) + "\n")
    assert stage.acceptance_counts(out) == {"collected": 473, "executed": 473, "group": 0}
    out.write_text(json.dumps({"passed": True, "suite": None}) + "\n")
    with pytest.raises(c.CandidateError, match="not a passing suite"):
        stage.acceptance_counts(out)


def test_runner_image_override(monkeypatch):
    monkeypatch.setenv("RUNNER_IMAGE", "ubuntu24/20261020 container quay.io/x@sha256:abc")
    assert stage.environment("linux-x86_64")["runner_image"].startswith("ubuntu24/")


def test_pid1_image_environment(tmp_path, monkeypatch):
    monkeypatch.delenv("PID1_IDENTITY", raising=False)
    assert stage.environment("linux-x86_64")["pid1_image"] == scorecard.NOT_USED
    identity = tmp_path / "identity.json"
    identity.write_text(json.dumps({"base_image": scorecard.PID1_IMAGES["linux-x86_64"], "base_image_id": "sha256:" + "e" * 64}))
    monkeypatch.setenv("PID1_IDENTITY", str(identity))
    env = stage.environment("linux-x86_64")
    assert env["pid1_image"] == f"{scorecard.PID1_IMAGES['linux-x86_64']} id sha256:{'e' * 64}"
    result = {"returncode": 0, "counts": {}, "environment": env}
    c.check_stage("linux-x86_64", "pid1", result)
    with pytest.raises(c.CandidateError, match="pinned"):
        c.check_stage("linux-aarch64", "pid1", result)
    assert "pid1_image" not in stage.environment("macos-arm64")


def test_orjson_environment(tmp_path, monkeypatch):
    monkeypatch.delenv("ORJSON_RECORD", raising=False)
    assert stage.environment("linux-aarch64")["orjson_cp315"] == "not-used"
    record = tmp_path / "record.json"
    record.write_text(json.dumps({"sdist": {"sha256": "d" * 64, "version": "3.13.0"},
                                  "wheel": {"name": "orjson-3.13.0-cp315-cp315-manylinux_2_28_aarch64.whl", "sha256": "c" * 64,
                                            "elf_machine": "aarch64", "max_glibc": "2.28"},
                                  "toolchain": {"rustc": "rustc 1.98.1", "maturin": "maturin 1.15.0"}}))
    monkeypatch.setenv("ORJSON_RECORD", str(record))
    value = stage.environment("linux-aarch64")["orjson_cp315"]
    assert value.startswith("orjson-3.13.0-cp315") and "max-glibc 2.28" in value and "maturin 1.15.0" in value
    assert "orjson_cp315" not in stage.environment("macos-arm64")


def test_build_environment(tmp_path):
    maturin = tmp_path / "maturin"
    maturin.write_text("#!/bin/sh\necho 'maturin 1.15.0'\n")
    maturin.chmod(0o755)
    values = stage.build_environment("macos-arm64" if __import__("sys").platform == "darwin" else "linux-x86_64", maturin)
    assert values["maturin"] == "maturin 1.15.0" and values["cargo"].startswith("cargo ")
    with pytest.raises(c.CandidateError, match="build tools missing"):
        stage.build_environment("linux-x86_64", tmp_path / "missing-maturin")
