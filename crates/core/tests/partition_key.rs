//! 0017 B 的真实 Db 入口：桶碰撞、坏 WAL 的隔离与写入交错。
#![allow(clippy::unwrap_used)]
use milvus_lite_core::{
    CollectionSchema, ColumnData, Compute, DataDir, DataType, Db, FieldSchema, InputValues,
    InsertBatch, Pk,
};
use std::{path::Path, sync::Arc};

fn open(root: &Path) -> Arc<Db> {
    Db::new(
        DataDir::open(root).unwrap(),
        Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
    )
    .unwrap()
}
fn schema(key: bool) -> CollectionSchema {
    CollectionSchema {
        fields: vec![
            FieldSchema {
                is_primary: true,
                auto_id: true,
                ..FieldSchema::new("id", DataType::Int64)
            },
            FieldSchema {
                is_partition_key: key,
                ..FieldSchema::new("k", DataType::Int64)
            },
            FieldSchema {
                dim: Some(2),
                ..FieldSchema::new("v", DataType::FloatVector)
            },
        ],
        ..Default::default()
    }
}
fn batch(keys: &[i64], ids: Option<&[i64]>) -> InsertBatch {
    let mut columns = vec![
        ("k".into(), InputValues::Int(keys.to_vec())),
        (
            "v".into(),
            InputValues::FloatVector {
                dim: 2,
                values: vec![1.; keys.len() * 2],
            },
        ),
    ];
    if let Some(ids) = ids {
        columns.push(("id".into(), InputValues::Int(ids.to_vec())));
    }
    InsertBatch {
        num_rows: keys.len(),
        columns,
    }
}

#[test]
fn distinct_key_values_in_one_hash_bucket_are_not_separate_wal_groups() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(true)).unwrap();
    // 参照 MD5("0") 与 MD5("11") 都为桶 10。
    assert_eq!(
        db.insert("", "c", batch(&[0, 1, 11, 0], None)).unwrap(),
        vec![Pk::Int64(1), Pk::Int64(4), Pk::Int64(2), Pk::Int64(3)]
    );
    let dir = root.path().join("collections/c");
    let wal = milvus_lite_core::storage::wal::read_prefix(&dir.join("wal/wal_data_000001.arrow"))
        .unwrap();
    assert_eq!(wal.batches.len(), 2);
    assert_eq!(wal.batches[0].num_rows(), 3);
    assert_eq!(wal.batches[1].num_rows(), 1);
    db.close();
    drop(db);
    let db = open(root.path());
    assert_eq!(
        db.collection("", "c")
            .unwrap()
            .read()
            .count_partition("_pk_10")
            .unwrap(),
        3
    );
    db.close();
}

#[test]
fn bad_key_wal_is_refused_before_cleanup_without_blocking_a_healthy_collection() {
    use milvus_lite_core::storage::{arrow_data, wal};
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "bad", schema(true)).unwrap();
    db.create_collection("", "healthy", schema(false)).unwrap();
    db.insert("", "healthy", batch(&[0], Some(&[9]))).unwrap();
    db.close();
    drop(db);
    let dir = root.path().join("collections/bad");
    let wal_dir = dir.join("wal");
    std::fs::create_dir(&wal_dir).unwrap();
    let batch = arrow_data::wal_data_batch(
        &schema(true),
        &[1],
        "_pk_16",
        &[
            ColumnData::Int64(vec![1]),
            ColumnData::Int64(vec![0]),
            ColumnData::FloatVector {
                dim: 2,
                values: vec![1., 1.],
            },
        ],
        None,
    )
    .unwrap();
    let mut writer =
        wal::WalWriter::create(&wal_dir, wal::WalKind::Data, 1, &batch.schema()).unwrap();
    writer.append(&batch).unwrap();
    drop(writer);
    let paths = [
        dir.join("schema.json"),
        dir.join("manifest.json"),
        wal_dir.join("wal_data_000001.arrow"),
    ];
    let before: Vec<_> = paths
        .iter()
        .map(std::fs::read)
        .collect::<Result<_, _>>()
        .unwrap();
    let db = open(root.path());
    assert!(
        db.collection("", "bad")
            .unwrap_err()
            .to_string()
            .contains("not registered")
    );
    assert_eq!(db.collection("", "healthy").unwrap().read().count(), 1);
    let after: Vec<_> = paths
        .iter()
        .map(std::fs::read)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(after, before);
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn key_request_is_serialized_against_other_writes_delete_flush_and_fresh_reads() {
    use std::sync::mpsc::channel;
    use std::time::Duration;
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(true)).unwrap();
    db.insert("", "c", batch(&[0], Some(&[99]))).unwrap();
    db.flush_collection("", "c").unwrap();
    let handle = db.collection("", "c").unwrap();
    let snapshot = handle.read().read_snapshot();
    let (entered, reached) = channel();
    let (resume, wait) = channel();
    let writer_db = Arc::clone(&db);
    let writer = std::thread::spawn(move || {
        milvus_lite_core::storage::wal::pause_append_after_io_after(1, entered, wait);
        writer_db.insert("", "c", batch(&[0, 1, 0], None))
    });
    reached.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(snapshot.count_filtered("").unwrap(), 1);
    let mut workers = Vec::new();
    let (begun, starts) = channel();
    let (finished, done) = channel();
    for action in ["insert", "delete", "flush", "get"] {
        let db = Arc::clone(&db);
        let begun = begun.clone();
        let finished = finished.clone();
        workers.push(std::thread::spawn(move || {
            begun.send(()).unwrap();
            match action {
                "insert" => {
                    db.insert("", "c", batch(&[1], Some(&[100]))).unwrap();
                }
                "delete" => {
                    db.delete("", "c", &[Pk::Int64(99)]).unwrap();
                }
                "flush" => db.flush_collection("", "c").unwrap(),
                "get" => {
                    let rows = db
                        .collection("", "c")
                        .unwrap()
                        .read()
                        .get(&[Pk::Int64(2), Pk::Int64(3), Pk::Int64(4)], &["id".into()])
                        .unwrap();
                    assert_eq!(
                        rows.rows.pks,
                        vec![Pk::Int64(2), Pk::Int64(3), Pk::Int64(4)]
                    );
                }
                _ => unreachable!(),
            }
            finished.send(()).unwrap();
        }));
    }
    for _ in 0..4 {
        starts.recv_timeout(Duration::from_secs(10)).unwrap();
    }
    assert!(done.try_recv().is_err());
    resume.send(()).unwrap();
    assert_eq!(
        writer.join().unwrap().unwrap(),
        vec![Pk::Int64(2), Pk::Int64(4), Pk::Int64(3)]
    );
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(handle.read().count(), 4);
    assert_eq!(snapshot.count_filtered("").unwrap(), 1);
    db.close();
    drop(handle);
    drop(db);
    let db = open(root.path());
    assert_eq!(db.collection("", "c").unwrap().read().count(), 4);
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn threshold_is_checked_after_the_entire_key_request_in_one_wal_generation() {
    use milvus_lite_core::storage::{manifest::Manifest, wal};
    use std::sync::mpsc::channel;
    use std::time::Duration;
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(true)).unwrap();
    let mut keys = vec![0; 10_000];
    keys.push(1);
    let (entered, reached) = channel();
    let (resume, wait) = channel();
    let writer_db = Arc::clone(&db);
    let writer = std::thread::spawn(move || {
        wal::pause_append_after_io_after(1, entered, wait);
        writer_db.insert("", "c", batch(&keys, None))
    });
    reached.recv_timeout(Duration::from_secs(10)).unwrap();
    let dir = root.path().join("collections/c");
    assert_eq!(
        Manifest::read(&dir.join("manifest.json"))
            .unwrap()
            .current_seq,
        0
    );
    let files: Vec<_> = std::fs::read_dir(dir.join("wal"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1);
    let prefix = wal::read_prefix(&files[0]).unwrap();
    assert_eq!(prefix.batches.len(), 2);
    assert_eq!(prefix.batches[0].num_rows(), 10_000);
    assert_eq!(prefix.batches[1].num_rows(), 1);
    resume.send(()).unwrap();
    let ids = writer.join().unwrap().unwrap();
    assert_eq!(ids.len(), 10_001);
    assert_eq!(
        Manifest::read(&dir.join("manifest.json"))
            .unwrap()
            .current_seq,
        10_001
    );
    assert_eq!(std::fs::read_dir(dir.join("wal")).unwrap().count(), 0);
    assert_eq!(db.collection("", "c").unwrap().read().count(), 10_001);
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn key_rows_remain_acknowledged_when_automatic_flush_fails_and_retry_preserves_all_buckets() {
    use milvus_lite_core::storage::{flush, fs_commit};
    for fault in ["before-replace", "after-replace", "wal-cleanup"] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        db.create_collection("", "c", schema(true)).unwrap();
        match fault {
            "before-replace" => {
                flush::inject_flush_worker_panic_once(fs_commit::CommitPhase::BeforeReplace)
            }
            "after-replace" => fs_commit::inject_manifest_dir_sync_failure_once(),
            "wal-cleanup" => flush::inject_wal_unlink_failure_once(),
            _ => unreachable!(),
        }
        let keys: Vec<_> = (0..10_001).map(|i| i % 2).collect();
        let result = db.insert("", "c", batch(&keys, None)).unwrap();
        assert_eq!(result.len(), 10_001, "{fault}");
        assert_eq!(db.collection("", "c").unwrap().read().count(), 10_001);
        // 沿用 A 的重试退避；显式 Flush 强制重试，不以 sleep 绕过门槛。
        db.flush_collection("", "c").unwrap();
        assert_eq!(
            db.insert("", "c", batch(&[1], None)).unwrap(),
            vec![Pk::Int64(10_002)]
        );
        db.close();
        drop(db);
        let db = open(root.path());
        assert_eq!(
            db.collection("", "c").unwrap().read().count(),
            10_002,
            "{fault}"
        );
        db.close();
    }
}

#[test]
fn key_default_outside_i64_is_a_business_error_without_wal_or_sequence_consumption() {
    for text in [
        "9223372036854775808",
        "-9223372036854775809",
        "340282366920938463463374607431768211456",
        "-340282366920938463463374607431768211456",
    ] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        let mut s = schema(true);
        s.fields[1].default_value = Some(serde_json::from_str(text).unwrap());
        db.create_collection("", "c", s).unwrap();
        let mut input = batch(&[0], None);
        input.columns.retain(|(name, _)| name != "k");
        let error = db.insert("", "c", input).unwrap_err();
        assert!(matches!(
            error,
            milvus_lite_core::EngineError::SchemaValidation(_)
        ));
        assert_eq!(
            error.to_string(),
            format!("field 'k' value {text} does not match dtype DataType.INT64")
        );
        assert!(!root.path().join("collections/c/wal").exists());
        assert_eq!(db.collection("", "c").unwrap().read().count(), 0);
        assert_eq!(
            db.insert("", "c", batch(&[0], None)).unwrap(),
            vec![Pk::Int64(1)]
        );
        db.close();
    }
}
