"""外部参照套件的服务适配和诊断；反事实不修改测试断言或写入数据。"""

import functools
import inspect
import json
import os
from pathlib import Path
import sys
import types

MODES = ("original", "scalar-nullable", "defaults", "scalar-nullable-defaults", "all-nullable", "partition-key")
_node = "<session>"
_phase = "setup"
_schemas = {}


def plain(value):
    if value is None or isinstance(value, (str, bool, int, float)):
        return value
    if isinstance(value, dict):
        return {str(k): plain(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [plain(v) for v in value]
    if hasattr(value, "WhichOneof"):
        kind = value.WhichOneof("data")
        return {"kind": kind, "value": plain(getattr(value, kind)) if kind else None}
    return repr(value)


def emit(kind, **data):
    with Path(os.environ["MLR_AUDIT_EVENTS"]).open("a", encoding="utf-8") as out:
        out.write(json.dumps({"nodeid": _node, "event": kind, **data}, ensure_ascii=True) + "\n")


def altered(dtype, kwargs, mode):
    result = dict(kwargs)
    if mode == "original":
        return result
    try:
        dtype = int(dtype)
    except (TypeError, ValueError):
        return result
    if mode == "partition-key":
        result.pop("is_partition_key", None)
        return result
    # ARRAY、时间和几何不属于第一切片，不能用移除属性掩盖其类型阻断。
    scalar = dtype in (1, 2, 3, 4, 5, 10, 11, 21, 23)
    if mode == "all-nullable" or (scalar and mode in ("scalar-nullable", "scalar-nullable-defaults")):
        result.pop("nullable", None)
    if mode in ("defaults", "scalar-nullable-defaults"):
        result.pop("default_value", None)
    return result


def install_observation(mode):
    from pymilvus import FieldSchema, MilvusClient

    original = FieldSchema.__init__

    @functools.wraps(original)
    def field_init(self, *args, **kwargs):
        dtype = kwargs.get("dtype", args[1] if len(args) > 1 else None)
        changed = altered(dtype, kwargs, mode)
        original(self, *args, **changed)
        self._mlr_audit_declaration = {
            "name": self.name, "dtype": int(self.dtype), "nullable": kwargs.get("nullable", False),
            "is_partition_key": kwargs.get("is_partition_key", False),
            "has_default": "default_value" in kwargs and kwargs["default_value"] is not None,
            "default": plain(kwargs.get("default_value")),
            "changed": plain({k: kwargs[k] for k in kwargs.keys() - changed.keys()}),
        }

    FieldSchema.__init__ = field_init
    create = MilvusClient.create_collection

    @functools.wraps(create)
    def create_collection(self, collection_name, *args, **kwargs):
        schema = kwargs.get("schema")
        fields = [] if schema is None else [
            getattr(field, "_mlr_audit_declaration", {
                "name": field.name, "dtype": int(field.dtype), "nullable": field.nullable,
                "has_default": field.default_value is not None,
            }) for field in schema.fields
        ]
        emit("create", collection=collection_name, fields=fields)
        _schemas[collection_name] = {"fields": fields, "creator_node": _node}
        return create(self, collection_name, *args, **kwargs)

    MilvusClient.create_collection = create_collection
    for name in ("insert", "upsert"):
        method = getattr(MilvusClient, name)

        def observed(self, collection_name, data, *args, _method=method, _name=name, **kwargs):
            rows = [data] if isinstance(data, dict) else data
            if isinstance(rows, list) and all(isinstance(row, dict) for row in rows):
                emit(_name, collection=collection_name, rows=[{
                    "keys": sorted(row), "null_fields": sorted(k for k, v in row.items() if v is None),
                } for row in rows])
            return _method(self, collection_name, data, *args, **kwargs)

        setattr(MilvusClient, name, observed)
    for name in ("insert", "upsert", "query", "search", "get", "delete"):
        method = getattr(MilvusClient, name)

        def use(self, collection_name, *args, _method=method, **kwargs):
            if collection_name in _schemas:
                emit("use", collection=collection_name, **_schemas[collection_name])
            return _method(self, collection_name, *args, **kwargs)

        setattr(MilvusClient, name, use)


def install_partition_observation(client_class=None):
    if client_class is None:
        from pymilvus import MilvusClient
        client_class = MilvusClient
    names = ("create_collection", "describe_collection", "create_partition", "drop_partition", "has_partition", "list_partitions", "get_partition_stats", "load_partitions", "release_partitions", "load_collection", "release_collection", "get_load_state", "get_loading_progress", "insert", "upsert", "delete", "get", "query", "search", "hybrid_search", "query_iterator", "search_iterator", "create_alias", "rename_collection", "create_database", "list_databases", "using_database")
    for name in names:
        method = getattr(client_class, name, None)
        if method is None:
            continue

        @functools.wraps(method)
        def traced(self, *args, _method=method, _name=name, **kwargs):
            try:
                bound = dict(inspect.signature(_method).bind(self, *args, **kwargs).arguments)
                bound.update(bound.get("kwargs", {}))
            except (TypeError, ValueError):
                # 记录不能先于 SDK 改变非法调用的异常。
                bound = kwargs
            selected = {key: plain(bound[key]) for key in ("collection_name", "partition_name", "partition_names", "num_partitions", "partial_update") if key in bound}
            emit("partition_api", method=_name, phase=_phase, stage="enter", parameters=selected)
            try:
                result = _method(self, *args, **kwargs)
            except Exception as error:
                code = getattr(error, "code", None)
                code = code() if callable(code) else code
                emit("partition_api", method=_name, phase=_phase, stage="error", parameters=selected, error_type=type(error).__name__, code=plain(getattr(code, "name", code)), reason=str(error))
                raise
            emit("partition_api", method=_name, phase=_phase, stage="return", parameters=selected)
            return result

        setattr(client_class, name, traced)


def install_rust_adapter():
    from milvus_lite.server_manager import ServerManager

    class NoDb:
        def close(self):
            pass

        def __getattr__(self, name):
            raise RuntimeError(f"GAP_INTERNAL_DB:{name}")

    class Server:
        def __init__(self, manager, path):
            self.manager, self.path = manager, path

        def stop(self, grace=None):
            self.manager.release_server(self.path)
            return types.SimpleNamespace(wait=lambda *args, **kwargs: True)

    def start(data_dir, **kwargs):
        if kwargs:
            raise RuntimeError(f"GAP_INTERNAL_KWARGS:{sorted(kwargs)}")
        manager = ServerManager()
        path = os.path.abspath(data_dir)
        uri = manager.start_and_get_uri(path)
        if uri is None:
            raise RuntimeError("GAP_START_FAILED")
        return Server(manager, path), NoDb(), int(uri.rsplit(":", 1)[1])

    for name, attrs in {
        "milvus_lite.adapter": {}, "milvus_lite.adapter.grpc": {},
        "milvus_lite.adapter.grpc.server": {"start_server_in_thread": start},
        "milvus_lite.index": {}, "milvus_lite.index.factory": {"is_faiss_available": lambda: True},
    }.items():
        module = types.ModuleType(name)
        module.__dict__.update(attrs)
        sys.modules[name] = module


def pytest_configure(config):
    mode = os.environ["MLR_AUDIT_MODE"]
    if mode not in MODES:
        raise ValueError(mode)
    if os.environ["MLR_AUDIT_BACKEND"] == "rust":
        install_rust_adapter()
    install_observation(mode)
    if os.environ.get("MLR_AUDIT_PARTITION_TRACE") == "1":
        import partition_trace
        config.pluginmanager.register(partition_trace, "mlr-partition-phase")
        install_partition_observation()


def pytest_runtest_logstart(nodeid, location):
    global _node
    _node = nodeid


def pytest_collection_finish(session):
    expected = json.loads(Path(os.environ["MLR_AUDIT_SELECTION"]).read_text())
    actual = [item.nodeid for item in session.items]
    if len(set(actual)) != len(actual) or set(actual) != set(expected):
        raise RuntimeError(f"用例集合改变：missing={set(expected)-set(actual)}, extra={set(actual)-set(expected)}")
