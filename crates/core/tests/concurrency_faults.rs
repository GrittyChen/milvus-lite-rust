//! 并发与失败使用同一份逐行真值；进程退出用有界的独立子进程隔离。
#![allow(clippy::unwrap_used)]
#![cfg(feature = "fault-injection")]

use milvus_lite_core::data::ColumnData;
use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, FieldSchema, InputValues, InsertBatch, Pk,
    QueryVectors, SearchRequest,
};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

// 子进程 exec 前会短暂继承本进程的 flock 文件描述符；隔离无关用例的关库重开，
// 每条用例内部的写者、读者与提交者仍真实并发。
static CASE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

#[test]
fn failed_case_does_not_poison_the_serialization_lock() {
    let failed = std::panic::catch_unwind(|| {
        let _case = CASE_LOCK.lock();
        panic!("deliberate isolated case failure");
    });
    assert!(failed.is_err());
    let _next = CASE_LOCK.lock();
}

fn schema() -> CollectionSchema {
    let mut id = FieldSchema::new("id", DataType::Int64);
    id.is_primary = true;
    let mut v = FieldSchema::new("v", DataType::FloatVector);
    v.dim = Some(2);
    CollectionSchema {
        fields: vec![id, v, FieldSchema::new("age", DataType::Int64)],
        enable_dynamic_field: false,
        properties: vec![],
    }
}
fn batch(id: i64, age: i64) -> InsertBatch {
    InsertBatch {
        num_rows: 1,
        columns: vec![
            ("id".into(), InputValues::Int(vec![id])),
            (
                "v".into(),
                InputValues::FloatVector {
                    dim: 2,
                    values: vec![age as f32, 0.0],
                },
            ),
            ("age".into(), InputValues::Int(vec![age])),
        ],
    }
}
fn open(path: &std::path::Path) -> std::sync::Arc<Db> {
    Db::new(
        DataDir::open(path).unwrap(),
        Compute::new(std::num::NonZeroUsize::new(2).unwrap()).unwrap(),
    )
    .unwrap()
}
fn rows(view: &milvus_lite_core::collection::ReadSnapshot) -> BTreeMap<i64, i64> {
    let result = view
        .query("age >= 0", &["age".into(), "v".into()], None, 0)
        .unwrap();
    let ColumnData::Int64(ages) = &result
        .rows
        .fields
        .iter()
        .find(|(name, _)| name == "age")
        .unwrap()
        .1
    else {
        panic!("age projection type")
    };
    let ColumnData::FloatVector { values, .. } = &result
        .rows
        .fields
        .iter()
        .find(|(name, _)| name == "v")
        .unwrap()
        .1
    else {
        panic!("vector projection type")
    };
    let mut model = BTreeMap::new();
    for (i, pk) in result.rows.pks.iter().enumerate() {
        let Pk::Int64(id) = pk else {
            panic!("primary key type")
        };
        assert_eq!(&values[i * 2..i * 2 + 2], &[ages[i] as f32, 0.0]);
        assert!(model.insert(*id, ages[i]).is_none());
    }
    let hits = view
        .search_filtered(
            SearchRequest {
                anns_field: None,
                vectors: QueryVectors::Dense(vec![vec![0.0, 0.0]]),
                metric_type: Some("L2".into()),
                limit: 10000,
                offset: 0,
                output_fields: vec![],
            },
            &Compute::new(std::num::NonZeroUsize::new(2).unwrap()).unwrap(),
            "age >= 50",
        )
        .unwrap();
    let ids = hits.hits[0]
        .rows
        .pks
        .iter()
        .map(|pk| match pk {
            Pk::Int64(id) => *id,
            _ => panic!("primary key type"),
        })
        .collect::<std::collections::BTreeSet<_>>();
    let filtered = model
        .iter()
        .filter(|(_, age)| **age >= 50)
        .map(|(id, _)| *id)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids, filtered);
    let selected = view
        .query("age >= 50", &["age".into()], None, 0)
        .unwrap()
        .rows
        .pks
        .into_iter()
        .map(|pk| match pk {
            Pk::Int64(id) => id,
            _ => panic!("primary key type"),
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(selected, filtered);
    assert_eq!(view.count_filtered("age >= 50").unwrap(), filtered.len());
    model
}

#[cfg(feature = "fault-injection")]
#[test]
fn parquet_decode_failure_prevents_registration_and_cleanup_but_io_can_retry() {
    let _case = CASE_LOCK.lock();
    use milvus_lite_core::storage::parquet_file::{ParquetReadFault, inject_read_failure_once};
    for fault in [
        ParquetReadFault::MetadataPanic,
        ParquetReadFault::BatchPanicAfter(0),
        ParquetReadFault::BatchPanicAfter(1),
        ParquetReadFault::ReadIo,
    ] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        db.create_collection("", "c", schema()).unwrap();
        db.insert("", "c", batch(1, 2)).unwrap();
        db.flush_collection("", "c").unwrap();
        db.close();
        let collection = root.path().join("collections/c");
        let orphan = collection.join("partitions/_default/data/orphan.parquet");
        std::fs::write(&orphan, b"keep").unwrap();
        let manifest = std::fs::read(collection.join("manifest.json")).unwrap();
        let reopened = open(root.path());
        inject_read_failure_once(fault);
        let error = reopened.collection("", "c").err().unwrap().to_string();
        assert!(
            error.contains(if matches!(fault, ParquetReadFault::ReadIo) {
                "I/O"
            } else {
                "panicked"
            }),
            "{error}"
        );
        assert_eq!(std::fs::read(&orphan).unwrap(), b"keep");
        assert_eq!(
            std::fs::read(collection.join("manifest.json")).unwrap(),
            manifest
        );
        if matches!(fault, ParquetReadFault::ReadIo) {
            assert_eq!(reopened.collection("", "c").unwrap().read().count(), 1);
        } else {
            assert_eq!(
                reopened.collection("", "c").err().unwrap().to_string(),
                error
            );
        }
        reopened.close();
        let healthy = open(root.path());
        assert_eq!(
            rows(&healthy.collection("", "c").unwrap().read().read_snapshot()),
            BTreeMap::from([(1, 2)])
        );
        assert!(!orphan.exists());
        healthy.close();
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn state_panics_seal_only_the_affected_collection_and_recover_from_disk() {
    let _case = CASE_LOCK.lock();
    use milvus_lite_core::IndexRequest;
    use milvus_lite_core::db::{StatePanicPoint, inject_state_panic_once};
    for point in [
        StatePanicPoint::Mutation,
        StatePanicPoint::Freeze,
        StatePanicPoint::Publish,
        StatePanicPoint::Index,
    ] {
        let root = tempfile::tempdir().unwrap();
        let db = Arc::new(open(root.path()));
        db.create_collection("", "c", schema()).unwrap();
        db.create_collection("", "healthy", schema()).unwrap();
        db.insert("", "c", batch(1, 1)).unwrap();
        let old = db.collection("", "c").unwrap().read().read_snapshot();
        inject_state_panic_once(point);
        let error = match point {
            StatePanicPoint::Mutation => db.insert("", "c", batch(2, 2)).map(|_| ()),
            StatePanicPoint::Index => db.create_index(
                "",
                "c",
                "v",
                IndexRequest {
                    index_type: "FLAT".into(),
                    metric_type: Some("L2".into()),
                    ..Default::default()
                },
            ),
            _ => db.flush_collection("", "c"),
        }
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("injected collection state panic")
        );
        assert_eq!(rows(&old), BTreeMap::from([(1, 1)]));
        let mut waiting = vec![];
        for write in [false, true] {
            let db = Arc::clone(&db);
            waiting.push(std::thread::spawn(move || {
                if write {
                    db.insert("", "c", batch(9, 9)).map(|_| ())
                } else {
                    db.collection("", "c").map(|_| ())
                }
            }));
        }
        for worker in waiting {
            assert!(worker.join().unwrap().is_err());
        }
        assert!(db.flush_collection("", "c").is_err());
        db.insert("", "healthy", batch(3, 3)).unwrap();
        db.close();
        let reopened = open(root.path());
        let view = reopened.collection("", "c").unwrap().read().read_snapshot();
        if matches!(point, StatePanicPoint::Index) {
            reopened
                .collection("", "c")
                .unwrap()
                .write()
                .load()
                .unwrap();
        }
        let view = if matches!(point, StatePanicPoint::Index) {
            reopened.collection("", "c").unwrap().read().read_snapshot()
        } else {
            view
        };
        assert_eq!(
            rows(&view),
            if matches!(point, StatePanicPoint::Mutation) {
                BTreeMap::from([(1, 1), (2, 2)])
            } else {
                BTreeMap::from([(1, 1)])
            }
        );
        reopened.close();
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn manifest_and_cleanup_panics_keep_their_commit_phase_and_can_retry() {
    let _case = CASE_LOCK.lock();
    use milvus_lite_core::storage::{
        flush::inject_wal_unlink_panic_once,
        fs_commit::{CommitPhase, inject_manifest_panic_once},
        runtime::hold_flush_retry_window,
    };
    for phase in [
        Some(CommitPhase::BeforeReplace),
        Some(CommitPhase::AfterReplace),
        None,
    ] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        db.create_collection("", "c", schema()).unwrap();
        db.insert("", "c", batch(0, 0)).unwrap();
        db.flush_collection("", "c").unwrap();
        db.insert("", "c", batch(1, 1)).unwrap();
        let dir = root.path().join("collections/c");
        let before = std::fs::read(dir.join("manifest.json")).unwrap();
        if let Some(phase) = phase {
            inject_manifest_panic_once(phase);
        } else {
            inject_wal_unlink_panic_once();
        }
        let error = db.flush_collection("", "c").unwrap_err().to_string();
        assert!(error.contains("panicked"), "{error}");
        assert_eq!(
            rows(&db.collection("", "c").unwrap().read().read_snapshot()),
            BTreeMap::from([(0, 0), (1, 1)])
        );
        if phase == Some(CommitPhase::BeforeReplace) {
            assert_eq!(std::fs::read(dir.join("manifest.json")).unwrap(), before);
            assert!(error.contains("BeforeReplace"));
        } else {
            assert_ne!(std::fs::read(dir.join("manifest.json")).unwrap(), before);
            if phase == Some(CommitPhase::AfterReplace) {
                assert!(
                    error.contains("directory durability is unconfirmed"),
                    "{error}"
                );
            } else {
                assert!(error.contains("WAL cleanup failed"), "{error}");
            }
        }
        let window = hold_flush_retry_window();
        if phase.is_some() {
            assert!(db.insert("", "c", batch(2, 2)).is_err());
        } else {
            db.insert("", "c", batch(2, 2)).unwrap();
        }
        db.flush_collection("", "c").unwrap();
        drop(window);
        db.insert("", "c", batch(3, 3)).unwrap();
        db.close();
        let reopened = open(root.path());
        let actual = rows(&reopened.collection("", "c").unwrap().read().read_snapshot());
        assert!(actual.contains_key(&1) && actual.contains_key(&3));
        assert_eq!(actual.contains_key(&2), phase.is_none());
        reopened.close();
    }
}

#[test]
fn flush_preparation_panic_is_retryable_before_manifest_replace() {
    let _case = CASE_LOCK.lock();
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(1, 1)).unwrap();
    milvus_lite_core::storage::runtime::inject_flush_prepare_panic_once();
    let error = db.flush_collection("", "c").unwrap_err().to_string();
    assert!(error.contains("BeforeReplace") && error.contains("injected flush preparation panic"));
    assert!(!root.path().join("collections/c/manifest.json").exists());
    assert_eq!(
        rows(&db.collection("", "c").unwrap().read().read_snapshot()),
        BTreeMap::from([(1, 1)])
    );
    db.flush_collection("", "c").unwrap();
    db.insert("", "c", batch(2, 2)).unwrap();
    db.close();
    let reopened = open(root.path());
    assert_eq!(
        rows(&reopened.collection("", "c").unwrap().read().read_snapshot()),
        BTreeMap::from([(1, 1), (2, 2)])
    );
    reopened.close();
}

#[test]
fn unlocked_flush_worker_panics_publish_only_after_the_actual_commit() {
    let _case = CASE_LOCK.lock();
    use milvus_lite_core::storage::{
        flush::inject_flush_worker_panic_once, fs_commit::CommitPhase,
    };
    for phase in [CommitPhase::BeforeReplace, CommitPhase::AfterReplace] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        db.create_collection("", "c", schema()).unwrap();
        db.insert("", "c", batch(1, 1)).unwrap();
        inject_flush_worker_panic_once(phase);
        let error = db.flush_collection("", "c").unwrap_err().to_string();
        assert!(error.contains("flush worker panicked"), "{error}");
        let manifest = root.path().join("collections/c/manifest.json");
        assert_eq!(manifest.exists(), phase == CommitPhase::AfterReplace);
        assert_eq!(
            rows(&db.collection("", "c").unwrap().read().read_snapshot()),
            BTreeMap::from([(1, 1)])
        );
        db.flush_collection("", "c").unwrap();
        db.insert("", "c", batch(2, 2)).unwrap();
        db.close();
        let reopened = open(root.path());
        assert_eq!(
            rows(&reopened.collection("", "c").unwrap().read().read_snapshot()),
            BTreeMap::from([(1, 1), (2, 2)])
        );
        reopened.close();
    }
}

#[test]
fn parallel_drops_preserve_live_aliases_and_remove_both_deleted_targets() {
    let _case = CASE_LOCK.lock();
    let root = tempfile::tempdir().unwrap();
    let db = Arc::new(open(root.path()));
    for name in ["a", "b", "live"] {
        db.create_collection("", name, schema()).unwrap();
    }
    for (target, alias) in [("a", "aa"), ("b", "bb"), ("live", "keep")] {
        db.create_alias("", target, alias).unwrap();
    }
    let mut workers = vec![];
    for name in ["a", "b"] {
        let db = Arc::clone(&db);
        workers.push(std::thread::spawn(move || db.drop_collection("", name)));
    }
    for worker in workers {
        worker.join().unwrap().unwrap();
    }
    let aliases: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join("aliases.json")).unwrap()).unwrap();
    assert_eq!(aliases, serde_json::json!({"keep":"live"}));
    db.close();
}

#[test]
fn directory_panics_before_commit_roll_back_and_alias_panics_after_commit_stay_retryable() {
    let _case = CASE_LOCK.lock();
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
    };
    for creating in [false, true] {
        for point in [
            DirectoryPoint::BeforeRename,
            DirectoryPoint::Aliases,
            DirectoryPoint::AliasesAfterRename,
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut db = open(root.path());
            db.create_collection("", "c", schema()).unwrap();
            db.insert("", "c", batch(1, 1)).unwrap();
            if creating {
                // 悬空别名只能来自打开之前的文件；别名表在首次使用后缓存。
                db.close();
                std::fs::write(root.path().join("aliases.json"), br#"{"old":"new"}"#).unwrap();
                db = open(root.path());
            } else {
                db.create_alias("", "c", "old").unwrap();
            }
            inject_directory_fault_once(
                if creating {
                    DirectoryOperation::Create
                } else {
                    DirectoryOperation::Drop
                },
                point,
                DirectoryFault::Panic,
            );
            let error = if creating {
                db.create_collection("", "new", schema())
            } else {
                db.drop_collection("", "c")
            }
            .unwrap_err()
            .to_string();
            assert!(error.contains("panicked"), "{error}");
            if point == DirectoryPoint::AliasesAfterRename {
                assert_eq!(
                    std::fs::read(root.path().join("aliases.json")).unwrap(),
                    b"{}"
                );
            }
            if !creating
                && matches!(
                    point,
                    DirectoryPoint::Aliases | DirectoryPoint::AliasesAfterRename
                )
            {
                assert!(error.contains("collection was removed"));
                assert!(db.collection("", "c").is_err());
                db.drop_collection("", "c").unwrap();
                assert_eq!(
                    std::fs::read(root.path().join("aliases.json")).unwrap(),
                    b"{}"
                );
            } else {
                assert_eq!(db.collection("", "c").unwrap().read().count(), 1);
                db.insert("", "c", batch(2, 2)).unwrap();
                if creating {
                    assert!(!root.path().join("collections/new").exists());
                    db.create_collection("", "new", schema()).unwrap();
                } else {
                    db.drop_collection("", "c").unwrap();
                }
            }
            db.close();
        }
    }
}

#[test]
fn close_panic_finishes_the_gate_and_retains_acknowledged_wal_for_reopen() {
    let _case = CASE_LOCK.lock();
    let root = tempfile::tempdir().unwrap();
    let db = Arc::new(open(root.path()));
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(1, 1)).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let closer = Arc::clone(&db);
    let worker = std::thread::spawn(move || {
        milvus_lite_core::db::inject_close_finish_panic_once();
        closer.close();
        closer.close();
        tx.send(()).unwrap();
    });
    rx.recv_timeout(Duration::from_secs(10)).unwrap();
    worker.join().unwrap();
    assert!(!root.path().join("collections/c/manifest.json").exists());
    assert!(
        root.path()
            .join("collections/c/wal/wal_data_000001.arrow")
            .exists()
    );
    let reopened = open(root.path());
    assert_eq!(
        rows(&reopened.collection("", "c").unwrap().read().read_snapshot()),
        BTreeMap::from([(1, 1)])
    );
    reopened.close();
}

#[test]
fn close_keeps_process_lock_until_an_inflight_flush_has_finished() {
    let _case = CASE_LOCK.lock();
    if let Ok(path) = std::env::var("MLR_LOCK_PROBE") {
        let result = DataDir::open(std::path::Path::new(&path));
        std::process::exit(if result.is_ok() {
            90
        } else if matches!(
            result,
            Err(milvus_lite_core::data_dir::DataDirError::Locked { .. })
        ) {
            91
        } else {
            92
        });
    }
    let root = tempfile::tempdir().unwrap();
    let db = Arc::new(open(root.path()));
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(1, 1)).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let writer = Arc::clone(&db);
    let writing = std::thread::spawn(move || {
        let _task = writer.enter().unwrap();
        milvus_lite_core::storage::runtime::pause_flush_before_commit_once(entered_tx, resume_rx);
        writer.flush_collection("", "c").unwrap();
    });
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let (close_tx, close_rx) = std::sync::mpsc::channel();
    let closing_db = Arc::clone(&db);
    let closing = std::thread::spawn(move || {
        closing_db.close();
        close_tx.send(()).unwrap();
    });
    let probe = || {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "close_keeps_process_lock_until_an_inflight_flush_has_finished",
            ])
            .env("MLR_LOCK_PROBE", root.path())
            .output()
            .unwrap()
            .status
            .code()
    };
    assert_eq!(probe(), Some(91));
    assert!(matches!(
        close_rx.recv_timeout(Duration::from_millis(100)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    resume_tx.send(()).unwrap();
    writing.join().unwrap();
    close_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    closing.join().unwrap();
    assert_eq!(probe(), Some(90));
    let reopened = open(root.path());
    assert_eq!(reopened.collection("", "c").unwrap().read().count(), 1);
    reopened.close();
}

#[test]
fn automatic_freeze_panic_does_not_turn_an_accepted_insert_into_an_error() {
    let _case = CASE_LOCK.lock();
    use milvus_lite_core::db::{StatePanicPoint, inject_state_panic_once};
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    let old = db.collection("", "c").unwrap().read().read_snapshot();
    let n = 10_000;
    let input = InsertBatch {
        num_rows: n,
        columns: vec![
            ("id".into(), InputValues::Int((0..n as i64).collect())),
            ("age".into(), InputValues::Int((0..n as i64).collect())),
            (
                "v".into(),
                InputValues::FloatVector {
                    dim: 2,
                    values: (0..n).flat_map(|i| [i as f32, 0.0]).collect(),
                },
            ),
        ],
    };
    inject_state_panic_once(StatePanicPoint::Freeze);
    assert_eq!(db.insert("", "c", input).unwrap().len(), n);
    assert!(rows(&old).is_empty());
    assert!(db.insert("", "c", batch(20_000, 1)).is_err());
    db.close();
    let reopened = open(root.path());
    assert_eq!(reopened.collection("", "c").unwrap().read().count(), n);
    assert_eq!(
        reopened
            .collection("", "c")
            .unwrap()
            .read()
            .get(&[Pk::Int64(0), Pk::Int64(9999)], &[])
            .unwrap()
            .rows
            .pks,
        [Pk::Int64(0), Pk::Int64(9999)]
    );
    reopened.close();
}

#[test]
fn index_commit_panics_use_the_same_replace_boundary_as_flush() {
    let _case = CASE_LOCK.lock();
    use milvus_lite_core::IndexRequest;
    use milvus_lite_core::storage::fs_commit::{CommitPhase, inject_manifest_panic_once};
    for phase in [CommitPhase::BeforeReplace, CommitPhase::AfterReplace] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        db.create_collection("", "c", schema()).unwrap();
        db.insert("", "c", batch(1, 1)).unwrap();
        db.flush_collection("", "c").unwrap();
        let path = root.path().join("collections/c/manifest.json");
        let before = std::fs::read(&path).unwrap();
        inject_manifest_panic_once(phase);
        let result = db.create_index(
            "",
            "c",
            "v",
            IndexRequest {
                index_type: "FLAT".into(),
                metric_type: Some("L2".into()),
                ..Default::default()
            },
        );
        if phase == CommitPhase::BeforeReplace {
            assert!(result.unwrap_err().to_string().contains("BeforeReplace"));
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert!(db.collection("", "c").unwrap().read().index("v").is_none());
        } else {
            result.unwrap();
            assert_ne!(std::fs::read(&path).unwrap(), before);
            assert_eq!(
                db.collection("", "c")
                    .unwrap()
                    .read()
                    .index("v")
                    .unwrap()
                    .metric_type,
                "L2"
            );
            db.flush_collection("", "c").unwrap();
        }
        db.close();
        let reopened = open(root.path());
        assert_eq!(
            reopened
                .collection("", "c")
                .unwrap()
                .read()
                .index("v")
                .is_some(),
            phase == CommitPhase::AfterReplace
        );
        reopened.close();
    }
}

#[test]
fn inherited_and_new_files_are_synced_before_replace_and_wal_unlinks_stay_ordered() {
    let _case = CASE_LOCK.lock();
    use milvus_lite_core::storage::fs_commit::{FsAction, begin_fs_trace, finish_fs_trace};
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(1, 1)).unwrap();
    db.flush_collection("", "c").unwrap();
    db.close();
    let db = open(root.path());
    db.insert("", "c", batch(2, 2)).unwrap();
    db.delete("", "c", &[Pk::Int64(1)]).unwrap();
    begin_fs_trace();
    db.flush_collection("", "c").unwrap();
    let events = finish_fs_trace();
    let target = root.path().join("collections/c/manifest.json");
    let replace = events
        .iter()
        .position(|event| event.action == FsAction::Rename && event.path == target)
        .unwrap();
    let wal = root.path().join("collections/c/wal");
    for suffix in [
        "schema.json",
        "manifest.json.tmp",
        "partitions/_default/data/data_000001_000001.parquet",
        "partitions/_default/data/data_000002_000002.parquet",
        "partitions/_default/delta/delta_000003_000003.parquet",
        "wal/wal_data_000002.arrow",
        "wal/wal_delta_000002.arrow",
    ] {
        let path = root.path().join("collections/c").join(suffix);
        assert!(
            events[..replace]
                .iter()
                .any(|event| event.action == FsAction::SyncFile && event.path == path),
            "missing pre-replace sync: {path:?}; {events:?}"
        );
    }
    for path in [
        root.path().join("collections/c"),
        root.path().join("collections"),
        wal.clone(),
    ] {
        assert!(
            events[..replace]
                .iter()
                .any(|event| event.action == FsAction::SyncDir && event.path == path),
            "{path:?}"
        );
    }
    let unlinks = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.action == FsAction::Unlink)
        .collect::<Vec<_>>();
    assert_eq!(unlinks.len(), 2);
    assert!(
        unlinks[0]
            .1
            .path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("wal_data_")
    );
    assert!(
        unlinks[1]
            .1
            .path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("wal_delta_")
    );
    for (i, (at, event)) in unlinks.iter().enumerate() {
        assert!(*at > replace);
        assert!(
            events[replace + 1..*at].iter().any(
                |previous| previous.action == FsAction::SyncFile && previous.path == event.path
            )
        );
        let end = unlinks
            .get(i + 1)
            .map(|(next, _)| *next)
            .unwrap_or(events.len());
        assert!(
            events[*at + 1..end]
                .iter()
                .any(|next| next.action == FsAction::SyncDir && next.path == wal)
        );
    }
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn concurrent_writes_reads_and_flush_crash_keep_all_acknowledged_rows() {
    let _case = CASE_LOCK.lock();
    if let Ok(path) = std::env::var("MLR_CONCURRENT_CRASH_DIR") {
        let seed: i64 = std::env::var("MLR_CONCURRENT_SEED")
            .unwrap()
            .parse()
            .unwrap();
        let path = std::path::Path::new(&path);
        let db = Arc::new(open(path));
        db.create_collection("", "c", schema()).unwrap();
        db.insert("", "c", batch(10000, 0)).unwrap();
        let model = Arc::new(parking_lot::Mutex::new(BTreeMap::from([(10000, 0)])));
        let stop = Arc::new(AtomicBool::new(false));
        let (read_tx, read_rx) = std::sync::mpsc::channel();
        let (fault_tx, fault_rx) = std::sync::mpsc::channel();
        let reader = {
            let db = Arc::clone(&db);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut n = 0;
                let mut witness = Some(read_tx);
                while !stop.load(Ordering::Acquire) {
                    let snapshot = db.collection("", "c").unwrap().read().read_snapshot();
                    rows(&snapshot);
                    if let Some(witness) = witness.take() {
                        witness.send(()).unwrap();
                    }
                    n += 1;
                }
                n
            })
        };
        let flusher = {
            let db = Arc::clone(&db);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                milvus_lite_core::storage::fs_commit::inject_manifest_dir_sync_failure_once();
                let mut failures = 0;
                let mut witness = Some(fault_tx);
                while !stop.load(Ordering::Acquire) {
                    if db.flush_collection("", "c").is_err() {
                        failures += 1;
                        if let Some(witness) = witness.take() {
                            witness.send(()).unwrap();
                        }
                    }
                    std::thread::yield_now();
                }
                failures
            })
        };
        read_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        fault_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let mut workers = vec![];
        for worker in 0..4 {
            let db = Arc::clone(&db);
            let model = Arc::clone(&model);
            workers.push(std::thread::spawn(move || {
                for i in 0..24 {
                    let id = worker * 100 + i;
                    let age = (seed + i) % 100;
                    for action in 0..4 {
                        let result = match action {
                            0 => db.insert("", "c", batch(id, age)).map(|_| ()),
                            1 => db
                                .upsert_with_pk_type("", "c", batch(id, age + 1))
                                .map(|_| ()),
                            2 => db.delete("", "c", &[Pk::Int64(id)]).map(|_| ()),
                            _ => db.insert("", "c", batch(id, age + 2)).map(|_| ()),
                        };
                        match result {
                            Ok(()) => {
                                if action == 2 {
                                    model.lock().remove(&id);
                                } else {
                                    model.lock().insert(
                                        id,
                                        age + if action == 0 {
                                            0
                                        } else if action == 1 {
                                            1
                                        } else {
                                            2
                                        },
                                    );
                                }
                            }
                            Err(error) => assert!(
                                error.to_string().contains("flush retry is throttled"),
                                "{error}"
                            ),
                        };
                        std::thread::yield_now();
                    }
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let expected = model
            .lock()
            .iter()
            .map(|(id, age)| (*id, *age))
            .collect::<Vec<_>>();
        assert!(expected.len() > 1);
        std::fs::write(
            path.join("expected.json"),
            serde_json::to_vec(&expected).unwrap(),
        )
        .unwrap();
        // 写者已经完成，读者与提交者仍可能在途；退出不调用 close 或补一次 flush。
        assert!(!reader.is_finished());
        assert!(!flusher.is_finished());
        std::process::exit(89);
    }
    for seed in [7, 29, 41] {
        let root = tempfile::tempdir().unwrap();
        let log = std::fs::File::create(root.path().join("child.log")).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "concurrent_writes_reads_and_flush_crash_keep_all_acknowledged_rows",
                "--nocapture",
            ])
            .env("MLR_CONCURRENT_CRASH_DIR", root.path())
            .env("MLR_CONCURRENT_SEED", seed.to_string())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(45);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!(
                    "concurrent child timed out: {}",
                    std::fs::read_to_string(root.path().join("child.log")).unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(
            status.code(),
            Some(89),
            "{}",
            std::fs::read_to_string(root.path().join("child.log")).unwrap()
        );
        let expected: Vec<(i64, i64)> =
            serde_json::from_slice(&std::fs::read(root.path().join("expected.json")).unwrap())
                .unwrap();
        let db = open(root.path());
        assert_eq!(
            rows(&db.collection("", "c").unwrap().read().read_snapshot()),
            expected.into_iter().collect()
        );
        db.close();
    }
}

#[test]
fn controlled_concurrent_failure_rejection_and_inflight_recovery_match_model() {
    let _case = CASE_LOCK.lock();
    if let Ok(path) = std::env::var("MLR_CONTROLLED_CRASH_DIR") {
        use milvus_lite_core::storage::{
            fs_commit::inject_manifest_dir_sync_failure_once,
            runtime::hold_flush_retry_window,
            wal::{inject_storage_full_after_bytes, pause_append_after_io_once},
        };
        let path = std::path::Path::new(&path);
        let db = Arc::new(open(path));
        db.create_collection("", "c", schema()).unwrap();
        db.create_collection("", "partial", schema()).unwrap();
        db.insert("", "c", batch(0, 0)).unwrap();
        db.insert("", "partial", batch(10, 1)).unwrap();
        db.flush_collection("", "partial").unwrap();
        let model = Arc::new(parking_lot::Mutex::new(BTreeMap::from([(0, 0)])));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let writer = {
            let db = Arc::clone(&db);
            let model = Arc::clone(&model);
            std::thread::spawn(move || {
                db.insert("", "c", batch(1, 60)).unwrap();
                model.lock().insert(1, 60);
                started_tx.send(()).unwrap();
                continue_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                db.insert("", "c", batch(2, 70)).unwrap();
                model.lock().insert(2, 70);
            })
        };
        let (fault_tx, fault_rx) = std::sync::mpsc::channel();
        let (retry_tx, retry_rx) = std::sync::mpsc::channel();
        let flusher = {
            let db = Arc::clone(&db);
            std::thread::spawn(move || {
                started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                inject_manifest_dir_sync_failure_once();
                let error = db.flush_collection("", "c").unwrap_err().to_string();
                assert!(error.contains("durability is unconfirmed"), "{error}");
                fault_tx.send(()).unwrap();
                // 不提前 recommit；拒绝者必须确实在待完成状态内执行。
                retry_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                db.flush_collection("", "c").unwrap();
            })
        };
        fault_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let denier = {
            let db = Arc::clone(&db);
            let model = Arc::clone(&model);
            std::thread::spawn(move || {
                let _window = hold_flush_retry_window();
                let mut rejected = 0;
                for action in 0..3 {
                    let result = match action {
                        0 => db.insert("", "c", batch(200, 80)).map(|_| ()),
                        1 => db.upsert_with_pk_type("", "c", batch(0, 99)).map(|_| ()),
                        _ => db.delete("", "c", &[Pk::Int64(1)]).map(|_| ()),
                    };
                    match result {
                        Ok(()) => match action {
                            0 => {
                                model.lock().insert(200, 80);
                            }
                            1 => {
                                model.lock().insert(0, 99);
                            }
                            _ => {
                                model.lock().remove(&1);
                            }
                        },
                        Err(error) => {
                            assert!(
                                error.to_string().contains("flush retry is throttled"),
                                "{error}"
                            );
                            rejected += 1;
                        }
                    }
                }
                rejected
            })
        };
        let rejected = denier.join().unwrap();
        assert_eq!(rejected, 3);
        retry_tx.send(()).unwrap();
        flusher.join().unwrap();
        continue_tx.send(()).unwrap();
        writer.join().unwrap();
        let confirmed = model
            .lock()
            .iter()
            .map(|(pk, age)| (*pk, *age))
            .collect::<Vec<_>>();
        std::fs::write(path.join("oracle.json"),serde_json::to_vec(&serde_json::json!({"confirmed":confirmed,"inflight":[[100,75]],"rejected":rejected})).unwrap()).unwrap();
        let (complete_tx, complete_rx) = std::sync::mpsc::channel();
        let (_complete_resume_tx, complete_resume_rx) = std::sync::mpsc::channel();
        let (partial_tx, partial_rx) = std::sync::mpsc::channel();
        let (_partial_resume_tx, partial_resume_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let complete = {
            let db = Arc::clone(&db);
            let done = done_tx.clone();
            std::thread::spawn(move || {
                pause_append_after_io_once(complete_tx, complete_resume_rx);
                let result = db.insert("", "c", batch(100, 75));
                done.send(result).unwrap();
            })
        };
        let partial = {
            let db = Arc::clone(&db);
            std::thread::spawn(move || {
                inject_storage_full_after_bytes(usize::MAX);
                pause_append_after_io_once(partial_tx, partial_resume_rx);
                let result = db.insert("", "partial", batch(300, 90));
                done_tx.send(result).unwrap();
            })
        };
        complete_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        partial_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(matches!(
            done_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert!(!complete.is_finished() && !partial.is_finished());
        std::process::exit(89);
    }
    let root = tempfile::tempdir().unwrap();
    let log = std::fs::File::create(root.path().join("child.log")).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "controlled_concurrent_failure_rejection_and_inflight_recovery_match_model",
            "--nocapture",
        ])
        .env("MLR_CONTROLLED_CRASH_DIR", root.path())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(45);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!(
                "controlled child timed out: {}",
                std::fs::read_to_string(root.path().join("child.log")).unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        status.code(),
        Some(89),
        "{}",
        std::fs::read_to_string(root.path().join("child.log")).unwrap()
    );
    let oracle: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join("oracle.json")).unwrap()).unwrap();
    assert_eq!(oracle["rejected"], 3);
    let confirmed: BTreeMap<i64, i64> =
        serde_json::from_value::<Vec<(i64, i64)>>(oracle["confirmed"].clone())
            .unwrap()
            .into_iter()
            .collect();
    let inflight: BTreeMap<i64, i64> =
        serde_json::from_value::<Vec<(i64, i64)>>(oracle["inflight"].clone())
            .unwrap()
            .into_iter()
            .collect();
    let db = open(root.path());
    let recovered = rows(&db.collection("", "c").unwrap().read().read_snapshot());
    for (pk, age) in &confirmed {
        assert_eq!(recovered.get(pk), Some(age), "confirmed row {pk}");
    }
    let allowed = confirmed
        .keys()
        .chain(inflight.keys())
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        recovered.keys().all(|pk| allowed.contains(pk)),
        "unexpected recovered rows: {recovered:?}; oracle {oracle}"
    );
    for (pk, age) in &inflight {
        if let Some(value) = recovered.get(pk) {
            assert_eq!(value, age);
        }
    }
    assert!(!recovered.contains_key(&200));
    assert_eq!(
        rows(&db.collection("", "partial").unwrap().read().read_snapshot()),
        BTreeMap::from([(10, 1)])
    );
    db.close();
}

#[test]
fn close_and_drop_record_retained_wal_sync_without_changing_retirement_order() {
    let _case = CASE_LOCK.lock();
    use milvus_lite_core::storage::fs_commit::{FsAction, begin_fs_trace, finish_fs_trace};
    for operation in 0..4 {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        db.create_collection("", "c", schema()).unwrap();
        db.insert("", "c", batch(1, 60)).unwrap();
        db.delete("", "c", &[Pk::Int64(200)]).unwrap();
        let dir = root.path().join("collections/c");
        let data = dir.join("wal/wal_data_000001.arrow");
        let delta = dir.join("wal/wal_delta_000001.arrow");
        if operation >= 2 {
            std::fs::rename(&dir, root.path().join("moved")).unwrap();
        }
        if operation == 3 {
            // 形成持有冻结代 File 的被移走句柄，不能只验证活跃 writer。
            // 恢复原路径，在发布前冻结，再移走；prepare panic 保留冻结代。
            std::fs::rename(root.path().join("moved"), &dir).unwrap();
            milvus_lite_core::storage::runtime::inject_flush_prepare_panic_once();
            assert!(db.flush_collection("", "c").is_err());
            std::fs::rename(&dir, root.path().join("moved")).unwrap();
        }
        begin_fs_trace();
        if operation == 0 {
            milvus_lite_core::db::inject_close_finish_panic_once();
            db.close();
        } else {
            db.drop_collection("", "c").unwrap();
        }
        let events = finish_fs_trace();
        for path in [&data, &delta] {
            assert!(
                events
                    .iter()
                    .any(|event| event.action == FsAction::SyncFile && event.path == *path),
                "missing original WAL sync: {path:?}; {events:?}"
            );
        }
        assert!(
            !events
                .iter()
                .any(|event| event.action == FsAction::Rename
                    && event.path.ends_with("manifest.json"))
        );
        if operation >= 2 {
            assert!(!dir.exists());
        }
        db.close();
    }
}

#[test]
fn alias_rename_then_panic_preserves_the_real_file_change_and_can_retry() {
    let _case = CASE_LOCK.lock();
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
    };
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    for name in ["c", "live"] {
        db.create_collection("", name, schema()).unwrap();
    }
    let path = root.path().join("aliases.json");
    db.create_alias("", "c", "old").unwrap();
    db.create_alias("", "live", "keep").unwrap();
    inject_directory_fault_once(
        DirectoryOperation::Drop,
        DirectoryPoint::AliasesAfterRename,
        DirectoryFault::Panic,
    );
    let error = db.drop_collection("", "c").unwrap_err().to_string();
    assert!(error.contains("collection was removed") && error.contains("panicked"));
    assert!(!root.path().join("collections/c").exists());
    let aliases: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(aliases, serde_json::json!({"keep":"live"}));
    db.drop_collection("", "c").unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap()).unwrap(),
        aliases
    );
    db.close();
}

#[test]
fn drop_sync_and_recommit_panics_seal_state_before_unlocking() {
    let _case = CASE_LOCK.lock();
    use milvus_lite_core::{
        db::{StatePanicPoint, inject_state_panic_once},
        storage::{
            fs_commit::inject_manifest_dir_sync_failure_once,
            runtime::{RetainedWalSyncFault, inject_retained_wal_sync_panic_once},
        },
    };
    for case in 0..3 {
        let dropping = case != 0;
        let displaced = case == 2;
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        db.create_collection("", "c", schema()).unwrap();
        db.insert("", "c", batch(1, 1)).unwrap();
        let old = db.collection("", "c").unwrap().read().read_snapshot();
        if dropping {
            db.delete("", "c", &[Pk::Int64(200)]).unwrap();
            if displaced {
                std::fs::rename(root.path().join("collections/c"), root.path().join("moved"))
                    .unwrap();
            }
            inject_retained_wal_sync_panic_once(RetainedWalSyncFault::Delta);
        } else {
            inject_manifest_dir_sync_failure_once();
            assert!(db.flush_collection("", "c").is_err());
            inject_state_panic_once(StatePanicPoint::Recommit);
        }
        let error = if dropping {
            db.drop_collection("", "c")
        } else {
            db.flush_collection("", "c")
        }
        .unwrap_err()
        .to_string();
        assert!(error.contains("panic"), "{error}");
        assert_eq!(root.path().join("collections/c").exists(), !displaced);
        assert!(db.collection("", "c").is_err());
        assert!(db.insert("", "c", batch(2, 2)).is_err());
        assert_eq!(rows(&old), BTreeMap::from([(1, 1)]));
        if dropping {
            db.drop_collection("", "c").unwrap();
            assert!(!root.path().join("collections/c").exists());
        } else {
            db.close();
            let reopened = open(root.path());
            assert_eq!(
                rows(&reopened.collection("", "c").unwrap().read().read_snapshot()),
                BTreeMap::from([(1, 1)])
            );
            reopened.close();
        }
        db.close();
    }
}

#[test]
fn alias_and_same_name_collection_creates_never_both_succeed() {
    let _case = CASE_LOCK.lock();
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    for round in 0..20 {
        let name = format!("x{round}");
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let alias = {
            let (db, barrier, name) = (Arc::clone(&db), Arc::clone(&barrier), name.clone());
            std::thread::spawn(move || {
                barrier.wait();
                db.create_alias("", "c", &name)
            })
        };
        barrier.wait();
        let created = db.create_collection("", &name, schema());
        let alias = alias.join().unwrap();
        match (&created, &alias) {
            (Ok(()), Err(milvus_lite_core::EngineError::AliasConflictsWithCollection(n)))
            | (Err(milvus_lite_core::EngineError::CollectionAlreadyExists(n)), Ok(())) => {
                assert_eq!(n, &name)
            }
            _ => panic!("round {round}: {created:?} / {alias:?}"),
        }
        assert_eq!(db.describe_alias("", &name).is_ok(), alias.is_ok());
        assert_eq!(
            root.path().join("collections").join(&name).exists(),
            created.is_ok()
        );
    }
    db.close();
}

#[test]
fn alias_create_racing_target_drop_never_leaves_an_alias_to_the_dropped_target() {
    let _case = CASE_LOCK.lock();
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    for round in 0..20 {
        let target = format!("t{round}");
        db.create_collection("", &target, schema()).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let alias = {
            let (db, barrier, target) = (Arc::clone(&db), Arc::clone(&barrier), target.clone());
            std::thread::spawn(move || {
                barrier.wait();
                db.create_alias("", &target, "a")
            })
        };
        barrier.wait();
        db.drop_collection("", &target).unwrap();
        match alias.join().unwrap() {
            Ok(()) | Err(milvus_lite_core::EngineError::CollectionNotFound(_)) => {}
            other => panic!("round {round}: {other:?}"),
        }
        // 别名先建成则随目标一起删除，后到则报目标不存在；两种顺序都不留下指向已删目标的别名。
        assert!(
            db.list_aliases("", "").unwrap().1.is_empty(),
            "round {round}"
        );
    }
    db.close();
}

#[test]
fn reads_and_writes_through_an_alias_follow_concurrent_repointing() {
    let _case = CASE_LOCK.lock();
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c1", schema()).unwrap();
    db.create_collection("", "c2", schema()).unwrap();
    db.insert("", "c1", batch(1, 1)).unwrap();
    db.insert("", "c2", batch(2, 2)).unwrap();
    db.insert("", "c2", batch(3, 3)).unwrap();
    db.create_alias("", "c1", "a").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let (db, stop) = (Arc::clone(&db), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut seen = std::collections::BTreeSet::new();
                while !stop.load(Ordering::Acquire) {
                    let handle = db.collection("", "a").unwrap();
                    let collection = handle.read();
                    seen.insert((collection.name().to_owned(), collection.count()));
                }
                seen
            })
        })
        .collect();
    let writer = {
        let (db, stop) = (Arc::clone(&db), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut id = 100;
            while !stop.load(Ordering::Acquire) {
                // 每条写入只落到解析当时的一个目标；两边合计不丢。
                db.insert("", "a", batch(id, id)).unwrap();
                id += 1;
            }
            id - 100
        })
    };
    for round in 0..40 {
        db.alter_alias("", if round % 2 == 0 { "c2" } else { "c1" }, "a")
            .unwrap();
    }
    stop.store(true, Ordering::Release);
    for reader in readers {
        for (name, _) in reader.join().unwrap() {
            assert!(name == "c1" || name == "c2", "{name}");
        }
    }
    let written = writer.join().unwrap();
    let total: usize = ["c1", "c2"]
        .iter()
        .map(|name| db.collection("", name).unwrap().read().count())
        .sum();
    assert_eq!(total, 3 + usize::try_from(written).unwrap());
    db.close();
}
