"""`milvus-lite dump` 端到端：子进程运行已安装的 CLI，读回 BulkWriter 产物（0037「测试」）。"""

from __future__ import annotations

import os
import re
import select
import signal
import subprocess
import sys
import time
from pathlib import Path

import pytest
from pymilvus import MilvusClient

import dump_helpers as h
from milvus_lite import cmdline
from milvus_lite.server_manager import server_manager_instance


def built(tmp_path, case):
    db = tmp_path / f"{case}.db"
    h.build(case, db)
    return db


def dumped(tmp_path, case, *extra):
    db = built(tmp_path, case)
    out = tmp_path / "out"
    result = h.dump(["-d", db, "-c", "c", "-p", out, *extra])
    return db, out, result


def ok(result, count, out):
    assert result.returncode == 0, result.stderr[-3000:]
    assert result.stdout.strip() == f"Dump collection c success: {count} rows -> {out}"


def test_auto_id_dynamic_and_unflushed_rows(tmp_path):
    _, out, result = dumped(tmp_path, "basic")
    ok(result, 3, out)
    assert h.rows(out)[0] == [
        {"vec": [1.0, 0.5], "text": "a", "$meta": {"extra": 1, "nested": {"k": [1, 2]}}},
        {"vec": [0.25, 0.0], "text": "b", "$meta": {}},
        {"vec": [0.0, 1.0], "text": "unflushed", "$meta": {"extra": "s"}},
    ]


def test_scalars_nullable_defaults_json_arrays_upsert_and_delete(tmp_path):
    # 与 3.x 相同，全量 upsert 合并而不替换旧的动态字段（pk 3 的 $x 保留）。
    _, out, result = dumped(tmp_path, "scalars")
    ok(result, 3, out)
    got, texts = h.rows(out)
    assert got == [
        {"pk": -1, "vec": [0.10000000149011612, 0.20000000298023224], "b": True, "i8": -3,
         "i64": -9223372036854775808, "i16": 5, "f": 1.0000000150474662e+30, "s": "中文\n\"q\"",
         "j": {"x": 1, "y": [1, "z"], "big": 12345678901234567890, "f": 1.5, "u": "é"},
         "arr": [1, 4611686018427387904], "barr": [True, False], "farr": [0.1], "$meta": {"dyn": {"a": [None]}}},
        {"pk": 2, "vec": [0.30000001192092896, 0.4000000059604645], "b": False, "i8": 7, "i64": None, "i16": 5,
         "f": 0.5, "s": "dv", "j": [1, 2], "arr": None, "barr": [], "farr": [], "$meta": {}},
        {"pk": 3, "vec": [9.0, 9.0], "b": False, "i8": 1, "i64": None, "i16": 5, "f": 2.5, "s": "dv", "j": None,
         "arr": None, "barr": [], "farr": [1.0], "$meta": {"$x": 1, "dyn": "u"}},
    ]
    assert "NaN" not in texts[0]


def test_sparse_vectors(tmp_path):
    _, out, result = dumped(tmp_path, "sparse")
    ok(result, 2, out)
    assert h.rows(out)[0] == [{"id": 1, "sp": {"3": 0.5, "10": 1.25}}, {"id": 2, "sp": {"0": 1.0}}]


def test_bm25_output_fields_are_left_to_the_importer(tmp_path):
    # 缺陷 A：3.x 因 function 输出字段被 BulkWriter 拒绝而失败。
    _, out, result = dumped(tmp_path, "bm25")
    ok(result, 2, out)
    assert h.rows(out)[0] == [{"text": "hello world"}, {"text": "milvus lite dump"}]


def test_array_of_varchar(tmp_path):
    # 缺陷 B：3.x 因 Describe 不回显元素 max_length 而 KeyError。
    _, out, result = dumped(tmp_path, "array_varchar")
    ok(result, 2, out)
    assert h.rows(out)[0] == [{"id": 1, "vec": [1.0, 1.0], "tags": ["a", "bc"]}, {"id": 2, "vec": [1.0, 2.0], "tags": None}]


@pytest.mark.parametrize("case, expected", [
    ("partitions", [{"id": i, "vec": [float(i), 1.0], "k": f"k{i}"} for i in range(6)]),
    ("explicit_partitions", [{"id": 1, "vec": [1.0, 1.0]}, {"id": 2, "vec": [2.0, 2.0]}]),
    ("tz_geo", [{"id": 1, "vec": [1.0, 1.0], "ts": "2024-01-01T19:04:05Z", "g": "POINT(1 2)"},
                {"id": 2, "vec": [1.0, 1.0], "ts": None, "g": None}]),
    ("multivec", [{"id": 1, "v1": [1.0, 2.0], "v2": [3.0, 4.0, 5.0]}]),
    ("released", [{"id": 1, "vector": [1.0, 2.0], "$meta": {}}]),
])
def test_other_schemas(tmp_path, case, expected):
    _, out, result = dumped(tmp_path, case)
    ok(result, len(expected), out)
    assert h.rows(out)[0] == expected


def test_varchar_primary_keys_page_through_every_row(tmp_path):
    _, out, result = dumped(tmp_path, "varchar_pk", "--batch-size", 1)
    ok(result, 6, out)
    assert sorted(row["pk"] for row in h.rows(out)[0]) == sorted(["b", "a", "ä", "日本", "z9", "0"])


def test_empty_collection_creates_no_file(tmp_path):
    _, out, result = dumped(tmp_path, "empty")
    ok(result, 0, out)
    assert h.outputs(out) == {}


def test_small_chunks_split_into_numbered_files(tmp_path):
    _, out, result = dumped(tmp_path, "many", "--chunk-size", 20000, "--batch-size", 7)
    ok(result, 2500, out)
    (files,) = h.outputs(out).values()
    assert len(files) > 1 and files == [f"{i}.json" for i in range(1, len(files) + 1)]
    assert [row["id"] for row in h.rows(out)[0]] == list(range(2500))


def test_existing_output_directory_is_kept(tmp_path):
    db = built(tmp_path, "explicit_partitions")
    out = tmp_path / "out"
    out.mkdir()
    (out / "keep.txt").write_text("keep")
    ok(h.dump(["-d", db, "-c", "c", "-p", out]), 2, out)
    assert (out / "keep.txt").read_text() == "keep" and len(h.outputs(out)) == 1


def test_console_script(tmp_path):
    db = built(tmp_path, "explicit_partitions")
    out = tmp_path / "out"
    script = Path(sys.executable).with_name("milvus-lite")
    result = subprocess.run([str(script), "dump", "-d", str(db), "-c", "c", "-p", str(out)],
                            capture_output=True, text=True, timeout=300)
    ok(result, 2, out)


def test_batch_size_zero_is_rejected(tmp_path):
    # 缺陷 C：3.x 报告成功并导出 0 行。
    _, out, result = dumped(tmp_path, "explicit_partitions", "--batch-size", 0)
    assert result.returncode == 2
    assert h.last_line(result.stderr) == "milvus-lite dump: error: argument --batch-size: must be between 1 and 16384"
    assert not out.exists()


@pytest.mark.parametrize("case, line", [
    ("non_finite", "MilvusLite dump failed: field 'd' of row with primary key 2 contains a non-finite float (nan), which JSON cannot represent"),
    ("non_finite_array", "MilvusLite dump failed: field 'da' of row with primary key 2 contains a non-finite float (inf), which JSON cannot represent"),
])
def test_non_finite_floats_fail_without_leaving_output(tmp_path, case, line):
    # 缺陷 E：3.x 写出裸 NaN / Infinity，不是合法 JSON。
    _, out, result = dumped(tmp_path, case)
    assert result.returncode == 1
    assert h.last_line(result.stderr) == line
    assert h.outputs(out) == {}


@pytest.mark.skipif(sys.platform == "win32", reason="RLIMIT_FSIZE 只在 Unix 上存在；本实现不发布 Windows（0013）")
def test_write_failure_is_reported_and_cleaned_up(tmp_path):
    # 缺陷 D：3.x 的写文件线程异常被吞，报告成功而没有文件。容器中以 root 运行时目录权限不起作用，
    # 因此用文件大小上限触发写失败；Python 启动时忽略 SIGXFSZ，write 返回 EFBIG。
    import resource

    db = built(tmp_path, "many")
    out = tmp_path / "out"

    def limit():
        resource.setrlimit(resource.RLIMIT_FSIZE, (4096, 4096))

    result = h.dump(["-d", db, "-c", "c", "-p", out], preexec_fn=limit)
    assert result.returncode == 1, result.stderr[-3000:]
    assert h.last_line(result.stderr).startswith("MilvusLite dump failed: failed to write dump files:")
    assert "File too large" in h.last_line(result.stderr)
    assert h.outputs(out) == {}


def _in_process_dump(db, out, monkeypatch, *, fail_on=None):
    import pymilvus.bulk_writer.buffer as buffer
    import pymilvus.bulk_writer.local_bulk_writer as local

    persist = buffer.Buffer.persist
    calls = []

    def flaky(self, *args, **kwargs):
        calls.append(1)
        if fail_on is not None and len(calls) == fail_on:
            raise OSError(28, "No space left on device")
        return persist(self, *args, **kwargs)

    info = local.logger.info

    def slow_info(message, *args, **kwargs):
        info(message, *args, **kwargs)
        # 写线程在 finally 里先离开 _working_thread、再打这条日志，异常随后才传到子类；把窗口拉长。
        if str(message).startswith("Flush thread finished"):
            time.sleep(0.5)

    monkeypatch.setattr(buffer.Buffer, "persist", flaky)
    monkeypatch.setattr(local.logger, "info", slow_info)
    try:
        return cmdline.dump_collection(str(db), "c", str(out), batch_size=100, chunk_size=20000)
    finally:
        server_manager_instance.release_server(str(db))


def test_real_writer_failure_in_a_background_flush_is_not_missed(tmp_path, monkeypatch):
    db = built(tmp_path, "many")
    out = tmp_path / "out"
    with pytest.raises(RuntimeError, match="failed to write dump files: .*No space left on device"):
        _in_process_dump(db, out, monkeypatch, fail_on=2)
    assert h.outputs(out) == {}


def test_failure_of_the_last_async_flush_is_seen_by_the_final_check(tmp_path, monkeypatch):
    # 精确构造竞态：最后一次异步写失败，写线程离开 _working_thread 之后、异常传到子类之前，
    # 主线程的同步 commit 已经结束；只等 _working_thread 的实现会在这里漏判。
    import pymilvus.bulk_writer.buffer as buffer
    import pymilvus.bulk_writer.local_bulk_writer as local
    from pymilvus import CollectionSchema, DataType, FieldSchema
    from pymilvus.bulk_writer import BulkFileType, LocalBulkWriter

    persist = buffer.Buffer.persist
    calls = []

    def first_fails(self, *args, **kwargs):
        calls.append(1)
        if len(calls) == 1:
            raise OSError(28, "No space left on device")
        return persist(self, *args, **kwargs)

    info = local.logger.info

    def slow_info(message, *args, **kwargs):
        info(message, *args, **kwargs)
        if str(message).startswith("Flush thread finished") and len(calls) == 1:
            time.sleep(1.5)

    monkeypatch.setattr(buffer.Buffer, "persist", first_fails)
    monkeypatch.setattr(local.logger, "info", slow_info)
    schema = CollectionSchema([FieldSchema("id", DataType.INT64, is_primary=True),
                               FieldSchema("vec", DataType.FLOAT_VECTOR, dim=2)])
    writer = cmdline._checked_writer_class(LocalBulkWriter)(
        schema=schema, local_path=str(tmp_path / "out"), chunk_size=64, file_type=BulkFileType.JSON)
    for i in range(10):
        writer.append_row({"id": i, "vec": [0.0, 1.0]})
        if writer._working_thread or calls:
            break
    # 等第一个异步写线程离开 _working_thread（它随后在日志里停留 1.5 秒才把异常交给子类）。
    deadline = time.monotonic() + 5
    while writer._working_thread and time.monotonic() < deadline:
        time.sleep(0.01)
    assert calls == [1] and writer._working_thread == {}
    writer.append_row({"id": 99, "vec": [0.0, 1.0]})
    writer.commit()
    writer.dump_wait()
    with pytest.raises(RuntimeError, match="failed to write dump files: .*No space left on device"):
        writer.dump_raise_if_failed()


def test_real_writer_success_with_slow_threads(tmp_path, monkeypatch):
    db = built(tmp_path, "many")
    out = tmp_path / "out"
    assert _in_process_dump(db, out, monkeypatch)["row_count"] == 2500
    assert [row["id"] for row in h.rows(out)[0]] == list(range(2500))


def test_uri_to_a_running_server(tmp_path):
    db = built(tmp_path, "explicit_partitions")
    server = subprocess.Popen([sys.executable, "-m", "milvus_lite", "server", "--data-dir", str(db),
                               "--host", "127.0.0.1", "--port", "0"],
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        # 启动失败时不能无限期阻塞在 readline 上。
        ready, _, _ = select.select([server.stdout], [], [], 60)
        assert ready, "server did not report its address within 60 seconds"
        line = server.stdout.readline()
        address = re.search(r"listening on (\S+) ", line).group(1)
        out = tmp_path / "out"
        # server 持有 LOCK 时 -d 打不开同一目录，只能走 --uri。
        locked = h.dump(["-d", db, "-c", "c", "-p", tmp_path / "locked"])
        assert locked.returncode == 1
        assert h.last_line(locked.stderr) == "MilvusLite dump failed: Open local milvus failed"
        ok(h.dump(["--uri", f"http://{address}", "-c", "c", "-p", out]), 2, out)
        assert h.rows(out)[0] == [{"id": 1, "vec": [1.0, 1.0]}, {"id": 2, "vec": [2.0, 2.0]}]
    finally:
        server.send_signal(signal.SIGINT)
        server.communicate(timeout=30)


@pytest.mark.parametrize("args, line", [
    (["-c", "nope"], "MilvusLite dump failed: Collection: nope not exists"),
    (["-c", "c", "--uri", "{tmp}/missing.db"], "MilvusLite dump failed: uri: {tmp}/missing.db not exists"),
])
def test_reported_failures(tmp_path, args, line):
    db = built(tmp_path, "empty")
    args = [a.replace("{tmp}", str(tmp_path)) for a in args]
    source = [] if "--uri" in args else ["-d", db]
    result = h.dump([*source, *args, "-p", tmp_path / "out"])
    assert result.returncode == 1
    assert h.last_line(result.stderr) == line.replace("{tmp}", str(tmp_path))
    assert not (tmp_path / "missing.db").exists()


def test_plain_directory_needs_the_db_suffix(tmp_path):
    data = tmp_path / "plain"
    data.mkdir()
    result = h.dump(["-d", data, "-c", "c", "-p", tmp_path / "out"])
    assert result.returncode == 1
    assert "is illegal, needs start with [unix, http, https, tcp] or a local file endswith [.db]" in h.last_line(result.stderr)


@pytest.mark.skipif(sys.platform == "win32", reason="本实现不发布 Windows（0013）")
@pytest.mark.parametrize("signum, code", [(signal.SIGINT, 130), (signal.SIGTERM, 143)])
def test_signals_clean_up_and_release_the_data_directory(tmp_path, signum, code):
    db = built(tmp_path, "large")
    out = tmp_path / "out"
    proc = subprocess.Popen([sys.executable, "-m", "milvus_lite", "dump", "-d", str(db), "-c", "c", "-p", str(out),
                             "--chunk-size", "20000"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        # 同步点：第一个文件出现时，导出正在进行（小 chunk 使每个文件约间隔 1 秒）。
        deadline = time.monotonic() + 120
        while not any(h.outputs(out).values()) and proc.poll() is None and time.monotonic() < deadline:
            time.sleep(0.05)
        assert proc.poll() is None, "dump finished before the signal"
        proc.send_signal(signum)
        _, stderr = proc.communicate(timeout=60)
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.communicate()
    assert proc.returncode == code, stderr[-3000:]
    assert h.last_line(stderr) == f"MilvusLite dump interrupted by {signal.Signals(signum).name}"
    assert h.outputs(out) == {}
    client = MilvusClient(str(db))
    try:
        assert client.query("c", filter="", output_fields=["count(*)"])[0]["count(*)"] == 4000
    finally:
        client.close()
        server_manager_instance.release_server(str(db))
