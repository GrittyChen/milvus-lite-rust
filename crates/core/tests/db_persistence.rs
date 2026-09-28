//! Db 的真实目录入口：写入、显式提交、关闭和重开的可观察状态。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, EngineError, FieldSchema, IndexRequest,
    InputValues, InsertBatch, LoadState, Pk,
};

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

#[test]
fn create_insert_flush_close_and_reopen() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("demo.db");
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    db.create_collection("", "c", schema()).unwrap();
    assert!(matches!(
        db.collection("", "c").unwrap().write().insert(batch(&[99])),
        Err(EngineError::Storage(_))
    ));
    assert_eq!(
        db.insert("", "c", batch(&[1, 2, 1])).unwrap(),
        [Pk::Int64(1), Pk::Int64(2), Pk::Int64(1)]
    );
    assert_eq!(db.collection("", "c").unwrap().read().count(), 2);
    db.flush_collection("", "c").unwrap();
    assert_eq!(db.insert("", "c", batch(&[3])).unwrap(), [Pk::Int64(3)]);
    db.close();
    drop(db);

    let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
    assert_eq!(reopened.list_collections("").unwrap().len(), 1);
    let handle = reopened.collection("", "c").unwrap();
    let collection = handle.read();
    assert_eq!(collection.count(), 3);
    let got = collection.get(&[Pk::Int64(1), Pk::Int64(3)], &[]).unwrap();
    assert_eq!(got.rows.pks, [Pk::Int64(1), Pk::Int64(3)]);
    drop(collection);
    reopened.close();
}

#[test]
fn close_nowait_keeps_unflushed_wal_for_recovery() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("wal.db");
    let db = std::sync::Arc::new(Db::new(DataDir::open(&path).unwrap(), compute()));
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[10, 20])).unwrap();
    db.close_nowait();
    db.close();
    drop(db);
    let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
    assert_eq!(reopened.collection("", "c").unwrap().read().count(), 2);
    reopened.close();
}

#[test]
fn drop_then_recreate_rejects_the_old_handle() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("drop.db");
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    db.create_collection("", "c", schema()).unwrap();
    let old = db.collection("", "c").unwrap();
    db.insert("", "c", batch(&[1])).unwrap();
    db.drop_collection("", "c").unwrap();
    assert!(old.write().insert(batch(&[2])).is_err());
    db.create_collection("", "c", schema()).unwrap();
    assert_eq!(db.collection("", "c").unwrap().read().count(), 0);
    db.close();
    let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
    assert_eq!(reopened.collection("", "c").unwrap().read().count(), 0);
    reopened.close();
}

#[test]
fn one_corrupt_collection_fails_closed_without_hiding_others() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("broken.db");
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    for name in ["broken", "healthy"] {
        db.create_collection("", name, schema()).unwrap();
        db.insert("", name, batch(&[1])).unwrap();
    }
    db.close();
    let manifest = path.join("collections/broken/manifest.json");
    std::fs::write(&manifest, b"{").unwrap();
    let files_before = std::fs::read_dir(path.join("collections/broken/partitions/_default/data"))
        .unwrap()
        .count();

    let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
    assert_eq!(
        reopened.list_collections("").unwrap(),
        vec![
            ("broken".to_owned(), milvus_lite_core::LoadState::Released),
            ("healthy".to_owned(), milvus_lite_core::LoadState::Loaded),
        ]
    );
    assert!(matches!(
        reopened.collection("", "broken"),
        Err(EngineError::Storage(_))
    ));
    assert_eq!(
        reopened.collection("", "healthy").unwrap().read().count(),
        1
    );
    assert_eq!(
        std::fs::read_dir(path.join("collections/broken/partitions/_default/data"))
            .unwrap()
            .count(),
        files_before
    );
    assert_eq!(std::fs::read(&manifest).unwrap(), b"{");
    reopened.close();
}

#[test]
fn future_schema_key_fails_as_unsupported_without_modifying_files() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("future.db");
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    db.create_collection("", "c", schema()).unwrap();
    db.close();
    let schema_path = path.join("collections/c/schema.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&schema_path).unwrap()).unwrap();
    value["future_key"] = serde_json::json!(1);
    let bytes = serde_json::to_vec(&value).unwrap();
    std::fs::write(&schema_path, &bytes).unwrap();
    let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
    assert_eq!(
        reopened.list_collections("").unwrap(),
        [("c".to_owned(), LoadState::Released)]
    );
    assert!(matches!(
        reopened.collection("", "c"),
        Err(EngineError::Unsupported { .. })
    ));
    assert_eq!(std::fs::read(&schema_path).unwrap(), bytes);
    reopened.close();
}

#[test]
fn index_spec_is_committed_and_recovered_with_load_state() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("index.db");
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1])).unwrap();
    db.create_index(
        "",
        "c",
        "v",
        IndexRequest {
            index_type: "FLAT".to_owned(),
            metric_type: Some("L2".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path.join("collections/c/manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["index_specs"]["v"]["metric_type"], "L2");
    db.close();

    let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
    let handle = reopened.collection("", "c").unwrap();
    let collection = handle.read();
    assert_eq!(collection.load_state(), LoadState::Released);
    assert_eq!(collection.index("v").unwrap().metric_type, "L2");
    assert_eq!(collection.count(), 1);
    drop(collection);
    drop(handle);
    reopened.close();
}

#[test]
fn insert_response_pk_type_comes_from_the_handle_that_received_the_write() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("recreate.db");
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    db.create_collection("", "c", schema()).unwrap();
    let old_handle = db.collection("", "c").unwrap();
    db.drop_collection("", "c").unwrap();
    let mut pk = FieldSchema::new("id", DataType::VarChar);
    pk.is_primary = true;
    pk.max_length = Some(16);
    let mut vector = FieldSchema::new("v", DataType::FloatVector);
    vector.dim = Some(2);
    db.create_collection(
        "",
        "c",
        CollectionSchema {
            fields: vec![pk, vector],
            enable_dynamic_field: false,
            properties: Vec::new(),
        },
    )
    .unwrap();
    let outcome = db
        .insert_with_pk_type(
            "",
            "c",
            InsertBatch {
                num_rows: 1,
                columns: vec![
                    ("id".to_owned(), InputValues::Str(vec!["new".to_owned()])),
                    (
                        "v".to_owned(),
                        InputValues::FloatVector {
                            dim: 2,
                            values: vec![1.0, 0.0],
                        },
                    ),
                ],
            },
        )
        .unwrap();
    assert!(outcome.varchar);
    assert_eq!(outcome.pks, [Pk::VarChar("new".to_owned())]);
    assert!(matches!(
        old_handle.write().insert(batch(&[2])),
        Err(EngineError::CollectionNotFound(_))
    ));
    db.close();
}

#[test]
fn create_cleans_dangling_aliases_before_the_new_name_can_reuse_them() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("aliases.db");
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    db.create_collection("", "existing", schema()).unwrap();
    std::fs::write(
        path.join("aliases.json"),
        br#"{"stale":"new","live":"existing"}"#,
    )
    .unwrap();
    db.create_collection("", "new", schema()).unwrap();
    let aliases: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path.join("aliases.json")).unwrap()).unwrap();
    assert_eq!(aliases, serde_json::json!({"live": "existing"}));
    db.close();
}

#[test]
fn create_checks_directory_shape_before_cleaning_aliases() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("occupied.db");
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    let aliases = br#"{"stale":"missing"}"#;
    std::fs::write(path.join("aliases.json"), aliases).unwrap();
    std::fs::write(path.join("collections"), b"occupied").unwrap();
    assert!(db.create_collection("", "missing", schema()).is_err());
    assert_eq!(std::fs::read(path.join("aliases.json")).unwrap(), aliases);
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn post_replace_failure_recommits_before_accepting_another_write() {
    use milvus_lite_core::storage::fs_commit::inject_manifest_dir_sync_failure_once;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("fault.db");
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1])).unwrap();
    inject_manifest_dir_sync_failure_once();
    assert!(db.flush_collection("", "c").is_err());
    assert_eq!(db.collection("", "c").unwrap().read().count(), 1);
    assert!(
        db.insert("", "c", batch(&[2]))
            .unwrap_err()
            .to_string()
            .contains("throttled")
    );
    db.flush_collection("", "c").unwrap();
    db.insert("", "c", batch(&[2])).unwrap();
    inject_manifest_dir_sync_failure_once();
    assert!(db.flush_collection("", "c").is_err());
    assert!(
        db.insert("", "c", batch(&[3]))
            .unwrap_err()
            .to_string()
            .contains("throttled")
    );
    std::thread::sleep(std::time::Duration::from_millis(1050));
    db.insert("", "c", batch(&[3])).unwrap();
    db.close();
    let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
    assert_eq!(reopened.collection("", "c").unwrap().read().count(), 3);
    reopened.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn partial_wal_append_does_not_resurrect_the_failed_batch() {
    use milvus_lite_core::storage::wal::inject_append_failure_after_bytes;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("partial.db");
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1])).unwrap();
    inject_append_failure_after_bytes(20);
    assert!(db.insert("", "c", batch(&[2])).is_err());
    db.insert("", "c", batch(&[3])).unwrap();
    db.close();
    let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
    let handle = reopened.collection("", "c").unwrap();
    assert_eq!(handle.read().count(), 2);
    assert_eq!(
        handle
            .read()
            .get(&[Pk::Int64(1), Pk::Int64(2), Pk::Int64(3)], &[])
            .unwrap()
            .rows
            .pks,
        [Pk::Int64(1), Pk::Int64(3)]
    );
    drop(handle);
    reopened.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn wal_cleanup_failures_do_not_block_later_inserts() {
    use milvus_lite_core::storage::flush::inject_wal_unlink_failure_once;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("cleanup.db");
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    db.create_collection("", "c", schema()).unwrap();
    for id in [1, 2] {
        db.insert("", "c", batch(&[id])).unwrap();
        inject_wal_unlink_failure_once();
        assert!(db.flush_collection("", "c").is_err());
    }
    db.insert("", "c", batch(&[3])).unwrap();
    db.close();
    let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
    assert_eq!(reopened.collection("", "c").unwrap().read().count(), 3);
    reopened.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn close_worker_spawn_failure_still_syncs_accepted_wal() {
    use milvus_lite_core::db::inject_close_worker_spawn_failure_once;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("worker.db");
    let db = std::sync::Arc::new(Db::new(DataDir::open(&path).unwrap(), compute()));
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1])).unwrap();
    inject_close_worker_spawn_failure_once();
    db.close_nowait();
    drop(db);
    let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
    assert_eq!(reopened.collection("", "c").unwrap().read().count(), 1);
    reopened.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn inherited_files_are_synced_before_the_first_commit_only() {
    use milvus_lite_core::storage::flush::inherited_sync_count;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("sync-once.db");
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    db.create_collection("", "c", schema()).unwrap();
    let before = inherited_sync_count();
    for id in [1, 2] {
        db.insert("", "c", batch(&[id])).unwrap();
        db.flush_collection("", "c").unwrap();
    }
    assert_eq!(inherited_sync_count() - before, 1);
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn index_recommit_does_not_delete_inherited_wal_before_first_generation_publish() {
    use milvus_lite_core::storage::flush::inject_wal_unlink_failure_once;
    use milvus_lite_core::storage::fs_commit::inject_manifest_dir_sync_failure_once;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("inherited-wal.db");
    let db = std::sync::Arc::new(Db::new(DataDir::open(&path).unwrap(), compute()));
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1])).unwrap();
    inject_wal_unlink_failure_once();
    assert!(db.flush_collection("", "c").is_err());
    db.close_nowait();
    db.close();
    drop(db);
    let wal = path.join("collections/c/wal/wal_data_000001.arrow");
    assert!(wal.exists());

    let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
    assert_eq!(reopened.collection("", "c").unwrap().read().count(), 1);
    inject_manifest_dir_sync_failure_once();
    reopened
        .create_index(
            "",
            "c",
            "v",
            IndexRequest {
                index_type: "FLAT".to_owned(),
                metric_type: Some("L2".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(wal.exists());
    reopened.flush_collection("", "c").unwrap();
    assert!(wal.exists());
    reopened.insert("", "c", batch(&[2])).unwrap();
    reopened.flush_collection("", "c").unwrap();
    assert!(!wal.exists());
    reopened.close();
    let final_open = Db::new(DataDir::open(&path).unwrap(), compute());
    assert_eq!(final_open.collection("", "c").unwrap().read().count(), 2);
    final_open.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn normalized_create_name_rolls_back_before_exposing_collection() {
    use milvus_lite_core::db::inject_create_name_verification_failure_once;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("normalized.db");
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    inject_create_name_verification_failure_once();
    assert!(db.create_collection("", "c", schema()).is_err());
    assert!(!path.join("collections/c").exists());
    assert!(db.list_collections("").unwrap().is_empty());
    db.create_collection("", "c", schema()).unwrap();
    assert_eq!(db.list_collections("").unwrap().len(), 1);
    db.close();
}
