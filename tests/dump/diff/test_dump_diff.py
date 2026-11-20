"""dump 差分：相同场景下本实现与 3.x 的产物一致；登记的差异逐项断言（0037「3.x 的缺陷」）。

除 many 外的场景用默认 chunk，只在最后同步提交一次，产物逐字节比较。many 用小 chunk 切出多个文件，行在文件间怎么切分取决于
pymilvus LocalBulkWriter 的线程时机：
缓冲区超过 chunk_size 后它起写线程并立即返回、继续追加，写线程开始运行时才换掉旧缓冲区，起得晚时后续的行落进上一个文件。
两侧共用这段代码，切分不是本实现的行为，因此按顺序拼接全部行逐行比较，并核对每个文件正好是这些行的标准序列化。
"""

from __future__ import annotations

import json
import os
import re
import sys

import pytest

import dump_helpers as h


def run(tmp_path, python, side, case, *extra, built=True, **kwargs):
    root = tmp_path / side
    root.mkdir()
    db = root / "x.db"
    if built:
        h.build(case, db, python=python)
    out = root / "out"
    result = h.dump(["-d", db, "-c", "c", "-p", out, *extra], python=python, **kwargs)
    # 文件按名字放进字典，多个 uuid 目录会互相覆盖而漏检；一次 dump 至多一个。
    assert len(h.outputs(out)) <= 1, h.outputs(out)
    files = {name: open(os.path.join(out, uuid, name), encoding="utf-8").read()
             for uuid, names in h.outputs(out).items() for name in names}
    return result, files, str(root)


def both(tmp_path, ref_python, case, *extra, **kwargs):
    ours = run(tmp_path, sys.executable, "rs", case, *extra, **kwargs)
    ref = run(tmp_path, ref_python, "v3", case, *extra, **kwargs)
    return ours, ref


@pytest.mark.parametrize("case, extra", [
    ("basic", ()),
    ("scalars", ()),
    ("sparse", ()),
    ("partitions", ()),
    ("explicit_partitions", ()),
    ("tz_geo", ()),
    ("multivec", ()),
    ("varchar_pk", ("--batch-size", 1)),
    ("empty", ()),
    ("released", ()),
    ("many", ("--chunk-size", 20000, "--batch-size", 7)),
])
def test_same_output(tmp_path, ref_python, case, extra):
    (ours, ours_files, ours_root), (ref, ref_files, ref_root) = both(tmp_path, ref_python, case, *extra)
    assert ours.returncode == ref.returncode == 0, (ours.stderr[-2000:], ref.stderr[-2000:])
    assert ours.stdout.replace(ours_root, "<root>") == ref.stdout.replace(ref_root, "<root>")
    if case != "many":
        # 默认 chunk 下只在最后同步提交一次，产物与时机无关，逐字节比较；切成多个文件本身就是差异。
        assert ours_files == ref_files
    else:
        # 小 chunk 必须切出多个文件，否则这一条没有覆盖多文件的路径。
        assert len(ours_files) > 1 and len(ref_files) > 1, (sorted(ours_files), sorted(ref_files))
        assert rows_text(ours_files) == rows_text(ref_files)


def rows_text(files):
    """按文件序号拼接全部行，每行保留键序的序列化文本。

    每个文件须正好是 json.dumps({"rows": [...]}, indent=2) 的文本且行非空（BulkWriter 不写空缓冲区）；
    文件名为正整数序号（字典的键互不相同），因此空缓冲区跳过的序号不影响比较。
    """
    assert all(re.fullmatch(r"[1-9][0-9]*\.json", name) for name in files), sorted(files)
    rows = []
    for name in sorted(files, key=lambda f: int(f.removesuffix(".json"))):
        data = json.loads(files[name])
        assert list(data) == ["rows"] and data["rows"] and files[name] == json.dumps(data, indent=2), name
        rows.extend(json.dumps(row) for row in data["rows"])
    return rows


def test_function_output_fields(tmp_path, ref_python):
    # 差异 A：3.x 把 BM25 输出字段交给 BulkWriter 而失败；本实现不导出它，导入时由 Milvus 重新生成。
    (ours, ours_files, _), (ref, ref_files, _) = both(tmp_path, ref_python, "bm25")
    # 3.x 的 traceback 之后还有 BulkWriter 的清理日志，异常行不一定在最后。
    assert ref.returncode == 1 and any(line.endswith("Field 'sp' is function output, no need to provide)>")
                                       for line in ref.stderr.splitlines())
    assert ref_files == {}
    assert ours.returncode == 0 and '"sp"' not in ours_files["1.json"] and '"text": "hello world"' in ours_files["1.json"]


def test_array_of_varchar(tmp_path, ref_python):
    # 差异 B：3.x 的 Describe 不回显元素 max_length，BulkWriter 报 KeyError；本实现只给校验用的 schema 副本补上。
    (ours, ours_files, _), (ref, ref_files, _) = both(tmp_path, ref_python, "array_varchar")
    assert ref.returncode == 1 and "KeyError: 'max_length'" in ref.stderr.splitlines()
    assert ours.returncode == 0 and '"bc"' in ours_files["1.json"]


def test_nullable_vectors(tmp_path, ref_python):
    # 0038：3.x 写入时把空向量换成零向量，导出 [0.0, 0.0]；本实现存为空值，导出 null。
    (ours, ours_files, _), (ref, ref_files, _) = both(tmp_path, ref_python, "nullable_vec")
    assert ours.returncode == ref.returncode == 0, (ours.stderr[-2000:], ref.stderr[-2000:])
    assert ours_files["1.json"].replace('"vec": null', '"vec": [\n        0.0,\n        0.0\n      ]') == ref_files["1.json"]
    assert '"vec": null' in ours_files["1.json"] and '"vec": null' not in ref_files["1.json"]


def test_batch_size_zero(tmp_path, ref_python):
    # 差异 C：3.x 报告成功、导出 0 行；本实现由参数解析拒绝。
    (ours, _, _), (ref, ref_files, _) = both(tmp_path, ref_python, "explicit_partitions", "--batch-size", 0)
    assert ref.returncode == 0 and "success: 0 rows" in ref.stdout and ref_files == {}
    assert ours.returncode == 2 and "--batch-size: must be between 1 and 16384" in h.last_line(ours.stderr)


@pytest.mark.skipif(sys.platform == "win32", reason="RLIMIT_FSIZE 只在 Unix 上存在")
def test_write_failure(tmp_path, ref_python):
    # 差异 D：3.x 的写文件线程异常被吞，报告成功；本实现报失败并删除本次的输出。
    import resource

    def limit():
        resource.setrlimit(resource.RLIMIT_FSIZE, (4096, 4096))

    (ours, ours_files, _), (ref, _, _) = both(tmp_path, ref_python, "many", preexec_fn=limit)
    assert ref.returncode == 0 and "success: 2500 rows" in ref.stdout
    assert ours.returncode == 1 and "failed to write dump files" in h.last_line(ours.stderr)
    assert ours_files == {}


@pytest.mark.parametrize("case", ["non_finite", "non_finite_array"])
def test_non_finite_floats(tmp_path, ref_python, case):
    # 差异 E：3.x 写出裸 NaN / Infinity，不是合法 JSON；本实现报错并不留输出。
    (ours, ours_files, _), (ref, ref_files, _) = both(tmp_path, ref_python, case)
    assert ref.returncode == 0 and any(token in ref_files["1.json"] for token in ("NaN", "Infinity"))
    assert ours.returncode == 1 and "contains a non-finite float" in h.last_line(ours.stderr)
    assert ours_files == {}


@pytest.mark.parametrize("case, extra, message", [
    ("empty", ("-c", "nope"), "Collection: nope not exists"),
])
def test_error_presentation(tmp_path, ref_python, case, extra, message):
    # 错误呈现：3.x 打印 traceback，本实现打印一行（沿用 0012）；退出码相同。
    def go(python, side):
        root = tmp_path / side
        root.mkdir()
        db = root / "x.db"
        h.build(case, db, python=python)
        return h.dump(["-d", db, *extra, "-p", root / "out"], python=python)

    ours, ref = go(sys.executable, "rs"), go(ref_python, "v3")
    assert ours.returncode == ref.returncode == 1
    assert "Traceback" in ref.stderr and h.last_line(ref.stderr) == f"RuntimeError: {message}"
    assert h.last_line(ours.stderr) == f"MilvusLite dump failed: {message}" and "Traceback" not in ours.stderr


def test_missing_db_file(tmp_path, ref_python):
    def go(python, side):
        root = tmp_path / side
        root.mkdir()
        return h.dump(["-d", root / "nope.db", "-c", "c", "-p", root / "out"], python=python), str(root)

    (ours, ours_root), (ref, ref_root) = go(sys.executable, "rs"), go(ref_python, "v3")
    assert ours.returncode == ref.returncode == 1
    assert h.last_line(ref.stderr).replace(ref_root, "<root>") == "RuntimeError: db_file: <root>/nope.db not exists"
    assert h.last_line(ours.stderr).replace(ours_root, "<root>") == "MilvusLite dump failed: db_file: <root>/nope.db not exists"
