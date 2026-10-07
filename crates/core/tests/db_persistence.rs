//! Db 的真实目录入口：写入、显式提交、关闭和重开的可观察状态。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, EngineError, FieldSchema, IndexRequest,
    InputValues, InsertBatch, LoadState, Pk, QueryVectors, SearchRequest,
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

#[cfg(unix)]
#[test]
fn drop_displaced_open_collection_retires_wal_and_old_handle_before_recreate() {
    for remove in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("displaced.db");
        let db = Db::new(DataDir::open(&path).unwrap(), compute());
        db.create_collection("", "c", schema()).unwrap();
        db.insert("", "c", batch(&[1, 2])).unwrap();
        db.delete("", "c", &[Pk::Int64(1)]).unwrap();
        let old = db.collection("", "c").unwrap();
        let old_view = old.read().read_snapshot();
        let collection_dir = path.join("collections/c");
        if remove {
            std::fs::remove_dir_all(&collection_dir).unwrap();
        } else {
            std::fs::rename(&collection_dir, root.path().join("moved")).unwrap();
        }
        db.drop_collection("", "c").unwrap();
        assert!(matches!(
            old.read().require_loaded(),
            Err(EngineError::CollectionNotFound(_))
        ));
        assert_eq!(
            old_view
                .search(
                    SearchRequest {
                        anns_field: None,
                        vectors: QueryVectors::Dense(vec![vec![1.0, 0.0]]),
                        metric_type: Some("L2".into()),
                        limit: 10,
                        offset: 0,
                        output_fields: vec![]
                    },
                    db.compute()
                )
                .unwrap()
                .hits[0]
                .rows
                .pks,
            [Pk::Int64(2)]
        );
        assert!(!collection_dir.exists());
        db.create_collection("", "c", schema()).unwrap();
        let new = db.collection("", "c").unwrap();
        assert!(!std::sync::Arc::ptr_eq(&old, &new));
        assert_eq!(new.read().count(), 0);
        db.insert("", "c", batch(&[9])).unwrap();
        db.flush_collection("", "c").unwrap();
        db.close();
        let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
        assert_eq!(
            reopened
                .collection("", "c")
                .unwrap()
                .read()
                .get(&[Pk::Int64(9)], &[])
                .unwrap()
                .rows
                .pks,
            [Pk::Int64(9)]
        );
        reopened.close();
    }
}

#[cfg(all(unix, feature = "fault-injection"))]
#[test]
fn displaced_drop_sync_failure_retains_handle_and_retries_without_creating_files() {
    use milvus_lite_core::storage::runtime::{
        RetainedWalSyncFault, inject_retained_wal_sync_failure_once,
    };
    for fault in [RetainedWalSyncFault::Data, RetainedWalSyncFault::Delta] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("displaced-failure.db");
        let db = Db::new(DataDir::open(&path).unwrap(), compute());
        db.create_collection("", "c", schema()).unwrap();
        db.insert("", "c", batch(&[1, 2])).unwrap();
        db.delete("", "c", &[Pk::Int64(1)]).unwrap();
        let old = db.collection("", "c").unwrap();
        std::fs::rename(path.join("collections/c"), root.path().join("moved")).unwrap();
        inject_retained_wal_sync_failure_once(fault);
        assert!(
            db.drop_collection("", "c")
                .unwrap_err()
                .to_string()
                .contains("injected retained WAL sync")
        );
        assert!(std::sync::Arc::ptr_eq(
            &old,
            &db.collection("", "c").unwrap()
        ));
        assert!(!path.join("collections/c").exists());
        db.drop_collection("", "c").unwrap();
        assert!(matches!(
            old.read().require_loaded(),
            Err(EngineError::CollectionNotFound(_))
        ));
        db.create_collection("", "c", schema()).unwrap();
        db.close();
    }
}

#[test]
fn drop_missing_schema_retires_the_open_handle_without_removing_the_residual_directory() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("missing-schema.db");
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1])).unwrap();
    let old = db.collection("", "c").unwrap();
    let collection = path.join("collections/c");
    let wal = collection.join("wal/wal_data_000001.arrow");
    let before = std::fs::read(&wal).unwrap();
    std::fs::remove_file(collection.join("schema.json")).unwrap();
    db.drop_collection("", "c").unwrap();
    assert!(matches!(
        old.read().require_loaded(),
        Err(EngineError::CollectionNotFound(_))
    ));
    assert!(collection.exists());
    assert!(matches!(
        db.create_collection("", "c", schema()),
        Err(EngineError::CollectionAlreadyExists(_))
    ));
    db.close();
    assert_eq!(std::fs::read(wal).unwrap(), before);
}

#[test]
fn empty_delete_does_not_create_wal_or_consume_sequence() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("empty-delete.db");
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    let mut auto_schema = schema();
    auto_schema.fields[0].auto_id = true;
    db.create_collection("", "c", auto_schema).unwrap();
    assert_eq!(db.delete("", "c", &[]).unwrap(), 0);
    assert!(!path.join("collections/c/wal").exists());
    assert!(db.delete_expr("", "c", "id in ['bad']", "").is_err());
    assert_eq!(
        db.insert(
            "",
            "c",
            InsertBatch {
                num_rows: 1,
                columns: vec![(
                    "v".to_owned(),
                    InputValues::FloatVector {
                        dim: 2,
                        values: vec![1.0, 0.0],
                    }
                )],
            },
        )
        .unwrap(),
        [Pk::Int64(2)]
    );
    db.close();
}

#[test]
fn delete_and_partial_upsert_survive_flush_and_reopen() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("mutations.db");
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1, 2])).unwrap();
    db.flush_collection("", "c").unwrap();
    assert_eq!(
        db.delete("", "c", &[Pk::Int64(1), Pk::Int64(99), Pk::Int64(1)])
            .unwrap(),
        3
    );
    assert_eq!(db.collection("", "c").unwrap().read().count(), 1);
    assert!(
        db.upsert_with_pk_type(
            "",
            "c",
            InsertBatch {
                num_rows: 1,
                columns: vec![("id".to_owned(), InputValues::Int(vec![1]))],
            },
        )
        .is_err()
    );
    assert_eq!(db.collection("", "c").unwrap().read().count(), 1);
    let partial = InsertBatch {
        num_rows: 1,
        columns: vec![("id".to_owned(), InputValues::Int(vec![2]))],
    };
    assert_eq!(
        db.upsert_with_pk_type("", "c", partial).unwrap().pks,
        [Pk::Int64(2)]
    );
    assert_eq!(db.collection("", "c").unwrap().read().count(), 1);
    db.flush_collection("", "c").unwrap();
    db.close();
    drop(db);

    let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
    let handle = reopened.collection("", "c").unwrap();
    let c = handle.read();
    assert_eq!(c.count(), 1);
    let got = c
        .get(&[Pk::Int64(1), Pk::Int64(2)], &["v".to_owned()])
        .unwrap();
    assert_eq!(got.rows.pks, [Pk::Int64(2)]);
    assert_eq!(
        got.rows.fields[0].1,
        milvus_lite_core::ColumnData::FloatVector {
            dim: 2,
            values: vec![1.0, 0.0],
        }
    );
    drop(c);
    reopened.close();
}

#[test]
fn upsert_repeated_pk_merges_with_earlier_row_in_same_batch() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("merge.db");
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    db.create_collection("", "c", schema()).unwrap();
    let merged = InsertBatch {
        num_rows: 2,
        columns: vec![
            ("id".to_owned(), InputValues::Int(vec![7, 7])),
            (
                "v".to_owned(),
                InputValues::FloatVector {
                    dim: 2,
                    values: vec![0.0, 1.0, 1.0, 0.0],
                },
            ),
        ],
    };
    assert_eq!(
        db.upsert_with_pk_type("", "c", merged).unwrap().pks,
        [Pk::Int64(7), Pk::Int64(7)]
    );
    assert_eq!(db.collection("", "c").unwrap().read().count(), 1);
    db.close();
    drop(db);
    let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
    assert_eq!(reopened.collection("", "c").unwrap().read().count(), 1);
    reopened.close();
}

#[test]
fn concurrent_partial_upserts_preserve_both_fields() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("concurrent-upsert.db");
    let db = std::sync::Arc::new(Db::new(DataDir::open(&path).unwrap(), compute()));
    let mut dynamic_schema = schema();
    dynamic_schema.enable_dynamic_field = true;
    db.create_collection("", "c", dynamic_schema).unwrap();
    db.insert("", "c", batch(&[1])).unwrap();
    let workers: Vec<_> = ["x", "y"]
        .into_iter()
        .map(|name| {
            let db = std::sync::Arc::clone(&db);
            std::thread::spawn(move || {
                db.upsert_with_pk_type(
                    "",
                    "c",
                    InsertBatch {
                        num_rows: 1,
                        columns: vec![
                            ("id".to_owned(), InputValues::Int(vec![1])),
                            (name.to_owned(), InputValues::Int(vec![1])),
                        ],
                    },
                )
                .unwrap();
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    let handle = db.collection("", "c").unwrap();
    let c = handle.read();
    let result = c
        .get(&[Pk::Int64(1)], &["x".to_owned(), "y".to_owned()])
        .unwrap();
    assert_eq!(
        result.rows.meta.unwrap()[0],
        serde_json::json!({"x": 1, "y": 1})
            .as_object()
            .unwrap()
            .clone()
    );
    drop(c);
    db.close();
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
fn close_nowait_keeps_unflushed_delta_wal_for_recovery() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("delta-wal.db");
    let db = std::sync::Arc::new(Db::new(DataDir::open(&path).unwrap(), compute()));
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1, 2])).unwrap();
    db.flush_collection("", "c").unwrap();
    db.delete("", "c", &[Pk::Int64(1)]).unwrap();
    db.close_nowait();
    db.close();
    drop(db);
    let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
    assert_eq!(reopened.collection("", "c").unwrap().read().count(), 1);
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
    let orphan = path.join("collections/broken/partitions/_default/data/unreferenced.parquet");
    std::fs::write(&orphan, b"preserve on failed open").unwrap();
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
    assert_eq!(std::fs::read(&orphan).unwrap(), b"preserve on failed open");
    reopened.close();
}

#[test]
fn invalid_snapshot_metadata_preserves_unreferenced_files() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("snapshot-error.db");
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1])).unwrap();
    db.close();
    let collection = path.join("collections/c");
    let orphan = collection.join("partitions/_default/data/unreferenced.parquet");
    std::fs::write(&orphan, b"keep").unwrap();
    let metadata = collection.join("snapshots/metadata");
    std::fs::create_dir_all(&metadata).unwrap();
    std::fs::write(metadata.join("broken.json"), b"{").unwrap();

    let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
    assert!(matches!(
        reopened.collection("", "c"),
        Err(EngineError::Storage(_))
    ));
    assert_eq!(std::fs::read(orphan).unwrap(), b"keep");
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
fn orphan_cleanup_io_failures_do_not_block_healthy_collection_reads() {
    use milvus_lite_core::storage::recovery::{
        OrphanCleanupFault, inject_orphan_cleanup_failure_once,
    };

    for (kind, name, fault) in [
        ("data", "orphan.parquet", OrphanCleanupFault::Remove),
        ("delta", "orphan.parquet", OrphanCleanupFault::Remove),
        (
            "indexes",
            "data_000999_000999.v.flat.idx",
            OrphanCleanupFault::Remove,
        ),
        ("data", "orphan.parquet", OrphanCleanupFault::Sync),
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("cleanup-error.db");
        let db = Db::new(DataDir::open(&path).unwrap(), compute());
        db.create_collection("", "c", schema()).unwrap();
        db.insert("", "c", batch(&[1, 2])).unwrap();
        db.close();
        drop(db);
        let dir = path.join("collections/c/partitions/_default").join(kind);
        std::fs::create_dir_all(&dir).unwrap();
        let orphan = dir.join(name);
        std::fs::write(&orphan, b"unreferenced").unwrap();

        let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
        inject_orphan_cleanup_failure_once(fault);
        let handle = reopened.collection("", "c").unwrap();
        assert_eq!(handle.read().count(), 2);
        assert_eq!(handle.read().schema().fields[0].name, "id");
        assert_eq!(
            handle
                .read()
                .get(&[Pk::Int64(1), Pk::Int64(2)], &[])
                .unwrap()
                .rows
                .pks,
            [Pk::Int64(1), Pk::Int64(2)]
        );
        assert_eq!(orphan.exists(), fault == OrphanCleanupFault::Remove);
        assert_eq!(
            reopened.list_collections("").unwrap(),
            [("c".to_owned(), LoadState::Loaded)]
        );
        reopened.insert("", "c", batch(&[3])).unwrap();
        assert_eq!(handle.read().count(), 3);
        drop(handle);
        reopened.close();
        drop(reopened);
        let final_open = Db::new(DataDir::open(&path).unwrap(), compute());
        assert_eq!(final_open.collection("", "c").unwrap().read().count(), 3);
        assert!(!orphan.exists());
        final_open.close();
    }
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
fn partial_delta_wal_append_keeps_the_deleted_row_live() {
    use milvus_lite_core::storage::wal::inject_append_failure_after_bytes;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("partial-delta.db");
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1, 2])).unwrap();
    inject_append_failure_after_bytes(20);
    assert!(db.delete("", "c", &[Pk::Int64(1)]).is_err());
    assert_eq!(db.collection("", "c").unwrap().read().count(), 2);
    db.delete("", "c", &[Pk::Int64(2)]).unwrap();
    db.close();
    drop(db);
    let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
    assert_eq!(reopened.collection("", "c").unwrap().read().count(), 1);
    assert_eq!(
        reopened
            .collection("", "c")
            .unwrap()
            .read()
            .get(&[Pk::Int64(1), Pk::Int64(2)], &[])
            .unwrap()
            .rows
            .pks,
        [Pk::Int64(1)]
    );
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
    db.delete("", "c", &[Pk::Int64(1)]).unwrap();
    inject_close_worker_spawn_failure_once();
    db.close_nowait();
    drop(db);
    let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
    assert_eq!(reopened.collection("", "c").unwrap().read().count(), 0);
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

#[cfg(feature = "fault-injection")]
#[test]
fn directory_sync_failure_and_panic_seal_all_mutations_but_preserve_open_reads() {
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
    };
    for op in [DirectoryOperation::Create, DirectoryOperation::Drop] {
        for point in [DirectoryPoint::CollectionsSync, DirectoryPoint::StagingSync] {
            for fault in [DirectoryFault::Error, DirectoryFault::Panic] {
                let root = tempfile::tempdir().unwrap();
                let path = root.path().join("sealed.db");
                let db = Db::new(DataDir::open(&path).unwrap(), compute());
                for name in ["existing", "lazy", "victim"] {
                    db.create_collection("", name, schema()).unwrap();
                    db.insert("", name, batch(&[1])).unwrap();
                }
                db.close();
                let db = Db::new(DataDir::open(&path).unwrap(), compute());
                let existing = db.collection("", "existing").unwrap();
                let victim = db.collection("", "victim").unwrap();
                let orphan = path.join("collections/lazy/partitions/_default/data/orphan");
                std::fs::write(&orphan, b"orphan").unwrap();
                db.insert("", "existing", batch(&[2])).unwrap();
                std::fs::write(path.join("aliases.json"), br#"{"old":"victim"}"#).unwrap();
                inject_directory_fault_once(op, point, fault);
                let result = if op == DirectoryOperation::Create {
                    db.create_collection("", "new", schema())
                } else {
                    db.drop_collection("", "victim")
                };
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("durability is unconfirmed")
                );
                assert_eq!(
                    path.join("collections/victim").exists(),
                    op == DirectoryOperation::Create
                );
                assert_eq!(
                    path.join("collections/new").exists(),
                    op == DirectoryOperation::Create
                );
                assert_eq!(
                    std::fs::read(path.join("aliases.json")).unwrap(),
                    br#"{"old":"victim"}"#
                );
                assert_eq!(existing.read().count(), 2);
                assert_eq!(
                    victim.read().get(&[Pk::Int64(1)], &[]).is_err(),
                    op == DirectoryOperation::Drop
                );
                assert!(
                    db.collection("", "lazy")
                        .unwrap_err()
                        .to_string()
                        .contains("blocked")
                );
                for result in [
                    db.insert("", "existing", batch(&[3])).map(|_| ()),
                    db.upsert_with_pk_type("", "existing", batch(&[3]))
                        .map(|_| ()),
                    db.delete("", "existing", &[Pk::Int64(1)]).map(|_| ()),
                    db.flush_collection("", "existing"),
                    db.create_index(
                        "",
                        "existing",
                        "v",
                        IndexRequest {
                            index_type: "FLAT".into(),
                            metric_type: Some("L2".into()),
                            ..Default::default()
                        },
                    ),
                    db.create_collection("", "later", schema()),
                    db.drop_collection("", "existing"),
                ] {
                    assert!(result.unwrap_err().to_string().contains("blocked"));
                }
                assert!(orphan.exists());
                // 注入已消耗，真实 fsync 成功也不应被拿来解除封锁。
                std::fs::File::open(path.join("collections"))
                    .unwrap()
                    .sync_all()
                    .unwrap();
                assert!(db.flush_collection("", "existing").is_err());
                let manifest = path.join("collections/existing/manifest.json");
                let before = std::fs::read(&manifest).unwrap();
                let wal = path.join("collections/existing/wal");
                let retained: Vec<_> = std::fs::read_dir(&wal)
                    .unwrap()
                    .map(|entry| {
                        let path = entry.unwrap().path();
                        let bytes = std::fs::read(&path).unwrap();
                        (path, bytes)
                    })
                    .collect();
                assert!(!retained.is_empty());
                db.close();
                assert_eq!(std::fs::read(&manifest).unwrap(), before);
                for (path, bytes) in retained {
                    assert_eq!(std::fs::read(path).unwrap(), bytes);
                }
                let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
                assert_eq!(
                    reopened.collection("", "existing").unwrap().read().count(),
                    2
                );
                assert!(reopened.collection("", "lazy").is_ok());
                assert!(!orphan.exists());
                reopened.insert("", "existing", batch(&[3])).unwrap();
                reopened.close();
            }
        }
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn rename_failure_keeps_collection_usable_and_retryable() {
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
    };
    let root = tempfile::tempdir().unwrap();
    let db = Db::new(DataDir::open(root.path()).unwrap(), compute());
    inject_directory_fault_once(
        DirectoryOperation::Create,
        DirectoryPoint::BeforeRename,
        DirectoryFault::Error,
    );
    assert!(db.create_collection("", "c", schema()).is_err());
    assert!(!root.path().join("collections/c").exists());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1])).unwrap();
    inject_directory_fault_once(
        DirectoryOperation::Drop,
        DirectoryPoint::BeforeRename,
        DirectoryFault::Error,
    );
    assert!(db.drop_collection("", "c").is_err());
    db.insert("", "c", batch(&[2])).unwrap();
    assert_eq!(db.collection("", "c").unwrap().read().count(), 2);
    db.drop_collection("", "c").unwrap();
    assert!(!root.path().join("collections/c").exists());
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn committed_drop_alias_error_is_explicit_and_retry_does_not_rebind_old_alias() {
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
    };
    for retry in ["same-drop", "recreate", "other-drop"] {
        let root = tempfile::tempdir().unwrap();
        let db = Db::new(DataDir::open(root.path()).unwrap(), compute());
        db.create_collection("", "c", schema()).unwrap();
        db.create_collection("", "other", schema()).unwrap();
        std::fs::write(
            root.path().join("aliases.json"),
            br#"{"old":"c","keep":"other"}"#,
        )
        .unwrap();
        inject_directory_fault_once(
            DirectoryOperation::Drop,
            DirectoryPoint::Aliases,
            DirectoryFault::Error,
        );
        let error = db.drop_collection("", "c").unwrap_err().to_string();
        assert!(error.contains("collection was removed, but aliases cleanup is pending"));
        assert!(!root.path().join("collections/c").exists());
        assert!(db.collection("", "c").is_err());
        assert!(
            std::fs::read_to_string(root.path().join("aliases.json"))
                .unwrap()
                .contains("old")
        );
        match retry {
            "recreate" => db.create_collection("", "c", schema()).unwrap(),
            "other-drop" => db.drop_collection("", "other").unwrap(),
            _ => db.drop_collection("", "c").unwrap(),
        }
        let aliases: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.path().join("aliases.json")).unwrap())
                .unwrap();
        assert!(aliases.get("old").is_none());
        if retry == "other-drop" {
            assert!(aliases.get("keep").is_none());
        } else {
            assert_eq!(aliases["keep"], "other");
        }
        db.close();
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn startup_staging_cleanup_failures_allow_healthy_collection_reads_and_retry() {
    use milvus_lite_core::data_dir::{StagingFault, inject_staging_failure_once};
    for fault in [StagingFault::Remove, StagingFault::Sync] {
        let root = tempfile::tempdir().unwrap();
        let db = Db::new(DataDir::open(root.path()).unwrap(), compute());
        db.create_collection("", "c", schema()).unwrap();
        db.insert("", "c", batch(&[1])).unwrap();
        db.close();
        let residue = root.path().join(".database-staging/residue");
        std::fs::create_dir_all(&residue).unwrap();
        std::fs::write(residue.join("keep"), b"residue").unwrap();
        inject_staging_failure_once(fault);
        let db = Db::new(DataDir::open(root.path()).unwrap(), compute());
        assert_eq!(db.collection("", "c").unwrap().read().count(), 1);
        assert_eq!(residue.exists(), fault == StagingFault::Remove);
        db.close();
        let db = Db::new(DataDir::open(root.path()).unwrap(), compute());
        assert!(!residue.exists());
        assert_eq!(db.collection("", "c").unwrap().read().count(), 1);
        db.close();
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn create_and_drop_wait_for_first_lazy_open_cleanup() {
    use milvus_lite_core::db::pause_open_cleanup_once;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;
    for creating in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let db = Db::new(DataDir::open(root.path()).unwrap(), compute());
        for name in ["lazy", "victim"] {
            db.create_collection("", name, schema()).unwrap();
        }
        db.insert("", "lazy", batch(&[1])).unwrap();
        db.close();
        let orphan = root
            .path()
            .join("collections/lazy/partitions/_default/data/orphan");
        std::fs::write(&orphan, b"orphan").unwrap();
        let db = Arc::new(Db::new(DataDir::open(root.path()).unwrap(), compute()));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let opener = Arc::clone(&db);
        let opening = std::thread::spawn(move || {
            pause_open_cleanup_once(entered_tx, resume_rx);
            assert_eq!(opener.collection("", "lazy").unwrap().read().count(), 1);
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(orphan.exists());
        let (starting_tx, starting_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let changer = Arc::clone(&db);
        let changing = std::thread::spawn(move || {
            starting_tx.send(()).unwrap();
            if creating {
                changer.create_collection("", "new", schema()).unwrap();
            } else {
                changer.drop_collection("", "victim").unwrap();
            }
            finished_tx.send(()).unwrap();
        });
        starting_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            finished_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(!root.path().join("collections/new").exists());
        assert!(root.path().join("collections/victim").exists());
        resume_tx.send(()).unwrap();
        finished_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        opening.join().unwrap();
        changing.join().unwrap();
        assert!(!orphan.exists());
        assert_eq!(root.path().join("collections/new").exists(), creating);
        assert_eq!(root.path().join("collections/victim").exists(), creating);
        db.close();
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn drop_wal_sync_failure_preserves_data_and_delta_and_accepts_later_writes() {
    use milvus_lite_core::storage::runtime::{
        RetainedWalSyncFault, inject_retained_wal_sync_failure_once,
    };
    for fault in [
        RetainedWalSyncFault::Data,
        RetainedWalSyncFault::Delta,
        RetainedWalSyncFault::Directory,
    ] {
        let root = tempfile::tempdir().unwrap();
        let db = Db::new(DataDir::open(root.path()).unwrap(), compute());
        db.create_collection("", "c", schema()).unwrap();
        db.insert("", "c", batch(&[1, 2])).unwrap();
        db.delete("", "c", &[Pk::Int64(1)]).unwrap();
        inject_retained_wal_sync_failure_once(fault);
        assert!(db.drop_collection("", "c").is_err());
        assert!(root.path().join("collections/c").exists());
        db.insert("", "c", batch(&[3])).unwrap();
        db.delete("", "c", &[Pk::Int64(2)]).unwrap();
        assert_eq!(
            db.collection("", "c")
                .unwrap()
                .read()
                .get(&[Pk::Int64(1), Pk::Int64(2), Pk::Int64(3)], &[])
                .unwrap()
                .rows
                .pks,
            [Pk::Int64(3)]
        );
        db.close();
        let reopened = Db::new(DataDir::open(root.path()).unwrap(), compute());
        assert_eq!(
            reopened
                .collection("", "c")
                .unwrap()
                .read()
                .get(&[Pk::Int64(1), Pk::Int64(2), Pk::Int64(3)], &[])
                .unwrap()
                .rows
                .pks,
            [Pk::Int64(3)]
        );
        reopened.drop_collection("", "c").unwrap();
        reopened.close();
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn reopen_uses_actual_directory_state_after_unconfirmed_rename() {
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
    };
    for op in [DirectoryOperation::Create, DirectoryOperation::Drop] {
        for rolled_back in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let db = Db::new(DataDir::open(root.path()).unwrap(), compute());
            if op == DirectoryOperation::Drop {
                db.create_collection("", "c", schema()).unwrap();
                db.insert("", "c", batch(&[1, 2])).unwrap();
                db.delete("", "c", &[Pk::Int64(1)]).unwrap();
                std::fs::write(root.path().join("aliases.json"), br#"{"old":"c"}"#).unwrap();
            }
            inject_directory_fault_once(op, DirectoryPoint::CollectionsSync, DirectoryFault::Error);
            let result = if op == DirectoryOperation::Create {
                db.create_collection("", "c", schema())
            } else {
                db.drop_collection("", "c")
            };
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("durability is unconfirmed")
            );
            db.close();
            // 人工构造断电后改名回退的实际目录形态，不将进程退出等同于断电。
            if rolled_back {
                let staging = root.path().join(".database-staging");
                if op == DirectoryOperation::Create {
                    std::fs::rename(
                        root.path().join("collections/c"),
                        staging.join("rolled-back-create"),
                    )
                    .unwrap();
                } else {
                    let residue = std::fs::read_dir(staging)
                        .unwrap()
                        .next()
                        .unwrap()
                        .unwrap()
                        .path();
                    std::fs::rename(residue, root.path().join("collections/c")).unwrap();
                }
            }
            let reopened = Db::new(DataDir::open(root.path()).unwrap(), compute());
            let exists = (op == DirectoryOperation::Create) != rolled_back;
            assert_eq!(reopened.collection("", "c").is_ok(), exists);
            if exists {
                let handle = reopened.collection("", "c").unwrap();
                assert_eq!(
                    handle.read().count(),
                    usize::from(op == DirectoryOperation::Drop)
                );
                if op == DirectoryOperation::Drop {
                    assert_eq!(
                        handle
                            .read()
                            .get(&[Pk::Int64(1), Pk::Int64(2)], &[])
                            .unwrap()
                            .rows
                            .pks,
                        [Pk::Int64(2)]
                    );
                    assert_eq!(
                        std::fs::read(root.path().join("aliases.json")).unwrap(),
                        br#"{"old":"c"}"#
                    );
                }
            }
            assert!(
                std::fs::read_dir(root.path().join(".database-staging"))
                    .unwrap()
                    .next()
                    .is_none()
            );
            reopened.close();
        }
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn create_alias_cleanup_fault_fires_before_directory_commit_and_can_retry() {
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
    };
    let root = tempfile::tempdir().unwrap();
    let db = Db::new(DataDir::open(root.path()).unwrap(), compute());
    db.create_collection("", "existing", schema()).unwrap();
    let aliases_path = root.path().join("aliases.json");
    let aliases = br#"{"old":"gone","live":"existing"}"#;
    std::fs::write(&aliases_path, aliases).unwrap();
    inject_directory_fault_once(
        DirectoryOperation::Create,
        DirectoryPoint::Aliases,
        DirectoryFault::Error,
    );
    let error = db
        .create_collection("", "gone", schema())
        .unwrap_err()
        .to_string();
    assert!(error.contains("injected directory lifecycle failure"));
    assert!(!root.path().join("collections/gone").exists());
    assert_eq!(std::fs::read(&aliases_path).unwrap(), aliases);
    db.create_collection("", "gone", schema()).unwrap();
    let aliases: serde_json::Value =
        serde_json::from_slice(&std::fs::read(aliases_path).unwrap()).unwrap();
    assert_eq!(aliases, serde_json::json!({"live": "existing"}));
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn staging_entry_type_failure_rejects_open_without_deleting_any_residue() {
    use milvus_lite_core::data_dir::{StagingFault, inject_staging_failure_once};
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join(".database-staging");
    std::fs::create_dir(&staging).unwrap();
    for name in ["first", "second"] {
        std::fs::write(staging.join(name), name.as_bytes()).unwrap();
    }
    inject_staging_failure_once(StagingFault::EntryTypeAfter(1));
    assert!(
        DataDir::open(root.path())
            .unwrap_err()
            .to_string()
            .contains("injected staging entry type failure")
    );
    for name in ["first", "second"] {
        assert_eq!(std::fs::read(staging.join(name)).unwrap(), name.as_bytes());
    }
    let db = Db::new(DataDir::open(root.path()).unwrap(), compute());
    assert!(std::fs::read_dir(staging).unwrap().next().is_none());
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn enospc_insert_upsert_delete_preserve_memory_and_recover_after_close() {
    use milvus_lite_core::storage::wal::inject_storage_full_after_bytes;
    for operation in ["insert", "upsert", "delete"] {
        for existing_writer in [false, true] {
            for cut in [0, 20, usize::MAX] {
                for continue_writing in [false, true] {
                    let root = tempfile::tempdir().unwrap();
                    let db = Db::new(DataDir::open(root.path()).unwrap(), compute());
                    db.create_collection("", "c", schema()).unwrap();
                    db.insert("", "c", batch(&[1, 2])).unwrap();
                    db.flush_collection("", "c").unwrap();
                    if existing_writer {
                        if operation == "delete" {
                            db.delete("", "c", &[Pk::Int64(2)]).unwrap();
                        } else {
                            db.insert("", "c", batch(&[3])).unwrap();
                        }
                    }
                    let handle = db.collection("", "c").unwrap();
                    let ids = [1, 2, 3, 4, 5, 6].map(Pk::Int64);
                    let before = handle.read().get(&ids, &["v".into()]).unwrap().rows;
                    inject_storage_full_after_bytes(cut);
                    let result = match operation {
                        "insert" => db.insert("", "c", batch(&[4, 5])).map(|_| ()),
                        "upsert" => {
                            let mut input = batch(&[1, 4]);
                            input.columns[1].1 = InputValues::FloatVector {
                                dim: 2,
                                values: vec![9.0, 0.0, 9.0, 0.0],
                            };
                            db.upsert_with_pk_type("", "c", input).map(|_| ())
                        }
                        _ => db
                            .delete("", "c", &[Pk::Int64(1), Pk::Int64(99)])
                            .map(|_| ()),
                    };
                    assert!(
                        matches!(result, Err(EngineError::Storage(_))),
                        "{operation}, existing={existing_writer}, cut={cut}"
                    );
                    let after = handle.read().get(&ids, &["v".into()]).unwrap().rows;
                    assert_eq!(after.pks, before.pks);
                    assert_eq!(after.fields, before.fields);
                    if continue_writing {
                        db.insert("", "c", batch(&[6])).unwrap();
                    }
                    let expected = handle.read().get(&ids, &["v".into()]).unwrap().rows;
                    db.close();
                    let reopened = Db::new(DataDir::open(root.path()).unwrap(), compute());
                    let recovered = reopened.collection("", "c").unwrap();
                    assert_eq!(recovered.read().count(), expected.pks.len());
                    let actual = recovered.read().get(&ids, &["v".into()]).unwrap().rows;
                    assert_eq!(actual.pks, expected.pks);
                    assert_eq!(actual.fields, expected.fields);
                    assert!(!actual.pks.contains(&Pk::Int64(4)));
                    assert!(!actual.pks.contains(&Pk::Int64(5)));
                    reopened.close();
                }
            }
        }
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn wal_read_io_failure_preserves_files_and_lazy_open_can_retry() {
    use milvus_lite_core::storage::{
        arrow_data,
        wal::{WalKind, WalReadFault, WalWriter, inject_read_fault_once},
    };
    let root = tempfile::tempdir().unwrap();
    let db = Db::new(DataDir::open(root.path()).unwrap(), compute());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1, 2])).unwrap();
    db.close();
    let dir = root.path().join("collections/c");
    let wal_dir = dir.join("wal");
    std::fs::create_dir_all(&wal_dir).unwrap();
    let input = arrow_data::wal_data_batch(
        &schema(),
        &[3],
        "_default",
        &[
            milvus_lite_core::ColumnData::Int64(vec![3]),
            milvus_lite_core::ColumnData::FloatVector {
                dim: 2,
                values: vec![1.0, 0.0],
            },
        ],
        None,
    )
    .unwrap();
    let mut writer = WalWriter::create(&wal_dir, WalKind::Data, 100, &input.schema()).unwrap();
    writer.append(&input).unwrap();
    let path = writer.path().to_path_buf();
    writer.into_file().sync_all().unwrap();
    let before = std::fs::read(&path).unwrap();
    let orphan = dir.join("partitions/_default/data/orphan");
    std::fs::write(&orphan, b"orphan").unwrap();
    let db = Db::new(DataDir::open(root.path()).unwrap(), compute());
    inject_read_fault_once(WalReadFault::Io);
    assert!(
        db.collection("", "c")
            .unwrap_err()
            .to_string()
            .contains("injected WAL read I/O failure")
    );
    assert!(orphan.exists());
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert_eq!(db.collection("", "c").unwrap().read().count(), 3);
    assert!(!orphan.exists());
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn failed_wal_append_consumes_auto_ids_in_process_but_reopen_uses_recoverable_sequence() {
    use milvus_lite_core::storage::wal::inject_storage_full_after_bytes;
    for continued in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let db = Db::new(DataDir::open(root.path()).unwrap(), compute());
        let mut auto_schema = schema();
        auto_schema.fields[0].auto_id = true;
        db.create_collection("", "c", auto_schema).unwrap();
        let input = |rows| InsertBatch {
            num_rows: rows,
            columns: vec![(
                "v".into(),
                InputValues::FloatVector {
                    dim: 2,
                    values: [1.0, 0.0].repeat(rows),
                },
            )],
        };
        assert_eq!(
            db.insert("", "c", input(2)).unwrap(),
            [Pk::Int64(1), Pk::Int64(2)]
        );
        inject_storage_full_after_bytes(usize::MAX);
        assert!(db.insert("", "c", input(2)).is_err());
        if continued {
            assert_eq!(db.insert("", "c", input(1)).unwrap(), [Pk::Int64(5)]);
        }
        db.close();
        let reopened = Db::new(DataDir::open(root.path()).unwrap(), compute());
        assert_eq!(
            reopened.insert("", "c", input(1)).unwrap(),
            [Pk::Int64(if continued { 6 } else { 3 })]
        );
        reopened.close();
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn frozen_wal_pre_sync_failure_stays_before_replace_and_explicit_flush_recovers() {
    use milvus_lite_core::storage::runtime::{
        RetainedWalSyncFault, hold_flush_retry_window, inject_retained_wal_sync_failure_once,
    };
    for fault in [
        RetainedWalSyncFault::Data,
        RetainedWalSyncFault::Delta,
        RetainedWalSyncFault::Directory,
    ] {
        let root = tempfile::tempdir().unwrap();
        let db = Db::new(DataDir::open(root.path()).unwrap(), compute());
        db.create_collection("", "c", schema()).unwrap();
        db.create_collection("", "other", schema()).unwrap();
        db.insert("", "c", batch(&[1, 2, 3])).unwrap();
        db.flush_collection("", "c").unwrap();
        db.insert("", "c", batch(&[4])).unwrap();
        db.delete("", "c", &[Pk::Int64(2)]).unwrap();
        let dir = root.path().join("collections/c");
        let manifest = std::fs::read(dir.join("manifest.json")).unwrap();
        let wal: Vec<_> = std::fs::read_dir(dir.join("wal"))
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let bytes = std::fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect();
        assert_eq!(wal.len(), 2);
        let _window = hold_flush_retry_window();
        inject_retained_wal_sync_failure_once(fault);
        let error = db.flush_collection("", "c").unwrap_err();
        assert!(matches!(error, EngineError::Storage(_)));
        assert!(error.to_string().contains("BeforeReplace"));
        assert!(
            error
                .to_string()
                .contains("injected retained WAL sync failure")
        );
        assert_eq!(std::fs::read(dir.join("manifest.json")).unwrap(), manifest);
        for (path, bytes) in &wal {
            assert_eq!(std::fs::read(path).unwrap(), *bytes);
        }
        let expected = [Pk::Int64(1), Pk::Int64(3), Pk::Int64(4)];
        assert_eq!(
            db.collection("", "c")
                .unwrap()
                .read()
                .get(&[1, 2, 3, 4, 99].map(Pk::Int64), &[])
                .unwrap()
                .rows
                .pks,
            expected
        );
        assert!(
            db.insert("", "c", batch(&[99]))
                .unwrap_err()
                .to_string()
                .contains("throttled")
        );
        db.insert("", "other", batch(&[99])).unwrap();
        db.flush_collection("", "c").unwrap();
        db.insert("", "c", batch(&[5])).unwrap();
        db.close();
        let reopened = Db::new(DataDir::open(root.path()).unwrap(), compute());
        let rows = reopened
            .collection("", "c")
            .unwrap()
            .read()
            .get(&[1, 2, 3, 4, 5, 99].map(Pk::Int64), &[])
            .unwrap()
            .rows;
        assert_eq!(
            rows.pks,
            [Pk::Int64(1), Pk::Int64(3), Pk::Int64(4), Pk::Int64(5)]
        );
        reopened.close();
    }
}
