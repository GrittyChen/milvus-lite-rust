//! 在独立测试进程中越过 manifest 提交点，核对重开时的文件与可见行。

#![cfg(feature = "fault-injection")]
#![allow(clippy::unwrap_used)]

use std::process::Command;

use milvus_lite_core::storage::flush::inject_wal_process_exit_after_unlink_once;
use milvus_lite_core::storage::fs_commit::{CommitPhase, inject_manifest_process_exit_once};
use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, FieldSchema, IndexRequest, InputValues,
    InsertBatch, LoadState, Pk,
};

fn compute() -> Compute {
    Compute::new(std::num::NonZeroUsize::new(2).unwrap()).unwrap()
}

fn schema() -> CollectionSchema {
    let mut id = FieldSchema::new("id", DataType::Int64);
    id.is_primary = true;
    let mut vector = FieldSchema::new("v", DataType::FloatVector);
    vector.dim = Some(2);
    CollectionSchema {
        fields: vec![id, vector],
        enable_dynamic_field: false,
        properties: Vec::new(),
    }
}

fn batch(rows: &[(i64, [f32; 2])]) -> InsertBatch {
    InsertBatch {
        num_rows: rows.len(),
        columns: vec![
            (
                "id".to_owned(),
                InputValues::Int(rows.iter().map(|(id, _)| *id).collect()),
            ),
            (
                "v".to_owned(),
                InputValues::FloatVector {
                    dim: 2,
                    values: rows.iter().flat_map(|(_, vector)| *vector).collect(),
                },
            ),
        ],
    }
}

fn prepared_mixed_db(path: &str) -> Db {
    let db = Db::new(
        DataDir::open(std::path::Path::new(path)).unwrap(),
        compute(),
    );
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[(1, [1.0, 0.0]), (2, [0.0, 1.0])]))
        .unwrap();
    db.flush_collection("", "c").unwrap();
    assert_eq!(db.delete("", "c", &[Pk::Int64(1)]).unwrap(), 1);
    db.upsert_with_pk_type("", "c", batch(&[(2, [2.0, 0.0])]))
        .unwrap();
    db.insert("", "c", batch(&[(3, [0.0, 2.0])])).unwrap();
    db
}

fn manifest_replace_crash_child(stage: &str) {
    let path = std::env::var("MILVUS_LITE_CRASH_DIR").unwrap();
    let db = prepared_mixed_db(&path);
    let phase = match stage {
        "before" => CommitPhase::BeforeReplace,
        "after" => CommitPhase::AfterReplace,
        _ => panic!("unknown crash stage {stage}"),
    };
    inject_manifest_process_exit_once(phase);
    db.flush_collection("", "c").unwrap();
    panic!("manifest crash injection did not fire");
}

fn wal_unlink_crash_child() {
    let path = std::env::var("MILVUS_LITE_CRASH_DIR").unwrap();
    let db = prepared_mixed_db(&path);
    inject_wal_process_exit_after_unlink_once();
    db.flush_collection("", "c").unwrap();
    panic!("WAL unlink crash injection did not fire");
}

fn create_index_crash_child(stage: &str) {
    let path = std::env::var("MILVUS_LITE_CRASH_DIR").unwrap();
    let db = Db::new(
        DataDir::open(std::path::Path::new(&path)).unwrap(),
        compute(),
    );
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[(1, [1.0, 0.0]), (2, [0.0, 1.0])]))
        .unwrap();
    if stage.starts_with("spec") {
        db.flush_collection("", "c").unwrap();
    }
    let phase = if stage.ends_with("before") {
        CommitPhase::BeforeReplace
    } else {
        CommitPhase::AfterReplace
    };
    inject_manifest_process_exit_once(phase);
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
    panic!("CreateIndex crash injection did not fire");
}

#[test]
fn mixed_data_and_delta_survive_crash_on_both_sides_of_manifest_replace() {
    if let Ok(stage) = std::env::var("MILVUS_LITE_CRASH_STAGE") {
        manifest_replace_crash_child(&stage);
    }
    for stage in ["before", "after"] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("crashed.db");
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "mixed_data_and_delta_survive_crash_on_both_sides_of_manifest_replace",
                "--nocapture",
            ])
            .env("MILVUS_LITE_CRASH_STAGE", stage)
            .env("MILVUS_LITE_CRASH_DIR", &path)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(86), "stage {stage}: {status:?}");

        let collection = path.join("collections/c");
        let manifest =
            milvus_lite_core::storage::manifest::Manifest::read(&collection.join("manifest.json"))
                .unwrap();
        assert_eq!(manifest.current_seq, if stage == "before" { 2 } else { 5 });
        assert_eq!(
            manifest.partitions["_default"].data_files.len(),
            if stage == "before" { 1 } else { 2 }
        );
        assert_eq!(
            manifest.partitions["_default"].delta_files.len(),
            if stage == "before" { 0 } else { 1 }
        );
        let wal = collection.join("wal");
        assert!(wal.join("wal_data_000002.arrow").exists());
        assert!(wal.join("wal_delta_000002.arrow").exists());

        let db = Db::new(DataDir::open(&path).unwrap(), compute());
        let handle = db.collection("", "c").unwrap();
        let part = path.join("collections/c/partitions/_default");
        for (kind, expected) in [
            ("data", if stage == "before" { 1 } else { 2 }),
            ("delta", if stage == "before" { 0 } else { 1 }),
        ] {
            let files = std::fs::read_dir(part.join(kind)).unwrap().count();
            assert_eq!(files, expected, "stage {stage}: orphan {kind} files remain");
        }
        let collection = handle.read();
        assert_eq!(collection.count(), 2);
        let got = collection
            .get(
                &[Pk::Int64(1), Pk::Int64(2), Pk::Int64(3)],
                &["v".to_owned()],
            )
            .unwrap();
        assert_eq!(got.rows.pks, [Pk::Int64(2), Pk::Int64(3)]);
        assert_eq!(
            got.rows.fields[0].1,
            milvus_lite_core::ColumnData::FloatVector {
                dim: 2,
                values: vec![2.0, 0.0, 0.0, 2.0],
            }
        );
        drop(collection);
        drop(handle);
        db.close();
        drop(db);

        let reopened = Db::new(DataDir::open(&path).unwrap(), compute());
        assert_eq!(reopened.collection("", "c").unwrap().read().count(), 2);
        reopened.close();
    }
}

#[test]
fn create_index_crash_preserves_pending_data_and_index_commit_boundary() {
    if let Ok(stage) = std::env::var("MILVUS_LITE_INDEX_CRASH_STAGE") {
        create_index_crash_child(&stage);
    }
    for stage in [
        "pending-before",
        "pending-after",
        "spec-before",
        "spec-after",
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("index-crash.db");
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "create_index_crash_preserves_pending_data_and_index_commit_boundary",
                "--nocapture",
            ])
            .env("MILVUS_LITE_INDEX_CRASH_STAGE", stage)
            .env("MILVUS_LITE_CRASH_DIR", &path)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(86), "stage {stage}: {status:?}");

        let collection_dir = path.join("collections/c");
        let pending = stage.starts_with("pending");
        let before = stage.ends_with("before");
        let manifest_path = collection_dir.join("manifest.json");
        let manifest = if pending && before {
            assert!(!manifest_path.exists());
            milvus_lite_core::storage::manifest::Manifest::default()
        } else {
            milvus_lite_core::storage::manifest::Manifest::read(&manifest_path).unwrap()
        };
        assert_eq!(manifest.current_seq, if pending && before { 0 } else { 2 });
        assert_eq!(
            manifest.partitions["_default"].data_files.len(),
            usize::from(!pending || !before)
        );
        assert_eq!(manifest.index_specs.contains_key("v"), !pending && !before);

        let db = Db::new(DataDir::open(&path).unwrap(), compute());
        let handle = db.collection("", "c").unwrap();
        assert_eq!(
            handle.read().load_state(),
            if !pending && !before {
                LoadState::Released
            } else {
                LoadState::Loaded
            }
        );
        handle.write().load();
        let collection = handle.read();
        assert_eq!(collection.count(), 2);
        assert_eq!(
            collection
                .get(&[Pk::Int64(1), Pk::Int64(2)], &[])
                .unwrap()
                .rows
                .pks,
            [Pk::Int64(1), Pk::Int64(2)]
        );
        drop(collection);
        drop(handle);
        db.close();
    }
}

#[test]
fn data_wal_unlinked_before_delta_wal_crash_does_not_resurrect_rows() {
    if std::env::var_os("MILVUS_LITE_WAL_UNLINK_CRASH").is_some() {
        wal_unlink_crash_child();
    }
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("wal-unlink.db");
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "data_wal_unlinked_before_delta_wal_crash_does_not_resurrect_rows",
            "--nocapture",
        ])
        .env("MILVUS_LITE_WAL_UNLINK_CRASH", "1")
        .env("MILVUS_LITE_CRASH_DIR", &path)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(87));

    let wal = path.join("collections/c/wal");
    assert!(!wal.join("wal_data_000002.arrow").exists());
    assert!(wal.join("wal_delta_000002.arrow").exists());
    let db = Db::new(DataDir::open(&path).unwrap(), compute());
    let handle = db.collection("", "c").unwrap();
    assert_eq!(handle.read().count(), 2);
    assert_eq!(
        handle
            .read()
            .get(&[Pk::Int64(1), Pk::Int64(2), Pk::Int64(3)], &[])
            .unwrap()
            .rows
            .pks,
        [Pk::Int64(2), Pk::Int64(3)]
    );
    drop(handle);
    db.close();
    assert!(!wal.join("wal_delta_000002.arrow").exists());
}
