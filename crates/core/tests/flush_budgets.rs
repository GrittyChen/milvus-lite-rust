//! 0027：flush 字节预算。场景对照参照实现 tests/engine/test_flush_budgets.py（Apache-2.0，1320d26）。
#![allow(clippy::unwrap_used)]
use std::path::{Path, PathBuf};
use std::sync::Arc;

use milvus_lite_core::flush_budget::FlushBudgets;
use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, FieldSchema, InputValues, InsertBatch, Pk,
};

const HUGE: u64 = 1 << 60;

fn open(root: &Path, memtable_bytes: u64, wal_bytes: u64) -> Arc<Db> {
    Db::with_flush_budgets(
        DataDir::open(root).unwrap(),
        Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
        FlushBudgets {
            memtable_bytes,
            wal_bytes,
        },
    )
    .unwrap()
}

fn schema(partition_key: bool) -> CollectionSchema {
    let mut fields = vec![
        FieldSchema {
            is_primary: true,
            ..FieldSchema::new("id", DataType::Int64)
        },
        FieldSchema {
            dim: Some(16),
            ..FieldSchema::new("v", DataType::FloatVector)
        },
    ];
    if partition_key {
        fields.push(FieldSchema {
            is_partition_key: true,
            ..FieldSchema::new("k", DataType::Int64)
        });
    }
    CollectionSchema {
        fields,
        ..Default::default()
    }
}

fn batch(ids: &[i64], partition_key: bool) -> InsertBatch {
    let mut columns = vec![
        ("id".into(), InputValues::Int(ids.to_vec())),
        (
            "v".into(),
            InputValues::FloatVector {
                dim: 16,
                values: vec![1.0; ids.len() * 16],
            },
        ),
    ];
    if partition_key {
        columns.push(("k".into(), InputValues::Int(ids.to_vec())));
    }
    InsertBatch {
        num_rows: ids.len(),
        columns,
    }
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn files(root: &Path, matches: impl Fn(&str) -> bool) -> usize {
    let mut all = Vec::new();
    collect(&root.join("collections/c"), &mut all);
    all.iter()
        .filter(|path| matches(path.file_name().unwrap().to_str().unwrap()))
        .count()
}

fn parquet_data_files(root: &Path) -> usize {
    files(root, |name| {
        name.starts_with("data_") && name.ends_with(".parquet")
    })
}

fn wal_files(root: &Path) -> usize {
    files(root, |name| {
        name.starts_with("wal_") && name.ends_with(".arrow")
    })
}

fn count(db: &Db) -> usize {
    db.collection("", "c").unwrap().read().count()
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// 保留未 checkpoint 的 WAL：运行中的目录副本等同于进程在此刻退出。
fn crash_copy(root: &Path) -> tempfile::TempDir {
    let copy = tempfile::tempdir().unwrap();
    copy_dir(root, copy.path());
    copy
}

#[test]
fn rows_alone_do_not_trigger_a_flush() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path(), HUGE, HUGE);
    db.create_collection("", "c", schema(false)).unwrap();
    let ids: Vec<i64> = (0..12_000).collect();
    db.insert("", "c", batch(&ids, false)).unwrap();
    assert_eq!(parquet_data_files(root.path()), 0);
    assert_eq!(count(&db), 12_000);
}

#[test]
fn repeated_rewrites_flush_by_retained_memory() {
    for partition_key in [false, true] {
        let root = tempfile::tempdir().unwrap();
        // 每批约 2 行 × 64 字节向量，加主键与系统列。
        let db = open(root.path(), 2_000, HUGE);
        db.create_collection("", "c", schema(partition_key))
            .unwrap();
        for _ in 0..60 {
            db.insert("", "c", batch(&[1, 2], partition_key)).unwrap();
        }
        assert!(
            parquet_data_files(root.path()) > 0,
            "partition_key={partition_key}"
        );
        assert_eq!(count(&db), 2);
    }
}

#[test]
fn repeated_deletes_flush_by_wal_bytes() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path(), HUGE, 4_000);
    db.create_collection("", "c", schema(false)).unwrap();
    db.insert("", "c", batch(&[1], false)).unwrap();
    db.flush_collection("", "c").unwrap();
    let before = files(root.path(), |name| name.starts_with("delta_"));
    for _ in 0..200 {
        db.delete("", "c", &[Pk::Int64(1)]).unwrap();
    }
    assert!(files(root.path(), |name| name.starts_with("delta_")) > before);
    assert_eq!(count(&db), 0);
}

#[test]
fn large_single_batch_flushes_after_it_is_applied() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path(), 500, HUGE);
    db.create_collection("", "c", schema(false)).unwrap();
    db.insert("", "c", batch(&[1, 2, 3, 4, 5, 6, 7, 8], false))
        .unwrap();
    assert_eq!(parquet_data_files(root.path()), 1);
    assert_eq!(count(&db), 8);
}

#[test]
fn recovered_wal_over_budget_is_checkpointed_before_serving() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path(), HUGE, HUGE);
    db.create_collection("", "c", schema(false)).unwrap();
    for _ in 0..20 {
        db.insert("", "c", batch(&[1, 2], false)).unwrap();
    }
    let copy = crash_copy(root.path());
    assert_eq!(parquet_data_files(copy.path()), 0);
    assert!(wal_files(copy.path()) > 0);

    let reopened = open(copy.path(), HUGE, 2_000);
    assert_eq!(count(&reopened), 2);
    assert_eq!(parquet_data_files(copy.path()), 1);
    assert_eq!(wal_files(copy.path()), 0);
}

#[test]
fn recovered_memtable_over_budget_is_checkpointed_before_serving() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path(), HUGE, HUGE);
    db.create_collection("", "c", schema(false)).unwrap();
    for _ in 0..20 {
        db.insert("", "c", batch(&[1, 2], false)).unwrap();
    }
    let copy = crash_copy(root.path());
    let reopened = open(copy.path(), 2_000, HUGE);
    assert_eq!(count(&reopened), 2);
    assert_eq!(parquet_data_files(copy.path()), 1);
}

#[test]
fn delete_only_wal_over_budget_is_checkpointed_on_open() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path(), HUGE, HUGE);
    db.create_collection("", "c", schema(false)).unwrap();
    db.insert("", "c", batch(&[1], false)).unwrap();
    db.flush_collection("", "c").unwrap();
    for _ in 0..50 {
        db.delete("", "c", &[Pk::Int64(1)]).unwrap();
    }
    let copy = crash_copy(root.path());
    assert!(wal_files(copy.path()) > 0);

    let reopened = open(copy.path(), HUGE, 1_000);
    assert_eq!(count(&reopened), 0);
    assert_eq!(wal_files(copy.path()), 0);
}

#[test]
fn checkpoint_resets_recovered_wal_accounting() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path(), HUGE, HUGE);
    db.create_collection("", "c", schema(false)).unwrap();
    for _ in 0..20 {
        db.insert("", "c", batch(&[1, 2], false)).unwrap();
    }
    let copy = crash_copy(root.path());
    let reopened = open(copy.path(), HUGE, 2_000);
    assert_eq!(count(&reopened), 2);
    assert_eq!(parquet_data_files(copy.path()), 1);
    // 预算已清零：一批小写入不会再次触发 flush。
    reopened.insert("", "c", batch(&[3], false)).unwrap();
    assert_eq!(parquet_data_files(copy.path()), 1);
    assert_eq!(count(&reopened), 3);
}

#[cfg(feature = "fault-injection")]
#[test]
fn failed_auto_flush_keeps_the_write_and_is_retried_by_the_next_write() {
    use milvus_lite_core::storage::fs_commit::inject_manifest_dir_sync_failure_once;

    let root = tempfile::tempdir().unwrap();
    let db = open(root.path(), 500, HUGE);
    db.create_collection("", "c", schema(false)).unwrap();
    inject_manifest_dir_sync_failure_once();
    // 写入已落 WAL 并可见；之后的提交故障不撤销它，由下一次写入重试。
    db.insert("", "c", batch(&[1, 2, 3, 4, 5, 6, 7, 8], false))
        .unwrap();
    assert_eq!(count(&db), 8);
    // 重试有节流：紧接着的写入被拒绝，节流窗口过后由下一次写入先完成提交。
    assert!(db.insert("", "c", batch(&[9], false)).is_err());
    assert_eq!(count(&db), 8);
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    db.insert("", "c", batch(&[9], false)).unwrap();
    assert_eq!(count(&db), 9);
    assert!(parquet_data_files(root.path()) >= 1);
    db.close();
    drop(db);
    let reopened = open(root.path(), HUGE, HUGE);
    assert_eq!(count(&reopened), 9);
}

/// 提交后、WAL 清理前崩溃：WAL 里的行都已在 manifest 中，活跃表为空，仍须提交一代才能回收。
#[test]
fn committed_wal_over_budget_is_reclaimed_with_an_empty_generation() {
    for partition_key in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path(), HUGE, HUGE);
        db.create_collection("", "c", schema(partition_key))
            .unwrap();
        for i in 0..20 {
            db.insert("", "c", batch(&[i, i + 100], partition_key))
                .unwrap();
        }
        let crashed = crash_copy(root.path());
        db.flush_collection("", "c").unwrap();
        let manifest = |root: &Path| -> serde_json::Value {
            milvus_lite_core::json_text::value(
                &std::fs::read(root.join("collections/c/manifest.json")).unwrap(),
            )
            .unwrap()
        };
        let before = manifest(root.path());
        db.close();
        drop(db);
        let wal = root.path().join("collections/c/wal");
        std::fs::create_dir_all(&wal).unwrap();
        copy_dir(&crashed.path().join("collections/c/wal"), &wal);
        assert!(wal_files(root.path()) > 0);

        let reopened = open(root.path(), HUGE, 1_000);
        assert_eq!(count(&reopened), 40, "partition_key={partition_key}");
        assert_eq!(wal_files(root.path()), 0, "partition_key={partition_key}");
        assert_eq!(manifest(root.path())["current_seq"], before["current_seq"]);
        reopened
            .insert("", "c", batch(&[1000, 1001], partition_key))
            .unwrap();
        assert_eq!(count(&reopened), 42);
        reopened.close();
        drop(reopened);
        let again = open(root.path(), HUGE, HUGE);
        assert_eq!(count(&again), 42);
        again.close();
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn failed_flush_on_open_is_not_cached_and_keeps_the_rows() {
    use milvus_lite_core::storage::{flush, fs_commit};
    for fault in ["before-replace", "dir-sync", "wal-unlink"] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path(), HUGE, HUGE);
        db.create_collection("", "c", schema(false)).unwrap();
        for i in 0..20 {
            db.insert("", "c", batch(&[i], false)).unwrap();
        }
        let copy = crash_copy(root.path());
        let reopened = open(copy.path(), HUGE, 500);
        match fault {
            "before-replace" => {
                flush::inject_flush_worker_panic_once(fs_commit::CommitPhase::BeforeReplace);
            }
            "dir-sync" => fs_commit::inject_manifest_dir_sync_failure_once(),
            _ => flush::inject_wal_unlink_failure_once(),
        }
        assert!(reopened.collection("", "c").is_err(), "{fault}");
        // 失败不缓存：下一次访问重新打开并完成修复。
        assert_eq!(count(&reopened), 20, "{fault}");
        reopened.close();
        drop(reopened);
        let again = open(copy.path(), HUGE, HUGE);
        assert_eq!(count(&again), 20, "{fault}");
        again.close();
    }
}

/// 打开时的 flush 失败后句柄被丢弃。它不能留在维护队列里：后台合并会用旧 manifest 覆盖重试后新实例的提交。
#[cfg(feature = "fault-injection")]
#[test]
fn failed_flush_on_open_does_not_leave_the_discarded_handle_in_maintenance() {
    use milvus_lite_core::storage::flush;
    use std::sync::mpsc;
    use std::time::Duration;

    let root = tempfile::tempdir().unwrap();
    let db = open(root.path(), HUGE, HUGE);
    db.create_collection("", "c", schema(false)).unwrap();
    // 3 个 segment，打开时的 flush 产生第 4 个，达到合并门槛。
    for i in 0..3 {
        db.insert("", "c", batch(&[i], false)).unwrap();
        db.flush_collection("", "c").unwrap();
    }
    db.insert("", "c", batch(&[100], false)).unwrap();
    db.insert("", "c", batch(&[101], false)).unwrap();
    let copy = crash_copy(root.path());
    db.close();
    drop(db);

    let db = open(copy.path(), HUGE, 200);
    let (entered, arrived) = mpsc::channel();
    let (resume, continued) = mpsc::channel();
    db.pause_compaction_once(
        milvus_lite_core::CompactionPausePoint::BeforePrepare,
        entered,
        continued,
    );
    flush::inject_wal_unlink_failure_once();
    assert!(db.collection("", "c").is_err());
    assert!(
        arrived.recv_timeout(Duration::from_millis(800)).is_err(),
        "a discarded handle reached compaction"
    );

    assert_eq!(count(&db), 5);
    db.insert("", "c", batch(&[200], false)).unwrap();
    db.flush_collection("", "c").unwrap();
    assert_eq!(count(&db), 6);
    // 注册后的实例按正常流程进入合并；放行后关闭才能排空维护线程。
    resume.send(()).unwrap();
    db.close();
    drop(db);
    let reopened = open(copy.path(), HUGE, HUGE);
    assert_eq!(count(&reopened), 6);
    reopened.close();
}
