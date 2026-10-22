"""orjson 受控构建只接受固定哈希的 sdist、64 位目标架构，以及不超过 GLIBC 2.28 的依赖。"""

import hashlib

import pytest

import orjson_wheel as ow


def elf(machine, order="little"):
    header = bytearray(64)
    header[:4], header[4], header[5] = b"\x7fELF", 2, 1 if order == "little" else 2
    header[18:20] = machine.to_bytes(2, order)
    return bytes(header)


def test_elf_machine():
    assert ow.elf_machine(elf(0x3E)) == "x86_64"
    assert ow.elf_machine(elf(0xB7)) == "aarch64"
    for data, message in [(elf(0x28), "unexpected ELF machine"), (b"MZ" + bytes(62), "not a 64-bit ELF"),
                          (elf(0x3E)[:4] + b"\x01" + elf(0x3E)[5:], "not a 64-bit ELF")]:
        with pytest.raises(SystemExit, match=message):
            ow.elf_machine(data)


def test_max_glibc():
    dump = "0000 DF *UND* GLIBC_2.17 memcpy\n0000 DF *UND* GLIBC_2.28 fcntl64\n0000 DF *UND* GLIBC_2.3.4 x\n"
    assert ow.max_glibc(dump) == (2, 28)
    assert ow.max_glibc(dump + "0000 DF *UND* GLIBC_2.34 pthread_create\n") > ow.MAX_GLIBC
    with pytest.raises(SystemExit, match="no GLIBC"):
        ow.max_glibc("0000 DF *UND* CXXABI_1.3 x\n")


def test_rejects_unpinned_sdist(tmp_path):
    sdist = tmp_path / "orjson-3.13.0.tar.gz"
    sdist.write_bytes(b"not the pinned archive")
    with pytest.raises(SystemExit, match="not the pinned"):
        ow.build(sdist, "python3.15", "maturin", tmp_path / "work", tmp_path / "out")
    assert not (tmp_path / "work").exists()


def fake_python(tmp_path, version, native_name, content):
    native = tmp_path / native_name
    native.write_bytes(content)
    script = tmp_path / "python"
    script.write_text(f"#!/bin/sh\necho '{{\"version\": \"{version}\", \"file\": \"{native}\"}}'\n")
    script.chmod(0o755)
    return str(script)


def test_check_installed(tmp_path):
    so = "orjson.cpython-315-x86_64-linux-gnu.so"
    record = {"wheel": {"name": "orjson-3.13.0-cp315-cp315-manylinux_2_28_x86_64.whl", "native": f"orjson/{so}",
                        "native_sha256": hashlib.sha256(b"built").hexdigest()}}
    assert ow.check_installed(record, fake_python(tmp_path, "3.13.0", so, b"built"))["version"] == "3.13.0"
    for version, name, content in [("3.13.0", so, b"pypi"), ("3.12.0", so, b"built"),
                                   ("3.13.0", "orjson.abi3.so", b"built")]:
        case = tmp_path / f"{version}-{name}-{content.decode()}"
        case.mkdir()
        with pytest.raises(SystemExit, match="not the controlled build"):
            ow.check_installed(record, fake_python(case, version, name, content))
