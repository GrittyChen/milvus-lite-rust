//! 已提交来源替换、删除证据与正常关闭的维护轮次。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, FieldSchema, InputValues, InsertBatch, Pk,
    QueryVectors, SearchRequest,
};
use std::sync::Arc;
#[cfg(feature = "fault-injection")]
use std::time::Duration;

fn open(path: &std::path::Path) -> Arc<Db> {
    Db::new(
        DataDir::open(path).unwrap(),
        Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
    )
    .unwrap()
}
fn schema() -> CollectionSchema {
    CollectionSchema {
        fields: vec![
            FieldSchema {
                is_primary: true,
                ..FieldSchema::new("id", DataType::Int64)
            },
            FieldSchema {
                dim: Some(2),
                ..FieldSchema::new("v", DataType::FloatVector)
            },
        ],
        enable_dynamic_field: false,
        properties: Vec::new(),
    }
}
fn batch(ids: &[i64]) -> InsertBatch {
    InsertBatch {
        num_rows: ids.len(),
        columns: vec![
            ("id".into(), InputValues::Int(ids.to_vec())),
            (
                "v".into(),
                InputValues::FloatVector {
                    dim: 2,
                    values: [1.0, 0.0].repeat(ids.len()),
                },
            ),
        ],
    }
}
fn manifest(path: &std::path::Path) -> milvus_lite_core::storage::manifest::Manifest {
    milvus_lite_core::storage::manifest::Manifest::read(&path.join("collections/c/manifest.json"))
        .unwrap()
}
fn ids(db: &Db) -> Vec<Pk> {
    let handle = db.collection("", "c").unwrap();
    let result = handle
        .read()
        .get(
            &[
                Pk::Int64(1),
                Pk::Int64(2),
                Pk::Int64(3),
                Pk::Int64(4),
                Pk::Int64(5),
            ],
            &[],
        )
        .unwrap();
    result.rows.pks
}

fn snapshot_ids(snapshot: &milvus_lite_core::collection::ReadSnapshot, db: &Db) -> Vec<Pk> {
    snapshot
        .search(
            SearchRequest {
                anns_field: None,
                vectors: QueryVectors::Dense(vec![vec![1.0, 0.0]]),
                metric_type: Some("L2".into()),
                limit: 5,
                offset: 0,
                output_fields: Vec::new(),
            },
            db.compute(),
        )
        .unwrap()
        .hits[0]
        .rows
        .pks
        .clone()
}

#[test]
fn normal_close_compacts_and_reopening_preserves_primary_results() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    for id in 1..=4 {
        db.insert("", "c", batch(&[id])).unwrap();
        db.flush_collection("", "c").unwrap();
    }
    let handle = db.collection("", "c").unwrap();
    let old = handle.read().read_snapshot();
    db.close();
    assert_eq!(
        manifest(root.path()).partitions["_default"]
            .data_files
            .len(),
        1
    );
    let reopened = open(root.path());
    assert_eq!(
        ids(&reopened),
        vec![Pk::Int64(1), Pk::Int64(2), Pk::Int64(3), Pk::Int64(4)]
    );
    assert_eq!(
        snapshot_ids(&old, &db),
        vec![Pk::Int64(1), Pk::Int64(2), Pk::Int64(3), Pk::Int64(4)]
    );
    reopened.close();
}

#[cfg(feature = "fault-injection")]
fn fixture(path: &std::path::Path, files: &[Vec<(i64, u64)>], deletes: &[(i64, u64)]) {
    use milvus_lite_core::{
        ColumnData,
        storage::{
            arrow_data, fs_commit, manifest::Manifest, parquet_file, schema_file::SchemaFile,
        },
    };
    let collection = path.join("collections/c");
    fs_commit::create_dir_synced(&collection.join("partitions/_default/data")).unwrap();
    let schema = schema();
    fs_commit::write_new_schema(
        &collection.join("schema.json"),
        &SchemaFile::from_core("c", &schema).to_bytes().unwrap(),
    )
    .unwrap();
    let mut manifest = Manifest::default();
    for (file_index, rows) in files.iter().enumerate() {
        let seqs: Vec<_> = rows.iter().map(|r| r.1).collect();
        let min = *seqs.iter().min().unwrap();
        let mut max = *seqs.iter().max().unwrap();
        while collection
            .join(format!(
                "partitions/_default/data/data_{min:06}_{max:06}.parquet"
            ))
            .exists()
        {
            max += 1;
        }
        let relative = format!("data/data_{min:06}_{max:06}.parquet");
        let columns = vec![
            ColumnData::Int64(rows.iter().map(|r| r.0).collect()),
            ColumnData::FloatVector {
                dim: 2,
                values: [file_index as f32 + 1.0, 0.0].repeat(rows.len()),
            },
        ];
        parquet_file::write_new(
            &collection.join("partitions/_default").join(&relative),
            &arrow_data::data_batch(&schema, &seqs, &columns, None).unwrap(),
        )
        .unwrap();
        manifest.current_seq = manifest.current_seq.max(*seqs.iter().max().unwrap());
        manifest
            .partitions
            .get_mut("_default")
            .unwrap()
            .data_files
            .push(relative);
    }
    if !deletes.is_empty() {
        fs_commit::create_dir_synced(&collection.join("partitions/_default/delta")).unwrap();
        let pks: Vec<_> = deletes.iter().map(|r| Pk::Int64(r.0)).collect();
        let seqs: Vec<_> = deletes.iter().map(|r| r.1).collect();
        let min = *seqs.iter().min().unwrap();
        let max = *seqs.iter().max().unwrap();
        let relative = format!("delta/delta_{min:06}_{max:06}.parquet");
        parquet_file::write_new(
            &collection.join("partitions/_default").join(&relative),
            &arrow_data::delta_batch(&schema, &pks, &seqs).unwrap(),
        )
        .unwrap();
        manifest.current_seq = manifest.current_seq.max(max);
        manifest
            .partitions
            .get_mut("_default")
            .unwrap()
            .delta_files
            .push(relative);
    }
    fs_commit::commit_manifest(&collection, &manifest).unwrap();
}

#[cfg(feature = "fault-injection")]
fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to.join(entry.file_name()));
        } else {
            std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn missing_deleted_primary_winner_and_recovered_hidden_location_have_equal_future_operations() {
    let root = tempfile::tempdir().unwrap();
    let mut terminal = vec![(1, 3)];
    terminal.extend((0..199_999).map(|i| (100 + i as i64, 8 + i as u64)));
    fixture(
        root.path(),
        &[
            terminal,
            vec![(1, 5)],
            vec![(2, 4)],
            vec![(3, 6)],
            vec![(4, 1)],
        ],
        &[(1, 7)],
    );
    let db = open(root.path());
    db.collection("", "c").unwrap();
    drain(&db);
    assert_eq!(
        manifest(root.path()).partitions["_default"]
            .data_files
            .len(),
        2
    );
    assert!(!ids(&db).contains(&Pk::Int64(1)));
    let clone = tempfile::tempdir().unwrap();
    copy_tree(root.path(), clone.path());
    let recovered = open(clone.path());
    drain(&recovered);
    assert_eq!(ids(&db), ids(&recovered));
    for target in [&db, &recovered] {
        target.upsert_with_pk_type("", "c", batch(&[1])).unwrap();
    }
    assert_eq!(ids(&db), ids(&recovered));
    for target in [&db, &recovered] {
        target.delete("", "c", &[Pk::Int64(1)]).unwrap();
    }
    assert_eq!(ids(&db), ids(&recovered));
    for target in [&db, &recovered] {
        target.insert("", "c", batch(&[1])).unwrap();
        target.flush_collection("", "c").unwrap();
    }
    assert_eq!(ids(&db), ids(&recovered));
    assert_eq!(
        manifest(root.path()).current_seq,
        manifest(clone.path()).current_seq
    );
    db.close();
    recovered.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn cross_source_equal_primary_sequences_refuse_to_change_the_recovery_winner() {
    let root = tempfile::tempdir().unwrap();
    let mut terminal = vec![(1, 5)];
    terminal.extend((0..199_999).map(|i| (100 + i as i64, 8 + i as u64)));
    fixture(
        root.path(),
        &[
            vec![(1, 5)],
            terminal,
            vec![(2, 4)],
            vec![(3, 6)],
            vec![(4, 1)],
        ],
        &[],
    );
    let before = std::fs::read(root.path().join("collections/c/manifest.json")).unwrap();
    let db = open(root.path());
    db.collection("", "c").unwrap();
    let report = db
        .wait_for_maintenance("", "c", Duration::from_secs(15))
        .unwrap();
    assert_eq!(report.failed, 1);
    assert!(
        report
            .last_failure
            .unwrap()
            .contains("equal primary sequence")
    );
    assert_eq!(
        std::fs::read(root.path().join("collections/c/manifest.json")).unwrap(),
        before
    );
    assert_eq!(
        ids(&db),
        vec![Pk::Int64(1), Pk::Int64(2), Pk::Int64(3), Pk::Int64(4)]
    );
    let winner = db
        .collection("", "c")
        .unwrap()
        .read()
        .get(&[Pk::Int64(1)], &["v".into()])
        .unwrap()
        .rows;
    db.close();
    let recovered = open(root.path());
    assert_eq!(
        ids(&recovered),
        vec![Pk::Int64(1), Pk::Int64(2), Pk::Int64(3), Pk::Int64(4)]
    );
    assert_eq!(
        recovered
            .collection("", "c")
            .unwrap()
            .read()
            .get(&[Pk::Int64(1)], &["v".into()])
            .unwrap()
            .rows,
        winner
    );
    recovered.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn maintenance_start_failure_releases_the_data_directory_lock() {
    let root = tempfile::tempdir().unwrap();
    milvus_lite_core::inject_maintenance_start_failure_once();
    assert!(
        Db::new(
            DataDir::open(root.path()).unwrap(),
            Compute::for_host().unwrap()
        )
        .is_err()
    );
    let db = open(root.path());
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn worker_faults_preserve_commit_classification_and_other_collections_continue() {
    use milvus_lite_core::CompactionFault as F;
    for fault in [
        F::PreparePanic,
        F::BeforeReplacePanic,
        F::AfterReplacePanic,
        F::DirectorySyncIo,
        F::PublishPanic,
        F::CleanupPanic,
    ] {
        let root = tempfile::tempdir().unwrap();
        fixture(
            root.path(),
            &[vec![(1, 1)], vec![(2, 2)], vec![(3, 3)], vec![(4, 4)]],
            &[],
        );
        let before = std::fs::read(root.path().join("collections/c/manifest.json")).unwrap();
        let db = open(root.path());
        db.inject_compaction_fault_once(fault);
        let handle = db.collection("", "c").unwrap();
        let report = db
            .wait_for_maintenance("", "c", Duration::from_secs(15))
            .unwrap();
        assert_eq!(report.failed, 1, "{fault:?}: {report:?}");
        let after = std::fs::read(root.path().join("collections/c/manifest.json")).unwrap();
        if matches!(fault, F::PreparePanic | F::BeforeReplacePanic) {
            assert_eq!(before, after);
        } else {
            assert_ne!(before, after);
            assert_eq!(
                manifest(root.path()).partitions["_default"]
                    .data_files
                    .len(),
                1
            );
        }
        if matches!(fault, F::PublishPanic) {
            assert!(db.collection("", "c").is_err());
        } else {
            assert_eq!(handle.read().count(), 4);
        }
        db.create_collection("", "healthy", schema()).unwrap();
        db.insert("", "healthy", batch(&[9])).unwrap();
        let healthy = db
            .wait_for_maintenance("", "healthy", Duration::from_secs(15))
            .unwrap();
        assert_eq!(healthy.failed, 0);
        db.close();
        let reopened = open(root.path());
        assert_eq!(
            ids(&reopened),
            vec![Pk::Int64(1), Pk::Int64(2), Pk::Int64(3), Pk::Int64(4)]
        );
        reopened.close();
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn output_file_sync_precedes_rename_and_manifest_and_is_not_repeated() {
    use milvus_lite_core::storage::fs_commit::FsAction;
    use std::sync::mpsc;
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    drain(&db);
    for id in 1..=3 {
        db.insert("", "c", batch(&[id])).unwrap();
        db.flush_collection("", "c").unwrap();
        drain(&db);
    }
    let (sender, receiver) = mpsc::channel();
    db.trace_next_maintenance(sender);
    db.insert("", "c", batch(&[4])).unwrap();
    db.flush_collection("", "c").unwrap();
    let trace = receiver.recv_timeout(Duration::from_secs(15)).unwrap();
    drain(&db);
    let file_sync = trace
        .iter()
        .position(|e| {
            e.action == FsAction::SyncFile
                && e.path.extension().is_some_and(|ext| ext == "tmp")
                && e.path
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with(".compaction-")
        })
        .unwrap();
    let rename = trace
        .iter()
        .position(|e| {
            e.action == FsAction::Rename && e.path.extension().is_some_and(|ext| ext == "parquet")
        })
        .unwrap();
    let data_sync = trace
        .iter()
        .enumerate()
        .find(|(i, e)| {
            *i > rename
                && e.action == FsAction::SyncDir
                && e.path.file_name().is_some_and(|n| n == "data")
        })
        .unwrap()
        .0;
    let manifest = trace
        .iter()
        .position(|e| {
            e.action == FsAction::Rename && e.path.file_name().is_some_and(|n| n == "manifest.json")
        })
        .unwrap();
    assert!(file_sync < rename && rename < data_sync && data_sync < manifest);
    assert!(
        !trace
            .iter()
            .any(|e| e.action == FsAction::SyncFile && e.path == trace[rename].path)
    );
    assert!(
        trace
            .iter()
            .filter(|e| e.action == FsAction::Unlink)
            .all(|e| trace.iter().position(|x| std::ptr::eq(x, e)).unwrap() > manifest)
    );
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn normal_close_drains_paused_round_and_keeps_lock_until_publication() {
    use std::sync::mpsc;
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    drain(&db);
    let (entered, arrived) = mpsc::channel();
    let (resume, continued) = mpsc::channel();
    db.pause_compaction_before_commit_once(entered, continued);
    for id in 1..=4 {
        db.insert("", "c", batch(&[id])).unwrap();
        db.flush_collection("", "c").unwrap();
    }
    arrived.recv_timeout(Duration::from_secs(15)).unwrap();
    let (finished, done) = mpsc::channel();
    let closing = Arc::clone(&db);
    let worker = std::thread::spawn(move || {
        closing.close();
        finished.send(()).unwrap();
    });
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while db.enter().is_ok() {
        assert!(std::time::Instant::now() < until);
        std::thread::yield_now();
    }
    assert!(DataDir::open(root.path()).is_err());
    assert!(done.try_recv().is_err());
    resume.send(()).unwrap();
    done.recv_timeout(Duration::from_secs(15)).unwrap();
    worker.join().unwrap();
    assert_eq!(
        manifest(root.path()).partitions["_default"]
            .data_files
            .len(),
        1
    );
    let reopened = open(root.path());
    assert_eq!(ids(&reopened).len(), 4);
    reopened.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn directory_change_cancels_prepared_round_without_losing_input_files() {
    use std::sync::mpsc;
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    drain(&db);
    let (entered, arrived) = mpsc::channel();
    let (resume, continued) = mpsc::channel();
    db.pause_compaction_before_commit_once(entered, continued);
    for id in 1..=4 {
        db.insert("", "c", batch(&[id])).unwrap();
        db.flush_collection("", "c").unwrap();
    }
    arrived.recv_timeout(Duration::from_secs(15)).unwrap();
    let before = std::fs::read(root.path().join("collections/c/manifest.json")).unwrap();
    let creating = Arc::clone(&db);
    let worker =
        std::thread::spawn(move || creating.create_collection("", "other", schema()).unwrap());
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while db.pending_namespace_changes() == 0 {
        assert!(std::time::Instant::now() < until);
        std::thread::yield_now();
    }
    resume.send(()).unwrap();
    worker.join().unwrap();
    let report = db
        .wait_for_maintenance("", "c", Duration::from_secs(15))
        .unwrap();
    assert!(report.cancelled >= 1);
    assert_eq!(
        std::fs::read(root.path().join("collections/c/manifest.json")).unwrap(),
        before
    );
    assert_eq!(ids(&db).len(), 4);
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn disk_snapshot_pins_data_delta_and_index_across_atomic_gc() {
    let root = tempfile::tempdir().unwrap();
    fixture(
        root.path(),
        &[vec![(1, 1)], vec![(2, 3)], vec![(3, 4)], vec![(4, 5)]],
        &[(1, 2)],
    );
    let collection = root.path().join("collections/c");
    let old_data = collection.join("partitions/_default/data/data_000001_000001.parquet");
    let old_delta = collection.join("partitions/_default/delta/delta_000002_000002.parquet");
    let index = collection.join("partitions/_default/indexes/data_000001_000001.v.flat.idx");
    std::fs::create_dir_all(index.parent().unwrap()).unwrap();
    std::fs::write(&index, b"pinned-index-fixture").unwrap();
    let metadata = collection.join("snapshots/metadata");
    std::fs::create_dir_all(&metadata).unwrap();
    std::fs::write(metadata.join("s.json"),serde_json::to_vec(&serde_json::json!({
        "snapshot_format_version":1,"id":"s","name":"s","description":"","collection_name":"c","created_at":"2026-10-09T00:00:00+00:00","created_seq":5,"manifest_version":1,
        "schema_file":"snapshots/manifests/s/schema.json","manifest_file":"snapshots/manifests/s/manifest.json",
        "data_files":{"_default":["data/data_000001_000001.parquet"]},"delta_files":{"_default":["delta/delta_000002_000002.parquet"]},"index_files":{"_default":["indexes/data_000001_000001.v.flat.idx"]}
    })).unwrap()).unwrap();
    let db = open(root.path());
    db.collection("", "c").unwrap();
    drain(&db);
    let current = manifest(root.path());
    assert_eq!(current.partitions["_default"].data_files.len(), 1);
    assert_eq!(
        current.partitions["_default"].delta_files,
        vec!["delta/delta_000002_000002.parquet"]
    );
    assert!(old_data.is_file());
    assert!(old_delta.is_file());
    assert_eq!(std::fs::read(&index).unwrap(), b"pinned-index-fixture");
    assert_eq!(ids(&db), vec![Pk::Int64(2), Pk::Int64(3), Pk::Int64(4)]);
    db.close();
    let reopened = open(root.path());
    drain(&reopened);
    assert!(old_data.is_file() && old_delta.is_file() && index.is_file());
    reopened.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn empty_compaction_gc_does_not_erase_new_uncommitted_delete_evidence() {
    use std::sync::mpsc;
    let root = tempfile::tempdir().unwrap();
    fixture(
        root.path(),
        &[vec![(1, 1)], vec![(2, 2)], vec![(3, 3)], vec![(4, 4)]],
        &[(1, 5), (2, 5), (3, 5), (4, 5)],
    );
    let db = open(root.path());
    let (entered, arrived) = mpsc::channel();
    let (resume, continued) = mpsc::channel();
    db.pause_compaction_before_commit_once(entered, continued);
    let handle = db.collection("", "c").unwrap();
    arrived.recv_timeout(Duration::from_secs(15)).unwrap();
    db.insert("", "c", batch(&[5])).unwrap();
    db.delete("", "c", &[Pk::Int64(5)]).unwrap();
    resume.send(()).unwrap();
    drain(&db);
    assert_eq!(handle.read().count(), 0);
    assert!(
        manifest(root.path()).partitions["_default"]
            .data_files
            .is_empty()
    );
    assert!(
        manifest(root.path()).partitions["_default"]
            .delta_files
            .is_empty()
    );
    db.flush_collection("", "c").unwrap();
    db.close();
    let reopened = open(root.path());
    assert_eq!(reopened.collection("", "c").unwrap().read().count(), 0);
    reopened.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn active_delete_does_not_shadow_the_committed_delete_used_to_remove_old_rows() {
    use std::sync::mpsc;
    let root = tempfile::tempdir().unwrap();
    fixture(
        root.path(),
        &[vec![(1, 1)], vec![(2, 3)], vec![(3, 4)], vec![(4, 5)]],
        &[(1, 2)],
    );
    let db = open(root.path());
    let (entered, arrived) = mpsc::channel();
    let (resume, continued) = mpsc::channel();
    db.pause_compaction_once(
        milvus_lite_core::CompactionPausePoint::BeforePrepare,
        entered,
        continued,
    );
    db.collection("", "c").unwrap();
    arrived.recv_timeout(Duration::from_secs(15)).unwrap();
    db.insert("", "c", batch(&[1])).unwrap();
    db.delete("", "c", &[Pk::Int64(1)]).unwrap();
    resume.send(()).unwrap();
    drain(&db);
    let m = manifest(root.path());
    assert_eq!(m.partitions["_default"].data_files.len(), 1);
    let data = &m.partitions["_default"].data_files[0];
    let batches = milvus_lite_core::storage::parquet_file::read_all(
        &root
            .path()
            .join("collections/c/partitions/_default")
            .join(data),
    )
    .unwrap();
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 3);
    assert!(!ids(&db).contains(&Pk::Int64(1)));
    db.flush_collection("", "c").unwrap();
    db.close();
    let reopened = open(root.path());
    assert_eq!(
        ids(&reopened),
        vec![Pk::Int64(2), Pk::Int64(3), Pk::Int64(4)]
    );
    reopened.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn recovery_keeps_committed_delete_separate_from_newer_wal_delete() {
    use milvus_lite_core::{
        ColumnData,
        storage::{
            arrow_data, fs_commit,
            wal::{WalKind, WalWriter},
        },
    };
    let root = tempfile::tempdir().unwrap();
    fixture(
        root.path(),
        &[vec![(1, 1)], vec![(2, 3)], vec![(3, 4)], vec![(4, 5)]],
        &[(1, 2)],
    );
    let wal = root.path().join("collections/c/wal");
    fs_commit::create_dir_synced(&wal).unwrap();
    let schema = schema();
    let batch = arrow_data::wal_data_batch(
        &schema,
        &[6],
        "_default",
        &[
            ColumnData::Int64(vec![1]),
            ColumnData::FloatVector {
                dim: 2,
                values: vec![1.0, 0.0],
            },
        ],
        None,
    )
    .unwrap();
    let mut writer = WalWriter::create(&wal, WalKind::Data, 1, &batch.schema()).unwrap();
    writer.append(&batch).unwrap();
    writer.into_file().sync_all().unwrap();
    let batch = arrow_data::wal_delta_batch(&schema, &[Pk::Int64(1)], &[7], "_default").unwrap();
    let mut writer = WalWriter::create(&wal, WalKind::Delta, 1, &batch.schema()).unwrap();
    writer.append(&batch).unwrap();
    writer.into_file().sync_all().unwrap();
    let original_data = std::fs::read(wal.join("wal_data_000001.arrow")).unwrap();
    let original_delta = std::fs::read(wal.join("wal_delta_000001.arrow")).unwrap();
    let db = open(root.path());
    db.collection("", "c").unwrap();
    drain(&db);
    let current = manifest(root.path());
    let data = &current.partitions["_default"].data_files[0];
    let batches = milvus_lite_core::storage::parquet_file::read_all(
        &root
            .path()
            .join("collections/c/partitions/_default")
            .join(data),
    )
    .unwrap();
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 3);
    assert_eq!(
        std::fs::read(wal.join("wal_data_000001.arrow")).unwrap(),
        original_data
    );
    assert_eq!(
        std::fs::read(wal.join("wal_delta_000001.arrow")).unwrap(),
        original_delta
    );
    assert_eq!(ids(&db), vec![Pk::Int64(2), Pk::Int64(3), Pk::Int64(4)]);
    db.close();
}

#[cfg(feature = "fault-injection")]
fn drain(db: &Db) -> milvus_lite_core::MaintenanceReport {
    let report = db
        .wait_for_maintenance("", "c", Duration::from_secs(15))
        .unwrap();
    assert_eq!(report.failed, 0, "{report:?}");
    report
}

#[cfg(feature = "fault-injection")]
#[test]
fn preparing_compaction_allows_flush_and_preserves_new_primary_winners() {
    use std::sync::mpsc;
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    drain(&db);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    db.pause_compaction_before_commit_once(entered_tx, resume_rx);
    for id in 1..=4 {
        db.insert("", "c", batch(&[id])).unwrap();
        db.flush_collection("", "c").unwrap();
    }
    entered_rx.recv_timeout(Duration::from_secs(15)).unwrap();
    let handle = db.collection("", "c").unwrap();
    let old = handle.read().read_snapshot();
    db.insert("", "c", batch(&[1, 5])).unwrap();
    db.delete("", "c", &[Pk::Int64(2)]).unwrap();
    db.flush_collection("", "c").unwrap();
    resume_tx.send(()).unwrap();
    let report = drain(&db);
    assert_eq!(report.accepted, 6);
    assert_eq!(
        ids(&db),
        vec![Pk::Int64(1), Pk::Int64(3), Pk::Int64(4), Pk::Int64(5)]
    );
    assert_eq!(
        snapshot_ids(&old, &db),
        vec![Pk::Int64(1), Pk::Int64(2), Pk::Int64(3), Pk::Int64(4)]
    );
    let before = ids(&db);
    db.close();
    let reopened = open(root.path());
    assert_eq!(ids(&reopened), before);
    reopened.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn create_and_delete_only_flush_each_have_one_trigger_and_empty_flush_has_none() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    assert_eq!(drain(&db).accepted, 1);
    db.flush_collection("", "c").unwrap();
    assert_eq!(drain(&db).accepted, 1);
    db.insert("", "c", batch(&[1])).unwrap();
    db.flush_collection("", "c").unwrap();
    assert_eq!(drain(&db).accepted, 2);
    db.delete("", "c", &[Pk::Int64(1)]).unwrap();
    db.flush_collection("", "c").unwrap();
    assert_eq!(drain(&db).accepted, 3);
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn promoted_outputs_from_a_failed_replace_are_removed_by_the_next_round() {
    use std::collections::BTreeSet;
    let root = tempfile::tempdir().unwrap();
    fixture(
        root.path(),
        &[vec![(1, 1)], vec![(2, 2)], vec![(3, 3)], vec![(4, 4)]],
        &[],
    );
    let before = std::fs::read(root.path().join("collections/c/manifest.json")).unwrap();
    let db = open(root.path());
    db.inject_compaction_fault_once(milvus_lite_core::CompactionFault::BeforeReplacePanic);
    db.collection("", "c").unwrap();
    assert_eq!(
        db.wait_for_maintenance("", "c", Duration::from_secs(15))
            .unwrap()
            .failed,
        1
    );
    assert_eq!(
        std::fs::read(root.path().join("collections/c/manifest.json")).unwrap(),
        before
    );
    let directory = root.path().join("collections/c/partitions/_default/data");
    let files = || {
        std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| format!("data/{}", entry.unwrap().file_name().to_str().unwrap()))
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(files().len(), 5);
    db.insert("", "c", batch(&[5])).unwrap();
    db.flush_collection("", "c").unwrap();
    let report = db
        .wait_for_maintenance("", "c", Duration::from_secs(15))
        .unwrap();
    assert_eq!(report.failed, 1);
    assert_eq!(
        files(),
        manifest(root.path()).partitions["_default"]
            .data_files
            .iter()
            .cloned()
            .collect()
    );
    assert_eq!(
        ids(&db),
        vec![
            Pk::Int64(1),
            Pk::Int64(2),
            Pk::Int64(3),
            Pk::Int64(4),
            Pk::Int64(5)
        ]
    );
    db.close();
}
