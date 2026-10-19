//! 持久化引擎的显式分区状态转换。
#![allow(clippy::unwrap_used)]
use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, FieldSchema, InputValues, InsertBatch, Pk,
};
use std::{path::Path, sync::Arc};
fn open(path: &Path) -> Arc<Db> {
    Db::new(
        DataDir::open(path).unwrap(),
        Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
    )
    .unwrap()
}
fn schema() -> CollectionSchema {
    let mut id = FieldSchema::new("id", DataType::Int64);
    id.is_primary = true;
    let mut v = FieldSchema::new("v", DataType::FloatVector);
    v.dim = Some(2);
    CollectionSchema {
        fields: vec![id, v],
        enable_dynamic_field: false,
        properties: vec![],
    }
}
fn batch(id: i64, value: f32) -> InsertBatch {
    InsertBatch {
        num_rows: 1,
        columns: vec![
            ("id".into(), InputValues::Int(vec![id])),
            (
                "v".into(),
                InputValues::FloatVector {
                    dim: 2,
                    values: vec![value, 0.],
                },
            ),
        ],
    }
}
fn put(db: &Db, part: &str, id: i64, value: f32) {
    db.insert_partition_with_pk_type("", "c", batch(id, value), part)
        .unwrap();
}
fn count(db: &Db, part: &str) -> usize {
    db.collection("", "c")
        .unwrap()
        .read()
        .count_partition(part)
        .unwrap()
}
#[test]
fn drop_preserves_global_deletes_and_old_snapshots_and_allows_safe_recreation() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    for part in ["p1", "p2"] {
        db.create_partition("", "c", part).unwrap();
    }
    put(&db, "p2", 1, 1.);
    db.flush_collection("", "c").unwrap();
    let old = db
        .collection("", "c")
        .unwrap()
        .read()
        .read_named_snapshot(&["p2".into()])
        .unwrap();
    db.delete_expr("", "c", "id in [1]", "p1").unwrap();
    db.flush_collection("", "c").unwrap();
    assert_eq!(count(&db, "p2"), 0);
    db.drop_partition("", "c", "p1").unwrap();
    assert_eq!(old.count_filtered("").unwrap(), 1);
    assert_eq!(count(&db, "p2"), 0);
    assert!(!root.path().join("collections/c/partitions/p1").exists());
    db.create_partition("", "c", "p1").unwrap();
    put(&db, "p1", 2, 2.);
    db.close();
    drop(db);
    let db = open(root.path());
    assert_eq!(count(&db, "p2"), 0);
    assert_eq!(count(&db, "p1"), 1);
    assert_eq!(db.collection("", "c").unwrap().read().count(), 1);
    db.close();
}
#[test]
fn drop_new_winner_restores_other_partition_and_get_keeps_order_and_duplicates() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    for part in ["z", "a"] {
        db.create_partition("", "c", part).unwrap();
    }
    put(&db, "z", 1, 1.);
    db.flush_collection("", "c").unwrap();
    put(&db, "a", 1, 2.);
    db.flush_collection("", "c").unwrap();
    let handle = db.collection("", "c").unwrap();
    let selected = handle
        .read()
        .prepare_partition_get(
            &[Pk::Int64(1), Pk::Int64(99), Pk::Int64(1)],
            &["v".into()],
            &["z".into()],
        )
        .unwrap()
        .read();
    assert_eq!(selected.rows.pks, [Pk::Int64(1), Pk::Int64(1)]);
    assert_eq!(
        selected.rows.fields[0].1,
        milvus_lite_core::ColumnData::FloatVector {
            dim: 2,
            values: vec![1., 0., 1., 0.]
        }
    );
    db.drop_partition("", "c", "a").unwrap();
    assert_eq!(
        handle
            .read()
            .get(&[Pk::Int64(1)], &["v".into()])
            .unwrap()
            .rows
            .fields[0]
            .1,
        milvus_lite_core::ColumnData::FloatVector {
            dim: 2,
            values: vec![1., 0.]
        }
    );
    db.close();
    drop(handle);
    drop(db);
    let db = open(root.path());
    assert_eq!(count(&db, "z"), 1);
    db.close();
}
#[test]
fn partition_compaction_retains_local_versions_and_delta_gc_uses_every_partition() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    for part in ["p1", "p2"] {
        db.create_partition("", "c", part).unwrap();
    }
    put(&db, "p1", 1, 1.);
    db.flush_collection("", "c").unwrap();
    db.delete_expr("", "c", "id in [1]", "p2").unwrap();
    db.flush_collection("", "c").unwrap();
    for id in 2..10 {
        put(&db, "p2", id, id as f32);
        db.flush_collection("", "c").unwrap();
    }
    // 另一分区的新值不能使 p1 仍可见的旧版本被合并丢弃。
    put(&db, "p1", 20, 20.);
    db.flush_collection("", "c").unwrap();
    put(&db, "p2", 20, 21.);
    db.flush_collection("", "c").unwrap();
    for id in 30..38 {
        put(&db, "p1", id, id as f32);
        db.flush_collection("", "c").unwrap();
    }
    db.close();
    drop(db); // 正常关闭排空真实维护队列。
    let db = open(root.path());
    assert_eq!(count(&db, "p1"), 9);
    assert_eq!(count(&db, "p2"), 9);
    let manifest = milvus_lite_core::storage::manifest::Manifest::read(
        &root.path().join("collections/c/manifest.json"),
    )
    .unwrap();
    for part in ["p1", "p2"] {
        assert!(
            manifest.partitions[part].data_files.len() < 10,
            "partition not compacted: {part}"
        );
    }
    assert_eq!(
        db.collection("", "c")
            .unwrap()
            .read()
            .get(&[Pk::Int64(1)], &[])
            .unwrap()
            .rows
            .pks
            .len(),
        0
    );
    db.close();
}
#[cfg(feature = "fault-injection")]
#[test]
fn drop_after_replace_sync_failure_publishes_but_defers_cleanup_until_recommit() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.create_partition("", "c", "p").unwrap();
    put(&db, "p", 1, 1.);
    db.flush_collection("", "c").unwrap();
    milvus_lite_core::storage::fs_commit::inject_manifest_dir_sync_failure_once();
    assert!(db.drop_partition("", "c", "p").is_err());
    assert!(!db.has_partition("", "c", "p").unwrap());
    assert!(root.path().join("collections/c/partitions/p/data").exists());
    db.flush_collection("", "c").unwrap();
    assert!(!root.path().join("collections/c/partitions/p").exists());
    db.create_partition("", "c", "p").unwrap();
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn partition_namespace_permit_blocks_mutations_through_every_publication_phase() {
    use milvus_lite_core::PartitionPausePoint::*;
    use std::{sync::mpsc, time::Duration};
    for drop in [false, true] {
        for point in [Checked, Flushed, BeforeReplace, Published] {
            let root = tempfile::tempdir().unwrap();
            let db = open(root.path());
            db.create_collection("", "c", schema()).unwrap();
            db.create_partition("", "c", "p").unwrap();
            put(&db, "_default", 1, 1.);
            db.close();
            std::mem::drop(db);
            // 未懒打开的 collection 也必须复用独占许可，不能递归取得共享许可。
            let db = open(root.path());
            let (entered_tx, entered_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            db.pause_partition_once(point, entered_tx, resume_rx);
            let worker_db = db.clone();
            let operation = std::thread::spawn(move || {
                if drop {
                    worker_db.drop_partition("", "c", "p")
                } else {
                    worker_db.create_partition("", "c", "new")
                }
            });
            entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let mut writers = Vec::new();
            let (done_tx, done_rx) = mpsc::channel();
            for delete in [false, true] {
                let db = db.clone();
                let done = done_tx.clone();
                let (attempt_tx, attempt_rx) = mpsc::channel();
                writers.push(std::thread::spawn(move || {
                    attempt_tx.send(()).unwrap();
                    if delete {
                        db.delete("", "c", &[Pk::Int64(1)]).unwrap();
                    } else {
                        put(&db, "_default", 2, 2.);
                    }
                    done.send(()).unwrap();
                }));
                attempt_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            assert!(matches!(
                done_rx.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            resume_tx.send(()).unwrap();
            operation.join().unwrap().unwrap();
            for writer in writers {
                writer.join().unwrap();
            }
            assert_eq!(db.collection("", "c").unwrap().read().count(), 1);
            db.close();
        }
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn partition_delta_copy_failures_leave_old_manifest_readable_and_retry_without_resurrection() {
    use milvus_lite_core::{PartitionCopyFault::*, inject_partition_copy_fault_once};
    for point in [Write, FileSync, DirectorySync] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        db.create_collection("", "c", schema()).unwrap();
        db.create_partition("", "c", "p").unwrap();
        put(&db, "_default", 1, 1.);
        db.flush_collection("", "c").unwrap();
        db.delete_expr("", "c", "id in [1]", "p").unwrap();
        db.flush_collection("", "c").unwrap();
        let path = root.path().join("collections/c/manifest.json");
        let before = std::fs::read(&path).unwrap();
        inject_partition_copy_fault_once(point);
        assert!(db.drop_partition("", "c", "p").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(db.has_partition("", "c", "p").unwrap());
        assert_eq!(count(&db, "_default"), 0);
        db.drop_partition("", "c", "p").unwrap();
        db.close();
        std::mem::drop(db);
        let db = open(root.path());
        assert_eq!(count(&db, "_default"), 0);
        db.close();
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn partition_drop_process_child() {
    let Ok(root) = std::env::var("MLR_PARTITION_CRASH_ROOT") else {
        return;
    };
    let phase = match std::env::var("MLR_PARTITION_CRASH_PHASE").unwrap().as_str() {
        "before" => milvus_lite_core::storage::fs_commit::CommitPhase::BeforeReplace,
        "after" => milvus_lite_core::storage::fs_commit::CommitPhase::AfterReplace,
        _ => panic!("invalid crash phase"),
    };
    let db = open(Path::new(&root));
    milvus_lite_core::storage::fs_commit::inject_manifest_process_exit_once(phase);
    db.drop_partition("", "c", "p").unwrap();
    panic!("crash point not reached");
}

#[cfg(feature = "fault-injection")]
#[test]
fn partition_drop_crash_reopens_exactly_old_or_new_manifest_and_retains_deletion() {
    for phase in ["before", "after"] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        db.create_collection("", "c", schema()).unwrap();
        db.create_partition("", "c", "p").unwrap();
        put(&db, "_default", 1, 1.);
        db.flush_collection("", "c").unwrap();
        db.delete_expr("", "c", "id in [1]", "p").unwrap();
        db.flush_collection("", "c").unwrap();
        db.close();
        std::mem::drop(db);
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "partition_drop_process_child", "--nocapture"])
            .env("MLR_PARTITION_CRASH_ROOT", root.path())
            .env("MLR_PARTITION_CRASH_PHASE", phase)
            .output()
            .unwrap();
        assert_eq!(
            result.status.code(),
            Some(86),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let db = open(root.path());
        assert_eq!(db.has_partition("", "c", "p").unwrap(), phase == "before");
        assert_eq!(count(&db, "_default"), 0);
        db.close();
    }
}

#[test]
fn index_change_preflights_all_partition_sidecars_before_any_deletion() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.create_index(
        "",
        "c",
        "v",
        milvus_lite_core::IndexRequest {
            index_type: "HNSW".into(),
            metric_type: Some("L2".into()),
            ..Default::default()
        },
    )
    .unwrap();
    for (part, id) in [("p1", 1), ("p2", 2)] {
        db.create_partition("", "c", part).unwrap();
        put(&db, part, id, id as f32);
        db.flush_collection("", "c").unwrap();
    }
    db.collection("", "c").unwrap().write().release();
    let collection = root.path().join("collections/c");
    let manifest =
        milvus_lite_core::storage::manifest::Manifest::read(&collection.join("manifest.json"))
            .unwrap();
    let mut sidecars = Vec::new();
    for part in ["p1", "p2"] {
        let stem = Path::new(&manifest.partitions[part].data_files[0])
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap();
        let dir = collection.join("partitions").join(part).join("indexes");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{stem}.v.hnsw.idx"));
        std::fs::write(&path, b"reference-sidecar").unwrap();
        sidecars.push(path);
    }
    std::fs::remove_file(&sidecars[1]).unwrap();
    std::fs::create_dir(&sidecars[1]).unwrap();
    assert!(
        db.drop_index("", "c", "v", "")
            .unwrap_err()
            .to_string()
            .contains("not a regular file")
    );
    assert_eq!(std::fs::read(&sidecars[0]).unwrap(), b"reference-sidecar");
    assert!(
        !collection
            .join("milvus-lite-rust.index-intent.json")
            .exists()
    );
    assert!(
        milvus_lite_core::storage::manifest::Manifest::read(&collection.join("manifest.json"))
            .unwrap()
            .index_specs
            .contains_key("v")
    );
    std::fs::remove_dir(&sidecars[1]).unwrap();
    std::fs::write(&sidecars[1], b"reference-sidecar").unwrap();
    db.drop_index("", "c", "v", "").unwrap();
    assert!(sidecars.iter().all(|path| !path.exists()));
    assert!(
        milvus_lite_core::storage::manifest::Manifest::read(&collection.join("manifest.json"))
            .unwrap()
            .index_specs
            .is_empty()
    );
    assert!(
        !collection
            .join("milvus-lite-rust.index-intent.json")
            .exists()
    );
    assert_eq!(db.collection("", "c").unwrap().read().count(), 2);
    db.close();
}

#[test]
fn partition_cleanup_preserves_unregistered_directory_files_not_owned_by_storage() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.create_partition("", "c", "p").unwrap();
    let foreign = root
        .path()
        .join("collections/c/partitions/unregistered/data/canary");
    std::fs::create_dir_all(foreign.parent().unwrap()).unwrap();
    std::fs::write(&foreign, b"unowned contents").unwrap();
    assert!(db.create_partition("", "c", "unregistered").is_err());
    db.drop_partition("", "c", "p").unwrap();
    assert_eq!(std::fs::read(&foreign).unwrap(), b"unowned contents");
    db.close();
    std::mem::drop(db);
    let db = open(root.path());
    assert_eq!(db.collection("", "c").unwrap().read().count(), 0);
    assert_eq!(std::fs::read(&foreign).unwrap(), b"unowned contents");
    db.close();
}

#[test]
fn drop_index_cleans_unpinned_sidecars_in_a_dropped_but_data_pinned_partition() {
    use milvus_lite_core::storage::{manifest::Manifest, snapshot::SnapshotMetadata};
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.create_index(
        "",
        "c",
        "v",
        milvus_lite_core::IndexRequest {
            index_type: "HNSW".into(),
            metric_type: Some("L2".into()),
            ..Default::default()
        },
    )
    .unwrap();
    db.create_partition("", "c", "p").unwrap();
    put(&db, "p", 1, 1.);
    db.flush_collection("", "c").unwrap();
    db.close();
    std::mem::drop(db);
    let collection = root.path().join("collections/c");
    let manifest = Manifest::read(&collection.join("manifest.json")).unwrap();
    for dir in ["metadata", "schemas/s", "manifests/s"] {
        std::fs::create_dir_all(collection.join("snapshots").join(dir)).unwrap();
    }
    std::fs::copy(
        collection.join("schema.json"),
        collection.join("snapshots/schemas/s/schema.json"),
    )
    .unwrap();
    std::fs::copy(
        collection.join("manifest.json"),
        collection.join("snapshots/manifests/s/manifest.json"),
    )
    .unwrap();
    let snapshot = SnapshotMetadata {
        snapshot_format_version: 1,
        id: "s".into(),
        name: "s".into(),
        description: String::new(),
        collection_name: "c".into(),
        created_at: "2026-10-19T00:00:00".into(),
        created_seq: manifest.current_seq,
        manifest_version: manifest.version,
        schema_file: "snapshots/schemas/s/schema.json".into(),
        manifest_file: "snapshots/manifests/s/manifest.json".into(),
        data_files: manifest
            .partitions
            .iter()
            .map(|(p, f)| (p.clone(), f.data_files.clone()))
            .collect(),
        delta_files: BTreeMap::new(),
        index_files: BTreeMap::new(),
    };
    std::fs::write(
        collection.join("snapshots/metadata/s.json"),
        serde_json::to_vec(&snapshot).unwrap(),
    )
    .unwrap();
    let stem = Path::new(&manifest.partitions["p"].data_files[0])
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap();
    let index_dir = collection.join("partitions/p/indexes");
    std::fs::create_dir_all(&index_dir).unwrap();
    let sidecar = index_dir.join(format!("{stem}.v.hnsw.idx"));
    std::fs::write(&sidecar, b"built after snapshot").unwrap();
    let data = collection
        .join("partitions/p")
        .join(&manifest.partitions["p"].data_files[0]);
    let original = std::fs::read(&data).unwrap();
    let db = open(root.path());
    db.drop_partition("", "c", "p").unwrap();
    let handle = db.collection("", "c").unwrap();
    handle.write().release();
    db.drop_index("", "c", "v", "").unwrap();
    assert!(!sidecar.exists());
    assert_eq!(std::fs::read(&data).unwrap(), original);
    db.create_partition("", "c", "p").unwrap();
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn partition_disk_commit_keeps_old_reads_available_until_atomic_publication() {
    use std::{sync::mpsc, time::Duration};
    for removing in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        db.create_collection("", "c", schema()).unwrap();
        if removing {
            db.create_partition("", "c", "p").unwrap();
        }
        put(&db, if removing { "p" } else { "_default" }, 1, 1.);
        db.flush_collection("", "c").unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let worker_db = db.clone();
        let worker = std::thread::spawn(move || {
            let _task = worker_db.enter().unwrap();
            milvus_lite_core::storage::fs_commit::pause_manifest_after_replace_once(
                entered_tx, resume_rx,
            );
            if removing {
                worker_db.drop_partition("", "c", "p")
            } else {
                worker_db.create_partition("", "c", "p")
            }
        });
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let reader_db = db.clone();
        let (read_tx, read_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let _task = reader_db.enter().unwrap();
            let has = reader_db.has_partition("", "c", "p").unwrap();
            let snapshot = reader_db
                .collection("", "c")
                .unwrap()
                .read()
                .read_snapshot();
            read_tx.send((has, snapshot)).unwrap();
        });
        // rename 已发生而目录同步尚未继续：读取必须完成，仍使用旧的同代内存状态。
        let read = read_rx.recv_timeout(Duration::from_secs(2));
        resume_tx.send(()).unwrap();
        reader.join().unwrap();
        worker.join().unwrap().unwrap();
        let (has, old) = read.unwrap();
        assert_eq!(has, removing);
        assert_eq!(old.count_filtered("").unwrap(), 1);
        assert_eq!(db.has_partition("", "c", "p").unwrap(), !removing);
        assert_eq!(
            db.collection("", "c").unwrap().read().count(),
            usize::from(!removing)
        );
        assert_eq!(old.count_filtered("").unwrap(), 1);
        db.close();
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn retained_partition_wal_process_child() {
    use milvus_lite_core::storage::{
        flush,
        fs_commit::{self, CommitPhase},
    };
    let Ok(root) = std::env::var("MLR_RETAINED_PARTITION_ROOT") else {
        return;
    };
    let mode = std::env::var("MLR_RETAINED_PARTITION_MODE").unwrap();
    let db = open(Path::new(&root));
    if mode.starts_with("seed-") {
        db.create_collection("", "c", schema()).unwrap();
        db.create_partition("", "c", "p").unwrap();
        put(&db, "_default", 9, 9.);
        put(&db, "_default", 1, 1.);
        db.flush_collection("", "c").unwrap();
        if mode != "seed-delta" {
            put(&db, "p", 2, 2.);
        }
        if mode != "seed-data" {
            db.delete_expr("", "c", "id in [1]", "p").unwrap();
        }
        fs_commit::inject_manifest_process_exit_once(CommitPhase::AfterReplace);
        db.flush_collection("", "c").unwrap();
        panic!("crash point missed");
    }
    assert!(db.has_partition("", "c", "p").unwrap());
    assert!(
        std::fs::read_dir(Path::new(&root).join("collections/c/wal"))
            .unwrap()
            .next()
            .is_some()
    );
    if mode == "drop-active" {
        put(&db, "_default", 10, 10.);
    }
    if mode == "drop-unlink-error" || mode == "drop-unlink-panic" {
        if mode == "drop-unlink-error" {
            flush::inject_wal_unlink_failure_once();
        } else {
            flush::inject_wal_unlink_panic_once();
        }
        assert!(db.drop_partition("", "c", "p").is_err());
        assert!(db.has_partition("", "c", "p").unwrap());
    }
    if mode == "drop-prepare-panic" {
        fs_commit::inject_manifest_panic_once(CommitPhase::BeforeReplace);
        assert!(db.drop_partition("", "c", "p").is_err());
        assert!(db.has_partition("", "c", "p").unwrap());
    }
    if mode == "drop-sync-error" {
        fs_commit::inject_manifest_dir_sync_failure_once();
        assert!(db.drop_partition("", "c", "p").is_err());
        assert!(db.has_partition("", "c", "p").unwrap());
    }
    db.drop_partition("", "c", "p").unwrap();
    assert!(!db.has_partition("", "c", "p").unwrap());
    assert!(
        std::fs::read_dir(Path::new(&root).join("collections/c/wal"))
            .unwrap()
            .next()
            .is_none()
    );
    // 正常关闭会补做清理；成功返回后的立即退出才验证 Drop 本身已排空 WAL。
    std::process::exit(0);
}

#[cfg(feature = "fault-injection")]
#[test]
fn drop_drains_recovered_committed_wal_even_when_no_rows_need_replay() {
    for seed in ["data", "delta", "mixed"] {
        for action in [
            "drop",
            "drop-active",
            "drop-unlink-error",
            "drop-unlink-panic",
            "drop-sync-error",
            "drop-prepare-panic",
        ] {
            let root = tempfile::tempdir().unwrap();
            for (mode, exit) in [(format!("seed-{seed}"), 86), (action.into(), 0)] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "retained_partition_wal_process_child",
                        "--nocapture",
                    ])
                    .env("MLR_RETAINED_PARTITION_ROOT", root.path())
                    .env("MLR_RETAINED_PARTITION_MODE", &mode)
                    .output()
                    .unwrap();
                assert_eq!(
                    output.status.code(),
                    Some(exit),
                    "{seed}/{mode}: {} {}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            let db = open(root.path());
            assert!(!db.has_partition("", "c", "p").unwrap());
            assert_eq!(
                count(&db, "_default"),
                (if seed == "data" { 2 } else { 1 }) + usize::from(action == "drop-active")
            );
            let rows = db
                .collection("", "c")
                .unwrap()
                .read()
                .get(&[Pk::Int64(9)], &["id".into()])
                .unwrap();
            assert_eq!(rows.rows.pks, [Pk::Int64(9)]);
            db.close();
        }
    }
}

#[cfg(unix)]
#[test]
fn create_checks_missing_partition_aliases_before_manifest_and_reopens_cleanly() {
    for (registered, requested) in [("_default", "_DEFAULT"), ("p", "P"), ("é", "e\u{301}")] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        db.create_collection("", "c", schema()).unwrap();
        if registered != "_default" {
            db.create_partition("", "c", registered).unwrap();
        }
        let parts = root.path().join("collections/c/partitions");
        let old = parts.join(registered);
        if old.exists() {
            std::fs::remove_dir(&old).unwrap();
        }
        let probe = root.path().join("probe");
        std::fs::create_dir(&probe).unwrap();
        std::fs::create_dir(probe.join(registered)).unwrap();
        let alias = probe.join(requested).exists();
        let path = root.path().join("collections/c/manifest.json");
        let before = std::fs::read(&path).ok();
        let result = db.create_partition("", "c", requested);
        if alias {
            assert!(result.unwrap_err().to_string().contains("same directory"));
            assert_eq!(std::fs::read(&path).ok(), before);
            assert!(!db.has_partition("", "c", requested).unwrap());
            assert!(!parts.join(requested).exists());
        } else {
            result.unwrap();
        }
        db.create_partition("", "c", "safe").unwrap();
        put(&db, registered, 9, 9.);
        if !alias {
            put(&db, requested, 10, 10.);
        }
        db.flush_collection("", "c").unwrap();
        db.close();
        drop(db);
        let db = open(root.path());
        assert_eq!(count(&db, registered), 1);
        if !alias {
            assert_eq!(count(&db, requested), 1);
        }
        db.close();
    }
}
