//! 0023 A 清空：判定顺序与零写入、清空后的读视图与元数据、auto_id、五种 `wal/` 状态、snapshot 与旧文件的清理、
//! 重开时按意图收尾，以及各阶段的故障、清理重试的重新入闸与进程退出。gRPC 码与文案、磁盘字节与 3.x 的逐行对照
//! 见差分测试。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use milvus_lite_core::storage::manifest::Manifest;
use milvus_lite_core::storage::snapshot::SnapshotMetadata;
use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, EngineError, FieldSchema, IndexRequest,
    InputValues, InsertBatch, LoadState, Pk,
};

const INTENT: &str = "milvus-lite-rust.truncate-intent.json";
const ID_A: &str = "0123456789abcdef0123456789abcdef";
const ID_B: &str = "fedcba9876543210fedcba9876543210";

fn compute() -> Compute {
    Compute::new(std::num::NonZeroUsize::new(2).unwrap()).unwrap()
}

fn schema() -> CollectionSchema {
    let mut pk = FieldSchema::new("id", DataType::Int64);
    pk.is_primary = true;
    let mut vector = FieldSchema::new("v", DataType::FloatVector);
    vector.dim = Some(2);
    CollectionSchema {
        fields: vec![pk, vector],
        enable_dynamic_field: false,
        properties: Vec::new(),
    }
}

fn batch(ids: &[i64]) -> InsertBatch {
    InsertBatch {
        num_rows: ids.len(),
        columns: vec![
            ("id".to_owned(), InputValues::Int(ids.to_vec())),
            (
                "v".to_owned(),
                InputValues::FloatVector {
                    dim: 2,
                    values: [1.0, 0.0].repeat(ids.len()),
                },
            ),
        ],
    }
}

fn vectors(rows: usize) -> InsertBatch {
    InsertBatch {
        num_rows: rows,
        columns: vec![(
            "v".to_owned(),
            InputValues::FloatVector {
                dim: 2,
                values: [1.0, 0.0].repeat(rows),
            },
        )],
    }
}

fn flat() -> IndexRequest {
    IndexRequest {
        index_type: "FLAT".to_owned(),
        metric_type: Some("L2".to_owned()),
        ..Default::default()
    }
}

fn open(path: &Path) -> std::sync::Arc<Db> {
    Db::new(DataDir::open(path).unwrap(), compute()).unwrap()
}

fn count(db: &Db, name: &str) -> usize {
    db.collection("", name).unwrap().read().count()
}

fn dir(root: &Path, name: &str) -> PathBuf {
    root.join("collections").join(name)
}

fn manifest(root: &Path, name: &str) -> Manifest {
    Manifest::read(&dir(root, name).join("manifest.json")).unwrap()
}

/// 全部 data / delta 文件（相对集合目录），不论是否被引用。
fn segment_files(root: &Path, name: &str) -> Vec<String> {
    let base = dir(root, name);
    tree(&base)
        .into_iter()
        .filter_map(|(path, bytes)| bytes.map(|_| path))
        .filter(|path| path.starts_with("partitions"))
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect()
}

/// 目录下全部路径与文件字节，用于断言零写入。
fn tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut entries = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                entries.insert(relative, None);
                stack.push(path);
            } else if meta.file_type().is_symlink() {
                entries.insert(relative, Some(b"<symlink>".to_vec()));
            } else {
                entries.insert(relative, Some(std::fs::read(&path).unwrap()));
            }
        }
    }
    entries
}

fn staging_is_empty(root: &Path) -> bool {
    std::fs::read_dir(root.join(".database-staging")).map_or(true, |mut e| e.next().is_none())
}

/// 清空之后的磁盘状态：没有意图、`wal/` 与任何 data / delta 文件，`.prev` 与 manifest 相同。
fn assert_truncated_on_disk(root: &Path, name: &str) {
    let base = dir(root, name);
    assert!(!base.join(INTENT).exists());
    assert!(!base.join("wal").exists());
    let files = segment_files(root, name);
    assert!(
        files
            .iter()
            .all(|f| !f.contains("/data/") && !f.contains("/delta/")),
        "{files:?}"
    );
    let current = std::fs::read(base.join("manifest.json")).unwrap();
    assert_eq!(
        std::fs::read(base.join("manifest.json.prev")).unwrap(),
        current
    );
    let parsed = Manifest::parse(&current).unwrap();
    assert_eq!(parsed.current_seq, 0);
    assert_eq!(parsed.active_wal_number, None);
    assert!(
        parsed
            .partitions
            .values()
            .all(|f| f.data_files.is_empty() && f.delta_files.is_empty())
    );
    assert!(staging_is_empty(root));
}

/// 以集合当前的 manifest 写一个 3.x 格式的 snapshot（另一个进程建立，因此调用方应在关闭期间调用）。
fn snapshot(root: &Path, collection: &str, name: &str, id: &str, extra_index: &[(&str, &str)]) {
    let base = dir(root, collection);
    let manifest = manifest(root, collection);
    let copies = base.join("snapshots/manifests").join(name);
    std::fs::create_dir_all(&copies).unwrap();
    std::fs::create_dir_all(base.join("snapshots/metadata")).unwrap();
    std::fs::copy(base.join("schema.json"), copies.join("schema.json")).unwrap();
    std::fs::copy(base.join("manifest.json"), copies.join("manifest.json")).unwrap();
    let mut index_files: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (partition, file) in extra_index {
        index_files
            .entry((*partition).to_owned())
            .or_default()
            .push(format!("indexes/{file}"));
    }
    let metadata = SnapshotMetadata {
        snapshot_format_version: 1,
        id: id.to_owned(),
        name: name.to_owned(),
        description: String::new(),
        collection_name: collection.to_owned(),
        created_at: "2026-10-30T00:00:00+00:00".to_owned(),
        created_seq: manifest.current_seq,
        manifest_version: manifest.version,
        schema_file: format!("snapshots/manifests/{name}/schema.json"),
        manifest_file: format!("snapshots/manifests/{name}/manifest.json"),
        data_files: manifest
            .partitions
            .iter()
            .filter(|(_, f)| !f.data_files.is_empty())
            .map(|(p, f)| (p.clone(), f.data_files.clone()))
            .collect(),
        delta_files: manifest
            .partitions
            .iter()
            .filter(|(_, f)| !f.delta_files.is_empty())
            .map(|(p, f)| (p.clone(), f.delta_files.clone()))
            .collect(),
        index_files,
    };
    std::fs::write(
        base.join("snapshots/metadata").join(format!("{name}.json")),
        serde_json::to_vec(&metadata).unwrap(),
    )
    .unwrap();
}

/// 为分区内第一个 data 文件写一个侧车（内容无关紧要，清理只按名字判定）。
fn sidecar(root: &Path, collection: &str, partition: &str) -> String {
    let manifest = manifest(root, collection);
    let stem = Path::new(&manifest.partitions[partition].data_files[0])
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let indexes = dir(root, collection)
        .join("partitions")
        .join(partition)
        .join("indexes");
    std::fs::create_dir_all(&indexes).unwrap();
    let name = format!("{stem}.v.FLAT.idx");
    std::fs::write(indexes.join(&name), b"sidecar").unwrap();
    name
}

#[test]
fn truncate_empties_data_and_keeps_partitions_indexes_aliases_and_load_state() {
    for loaded in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        let mut with_props = schema();
        with_props.properties = vec![("b".into(), "2".into()), ("a".into(), "1".into())];
        db.create_collection("", "c", with_props).unwrap();
        db.create_partition("", "c", "p").unwrap();
        db.insert("", "c", batch(&[1, 2, 3])).unwrap();
        db.insert_partition_with_pk_type("", "c", batch(&[10]), "p")
            .unwrap();
        db.flush_collection("", "c").unwrap();
        db.delete("", "c", &[Pk::Int64(1)]).unwrap();
        db.flush_collection("", "c").unwrap();
        db.insert("", "c", batch(&[4])).unwrap();
        db.delete("", "c", &[Pk::Int64(2)]).unwrap();
        db.create_index("", "c", "v", flat()).unwrap();
        db.create_alias("", "c", "a").unwrap();
        let handle = db.collection("", "c").unwrap();
        if loaded {
            handle.write().load().unwrap();
        } else {
            handle.write().release();
        }
        let before = manifest(root.path(), "c");
        assert!(root.path().join("collections/c/wal").exists());

        db.truncate_collection("", "a").unwrap();

        // 句柄身份不变，读视图立即为空。
        let c = handle.read();
        assert_eq!(c.count(), 0);
        if loaded {
            assert!(
                c.get(&[Pk::Int64(3), Pk::Int64(10)], &[])
                    .unwrap()
                    .rows
                    .pks
                    .is_empty()
            );
        }
        assert_eq!(c.count_partition("p").unwrap(), 0);
        assert_eq!(
            c.load_state(),
            if loaded {
                LoadState::Loaded
            } else {
                LoadState::Released
            }
        );
        assert_eq!(c.indexes().len(), 1);
        // 与 3.x 相同：清空按磁盘上的 `schema.json` 重建，建表时已排序写出。
        assert_eq!(
            c.schema().properties,
            [
                ("a".to_owned(), "1".to_owned()),
                ("b".to_owned(), "2".to_owned())
            ]
        );
        drop(c);
        assert_eq!(db.list_partitions("", "a").unwrap().0, ["_default", "p"]);
        assert_eq!(db.describe_alias("", "a").unwrap(), "c");
        let after = manifest(root.path(), "c");
        assert_eq!(after.version, before.version + 1);
        assert_eq!(after.index_specs, before.index_specs);
        assert_eq!(after.schema_version, before.schema_version);
        assert_truncated_on_disk(root.path(), "c");

        // 清空后的写入从新的 `_seq` 开始，flush 与重开照常。
        db.insert("", "c", batch(&[1, 2])).unwrap();
        db.insert_partition_with_pk_type("", "c", batch(&[10]), "p")
            .unwrap();
        db.flush_collection("", "c").unwrap();
        db.delete("", "c", &[Pk::Int64(2)]).unwrap();
        assert_eq!(count(&db, "c"), 2);
        drop(handle);
        db.close();
        let db = open(root.path());
        let c = db.collection("", "c").unwrap();
        assert_eq!(c.read().count(), 2);
        // 重开后有索引定义即 released（3.x 相同），属性按磁盘上的规范顺序。
        assert_eq!(c.read().load_state(), LoadState::Released);
        assert_eq!(
            c.read().schema().properties,
            [
                ("a".to_owned(), "1".to_owned()),
                ("b".to_owned(), "2".to_owned())
            ]
        );
        drop(c);
        db.close();
    }
}

#[test]
fn auto_id_restarts_from_one_and_continues_after_reopen() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    let mut auto = schema();
    auto.fields[0].auto_id = true;
    db.create_collection("", "c", auto).unwrap();
    let ids = |db: &Db, rows| db.insert("", "c", vectors(rows)).unwrap();
    assert_eq!(ids(&db, 5).len(), 5);
    db.flush_collection("", "c").unwrap();
    db.truncate_collection("", "c").unwrap();
    assert_eq!(ids(&db, 3), [Pk::Int64(1), Pk::Int64(2), Pk::Int64(3)]);
    db.close();
    let db = open(root.path());
    assert_eq!(ids(&db, 2), [Pk::Int64(4), Pk::Int64(5)]);
    db.close();
}

#[test]
fn truncate_checks_follow_3x_order_and_write_nothing_on_rejection() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1])).unwrap();
    db.close();
    let db = open(root.path());
    let before = tree(root.path());
    for name in ["nope", "", "a/b", ".."] {
        assert!(
            matches!(
                db.truncate_collection("", name),
                Err(EngineError::CollectionNotFound(n)) if n == name
            ),
            "{name}"
        );
    }
    assert!(matches!(
        db.truncate_collection("other", "c"),
        Err(EngineError::DatabaseNotFound(_))
    ));
    assert_eq!(tree(root.path()), before);
    db.close();

    // `wal/` 不是目录（文件时打开即失败，符号链接时清空在任何变更之前拒绝）：零写入失败。
    let wal = dir(root.path(), "c").join("wal");
    let saved = root.path().join("saved-wal");
    std::fs::rename(&wal, &saved).unwrap();
    std::fs::write(&wal, b"not a directory").unwrap();
    let db = open(root.path());
    let before = tree(root.path());
    assert!(matches!(
        db.truncate_collection("", "c"),
        Err(EngineError::Storage(_))
    ));
    assert_eq!(tree(root.path()), before);
    db.close();
    std::fs::remove_file(&wal).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&saved, &wal).unwrap();
        let db = open(root.path());
        assert_eq!(count(&db, "c"), 1);
        let before = tree(root.path());
        assert!(matches!(
            db.truncate_collection("", "c"),
            Err(EngineError::Storage(_))
        ));
        assert_eq!(tree(root.path()), before);
        db.close();
        std::fs::remove_file(&wal).unwrap();
    }
    std::fs::rename(&saved, &wal).unwrap();

    // 未处理的 0014 删除意图：按 0014 的封锁报错，零写入。
    let index_intent = dir(root.path(), "c").join("milvus-lite-rust.index-intent.json");
    std::fs::write(&index_intent, b"{").unwrap();
    let db = open(root.path());
    let _ = db.collection("", "c").unwrap();
    let before = tree(root.path());
    let error = db.truncate_collection("", "c").unwrap_err();
    assert!(
        error.to_string().contains("index changes are blocked"),
        "{error}"
    );
    assert_eq!(tree(root.path()), before);
    db.close();
    std::fs::remove_file(&index_intent).unwrap();

    // 0006 第 14 项的范围外 schema：Unsupported，零写入。
    let path = dir(root.path(), "c").join("schema.json");
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        original.replace("\"properties\": {}", "\"properties\": {\"n\": 1}"),
    )
    .unwrap();
    let db = open(root.path());
    let before = tree(root.path());
    assert!(matches!(
        db.truncate_collection("", "c"),
        Err(EngineError::Unsupported { .. })
    ));
    assert_eq!(tree(root.path()), before);
    db.close();
    std::fs::write(&path, original).unwrap();
    let db = open(root.path());
    assert_eq!(count(&db, "c"), 1);
    db.close();
}

#[test]
fn every_wal_state_truncates_and_later_writes_survive_reopen() {
    for state in [
        "never-written",
        "schema-only",
        "after-truncate",
        "empty-wal",
        "no-wal",
    ] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        db.create_collection("", "c", schema()).unwrap();
        let db = match state {
            "never-written" => db,
            "schema-only" => {
                db.close();
                open(root.path())
            }
            "after-truncate" => {
                db.insert("", "c", batch(&[1])).unwrap();
                db.truncate_collection("", "c").unwrap();
                db
            }
            "empty-wal" => {
                db.close();
                std::fs::create_dir(dir(root.path(), "c").join("wal")).unwrap();
                open(root.path())
            }
            _ => {
                db.insert("", "c", batch(&[1])).unwrap();
                db.close();
                let wal = dir(root.path(), "c").join("wal");
                assert!(std::fs::read_dir(&wal).unwrap().next().is_none());
                std::fs::remove_dir(&wal).unwrap();
                open(root.path())
            }
        };
        db.truncate_collection("", "c").unwrap();
        assert_eq!(count(&db, "c"), 0, "{state}");
        assert_truncated_on_disk(root.path(), "c");
        db.insert("", "c", batch(&[7, 8])).unwrap();
        db.flush_collection("", "c").unwrap();
        db.insert("", "c", batch(&[9])).unwrap();
        db.close();
        let db = open(root.path());
        assert_eq!(count(&db, "c"), 3, "{state}");
        db.close();
    }
}

#[test]
fn snapshots_and_the_files_they_pin_are_removed() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.create_partition("", "c", "p").unwrap();
    db.insert("", "c", batch(&[1, 2])).unwrap();
    db.insert_partition_with_pk_type("", "c", batch(&[10]), "p")
        .unwrap();
    db.flush_collection("", "c").unwrap();
    db.delete("", "c", &[Pk::Int64(1)]).unwrap();
    db.flush_collection("", "c").unwrap();
    db.close();
    // 两个 snapshot 共享同一 data 文件与侧车；第二个还钉住之后被 Drop 的分区 `p` 的数据。
    let shared = sidecar(root.path(), "c", "_default");
    let dropped = sidecar(root.path(), "c", "p");
    snapshot(root.path(), "c", "s1", ID_A, &[("_default", &shared)]);
    snapshot(
        root.path(),
        "c",
        "s2",
        ID_B,
        &[("_default", &shared), ("p", &dropped)],
    );
    let db = open(root.path());
    db.drop_partition("", "c", "p").unwrap();
    db.insert("", "c", batch(&[3])).unwrap();
    db.flush_collection("", "c").unwrap();
    db.close();
    let pinned_dropped = dir(root.path(), "c").join("partitions/p");
    assert!(pinned_dropped.exists());
    let db = open(root.path());
    db.truncate_collection("", "c").unwrap();
    assert_truncated_on_disk(root.path(), "c");
    let base = dir(root.path(), "c");
    assert!(
        std::fs::read_dir(base.join("snapshots/metadata"))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(
        std::fs::read_dir(base.join("snapshots/manifests"))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(
        segment_files(root.path(), "c").is_empty(),
        "{:?}",
        segment_files(root.path(), "c")
    );
    db.insert("", "c", batch(&[5])).unwrap();
    db.flush_collection("", "c").unwrap();
    db.close();
    let db = open(root.path());
    assert_eq!(count(&db, "c"), 1);
    db.close();
}

#[test]
fn snapshots_without_3x_identifiers_are_refused_before_any_change() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1])).unwrap();
    db.flush_collection("", "c").unwrap();
    db.close();
    snapshot(root.path(), "c", "s", "not-a-uuid", &[]);
    let db = open(root.path());
    let _ = db.collection("", "c").unwrap();
    let before = tree(root.path());
    assert!(matches!(
        db.truncate_collection("", "c"),
        Err(EngineError::Storage(_))
    ));
    assert_eq!(tree(root.path()), before);
    db.insert("", "c", batch(&[2])).unwrap();
    assert_eq!(count(&db, "c"), 2);
    db.close();
}

#[test]
fn unsorted_schema_keeps_the_in_process_order_and_is_rewritten_canonically() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    let mut with_props = schema();
    with_props.properties = vec![("z".into(), "1".into()), ("a".into(), "2".into())];
    db.create_collection("", "c", with_props).unwrap();
    db.close();
    let path = dir(root.path(), "c").join("schema.json");
    let canonical = std::fs::read(&path).unwrap();
    // 0.3.0rc3 写出的未排序文件：键序按插入顺序，属性 z 在 a 之前。
    let mut value: serde_json::Value = serde_json::from_slice(&canonical).unwrap();
    let object = value.as_object_mut().unwrap();
    let mut unsorted = serde_json::Map::new();
    for key in [
        "schema_format_version",
        "collection_name",
        "fields",
        "properties",
    ] {
        if let Some(v) = object.remove(key) {
            unsorted.insert(key.to_owned(), v);
        }
    }
    unsorted.append(object);
    unsorted.insert(
        "properties".to_owned(),
        serde_json::json!({"z": "1", "a": "2"}),
    );
    std::fs::write(&path, serde_json::to_vec(&unsorted).unwrap()).unwrap();
    let db = open(root.path());
    let order = |db: &Db| -> Vec<String> {
        db.collection("", "c")
            .unwrap()
            .read()
            .schema()
            .properties
            .iter()
            .map(|(k, _)| k.clone())
            .collect()
    };
    assert_eq!(order(&db), ["z", "a"]);
    db.truncate_collection("", "c").unwrap();
    assert_eq!(order(&db), ["z", "a"]);
    assert_eq!(std::fs::read(&path).unwrap(), canonical);
    db.close();
    let db = open(root.path());
    assert_eq!(order(&db), ["a", "z"]);
    db.close();
}

/// 手写一份意图：`old` 为旧 manifest 原文，候选按第 4 步推导、以紧凑 JSON 写出。
fn write_intent(root: &Path, name: &str, old: Option<&str>) -> String {
    let mut next: serde_json::Value = match old {
        Some(text) => serde_json::from_str(text).unwrap(),
        None => serde_json::to_value(Manifest::default()).unwrap(),
    };
    next["version"] = (next["version"].as_u64().unwrap() + 1).into();
    next["current_seq"] = 0.into();
    next["active_wal_number"] = serde_json::Value::Null;
    for files in next["partitions"].as_object_mut().unwrap().values_mut() {
        files["data_files"] = serde_json::json!([]);
        files["delta_files"] = serde_json::json!([]);
    }
    let candidate = next.to_string();
    let intent = serde_json::json!({
        "format_version": 1,
        "operation": "truncate_collection",
        "old_manifest": old,
        "candidate_manifest": candidate,
        "snapshots": [],
        "index_files": [],
    });
    std::fs::write(
        dir(root, name).join(INTENT),
        serde_json::to_vec(&intent).unwrap(),
    )
    .unwrap();
    candidate
}

#[test]
fn reopen_settles_truncate_intents_by_the_current_manifest() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1, 2])).unwrap();
    db.flush_collection("", "c").unwrap();
    db.close();
    let base = dir(root.path(), "c");
    let old = std::fs::read_to_string(base.join("manifest.json")).unwrap();

    // 当前 manifest 等于旧 manifest（提交前退出）或两者都不等（之后被改写）：只删除意图。
    for current in [None, Some("other")] {
        write_intent(root.path(), "c", Some(&old));
        if current.is_some() {
            let mut changed: serde_json::Value = serde_json::from_str(&old).unwrap();
            changed["version"] = 99.into();
            std::fs::write(base.join("manifest.json"), changed.to_string()).unwrap();
        }
        let mut expected = tree(root.path());
        expected.remove(&PathBuf::from("collections/c").join(INTENT));
        let db = open(root.path());
        assert_eq!(count(&db, "c"), 2);
        db.close();
        // 重开只删除意图；正常关闭不改写已提交的 manifest。
        assert_eq!(tree(root.path()), expected, "{current:?}");
        std::fs::write(base.join("manifest.json"), &old).unwrap();
    }

    // 意图损坏、或与 0014 删除意图同时存在：打开失败，不删除任何文件，修好后下次访问成功（不缓存）。
    let index_intent = base.join("milvus-lite-rust.index-intent.json");
    for broken in ["corrupt", "both"] {
        if broken == "corrupt" {
            std::fs::write(base.join(INTENT), b"{").unwrap();
        } else {
            write_intent(root.path(), "c", Some(&old));
            std::fs::write(&index_intent, b"{}").unwrap();
        }
        let before = tree(root.path());
        let db = open(root.path());
        let error = db.collection("", "c").map(|_| ()).unwrap_err().to_string();
        assert!(error.contains(INTENT), "{error}");
        assert_eq!(tree(root.path()), before);
        std::fs::remove_file(base.join(INTENT)).unwrap();
        let _ = std::fs::remove_file(&index_intent);
        assert_eq!(count(&db, "c"), 2);
        db.close();
    }

    // 当前 manifest 等于候选（提交之后退出；`wal/` 已在提交前移走）：补做清理，旧文件与 `.prev` 一并处理。
    std::fs::remove_dir(base.join("wal")).unwrap();
    let candidate = write_intent(root.path(), "c", Some(&old));
    std::fs::write(base.join("manifest.json"), &candidate).unwrap();
    let db = open(root.path());
    assert_eq!(count(&db, "c"), 0);
    assert_truncated_on_disk(root.path(), "c");
    db.insert("", "c", batch(&[3])).unwrap();
    db.close();
    let db = open(root.path());
    assert_eq!(count(&db, "c"), 1);
    db.close();
}

#[cfg(feature = "fault-injection")]
mod faults {
    use super::*;
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
        pause_truncate_after_publish_once, use_truncate_staging_name_once,
    };
    use milvus_lite_core::storage::fs_commit::{
        CommitPhase, inject_manifest_dir_sync_failure_once, inject_manifest_panic_once,
        inject_manifest_process_exit_once,
    };

    fn inject(point: DirectoryPoint, fault: DirectoryFault) {
        inject_directory_fault_once(DirectoryOperation::Truncate, point, fault);
    }

    /// 已打开的 `c`：已提交 2 行（其中 1 行随后被删除并提交），WAL 中还有未提交的 1 行插入与 1 行删除；
    /// snapshot `s` 钉住第一次 flush 的文件；`other` 用于观察整库写入闸门。
    fn seeded(root: &Path) -> std::sync::Arc<Db> {
        let db = open(root);
        db.create_collection("", "c", schema()).unwrap();
        db.create_collection("", "other", schema()).unwrap();
        db.insert("", "c", batch(&[1, 2])).unwrap();
        db.flush_collection("", "c").unwrap();
        db.close();
        snapshot(root, "c", "s", ID_A, &[]);
        let db = open(root);
        db.delete("", "c", &[Pk::Int64(1)]).unwrap();
        db.flush_collection("", "c").unwrap();
        db.insert("", "c", batch(&[3, 4])).unwrap();
        db.delete("", "c", &[Pk::Int64(4)]).unwrap();
        assert_eq!(count(&db, "c"), 2);
        db
    }

    /// 清空之前的状态：数据完整、可写、重开后不重复不丢失，意图与 staging 都不存在。
    fn assert_before(root: &Path, db: std::sync::Arc<Db>) {
        // 先打开集合：重开时的意图在第一次访问时处理。
        assert_eq!(count(&db, "c"), 2);
        assert!(!dir(root, "c").join(INTENT).exists());
        assert!(staging_is_empty(root));
        db.insert("", "c", batch(&[5])).unwrap();
        db.flush_collection("", "c").unwrap();
        db.insert("", "c", batch(&[6])).unwrap();
        assert_eq!(count(&db, "c"), 4);
        db.close();
        let db = open(root);
        let c = db.collection("", "c").unwrap();
        let mut pks = c
            .read()
            .get(&(1..=6).map(Pk::Int64).collect::<Vec<_>>(), &[])
            .unwrap()
            .rows
            .pks;
        pks.sort_by_key(|pk| match pk {
            Pk::Int64(v) => *v,
            Pk::VarChar(_) => 0,
        });
        assert_eq!(pks, [2, 3, 5, 6].map(Pk::Int64));
        drop(c);
        db.close();
    }

    /// 清空之后、清理完成的状态：空集合可写，snapshot 与旧文件已删除。
    fn assert_after(root: &Path, db: &Db) {
        assert_eq!(count(db, "c"), 0);
        assert_truncated_on_disk(root, "c");
        assert!(!dir(root, "c").join("snapshots/metadata/s.json").exists());
        db.insert("", "c", batch(&[7])).unwrap();
        db.flush_collection("", "c").unwrap();
        assert_eq!(count(db, "c"), 1);
    }

    /// 清理已完成：意图、staging 与 snapshot 都已删除，清空之前的全部 data / delta 与侧车都不在了。
    fn assert_cleaned(root: &Path, old: &[String]) {
        assert!(!dir(root, "c").join(INTENT).exists());
        assert!(staging_is_empty(root));
        assert!(!dir(root, "c").join("snapshots/metadata/s.json").exists());
        let remaining: Vec<_> = segment_files(root, "c")
            .into_iter()
            .filter(|f| old.contains(f))
            .collect();
        assert!(remaining.is_empty(), "{remaining:?}");
    }

    /// `schema.json` 已是规范字节时清理只同步目录；要走到替换（及其之后的故障点）须先写成非规范字节。
    fn unsort_schema(root: &Path) {
        let path = dir(root, "c").join("schema.json");
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        std::fs::write(&path, value.to_string()).unwrap();
    }

    fn blocked<T>(result: Result<T, EngineError>) -> bool {
        result.is_err_and(|e| e.to_string().contains("blocked"))
    }

    #[test]
    fn failures_before_the_commit_leave_the_collection_untouched() {
        for (point, fault) in [
            (DirectoryPoint::Wal, DirectoryFault::Error),
            (DirectoryPoint::Wal, DirectoryFault::Panic),
            (DirectoryPoint::Intent, DirectoryFault::Error),
            (DirectoryPoint::Intent, DirectoryFault::Panic),
            (DirectoryPoint::IntentAfterRename, DirectoryFault::Error),
            (DirectoryPoint::Manifest, DirectoryFault::Error),
            (DirectoryPoint::Manifest, DirectoryFault::Panic),
        ] {
            let root = tempfile::tempdir().unwrap();
            let db = seeded(root.path());
            inject(point, fault);
            let error = db.truncate_collection("", "c").unwrap_err();
            assert!(
                matches!(error, EngineError::Storage(_)),
                "{point:?}: {error}"
            );
            // 没有进入封锁或清理未完成状态。
            db.create_alias("", "c", "a").unwrap();
            assert_before(root.path(), db);
        }
        // manifest 替换之前 panic。
        let root = tempfile::tempdir().unwrap();
        let db = seeded(root.path());
        db.flush_collection("", "c").unwrap();
        inject_manifest_panic_once(CommitPhase::BeforeReplace);
        assert!(db.truncate_collection("", "c").is_err());
        assert_before(root.path(), db);

        // staging 目标已存在：零写入失败。
        let root = tempfile::tempdir().unwrap();
        let db = seeded(root.path());
        db.flush_collection("", "c").unwrap();
        std::fs::create_dir_all(root.path().join(".database-staging/taken/x")).unwrap();
        let before = tree(root.path());
        use_truncate_staging_name_once("taken");
        assert!(db.truncate_collection("", "c").is_err());
        assert_eq!(tree(root.path()), before);
        std::fs::remove_dir_all(root.path().join(".database-staging/taken")).unwrap();
        assert_before(root.path(), db);
    }

    #[test]
    fn wal_directory_sync_failure_blocks_writes_and_reopens_untruncated() {
        let root = tempfile::tempdir().unwrap();
        let db = seeded(root.path());
        inject(DirectoryPoint::WalSync, DirectoryFault::Error);
        let error = db.truncate_collection("", "c").unwrap_err().to_string();
        assert!(error.contains("durability is unconfirmed"), "{error}");
        assert!(blocked(db.insert("", "other", batch(&[1]))));
        assert!(blocked(db.truncate_collection("", "c")));
        assert_eq!(count(&db, "c"), 2);
        db.close();
        // 已提交的 WAL 移入 staging、打开时清空；全部行已在 manifest 中。
        let db = open(root.path());
        assert_before(root.path(), db);
    }

    #[test]
    fn failures_after_the_commit_finish_on_retry_or_reopen() {
        let pending = [
            (DirectoryPoint::Schema, DirectoryFault::Error),
            (DirectoryPoint::Schema, DirectoryFault::Panic),
            (DirectoryPoint::Snapshots, DirectoryFault::Error),
            (DirectoryPoint::Files, DirectoryFault::Error),
            (DirectoryPoint::Files, DirectoryFault::Panic),
            (DirectoryPoint::Prev, DirectoryFault::Error),
            (DirectoryPoint::PrevAfterRename, DirectoryFault::Error),
            (DirectoryPoint::IntentRemove, DirectoryFault::Error),
            (DirectoryPoint::IntentRemoveAfter, DirectoryFault::Error),
        ];
        for reopen in [false, true] {
            for (point, fault) in pending {
                let root = tempfile::tempdir().unwrap();
                let db = seeded(root.path());
                inject(point, fault);
                let error = db.truncate_collection("", "c").unwrap_err().to_string();
                assert!(error.contains("cleanup is pending"), "{point:?}: {error}");
                // 已清空：读看到空集合；其他集合与只读操作照常，没有进入封锁。
                assert_eq!(count(&db, "c"), 0);
                db.insert("", "other", batch(&[1])).unwrap();
                db.alter_collection("", "c", &[("k".into(), "v".into())], &[])
                    .unwrap();
                if reopen {
                    db.close();
                    let db = open(root.path());
                    assert_after(root.path(), &db);
                    db.close();
                } else {
                    // 下一次写入类请求在独占许可下补做清理，再从头执行（各入口见下一个用例）。
                    db.flush_collection("", "c").unwrap();
                    assert_after(root.path(), &db);
                    db.close();
                }
            }
        }

        // `schema.json` 替换之后的目录同步失败：整库写入封锁，重开后补做。
        let root = tempfile::tempdir().unwrap();
        let db = seeded(root.path());
        unsort_schema(root.path());
        inject(DirectoryPoint::SchemaAfterRename, DirectoryFault::Error);
        let error = db.truncate_collection("", "c").unwrap_err().to_string();
        assert!(
            error.contains("schema.json durability is unconfirmed"),
            "{error}"
        );
        assert_eq!(count(&db, "c"), 0);
        assert!(blocked(db.insert("", "other", batch(&[1]))));
        assert!(blocked(db.insert("", "c", batch(&[1]))));
        db.close();
        let db = open(root.path());
        assert_after(root.path(), &db);
        db.close();

        // manifest 替换之后目录同步失败：新读视图照常发布，下一次写入先重新提交、再清理。
        for reopen in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let db = seeded(root.path());
            db.flush_collection("", "c").unwrap();
            inject_manifest_dir_sync_failure_once();
            let error = db.truncate_collection("", "c").unwrap_err().to_string();
            assert!(
                error.contains("manifest durability is unconfirmed"),
                "{error}"
            );
            assert_eq!(count(&db, "c"), 0);
            assert!(dir(root.path(), "c").join(INTENT).exists());
            if reopen {
                db.close();
                let db = open(root.path());
                assert_after(root.path(), &db);
                db.close();
            } else {
                db.flush_collection("", "c").unwrap();
                assert_after(root.path(), &db);
                db.close();
            }
        }

        // 重建读视图失败：撤表，下次访问按重开路径处理意图。
        let root = tempfile::tempdir().unwrap();
        let db = seeded(root.path());
        let old = db.collection("", "c").unwrap();
        inject(DirectoryPoint::Rebuild, DirectoryFault::Error);
        let error = db.truncate_collection("", "c").unwrap_err().to_string();
        assert!(error.contains("cannot be rebuilt"), "{error}");
        assert!(!std::sync::Arc::ptr_eq(
            &old,
            &db.collection("", "c").unwrap()
        ));
        drop(old);
        assert_after(root.path(), &db);
        db.close();
    }

    #[test]
    fn every_write_entry_retries_the_cleanup_exclusively_and_then_runs_once() {
        type Entry = (&'static str, fn(&Db) -> Result<(), EngineError>);
        let entries: [Entry; 9] = [
            ("insert", |db| db.insert("", "c", batch(&[8])).map(|_| ())),
            ("delete", |db| {
                db.delete("", "c", &[Pk::Int64(8)]).map(|_| ())
            }),
            ("upsert", |db| {
                db.upsert_with_pk_type("", "c", batch(&[8])).map(|_| ())
            }),
            ("delete-expr", |db| {
                db.delete_expr("", "c", "id in [8]", "").map(|_| ())
            }),
            ("flush", |db| db.flush_collection("", "c")),
            ("create-index", |db| db.create_index("", "c", "v", flat())),
            ("drop-index", |db| db.drop_index("", "c", "v", "")),
            ("create-partition", |db| db.create_partition("", "c", "p")),
            ("drop-partition", |db| db.drop_partition("", "c", "q")),
        ];
        for (label, entry) in entries {
            let root = tempfile::tempdir().unwrap();
            let db = seeded(root.path());
            if label == "drop-index" {
                db.create_index("", "c", "v", flat()).unwrap();
                db.collection("", "c").unwrap().write().release();
            }
            if label == "drop-partition" {
                db.create_partition("", "c", "q").unwrap();
            }
            let old = segment_files(root.path(), "c");
            inject(DirectoryPoint::Files, DirectoryFault::Error);
            assert!(db.truncate_collection("", "c").is_err());
            // 重试仍失败：原请求报错且没有生效。
            inject(DirectoryPoint::Files, DirectoryFault::Error);
            let error = entry(&db).unwrap_err().to_string();
            assert!(error.contains("cleanup is pending"), "{label}: {error}");
            assert_eq!(count(&db, "c"), 0, "{label}");
            assert!(dir(root.path(), "c").join(INTENT).exists());
            // 重试成功：原请求从头执行一次。
            entry(&db).unwrap();
            assert_cleaned(root.path(), &old);
            let expected = usize::from(matches!(label, "insert" | "upsert"));
            assert_eq!(count(&db, "c"), expected, "{label}");
            let partitions = db.list_partitions("", "c").unwrap().0;
            assert_eq!(
                partitions.contains(&"p".to_owned()),
                label == "create-partition"
            );
            assert!(!partitions.contains(&"q".to_owned()), "{label}");
            let indexed = db.collection("", "c").unwrap().read().indexes().len();
            assert_eq!(indexed, usize::from(label == "create-index"), "{label}");
            db.close();
            let db = open(root.path());
            assert_eq!(count(&db, "c"), expected, "{label}");
            db.close();
        }

        // 独占重试中 `schema.json` 替换之后目录同步失败：整库写入封锁，原请求不生效。
        let root = tempfile::tempdir().unwrap();
        let db = seeded(root.path());
        inject(DirectoryPoint::Files, DirectoryFault::Error);
        assert!(db.truncate_collection("", "c").is_err());
        unsort_schema(root.path());
        inject(DirectoryPoint::SchemaAfterRename, DirectoryFault::Error);
        let error = db.insert("", "c", batch(&[8])).unwrap_err().to_string();
        assert!(
            error.contains("schema.json durability is unconfirmed"),
            "{error}"
        );
        assert_eq!(count(&db, "c"), 0);
        assert!(blocked(db.insert("", "other", batch(&[1]))));
        assert!(blocked(db.insert("", "c", batch(&[8]))));
        db.close();
        let db = open(root.path());
        assert_after(root.path(), &db);
        db.close();
    }

    #[test]
    fn reopen_sync_failures_are_not_cached() {
        let root = tempfile::tempdir().unwrap();
        let db = seeded(root.path());
        inject(DirectoryPoint::Files, DirectoryFault::Error);
        assert!(db.truncate_collection("", "c").is_err());
        db.close();
        for point in [
            DirectoryPoint::ManifestSync,
            DirectoryPoint::Schema,
            DirectoryPoint::Snapshots,
            DirectoryPoint::Files,
            DirectoryPoint::Prev,
        ] {
            let db = open(root.path());
            inject(point, DirectoryFault::Error);
            assert!(db.collection("", "c").is_err(), "{point:?}");
            assert!(dir(root.path(), "c").join(INTENT).exists(), "{point:?}");
            db.close();
        }
        let db = open(root.path());
        inject(DirectoryPoint::ManifestSync, DirectoryFault::Error);
        assert!(db.collection("", "c").is_err());
        assert_after(root.path(), &db);
        db.close();
    }

    /// 清理做了一半就中断（snapshot 元数据已删而 manifest 副本还在、部分旧文件已删）：重开补完；
    /// 同名但 id 不同的新 snapshot 不属于意图，必须保留。
    #[test]
    fn reopen_finishes_a_partly_done_cleanup_and_keeps_snapshots_it_does_not_own() {
        for newer in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let db = seeded(root.path());
            let old = segment_files(root.path(), "c");
            assert!(!old.is_empty());
            inject(DirectoryPoint::Files, DirectoryFault::Error);
            assert!(db.truncate_collection("", "c").is_err());
            db.close();
            let base = dir(root.path(), "c");
            // 上一轮已删元数据与副本之后，当时还剩下的状态：补回副本目录，并手工删掉一个旧文件。
            let copies = base.join("snapshots/manifests/s");
            std::fs::create_dir_all(&copies).unwrap();
            std::fs::write(copies.join("manifest.json"), b"{}").unwrap();
            assert!(!base.join("snapshots/metadata/s.json").exists());
            std::fs::remove_file(base.join(&old[0])).unwrap();
            if newer {
                // 3.x 在这之后建立的同名新 snapshot，id 不同。
                snapshot(root.path(), "c", "s", ID_B, &[]);
            }
            let db = open(root.path());
            assert_eq!(count(&db, "c"), 0);
            assert!(!base.join(INTENT).exists());
            assert!(staging_is_empty(root.path()));
            assert!(
                segment_files(root.path(), "c")
                    .iter()
                    .all(|f| !old.contains(f))
            );
            assert_eq!(base.join("snapshots/metadata/s.json").exists(), newer);
            assert_eq!(copies.exists(), newer);
            db.close();
        }
    }

    /// 清理未完成的集合被 drop 或改名：意图随目录走，drop 之后没有残留；改名之后新名首次打开补完清理。
    #[test]
    fn dropping_or_renaming_a_collection_with_unfinished_cleanup_leaves_nothing_behind() {
        for rename in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let db = seeded(root.path());
            inject(DirectoryPoint::Files, DirectoryFault::Error);
            assert!(db.truncate_collection("", "c").is_err());
            if rename {
                db.rename_collection("", "c", "d", "").unwrap();
                assert_eq!(count(&db, "d"), 0);
                db.insert("", "d", batch(&[7])).unwrap();
                db.flush_collection("", "d").unwrap();
                assert!(!dir(root.path(), "d").join(INTENT).exists());
            } else {
                db.drop_collection("", "c").unwrap();
                assert!(!dir(root.path(), "c").exists());
            }
            db.close();
            // 暂存目录里移出的 `wal/` 在下次启动清掉；两种情形都不能让旧数据回来。
            let db = open(root.path());
            assert!(staging_is_empty(root.path()), "rename={rename}");
            if rename {
                assert_eq!(count(&db, "d"), 1);
                assert!(
                    !dir(root.path(), "d")
                        .join("snapshots/metadata/s.json")
                        .exists()
                );
            } else {
                db.create_collection("", "c", schema()).unwrap();
                assert_eq!(count(&db, "c"), 0);
            }
            db.close();
        }
    }

    /// 清理未完成期间不排合并；清理完成后维护恢复，随后的写入照常触发合并。
    #[test]
    fn maintenance_resumes_after_the_cleanup_finishes() {
        let root = tempfile::tempdir().unwrap();
        let db = seeded(root.path());
        inject(DirectoryPoint::Files, DirectoryFault::Error);
        assert!(db.truncate_collection("", "c").is_err());
        for id in 1..=4 {
            // 第一次写入先在独占许可下补完清理。
            db.insert("", "c", batch(&[id])).unwrap();
            db.flush_collection("", "c").unwrap();
        }
        assert!(!dir(root.path(), "c").join(INTENT).exists());
        let report = db
            .wait_for_maintenance("", "c", std::time::Duration::from_secs(15))
            .unwrap();
        assert_eq!(report.failed, 0, "{report:?}");
        assert!(report.completed >= 1, "{report:?}");
        assert_eq!(count(&db, "c"), 4);
        db.close();
    }

    /// 继承了上一次运行遗留的已提交 WAL 文件的集合再清空：整个 `wal/` 移走，旧行不会被重放回来。
    #[test]
    fn a_collection_that_inherited_committed_wal_files_truncates_without_replay() {
        use milvus_lite_core::storage::flush::inject_wal_unlink_failure_once;
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        db.create_collection("", "c", schema()).unwrap();
        db.insert("", "c", batch(&[1])).unwrap();
        inject_wal_unlink_failure_once();
        assert!(db.flush_collection("", "c").is_err());
        db.close_nowait();
        db.close();
        drop(db);
        let wal = dir(root.path(), "c").join("wal/wal_data_000001.arrow");
        assert!(wal.exists());
        let db = open(root.path());
        assert_eq!(count(&db, "c"), 1);
        db.truncate_collection("", "c").unwrap();
        assert_eq!(count(&db, "c"), 0);
        assert_truncated_on_disk(root.path(), "c");
        db.insert("", "c", batch(&[2])).unwrap();
        db.close();
        let db = open(root.path());
        assert_eq!(count(&db, "c"), 1);
        db.close();
    }

    #[test]
    fn reads_during_cleanup_see_the_empty_collection_and_writes_wait() {
        let root = tempfile::tempdir().unwrap();
        let db = seeded(root.path());
        assert_eq!(count(&db, "other"), 0);
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (resume, resume_rx) = std::sync::mpsc::channel();
        let truncator = {
            let db = std::sync::Arc::clone(&db);
            std::thread::spawn(move || {
                pause_truncate_after_publish_once(entered_tx, resume_rx);
                db.truncate_collection("", "c")
            })
        };
        entered.recv().unwrap();
        // 已打开的集合可读，`c` 已是空集合；任何写入与 Flush 等待清空返回。
        assert_eq!(count(&db, "c"), 0);
        assert_eq!(count(&db, "other"), 0);
        assert!(dir(root.path(), "c").join(INTENT).exists());
        let waiters: Vec<_> = [
            Box::new(|db: &Db| db.insert("", "c", batch(&[9])).map(|_| ()))
                as Box<dyn Fn(&Db) -> Result<(), EngineError> + Send>,
            Box::new(|db: &Db| db.flush_collection("", "c")),
            Box::new(|db: &Db| db.insert("", "other", batch(&[1])).map(|_| ())),
        ]
        .into_iter()
        .map(|request| {
            let db = std::sync::Arc::clone(&db);
            std::thread::spawn(move || request(&db))
        })
        .collect();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(waiters.iter().all(|waiter| !waiter.is_finished()));
        resume.send(()).unwrap();
        truncator.join().unwrap().unwrap();
        for waiter in waiters {
            waiter.join().unwrap().unwrap();
        }
        assert_eq!(count(&db, "c"), 1);
        assert_eq!(count(&db, "other"), 1);
        assert!(!dir(root.path(), "c").join(INTENT).exists());
        assert!(
            !dir(root.path(), "c")
                .join("snapshots/metadata/s.json")
                .exists()
        );
        db.close();
        let db = open(root.path());
        assert_eq!(count(&db, "c"), 1);
        db.close();
    }

    #[test]
    fn process_exit_at_each_truncate_step_reopens_before_or_after() {
        use std::process::Command;
        // （阶段，是否已清空，退出码）
        let points = [
            ("wal", false, 88),
            ("wal-sync", false, 88),
            ("intent", false, 88),
            ("intent-after", false, 88),
            ("manifest", false, 88),
            ("before-replace", false, 86),
            ("after-replace", true, 86),
            ("rebuild", true, 88),
            ("schema", true, 88),
            ("schema-after", true, 88),
            ("snapshots", true, 88),
            ("files", true, 88),
            ("prev", true, 88),
            ("intent-remove", true, 88),
            ("intent-remove-after", true, 88),
            ("staging-cleanup", true, 88),
        ];
        if let Ok(stage) = std::env::var("MILVUS_LITE_TRUNCATE_CRASH") {
            let path = PathBuf::from(std::env::var("MILVUS_LITE_CRASH_DIR").unwrap());
            let db = seeded(&path);
            let point = match stage.as_str() {
                // 先 flush，使 manifest 的故障点落在清空自己的替换上，而不是第 2 步的 flush。
                "before-replace" => {
                    db.flush_collection("", "c").unwrap();
                    inject_manifest_process_exit_once(CommitPhase::BeforeReplace);
                    None
                }
                "after-replace" => {
                    db.flush_collection("", "c").unwrap();
                    inject_manifest_process_exit_once(CommitPhase::AfterReplace);
                    None
                }
                "wal" => Some(DirectoryPoint::Wal),
                "wal-sync" => Some(DirectoryPoint::WalSync),
                "intent" => Some(DirectoryPoint::Intent),
                "intent-after" => Some(DirectoryPoint::IntentAfterRename),
                "manifest" => Some(DirectoryPoint::Manifest),
                "rebuild" => Some(DirectoryPoint::Rebuild),
                "schema" => Some(DirectoryPoint::Schema),
                "schema-after" => Some(DirectoryPoint::SchemaAfterRename),
                "snapshots" => Some(DirectoryPoint::Snapshots),
                "files" => Some(DirectoryPoint::Files),
                "prev" => Some(DirectoryPoint::Prev),
                "intent-remove" => Some(DirectoryPoint::IntentRemove),
                "intent-remove-after" => Some(DirectoryPoint::IntentRemoveAfter),
                _ => Some(DirectoryPoint::StagingCleanup),
            };
            if let Some(point) = point {
                inject(point, DirectoryFault::Exit);
            }
            if stage == "schema-after" {
                unsort_schema(&path);
            }
            db.truncate_collection("", "c").unwrap();
            panic!("truncate crash injection did not fire");
        }
        // 同 crash_recovery：验证过程放在独立父进程中，避免兄弟子进程短暂继承刚关闭的 LOCK。
        let test = "faults::process_exit_at_each_truncate_step_reopens_before_or_after";
        if std::env::var_os("MILVUS_LITE_TRUNCATE_PARENT").is_none() {
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture"])
                .env("MILVUS_LITE_TRUNCATE_PARENT", "1")
                .status()
                .unwrap();
            assert!(
                status.success(),
                "isolated truncate recovery parent failed: {status:?}"
            );
            return;
        }
        for (stage, truncated, code) in points {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("truncate-crash.db");
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture"])
                .env("MILVUS_LITE_TRUNCATE_CRASH", stage)
                .env("MILVUS_LITE_CRASH_DIR", &path)
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(code), "{stage}");
            let db = open(&path);
            if truncated {
                assert_after(&path, &db);
                db.close();
            } else {
                assert_before(&path, db);
            }
        }
    }
}
