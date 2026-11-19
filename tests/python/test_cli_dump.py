"""`milvus-lite dump` 的单测：fake 依赖，不需要 BulkWriter（0037「测试」）。

前 5 条改写自 milvus-lite 1320d26 tests/test_dump_tool.py（Apache-2.0）；server 分发与模块入口
另 2 条已原样复制在 test_cli_entrypoints.py。改写点：本实现改用 MilvusClient（3.x 用 ORM），
fake 对象随之改为 _FakeClient / _FakeSchema；连接参数断言改为断言 MilvusClient 的 uri；
自增主键用例的 schema 由 describe_collection 构造。用例主体（行内容、计数、关闭）保持原意。
"""

from __future__ import annotations

import math
import os
import signal
import sys
import threading
import time
from types import SimpleNamespace

import numpy as np
import pytest

from milvus_lite import cmdline


class _FakeIterator:
    def __init__(self, batches):
        self._batches = list(batches)
        self.closed = False

    def next(self):
        if self._batches:
            return self._batches.pop(0)
        return []

    def close(self):
        self.closed = True


class _FakeDataType:
    INT64 = "INT64"
    VARCHAR = "VARCHAR"
    ARRAY = "ARRAY"
    FLOAT_VECTOR = "FLOAT_VECTOR"
    SPARSE_FLOAT_VECTOR = "SPARSE_FLOAT_VECTOR"
    BFLOAT16_VECTOR = "BFLOAT16_VECTOR"
    BINARY_VECTOR = "BINARY_VECTOR"
    FLOAT16_VECTOR = "FLOAT16_VECTOR"


def _field(name, dtype, **kwargs):
    return SimpleNamespace(name=name, dtype=dtype, params=kwargs.pop("params", {}), **kwargs)


def _default_fields():
    return [
        _field("id", "INT64", is_primary=True, auto_id=True),
        _field("vec", "FLOAT_VECTOR"),
        _field("text", "VARCHAR"),
    ]


def _default_batches():
    return [
        [
            {"id": 10, "vec": [1.0, 0.0], "text": "a"},
            {"id": 11, "vec": [0.0, 1.0], "text": "b"},
        ],
        [
            {"id": 12, "vec": [0.5, 0.5], "text": "c"},
        ],
    ]


class _FakeSchema:
    @staticmethod
    def construct_from_dict(raw):
        return SimpleNamespace(fields=raw["fields"], auto_id=False)


class _FakeClient:
    last = None
    fields = None
    batches = None
    exists = True

    def __init__(self, uri=None):
        self.uri = uri
        self.loaded = False
        self.closed = False
        self.iterator = _FakeIterator(_FakeClient.batches)
        _FakeClient.last = self

    def has_collection(self, name):
        return _FakeClient.exists and name == "docs"

    def describe_collection(self, name):
        return {"fields": _FakeClient.fields}

    def load_collection(self, name):
        self.loaded = True

    def query(self, name, filter=None, output_fields=None):
        assert filter == ""
        assert output_fields == ["count(*)"]
        return [{"count(*)": 3}]

    def query_iterator(self, name, batch_size=None, output_fields=None):
        assert batch_size == 2
        assert output_fields == ["*"]
        return self.iterator

    def close(self):
        self.closed = True


class _FakeBulkFileType:
    JSON = "json"


class _FakeWriter:
    instances = []

    def __init__(self, schema, local_path, chunk_size, file_type):
        self.schema = schema
        self.local_path = local_path
        self.chunk_size = chunk_size
        self.file_type = file_type
        self.rows = []
        self.committed = False
        _FakeWriter.instances.append(self)

    def append_row(self, row):
        self.rows.append(row)

    def commit(self):
        self.committed = True


@pytest.fixture
def fake_dump_deps(monkeypatch):
    _FakeClient.fields = _default_fields()
    _FakeClient.batches = _default_batches()
    _FakeClient.exists = True
    _FakeClient.last = None
    _FakeWriter.instances = []
    api = SimpleNamespace(MilvusClient=_FakeClient, CollectionSchema=_FakeSchema, DataType=_FakeDataType, numpy=np)
    monkeypatch.setattr(cmdline, "_load_pymilvus_api", lambda: api)
    monkeypatch.setattr(cmdline, "_load_bulk_writer", lambda: (_FakeWriter, _FakeBulkFileType))
    return api


@pytest.fixture
def db_path(tmp_path):
    path = tmp_path / "demo.db"
    path.mkdir()
    return path


# ---- 改写自 3.x test_dump_tool.py ----


def test_dump_collection_writes_rows_and_drops_auto_id(tmp_path, db_path, fake_dump_deps):
    out_path = tmp_path / "dump"

    summary = cmdline.dump_collection(
        str(db_path),
        "docs",
        str(out_path),
        batch_size=2,
        chunk_size=1024,
    )

    assert summary == {
        "collection": "docs",
        "path": str(out_path),
        "row_count": 3,
    }
    assert _FakeClient.last.uri == str(db_path)
    assert _FakeClient.last.closed is True
    assert _FakeClient.last.loaded is True
    assert _FakeClient.last.iterator.closed is True

    writer = _FakeWriter.instances[0]
    assert writer.local_path == str(out_path)
    assert writer.chunk_size == 1024
    assert writer.file_type == _FakeBulkFileType.JSON
    assert writer.committed is True
    assert writer.rows == [
        {"vec": [1.0, 0.0], "text": "a"},
        {"vec": [0.0, 1.0], "text": "b"},
        {"vec": [0.5, 0.5], "text": "c"},
    ]


def test_dump_collection_accepts_uri_without_local_path(tmp_path, fake_dump_deps):
    out_path = tmp_path / "dump"

    summary = cmdline.dump_collection(
        None,
        "docs",
        str(out_path),
        uri="http://127.0.0.1:19530",
        batch_size=2,
    )

    assert summary["row_count"] == 3
    assert _FakeClient.last.uri == "http://127.0.0.1:19530"


def test_dump_collection_rejects_missing_db_file(tmp_path):
    with pytest.raises(RuntimeError, match="not exists"):
        cmdline.dump_collection(
            str(tmp_path / "missing.db"),
            "docs",
            str(tmp_path / "dump"),
        )


def test_dump_collection_rejects_unknown_collection(tmp_path, db_path, fake_dump_deps):
    _FakeClient.exists = False

    with pytest.raises(RuntimeError, match="Collection: docs not exists"):
        cmdline.dump_collection(str(db_path), "docs", str(tmp_path / "dump"))

    assert _FakeClient.last.closed is True


def test_main_dispatches_dump(monkeypatch, tmp_path, capsys):
    calls = []

    def fake_dump_collection(db_file, collection_name, path, **kwargs):
        calls.append((db_file, collection_name, path, kwargs))
        return {"collection": collection_name, "path": path, "row_count": 7}

    monkeypatch.setattr(cmdline, "dump_collection", fake_dump_collection)

    rc = cmdline.main([
        "dump",
        "-d", str(tmp_path / "demo.db"),
        "-c", "docs",
        "-p", str(tmp_path / "dump"),
        "--batch-size", "9",
        "--chunk-size", "123",
    ])

    assert rc == 0
    assert calls == [(
        str(tmp_path / "demo.db"),
        "docs",
        str(tmp_path / "dump"),
        {"uri": None, "batch_size": 9, "chunk_size": 123},
    )]
    assert "Dump collection docs success: 7 rows" in capsys.readouterr().out


# ---- 本实现的修正与边界（0037「决策」） ----


def test_function_output_fields_are_not_exported(tmp_path, db_path, fake_dump_deps):
    _FakeClient.fields = [
        _field("id", "INT64", is_primary=True, auto_id=False),
        _field("text", "VARCHAR"),
        _field("sp", "SPARSE_FLOAT_VECTOR", is_function_output=True),
    ]
    _FakeClient.batches = [[{"id": 1, "text": "a", "sp": {1: 0.5}}], [{"id": 2, "text": "b"}]]

    cmdline.dump_collection(str(db_path), "docs", str(tmp_path / "dump"), batch_size=2)

    assert _FakeWriter.instances[0].rows == [{"id": 1, "text": "a"}, {"id": 2, "text": "b"}]


def test_array_varchar_gets_max_length_only_in_the_writer_schema(tmp_path, db_path, fake_dump_deps):
    missing = _field("tags", "ARRAY", element_type="VARCHAR", params={"max_capacity": 4})
    present = _field("names", "ARRAY", element_type="VARCHAR", params={"max_capacity": 4, "max_length": 9})
    ints = _field("nums", "ARRAY", element_type="INT64", params={"max_capacity": 4})
    _FakeClient.fields = [_field("id", "INT64", is_primary=True, auto_id=False), missing, present, ints]
    _FakeClient.batches = []

    cmdline.dump_collection(str(db_path), "docs", str(tmp_path / "dump"), batch_size=2)

    fields = {field.name: field for field in _FakeWriter.instances[0].schema.fields}
    assert fields["tags"].params == {"max_capacity": 4, "max_length": cmdline.ARRAY_VARCHAR_MAX_LENGTH}
    assert fields["names"].params == {"max_capacity": 4, "max_length": 9}
    assert fields["nums"].params == {"max_capacity": 4}
    assert missing.params == {"max_capacity": 4}


@pytest.mark.parametrize("args, message", [
    (["--batch-size", "0"], "--batch-size: must be between 1 and 16384"),
    (["--batch-size", "16385"], "--batch-size: must be between 1 and 16384"),
    (["--batch-size", "x"], "--batch-size: invalid int value: 'x'"),
    (["--chunk-size", "0"], "--chunk-size: must be at least 1"),
])
def test_argument_bounds_are_rejected_by_argparse(tmp_path, capsys, args, message):
    with pytest.raises(SystemExit) as exc_info:
        cmdline.main(["dump", "-d", str(tmp_path / "x.db"), "-c", "c", "-p", str(tmp_path / "o"), *args])
    assert exc_info.value.code == 2
    assert message in capsys.readouterr().err


@pytest.mark.parametrize("kwargs, message", [
    ({"batch_size": 0}, "batch size must be between 1 and 16384"),
    ({"batch_size": 16385}, "batch size must be between 1 and 16384"),
    ({"chunk_size": 0}, "chunk size must be at least 1"),
])
def test_direct_calls_check_the_same_bounds(tmp_path, db_path, kwargs, message):
    with pytest.raises(RuntimeError, match=message):
        cmdline.dump_collection(str(db_path), "docs", str(tmp_path / "dump"), **kwargs)


def test_local_db_uri_must_exist(tmp_path):
    with pytest.raises(RuntimeError, match="not exists"):
        cmdline.dump_collection(None, "docs", str(tmp_path / "dump"), uri=str(tmp_path / "missing.db"))
    assert not (tmp_path / "missing.db").exists()


@pytest.mark.parametrize("value", [math.nan, math.inf, -math.inf, np.float32("nan"), np.array([1.0, np.inf])])
def test_non_finite_floats_are_rejected_with_the_field_and_primary_key(tmp_path, db_path, fake_dump_deps, value):
    _FakeClient.fields = [_field("id", "INT64", is_primary=True, auto_id=False), _field("f", "DOUBLE")]
    _FakeClient.batches = [[{"id": 1, "f": 0.5}, {"id": 2, "f": {"deep": [value]}}]]

    with pytest.raises(RuntimeError, match=r"field 'f' of row with primary key 2 contains a non-finite float"):
        cmdline.dump_collection(str(db_path), "docs", str(tmp_path / "dump"), batch_size=2)


def test_non_finite_floats_with_auto_id_report_the_row_number(tmp_path, db_path, fake_dump_deps):
    _FakeClient.batches = [[{"id": 10, "vec": [1.0, 0.0], "text": "a"}, {"id": 11, "vec": [math.nan, 0.0], "text": "b"}]]

    with pytest.raises(RuntimeError, match=r"field 'vec' of row #2 contains a non-finite float"):
        cmdline.dump_collection(str(db_path), "docs", str(tmp_path / "dump"), batch_size=2)


def test_float16_bytes_are_checked_but_passed_through(tmp_path, db_path, fake_dump_deps):
    _FakeClient.fields = [_field("id", "INT64", is_primary=True, auto_id=False), _field("h", "FLOAT16_VECTOR")]
    good = np.array([1.0, 2.0], dtype=np.float16).tobytes()
    bad = np.array([1.0, np.inf], dtype=np.float16).tobytes()
    _FakeClient.batches = [[{"id": 1, "h": good}], [{"id": 2, "h": bad}]]

    with pytest.raises(RuntimeError, match=r"field 'h' of row with primary key 2"):
        cmdline.dump_collection(str(db_path), "docs", str(tmp_path / "dump"), batch_size=2)
    assert _FakeWriter.instances[0].rows == [{"id": 1, "h": good}]


class _DiskWriter(_FakeWriter):
    """模拟 LocalBulkWriter 的目录布局：<local_path>/<uuid>/。"""

    UUID = "8d6c0a10-0000-4000-8000-000000000000"

    def __init__(self, schema, local_path, chunk_size, file_type):
        super().__init__(schema, local_path, chunk_size, file_type)
        self.uuid = self.UUID
        self.data_path = os.path.join(local_path, self.uuid)
        os.makedirs(self.data_path)

    def append_row(self, row):
        super().append_row(row)
        with open(os.path.join(self.data_path, f"{len(self.rows)}.json"), "w") as f:
            f.write("{}")


def test_failure_removes_only_the_uuid_directory_of_this_run(tmp_path, db_path, fake_dump_deps, monkeypatch):
    monkeypatch.setattr(cmdline, "_load_bulk_writer", lambda: (_DiskWriter, _FakeBulkFileType))
    out = tmp_path / "dump"
    out.mkdir()
    (out / "keep.txt").write_text("keep")
    _FakeClient.batches = [[{"id": 10, "vec": [1.0, 0.0], "text": "a"}, {"id": 11, "vec": [math.nan, 0.0], "text": "b"}]]

    with pytest.raises(RuntimeError) as exc_info:
        cmdline.dump_collection(str(db_path), "docs", str(out), batch_size=2)

    assert sorted(os.listdir(out)) == ["keep.txt"]
    assert not hasattr(exc_info.value, "dump_residue")


def test_failure_reports_a_directory_it_cannot_identify(tmp_path, db_path, fake_dump_deps, monkeypatch):
    class Elsewhere(_DiskWriter):
        def __init__(self, schema, local_path, chunk_size, file_type):
            super().__init__(schema, str(tmp_path / "other"), chunk_size, file_type)

    monkeypatch.setattr(cmdline, "_load_bulk_writer", lambda: (Elsewhere, _FakeBulkFileType))
    _FakeClient.batches = [[{"id": 10, "vec": [math.nan, 0.0], "text": "a"}]]

    with pytest.raises(RuntimeError) as exc_info:
        cmdline.dump_collection(str(db_path), "docs", str(tmp_path / "dump"), batch_size=2)

    residue = exc_info.value.dump_residue
    assert residue == os.path.join(str(tmp_path / "other"), _DiskWriter.UUID)
    assert os.path.isdir(residue)


class _ThreadedWriter:
    """模拟 LocalBulkWriter 的写线程：finally 中先从 _working_thread 删除自己，异常随后才抛出。"""

    fail = False
    linger = 0.0

    def __init__(self, schema, local_path, chunk_size, file_type):
        self._working_thread = {}
        self.rows = []

    def append_row(self, row):
        self.rows.append(row)

    def commit(self, _async=False):
        thread = threading.Thread(target=self._flush)
        self._working_thread[thread.name] = thread
        thread.start()
        if not _async:
            thread.join()

    def _flush(self):
        try:
            if type(self).fail:
                raise OSError(28, "No space left on device")
        finally:
            del self._working_thread[threading.current_thread().name]
            time.sleep(type(self).linger)


def test_flush_failures_are_detected_after_commit(tmp_path, db_path, fake_dump_deps, monkeypatch):
    monkeypatch.setattr(_ThreadedWriter, "fail", True)
    monkeypatch.setattr(cmdline, "_load_bulk_writer", lambda: (_ThreadedWriter, _FakeBulkFileType))

    with pytest.raises(RuntimeError, match="failed to write dump files: OSError: .*No space left on device"):
        cmdline.dump_collection(str(db_path), "docs", str(tmp_path / "dump"), batch_size=2)


def test_flush_failure_after_leaving_working_thread_is_not_missed(tmp_path, monkeypatch):
    monkeypatch.setattr(_ThreadedWriter, "fail", True)
    monkeypatch.setattr(_ThreadedWriter, "linger", 0.3)
    writer = cmdline._checked_writer_class(_ThreadedWriter)(schema=None, local_path=None, chunk_size=1, file_type=None)

    writer.commit(_async=True)
    deadline = time.monotonic() + 5
    while writer._working_thread and time.monotonic() < deadline:
        time.sleep(0.01)
    assert writer._working_thread == {}
    writer.dump_wait()
    with pytest.raises(RuntimeError, match="failed to write dump files"):
        writer.dump_raise_if_failed()


def test_successful_flush_is_not_reported(tmp_path):
    writer = cmdline._checked_writer_class(_ThreadedWriter)(schema=None, local_path=None, chunk_size=1, file_type=None)
    writer.commit()
    writer.dump_wait()
    writer.dump_raise_if_failed()


class _PymilvusError(Exception):
    __module__ = "pymilvus.exceptions"

    def __init__(self, message):
        super().__init__(f"<_PymilvusError: (code=1, message={message})>")
        self.message = message


@pytest.mark.parametrize("error, line", [
    (RuntimeError("plain message"), "MilvusLite dump failed: plain message"),
    (KeyError("x"), "MilvusLite dump failed: KeyError: 'x'"),
    (_PymilvusError("Open local milvus failed"), "MilvusLite dump failed: Open local milvus failed"),
])
def test_failures_are_reported_in_one_line(monkeypatch, tmp_path, capsys, error, line):
    def fail(*_args, **_kwargs):
        raise error

    monkeypatch.setattr(cmdline, "dump_collection", fail)
    rc = cmdline.main(["dump", "-d", str(tmp_path / "x.db"), "-c", "c", "-p", str(tmp_path / "o")])
    assert rc == 1
    assert capsys.readouterr().err.splitlines() == [line]


def test_residue_is_named_in_the_failure_line(monkeypatch, tmp_path, capsys):
    def fail(*_args, **_kwargs):
        error = RuntimeError("boom")
        error.dump_residue = "/somewhere/uuid"
        raise error

    monkeypatch.setattr(cmdline, "dump_collection", fail)
    assert cmdline.main(["dump", "-d", str(tmp_path / "x.db"), "-c", "c", "-p", str(tmp_path / "o")]) == 1
    assert capsys.readouterr().err.strip() == "MilvusLite dump failed: boom (incomplete output left at /somewhere/uuid)"


@pytest.mark.parametrize("missing", ["pymilvus", "bulk_writer"])
def test_missing_dependencies_are_reported_before_connecting(tmp_path, db_path, fake_dump_deps, monkeypatch, missing):
    def unavailable():
        raise RuntimeError(f"milvus-lite dump requires {missing}")

    monkeypatch.setattr(cmdline, "_load_pymilvus_api" if missing == "pymilvus" else "_load_bulk_writer", unavailable)
    with pytest.raises(RuntimeError, match=f"requires {missing}"):
        cmdline.dump_collection(str(db_path), "docs", str(tmp_path / "dump"), batch_size=2)
    assert _FakeClient.last is None


def test_progress_uses_tqdm_when_available(monkeypatch):
    calls = []

    class FakeTqdm:
        def __init__(self, total, desc):
            calls.append((total, desc))

    monkeypatch.setitem(sys.modules, "tqdm", SimpleNamespace(tqdm=FakeTqdm))
    assert isinstance(cmdline._make_progress(3, "d"), FakeTqdm)
    assert calls == [(3, "d")]


def test_progress_is_silent_without_tqdm(monkeypatch):
    monkeypatch.setitem(sys.modules, "tqdm", None)
    progress = cmdline._make_progress(3, "d")
    progress.update(1)
    progress.close()
    assert isinstance(progress, cmdline._NoopProgress)


def test_dump_refuses_to_run_outside_the_main_thread(tmp_path):
    result = {}
    stderr = []

    def run():
        import contextlib
        import io

        buffer = io.StringIO()
        with contextlib.redirect_stderr(buffer):
            result["rc"] = cmdline.main(["dump", "-d", str(tmp_path / "x.db"), "-c", "c", "-p", str(tmp_path / "o")])
        stderr.append(buffer.getvalue())

    thread = threading.Thread(target=run)
    thread.start()
    thread.join()
    assert result["rc"] == 1
    assert stderr == ["MilvusLite dump failed: dump must run in the main thread\n"]


@pytest.mark.parametrize("signum, code", [(signal.SIGINT, 130), (signal.SIGTERM, 143)])
def test_signals_interrupt_the_dump_and_leave_default_handlers(monkeypatch, tmp_path, capsys, signum, code):
    saved = {item: signal.getsignal(item) for item in (signal.SIGINT, signal.SIGTERM)}
    seen = []

    def interrupted(*_args, **_kwargs):
        try:
            os.kill(os.getpid(), signum)
            time.sleep(5)
        except BaseException as error:
            seen.append(type(error).__name__)
            seen.append(signal.getsignal(signal.SIGINT) is signal.SIG_DFL and signal.getsignal(signal.SIGTERM) is signal.SIG_DFL)
            raise
        return {}

    monkeypatch.setattr(cmdline, "dump_collection", interrupted)
    try:
        rc = cmdline.main(["dump", "-d", str(tmp_path / "x.db"), "-c", "c", "-p", str(tmp_path / "o")])
        after = {item: signal.getsignal(item) for item in saved}
    finally:
        for item, handler in saved.items():
            signal.signal(item, handler)
    assert rc == code
    assert seen == ["_DumpInterrupted", True]
    assert after == {signal.SIGINT: signal.SIG_DFL, signal.SIGTERM: signal.SIG_DFL}
    assert capsys.readouterr().err.strip() == f"MilvusLite dump interrupted by {signal.Signals(signum).name}"


def test_handlers_are_restored_after_a_normal_run(monkeypatch, tmp_path):
    saved = {item: signal.getsignal(item) for item in (signal.SIGINT, signal.SIGTERM)}
    monkeypatch.setattr(cmdline, "dump_collection", lambda *a, **k: {"collection": "c", "path": "p", "row_count": 0})
    assert cmdline.main(["dump", "-d", str(tmp_path / "x.db"), "-c", "c", "-p", str(tmp_path / "o")]) == 0
    assert {item: signal.getsignal(item) for item in saved} == saved


def test_server_arguments_are_still_validated(capsys):
    with pytest.raises(SystemExit) as exc_info:
        cmdline.main(["server", "--data-dir", "x", "--port", "70000"])
    assert exc_info.value.code == 2
    assert "port must be between 0 and 65535" in capsys.readouterr().err


class _AsyncDiskWriter(_DiskWriter):
    """第一行之后起一个慢写线程，模拟失败时仍在途的异步 flush。"""

    linger = 0.5

    def __init__(self, schema, local_path, chunk_size, file_type):
        super().__init__(schema, local_path, chunk_size, file_type)
        self._working_thread = {}
        self.late = []

    def append_row(self, row):
        super().append_row(row)
        if len(self.rows) == 1:
            # 与 LocalBulkWriter 一样以 self._flush 为线程入口，被测子类据此登记写线程。
            thread = threading.Thread(target=self._flush)
            self._working_thread[thread.name] = thread
            thread.start()

    def _flush(self):
        time.sleep(type(self).linger)
        self.late.append(os.path.isdir(self.data_path))
        with open(os.path.join(self.data_path, "late.json"), "w") as f:
            f.write("{}")


def _nan_after_one_row():
    return [[{"id": 10, "vec": [1.0, 0.0], "text": "a"}], [{"id": 11, "vec": [math.nan, 0.0], "text": "b"}]]


def test_cleanup_waits_for_in_flight_writes(tmp_path, db_path, fake_dump_deps, monkeypatch):
    monkeypatch.setattr(cmdline, "_load_bulk_writer", lambda: (_AsyncDiskWriter, _FakeBulkFileType))
    _FakeClient.batches = _nan_after_one_row()
    out = tmp_path / "dump"
    out.mkdir()
    (out / "keep.txt").write_text("keep")

    with pytest.raises(RuntimeError, match="non-finite"):
        cmdline.dump_collection(str(db_path), "docs", str(out), batch_size=2)

    writer = _FakeWriter.instances[0]
    assert writer.late == [True]
    assert sorted(os.listdir(out)) == ["keep.txt"]


def test_a_signal_during_cleanup_still_waits_then_reports_the_interrupt(tmp_path, db_path, fake_dump_deps, monkeypatch, capsys):
    monkeypatch.setattr(cmdline, "_load_bulk_writer", lambda: (_AsyncDiskWriter, _FakeBulkFileType))
    monkeypatch.setattr(_AsyncDiskWriter, "linger", 1.0)
    _FakeClient.batches = _nan_after_one_row()
    out = tmp_path / "dump"
    saved = {item: signal.getsignal(item) for item in (signal.SIGINT, signal.SIGTERM)}
    timer = threading.Timer(0.3, os.kill, (os.getpid(), signal.SIGINT))
    try:
        timer.start()
        rc = cmdline.main(["dump", "-d", str(db_path), "-c", "docs", "-p", str(out), "--batch-size", "2"])
    finally:
        timer.cancel()
        for item, handler in saved.items():
            signal.signal(item, handler)

    assert rc == 130
    assert _FakeWriter.instances[0].late == [True]
    assert os.listdir(out) == []
    assert capsys.readouterr().err.strip().splitlines()[-1] == "MilvusLite dump interrupted by SIGINT"


def test_client_construction_failure_still_releases_the_local_server(tmp_path, db_path, fake_dump_deps, monkeypatch):
    released = []

    class Failing(_FakeClient):
        def __init__(self, uri=None):
            raise ValueError("boom after connect")

    from milvus_lite import server_manager

    monkeypatch.setattr(fake_dump_deps, "MilvusClient", Failing)
    monkeypatch.setattr(server_manager.server_manager_instance, "release_server", released.append)
    with pytest.raises(ValueError, match="boom after connect"):
        cmdline.dump_collection(str(db_path), "docs", str(tmp_path / "dump"), batch_size=2)
    assert released == [str(db_path)]


def test_remote_dump_does_not_touch_local_servers(tmp_path, fake_dump_deps, monkeypatch):
    released = []
    from milvus_lite import server_manager

    monkeypatch.setattr(server_manager.server_manager_instance, "release_server", released.append)
    cmdline.dump_collection(None, "docs", str(tmp_path / "dump"), uri="http://127.0.0.1:19530", batch_size=2)
    assert released == []


def test_auto_id_declared_on_the_schema_drops_the_primary_key(tmp_path, db_path, fake_dump_deps, monkeypatch):
    class AutoSchema:
        @staticmethod
        def construct_from_dict(raw):
            return SimpleNamespace(fields=raw["fields"], auto_id=True)

    monkeypatch.setattr(fake_dump_deps, "CollectionSchema", AutoSchema)
    _FakeClient.fields = [_field("id", "INT64", is_primary=True), _field("text", "VARCHAR")]
    _FakeClient.batches = [[{"id": 1, "text": "a"}]]
    cmdline.dump_collection(str(db_path), "docs", str(tmp_path / "dump"), batch_size=2)
    assert _FakeWriter.instances[0].rows == [{"text": "a"}]


def test_db_suffix_means_local_like_pymilvus(tmp_path):
    # pymilvus 先看 .db 后缀再看协议，带协议的 .db 地址同样按本地目录处理。
    with pytest.raises(RuntimeError, match="uri: http://127.0.0.1:19530/x.db not exists"):
        cmdline.dump_collection(None, "docs", str(tmp_path / "dump"), uri="http://127.0.0.1:19530/x.db")
