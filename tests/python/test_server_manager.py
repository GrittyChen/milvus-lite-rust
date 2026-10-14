"""server_manager 的生命周期用例，对应 docs/decisions/0003-in-process-server-lifecycle.md「验证」。

涉及进程退出与 fork 的用例都在子进程里跑脚本，避免在 pytest 进程里 fork 或注册 atexit。
fork 用例只在有 os.fork 的平台上跑（Windows 没有 fork）。
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import textwrap
import threading
import time
from pathlib import Path

import grpc
import pytest
from pymilvus import MilvusClient
from pymilvus.exceptions import MilvusException

from milvus_lite.server_manager import ServerManager

HAS_FORK = hasattr(os, "fork")
needs_fork = pytest.mark.skipif(not HAS_FORK, reason="os.fork 在 Windows 上不存在")


def run_script(code: str, cwd: Path, timeout: float = 60) -> list[str]:
    """在新的 Python 进程里运行脚本，返回以 ``OUT `` 开头的输出行（去掉前缀）。"""
    proc = subprocess.run(
        [sys.executable, "-c", textwrap.dedent(code)],
        cwd=cwd,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    assert proc.returncode == 0, proc.stderr
    return [line[4:] for line in proc.stdout.splitlines() if line.startswith("OUT ")]


def can_open_in_other_process(path: Path) -> bool:
    out = run_script(
        f"""
        from milvus_lite.server_manager import server_manager_instance as sm
        print("OUT", sm.start_and_get_uri({str(path)!r}) is not None)
        """,
        path.parent,
    )
    return out == ["True"]


def lock_is_free(path: Path) -> bool:
    """在本进程内尝试对 ``path/LOCK`` 加独占锁（与 Rust 侧同一种锁），成功后立即释放。"""
    with open(path / "LOCK", "r+b") as f:
        try:
            if sys.platform == "win32":
                import msvcrt

                msvcrt.locking(f.fileno(), msvcrt.LK_NBLCK, 1)
                msvcrt.locking(f.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                import fcntl

                fcntl.flock(f.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                fcntl.flock(f.fileno(), fcntl.LOCK_UN)
        except OSError:
            return False
    return True


@pytest.fixture
def manager():
    m = ServerManager()
    yield m
    m.release_all()


def test_same_path_relative_and_absolute_reuse_server(manager, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    uri_rel = manager.start_and_get_uri("x.db")
    uri_abs = manager.start_and_get_uri(str(tmp_path / "x.db"))
    assert uri_rel is not None
    assert uri_rel == uri_abs


def test_symlink_spelling_returns_none_with_lock_reason(manager, tmp_path, caplog):
    real = tmp_path / "real.db"
    link = tmp_path / "link.db"
    assert manager.start_and_get_uri(str(real)) is not None
    link.symlink_to(real)
    assert manager.start_and_get_uri(str(link)) is None
    assert "holds the lock" in caplog.text


def test_concurrent_first_open_starts_one_server(tmp_path):
    for round_ in range(5):
        m = ServerManager()
        path = str(tmp_path / f"c{round_}.db")
        barrier = threading.Barrier(8)
        results: list[str | None] = [None] * 8

        def worker(i: int) -> None:
            barrier.wait()
            results[i] = m.start_and_get_uri(path)

        threads = [threading.Thread(target=worker, args=(i,)) for i in range(8)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        try:
            assert results[0] is not None
            assert results == [results[0]] * 8
            assert len(m._servers) == 1
        finally:
            m.release_all()


def test_lock_held_by_other_process_returns_none(manager, tmp_path, caplog):
    path = tmp_path / "held.db"
    holder = subprocess.Popen(
        [
            sys.executable,
            "-c",
            textwrap.dedent(
                f"""
                import sys
                from milvus_lite.server_manager import server_manager_instance as sm
                assert sm.start_and_get_uri({str(path)!r})
                print("ready", flush=True)
                sys.stdin.read()
                """
            ),
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
    )
    try:
        assert holder.stdout.readline().strip() == "ready"
        assert manager.start_and_get_uri(str(path)) is None
        assert "another process holds the lock" in caplog.text
    finally:
        holder.stdin.close()
        holder.wait(timeout=30)


def _make_empty_3x_layout(path: Path) -> None:
    # 与 3.x 打开后又关闭的空目录一致：LOCK 与三个空目录（0003「同路径复用」）。
    path.mkdir()
    (path / "LOCK").touch()
    for d in ("collections", "databases", ".database-staging"):
        (path / d).mkdir()


def test_empty_dir_left_by_3x_opens(manager, tmp_path):
    path = tmp_path / "empty3x.db"
    _make_empty_3x_layout(path)
    assert manager.start_and_get_uri(str(path)) is not None


def test_invalid_collection_schema_fails_closed_on_access(manager, tmp_path):
    path = tmp_path / "data.db"
    _make_empty_3x_layout(path)
    schema = path / "collections" / "docs" / "schema.json"
    schema.parent.mkdir()
    schema.write_bytes(b"{")
    uri = manager.start_and_get_uri(str(path))
    assert uri is not None
    client = MilvusClient(uri)
    assert client.list_collections() == ["docs"]
    with pytest.raises(MilvusException) as error:
        client.describe_collection("docs")
    assert error.value.code == 1
    assert schema.read_bytes() == b"{"
    client.close()
    manager.release_server(str(path))
    assert lock_is_free(path)


def test_staging_residue_is_removed_after_lock(manager, tmp_path):
    path = tmp_path / "staging.db"
    _make_empty_3x_layout(path)
    residue = path / ".database-staging" / "database-x" / "collections"
    residue.mkdir(parents=True)
    assert residue.is_dir()
    uri = manager.start_and_get_uri(str(path))
    assert uri is not None
    assert not residue.exists()
    client = MilvusClient(uri)
    assert client.list_collections() == []
    client.close()
    manager.release_server(str(path))
    assert lock_is_free(path)


def test_invalid_aliases_fail_create_without_rewriting_file(manager, tmp_path):
    path = tmp_path / "aliases.db"
    _make_empty_3x_layout(path)
    aliases = path / "aliases.json"
    aliases.write_bytes(b"")
    uri = manager.start_and_get_uri(str(path))
    assert uri is not None
    client = MilvusClient(uri)
    assert client.list_collections() == []
    with pytest.raises(MilvusException) as error:
        client.create_collection("c", dimension=2)
    assert error.value.code == 1
    assert aliases.read_bytes() == b""
    assert not (path / "collections" / "c").exists()
    client.close()
    manager.release_server(str(path))
    assert lock_is_free(path)


def test_release_server_then_reopen(manager, tmp_path):
    path = tmp_path / "r.db"
    first = manager.start_and_get_uri(str(path))
    assert first is not None
    manager.release_server(str(path))
    assert can_open_in_other_process(path)
    second = manager.start_and_get_uri(str(path))
    assert second is not None
    assert MilvusClient(second).get_server_version().startswith("milvus_lite-")


def test_normal_exit_releases_lock_without_hanging(tmp_path):
    path = tmp_path / "exit.db"
    # 退出时仍有打开的客户端连接：atexit 停止与句柄析构都不得挂起。
    run_script(
        f"""
        from pymilvus import MilvusClient
        c = MilvusClient({str(path)!r})
        print("OUT", c.get_server_version())
        """,
        tmp_path,
        timeout=30,
    )
    assert can_open_in_other_process(path)


def _slow_search_env(tmp_path, seconds, pick=None):
    """起一个服务，建好使一次检索耗时约 ``seconds`` 秒的 collection，返回 (manager, path, 发起检索的函数)。

    ``pick`` 为 ``min`` 时按多次测量中最快的一次、``max`` 时按最慢的一次，把耗时按条数线性近似换算，
    使检索大致偏长或偏短于 ``seconds``；这是近似估计，调用方须在前提上自留余量，不能当作保证。
    """
    from pymilvus.grpc_gen import common_pb2, milvus_pb2, milvus_pb2_grpc, schema_pb2
    from rawproto import raw_insert

    manager = ServerManager()
    path = tmp_path / "slow.db"
    uri = manager.start_and_get_uri(str(path))
    channel = grpc.insecure_channel(uri.removeprefix("http://"))
    stub = milvus_pb2_grpc.MilvusServiceStub(channel)
    dim, rows = 128, 50_000
    schema = schema_pb2.CollectionSchema(fields=[
        schema_pb2.FieldSchema(name="id", data_type=5, is_primary_key=True),
        schema_pb2.FieldSchema(name="v", data_type=101,
                               type_params=[common_pb2.KeyValuePair(key="dim", value=str(dim))]),
    ])
    stub.CreateCollection(milvus_pb2.CreateCollectionRequest(collection_name="s", schema=schema.SerializeToString()))
    raw = channel.unary_unary("/milvus.proto.milvus.MilvusService/Insert",
                              response_deserializer=milvus_pb2.MutationResult.FromString)
    assert raw(raw_insert("s", range(rows), dim)).status.code == 0

    def search(nq):
        pg = common_pb2.PlaceholderGroup()
        pg.placeholders.add(tag="$0", type=101, values=[bytes(4 * dim)] * nq)
        # 只取 id：默认会带回向量字段，查询条数多时响应会超过客户端 4MB 的接收上限。
        return stub.Search(milvus_pb2.SearchRequest(
            collection_name="s", placeholder_group=pg.SerializeToString(), output_fields=["id"],
            search_params=[common_pb2.KeyValuePair(key="topk", value="1")]))

    # 检索并行、多条查询一起打分，条数少时单条摊到的开销偏大；放大条数直到一次检索足够长，
    # 用它反推的单条耗时才接近目标条数下的实际值。
    nq = 8
    while True:
        start = time.monotonic()
        search(nq)
        took = time.monotonic() - start
        if took >= 0.3:
            break
        nq *= 4
    if pick is not None:
        for _ in range(2):
            start = time.monotonic()
            search(nq)
            took = pick(took, time.monotonic() - start)
    nq = max(1, int(seconds * nq / took))
    return manager, path, lambda: search(nq)


def _run_in_thread(fn):
    result = {}

    def target():
        try:
            result["value"] = fn()
        except grpc.RpcError as e:
            result["error"] = e.code()
        result["done_at"] = time.monotonic()

    t = threading.Thread(target=target)
    t.start()
    return t, result


def test_stop_lets_short_in_flight_request_finish(tmp_path):
    # 0003「验证」正常退出的冒烟：短请求在停服时照常返回、之后 LOCK 已释放。宽限期内已入场的
    # 请求不被取消，由 grpc 的 rpc_admitted_before_stop_completes_normally_within_grace 以钩子判定。
    # 这里按多次测量中最慢的一次近似换算到约 0.5 秒，与宽限期之间按线性估计留约 4 倍余量；请求也可能
    # 在停服前就已完成，此时本用例不覆盖在途情形。
    manager, path, search = _slow_search_env(tmp_path, seconds=0.5, pick=max)
    t, result = _run_in_thread(search)
    time.sleep(0.3)
    manager.release_server(str(path))
    t.join()
    assert "value" in result and result["value"].status.code == 0, result
    assert lock_is_free(path)


def test_stop_cancels_long_request_then_releases_lock_and_reopens(tmp_path):
    # 0003「进程退出」的冒烟：宽限期（2 秒）满后仍在进行的请求被取消，客户端收到 UNAVAILABLE；停服后
    # LOCK 已释放，重开后数据完整。取消早于停服完成、停服等已开始的引擎任务结束，由 grpc 的
    # stop_retains_lock_until_admitted_rpc_finishes_after_runtime_shutdown 以钩子和事件顺序判定；这里不比较
    # 两次请求的耗时，也不比较线程记录时间的先后（记录时刻受线程调度影响）。
    # 唯一的前提是检索长于宽限期：按多次测量中最快的一次近似换算到约 10 秒，按线性估计留约 4 倍余量。
    manager, path, search = _slow_search_env(tmp_path, seconds=10.0, pick=min)
    t, result = _run_in_thread(search)
    time.sleep(0.3)
    stop_started = time.monotonic()
    manager.release_server(str(path))
    t.join(timeout=60)
    assert not t.is_alive()
    if "value" in result:
        # 仅为诊断：区分“检索短于宽限期、前提不成立”与“宽限期满仍未取消”。
        pytest.fail(
            f"request finished {result['done_at'] - stop_started:.2f}s after stop began "
            "without being cancelled; under 2s means the search was shorter than the grace period"
        )
    assert result.get("error") == grpc.StatusCode.UNAVAILABLE, result
    assert lock_is_free(path)
    reopened = ServerManager()
    client = MilvusClient(reopened.start_and_get_uri(str(path)))
    try:
        client.load_collection("s")
        assert client.query("s", output_fields=["count(*)"])[0]["count(*)"] == 50_000
    finally:
        client.close()
        reopened.release_server(str(path))
    assert lock_is_free(path)


def test_unimplemented_rpc_reason(manager, tmp_path):
    uri = manager.start_and_get_uri(str(tmp_path / "u.db"))
    channel = grpc.insecure_channel(uri.removeprefix("http://"))
    stage = "not implemented in this build of milvus-lite-rust yet"
    for path, details in [
        (
            "/milvus.proto.milvus.MilvusService/GetCompactionPlans",
            f"MilvusLite does not support GetCompactionPlans: {stage}",
        ),
        # 3.x 自身也拒绝的 RPC，说明文字与 3.x 一致。
        (
            "/milvus.proto.milvus.MilvusService/LoadPartitions",
            "MilvusLite does not support LoadPartitions: "
            "partition-level load is not supported; use load_collection instead",
        ),
        (
            "/milvus.proto.milvus.ClientTelemetryService/GetClientTelemetry",
            f"MilvusLite does not support GetClientTelemetry: {stage}",
        ),
        ("/no.such.Service/Foo", f"MilvusLite does not support Foo: {stage}"),
    ]:
        with pytest.raises(grpc.RpcError) as info:
            channel.unary_unary(path)(b"")
        assert info.value.code() == grpc.StatusCode.UNIMPLEMENTED
        assert info.value.details() == details
    # 连接不因 UNIMPLEMENTED 而失效。
    assert MilvusClient(uri).get_server_version().startswith("milvus_lite-")


def test_dropping_running_handle_does_not_block_and_releases_lock(tmp_path):
    # 不调 stop 直接丢弃运行中的句柄（创建者进程内的 Drop）：不得阻塞，LOCK 在进程仍存活时即释放；
    # 解释器随后退出也不得挂起。
    path = tmp_path / "drop.db"
    out = run_script(
        f"""
        import gc, sys, time
        sys.path.insert(0, {str(Path(__file__).parent)!r})
        from test_server_manager import lock_is_free
        from milvus_lite._native import NativeServer
        from pathlib import Path
        server = NativeServer.start({str(path)!r}, "127.0.0.1", 0, "test")
        print("OUT held", not lock_is_free(Path({str(path)!r})))
        t = time.monotonic()
        del server
        gc.collect()
        print("OUT drop-seconds-lt-1", time.monotonic() - t < 1)
        print("OUT released", lock_is_free(Path({str(path)!r})))
        """,
        tmp_path,
        timeout=30,
    )
    assert out == ["held True", "drop-seconds-lt-1 True", "released True"]
    # 句柄存活到解释器退出：由解释器析构，同样不得挂起。
    run_script(
        f"""
        from milvus_lite._native import NativeServer
        server = NativeServer.start({str(tmp_path / "live.db")!r}, "127.0.0.1", 0, "test")
        """,
        tmp_path,
        timeout=30,
    )
    assert lock_is_free(tmp_path / "live.db")


def test_official_package_is_refused(tmp_path):
    dist = tmp_path / "site" / "milvus_lite-3.2.1.dist-info"
    dist.mkdir(parents=True)
    (dist / "METADATA").write_text("Metadata-Version: 2.1\nName: milvus-lite\nVersion: 3.2.1\n")
    proc = subprocess.run(
        [sys.executable, "-c", "import milvus_lite"],
        env={**os.environ, "PYTHONPATH": str(tmp_path / "site")},
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert proc.returncode != 0
    assert "cannot coexist" in proc.stderr


FORK_PRELUDE = """
import os, subprocess, sys
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as sm

def other_process_can_open(path):
    code = ("from milvus_lite.server_manager import server_manager_instance as sm;"
            f"print(sm.start_and_get_uri({path!r}) is not None)")
    return subprocess.run([sys.executable, "-c", code], capture_output=True,
                          text=True, timeout=30).stdout.strip() == "True"

def fork_and_wait(child):
    pid = os.fork()
    if pid == 0:
        try:
            child()
        except BaseException as e:
            print("OUT child-error", repr(e), flush=True)
            os._exit(1)
        sys.stdout.flush()
        sys.exit(0)  # 正常退出：触发子进程的 atexit（0003 E4 的场景）
    _, status = os.waitpid(pid, 0)
    print("OUT child-exit", os.waitstatus_to_exitcode(status), flush=True)
"""


@needs_fork
@pytest.mark.parametrize("child_stop", [False, True])
def test_explicit_unlock_and_inherited_native_stop_guard(tmp_path, child_stop):
    probe = Path(__file__).with_name("inherited_lock_probe.py")
    command = [sys.executable, str(probe), "rust", str(tmp_path / "held.db")]
    if child_stop:
        command.append("--child-stop")
    result = subprocess.run(command, capture_output=True, text=True, timeout=60)
    assert result.returncode == 0, result.stderr
    assert json.loads(result.stdout) == {
        "held_before_close": True,
        "free_after_close": True,
        "reopen_while_child_alive": True,
        "child_stop_rejected": child_stop,
        "child_exit": 0,
    }


@needs_fork
def test_fork_child_uses_parent_server_and_exit_keeps_parent_lock(tmp_path):
    out = run_script(
        FORK_PRELUDE
        + """
uri = sm.start_and_get_uri("p.db")

def child():
    print("OUT child-uri-same", sm.start_and_get_uri("p.db") == uri, flush=True)
    print("OUT child-version", MilvusClient("p.db").get_server_version(), flush=True)
    sm.release_server("p.db")
    print("OUT child-uri-after-release-same", sm.start_and_get_uri("p.db") == uri, flush=True)

fork_and_wait(child)
print("OUT parent-still-locked", not other_process_can_open("p.db"))
print("OUT parent-still-serving", MilvusClient(uri).get_server_version().startswith("milvus_lite-"))
""",
        tmp_path,
    )
    assert "child-exit 0" in out
    assert "child-uri-same True" in out
    assert any(line.startswith("child-version milvus_lite-") for line in out)
    assert "child-uri-after-release-same True" in out
    assert "parent-still-locked True" in out
    assert "parent-still-serving True" in out


@needs_fork
def test_fork_child_owns_new_path(tmp_path):
    out = run_script(
        FORK_PRELUDE
        + """
sm.start_and_get_uri("p.db")

def child():
    uri = sm.start_and_get_uri("c.db")
    print("OUT child-new-ok", MilvusClient(uri).get_server_version().startswith("milvus_lite-"), flush=True)

fork_and_wait(child)
print("OUT child-lock-released", other_process_can_open("c.db"))
""",
        tmp_path,
    )
    assert "child-exit 0" in out
    assert "child-new-ok True" in out
    assert "child-lock-released True" in out


@needs_fork
def test_fork_during_parallel_search(tmp_path):
    # 0005「验证」：父进程的计算池正在并行检索时 fork。子进程为新路径启动自己的服务与计算池并检索；
    # 父进程的检索照常完成。行数足够多，使每次检索主要是服务端的计算；第一次检索返回后才开始 fork，
    # 并连续 fork 几次，每次都落在检索进行之中。子进程设 alarm，死锁时被 SIGALRM 杀掉而不是挂住测试。
    out = run_script(
        FORK_PRELUDE
        + """
import signal, threading
import numpy as np
dim = 128
rng = np.random.default_rng(7)
parent = MilvusClient(sm.start_and_get_uri("p.db"))
parent.create_collection("c", dimension=dim)
data = rng.random((100_000, dim), dtype=np.float32)
for i in range(0, len(data), 10_000):
    parent.insert("c", [{"id": j, "vector": data[j].tolist()} for j in range(i, i + 10_000)])
queries = rng.random((200, dim), dtype=np.float32).tolist()
searching, stop, results = threading.Event(), threading.Event(), []

def search_loop():
    while not stop.is_set():
        results.append(len(parent.search("c", data=queries, limit=3)))
        searching.set()

t = threading.Thread(target=search_loop)
t.start()
searching.wait()

def child(iteration):
    signal.alarm(20)
    c = MilvusClient(sm.start_and_get_uri(f"c{iteration}.db"))
    c.create_collection("k", dimension=4, metric_type="L2")
    c.insert("k", [{"id": i, "vector": [i, 1, 0, 0]} for i in range(100)])
    print("OUT child-search", [h["id"] for h in c.search("k", data=[[99, 1, 0, 0]], limit=2)[0]], flush=True)
    sm.release_server(f"c{iteration}.db")

for iteration in range(3):
    fork_and_wait(lambda iteration=iteration: child(iteration))
stop.set()
t.join()
print("OUT parent-searches-ok", len(results) >= 2 and all(n == 200 for n in results), flush=True)
""",
        tmp_path,
        timeout=180,
    )
    assert out.count("child-exit 0") == 3
    assert out.count("child-search [99, 98]") == 3
    assert "parent-searches-ok True" in out


@needs_fork
def test_fork_while_table_lock_is_held_does_not_deadlock(tmp_path):
    # 另一线程持有表锁（等同于正在启动服务）时 fork：fork 前的钩子要等锁释放，
    # 子进程拿到的表锁不能处于永久加锁状态。子进程设 alarm，死锁时被 SIGALRM 杀掉而不是挂住测试。
    out = run_script(
        FORK_PRELUDE
        + """
import signal, threading, time
held = threading.Event()

def hold_table_lock():
    with sm._lock:
        held.set()
        time.sleep(0.5)

holder = threading.Thread(target=hold_table_lock)
holder.start()
held.wait()
print("OUT locked-at-fork", sm._lock.locked(), flush=True)

def child():
    signal.alarm(10)
    uri = sm.start_and_get_uri("fresh.db")
    print("OUT child-fresh-ok", uri is not None, flush=True)

fork_and_wait(child)
holder.join()
""",
        tmp_path,
        timeout=60,
    )
    assert "locked-at-fork True" in out
    assert "child-exit 0" in out
    assert "child-fresh-ok True" in out
