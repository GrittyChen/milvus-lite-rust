//! 存量索引定义的 load 判定：3.x 自身建不成的向量索引定义，在有行的已提交 segment 存在时
//! load 失败并保持 released（0002「结论」）。3.x 的 `create_index` 不校验向量字段的
//! index_type，且 FAISS 的非法 metric 只在需要构建时失败。这里改写 manifest 并控制侧车，
//! 验证不读内容、逐来源判断、状态与恢复；正式的两侧生产 / 消费另有 storage_interop。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

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

fn open(path: &Path) -> Arc<Db> {
    Db::new(DataDir::open(path).unwrap(), compute()).unwrap()
}

/// 建 collection 与 FLAT 索引，写入 `ids` 并按 `flush` 决定是否提交为 segment，最后删除 `deleted`。
fn create(db: &Db, name: &str, ids: &[i64], flush: bool, deleted: &[i64]) {
    db.create_collection("", name, schema()).unwrap();
    db.create_index(
        "",
        name,
        "v",
        IndexRequest {
            index_type: "FLAT".to_owned(),
            metric_type: Some("L2".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    if !ids.is_empty() {
        db.insert("", name, batch(ids)).unwrap();
    }
    if flush {
        db.flush_collection("", name).unwrap();
    }
    if !deleted.is_empty() {
        let pks: Vec<Pk> = deleted.iter().map(|&id| Pk::Int64(id)).collect();
        db.delete("", name, &pks).unwrap();
        db.flush_collection("", name).unwrap();
    }
}

fn manifest_path(path: &Path, name: &str) -> std::path::PathBuf {
    path.join("collections").join(name).join("manifest.json")
}

/// 模拟 3.x 写下的定义：只改向量字段的 index_type。
fn rewrite_index_type(path: &Path, name: &str, index_type: &str) {
    let file = manifest_path(path, name);
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    manifest["index_specs"]["v"]["index_type"] = index_type.into();
    std::fs::write(&file, serde_json::to_vec(&manifest).unwrap()).unwrap();
}

fn data_files(path: &Path, name: &str) -> usize {
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(manifest_path(path, name)).unwrap()).unwrap();
    manifest["partitions"]["_default"]["data_files"]
        .as_array()
        .map_or(0, Vec::len)
}

fn live_pks(db: &Db, name: &str, ids: &[i64]) -> Vec<Pk> {
    let pks: Vec<Pk> = ids.iter().map(|&id| Pk::Int64(id)).collect();
    db.collection("", name)
        .unwrap()
        .read()
        .get(&pks, &[])
        .unwrap()
        .rows
        .pks
}

fn load_error(db: &Db, name: &str) -> String {
    match db.collection("", name).unwrap().write().load() {
        Err(EngineError::InvalidArgument(message)) => message,
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

const UNBUILDABLE: [(&str, &str); 4] = [
    (
        "IVF_PQ",
        "index_type='IVF_PQ' is reserved for a future phase; \
         supported: HNSW, HNSW_SQ, IVF_FLAT, IVF_SQ8, BRUTE_FORCE",
    ),
    (
        "INVERTED",
        "scalar index INVERTED does not support field type FLOAT_VECTOR",
    ),
    ("DISKANN", "unknown index_type: 'DISKANN'"),
    ("FOO", "unknown index_type: 'FOO'"),
];

#[test]
fn unbuildable_definitions_fail_load_and_stay_released_without_writing() {
    for (index_type, message) in UNBUILDABLE {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("unbuildable.db");
        let db = open(&path);
        create(&db, "c", &[1, 2], true, &[]);
        db.close();
        assert_eq!(data_files(&path, "c"), 1);
        rewrite_index_type(&path, "c", index_type);

        let db = open(&path);
        let handle = db.collection("", "c").unwrap();
        let manifest = std::fs::read(manifest_path(&path, "c")).unwrap();
        // 3.x 每次 load 都重新尝试建索引，失败不改变状态。
        for _ in 0..2 {
            assert_eq!(load_error(&db, "c"), message, "{index_type}");
            assert_eq!(handle.read().load_state(), LoadState::Released);
            assert!(matches!(
                handle.read().require_loaded(),
                Err(EngineError::CollectionNotLoaded(_))
            ));
        }
        assert_eq!(std::fs::read(manifest_path(&path, "c")).unwrap(), manifest);
        assert_eq!(handle.read().index("v").unwrap().index_type, index_type);
        drop(handle);
        db.close();
    }
}

#[test]
fn definitions_that_3x_can_build_or_skip_still_load() {
    for index_type in [
        "FLAT",
        "BRUTE_FORCE",
        "AUTOINDEX",
        "HNSW",
        "HNSW_SQ",
        "IVF_FLAT",
        "IVF_SQ8",
        "SPARSE_INVERTED_INDEX",
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("buildable.db");
        let db = open(&path);
        create(&db, "c", &[1, 2], true, &[]);
        db.close();
        rewrite_index_type(&path, "c", index_type);

        let db = open(&path);
        db.collection("", "c").unwrap().write().load().unwrap();
        assert_eq!(
            live_pks(&db, "c", &[1, 2]),
            [Pk::Int64(1), Pk::Int64(2)],
            "{index_type}"
        );
        db.close();
    }
}

#[test]
fn wal_only_rows_load_and_later_segments_wait_for_the_next_load() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("wal.db");
    let db = open(&path);
    create(&db, "c", &[], false, &[]);
    db.close();
    rewrite_index_type(&path, "c", "IVF_PQ");

    let db = open(&path);
    db.insert("", "c", batch(&[1, 2])).unwrap();
    assert_eq!(data_files(&path, "c"), 0);
    let handle = db.collection("", "c").unwrap();
    handle.write().load().unwrap();
    assert_eq!(live_pks(&db, "c", &[1, 2]), [Pk::Int64(1), Pk::Int64(2)]);

    // 3.x 在 loaded 状态下为新 segment 建索引失败只记日志，检索照常；再次 load 直接返回。
    db.flush_collection("", "c").unwrap();
    assert_eq!(data_files(&path, "c"), 1);
    handle.write().load().unwrap();
    assert_eq!(live_pks(&db, "c", &[1, 2]), [Pk::Int64(1), Pk::Int64(2)]);

    handle.write().release();
    assert_eq!(
        load_error(&db, "c"),
        "index_type='IVF_PQ' is reserved for a future phase; \
         supported: HNSW, HNSW_SQ, IVF_FLAT, IVF_SQ8, BRUTE_FORCE"
    );
    assert_eq!(handle.read().load_state(), LoadState::Released);
    drop(handle);
    db.close();
}

#[test]
fn committed_rows_hidden_by_deletes_still_fail_load() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("deleted.db");
    let db = open(&path);
    create(&db, "c", &[1, 2], true, &[1, 2]);
    db.close();
    assert_eq!(data_files(&path, "c"), 1);
    rewrite_index_type(&path, "c", "DISKANN");

    let db = open(&path);
    assert_eq!(db.collection("", "c").unwrap().read().count(), 0);
    assert_eq!(load_error(&db, "c"), "unknown index_type: 'DISKANN'");
    db.close();
}

#[test]
fn failed_load_leaves_other_collections_usable() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("isolated.db");
    let db = open(&path);
    create(&db, "bad", &[1], true, &[]);
    create(&db, "good", &[2], true, &[]);
    db.close();
    rewrite_index_type(&path, "bad", "IVF_PQ");

    let db = open(&path);
    load_error(&db, "bad");
    db.collection("", "good").unwrap().write().load().unwrap();
    assert_eq!(live_pks(&db, "good", &[2]), [Pk::Int64(2)]);
    assert_eq!(
        db.collection("", "bad").unwrap().read().load_state(),
        LoadState::Released
    );
    db.close();
}

fn rewrite_metric(path: &Path, kind: &str, metric: &str) {
    let file = manifest_path(path, "c");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    value["index_specs"]["v"]["index_type"] = kind.into();
    value["index_specs"]["v"]["metric_type"] = metric.into();
    std::fs::write(file, serde_json::to_vec(&value).unwrap()).unwrap();
}

fn sidecars(path: &Path, kind: &str) -> Vec<std::path::PathBuf> {
    let dir = path.join("collections/c/partitions/_default/indexes");
    std::fs::create_dir_all(&dir).unwrap();
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(manifest_path(path, "c")).unwrap()).unwrap();
    value["partitions"]["_default"]["data_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            let stem = Path::new(file.as_str().unwrap())
                .file_stem()
                .unwrap()
                .to_str()
                .unwrap();
            let file = dir.join(format!("{stem}.v.{}.idx", kind.to_lowercase()));
            // load 只看存在性；损坏内容属于原有“不读 3.x 侧车”的约定。
            std::fs::write(&file, b"not a FAISS file").unwrap();
            file
        })
        .collect()
}

#[test]
fn invalid_metric_checks_each_nonempty_segment_and_recovers_after_drop() {
    for kind in ["AUTOINDEX", "HNSW", "HNSW_SQ", "IVF_FLAT", "IVF_SQ8"] {
        for metric in ["BM25", "NONE"] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("metric.db");
            let db = open(&path);
            create(&db, "c", &[1], true, &[]);
            db.insert("", "c", batch(&[2])).unwrap();
            db.flush_collection("", "c").unwrap();
            create(&db, "healthy", &[9], true, &[]);
            db.close();
            rewrite_metric(&path, kind, metric);
            let files = sidecars(&path, kind);
            assert_eq!(files.len(), 2);
            let db = open(&path);
            let handle = db.collection("", "c").unwrap();
            handle.write().load().unwrap();
            assert_eq!(live_pks(&db, "c", &[1, 2]), [Pk::Int64(1), Pk::Int64(2)]);
            handle.write().release();
            std::fs::remove_file(&files[1]).unwrap();
            let before = std::fs::read(manifest_path(&path, "c")).unwrap();
            for _ in 0..2 {
                assert_eq!(
                    load_error(&db, "c"),
                    format!("unsupported metric '{metric}'; expected COSINE / L2 / IP")
                );
                assert_eq!(handle.read().load_state(), LoadState::Released);
                assert!(matches!(
                    handle.read().require_loaded(),
                    Err(EngineError::CollectionNotLoaded(_))
                ));
            }
            assert_eq!(std::fs::read(manifest_path(&path, "c")).unwrap(), before);
            assert_eq!(std::fs::read(&files[0]).unwrap(), b"not a FAISS file");
            db.collection("", "healthy")
                .unwrap()
                .write()
                .load()
                .unwrap();
            assert_eq!(live_pks(&db, "healthy", &[9]), [Pk::Int64(9)]);
            std::fs::remove_file(&files[0]).unwrap();
            db.drop_index("", "c", "v", "").unwrap();
            assert!(handle.read().indexes().is_empty());
            assert_eq!(handle.read().load_state(), LoadState::Loaded);
            handle.write().load().unwrap();
            assert_eq!(live_pks(&db, "c", &[1, 2]), [Pk::Int64(1), Pk::Int64(2)]);
            db.close();
        }
    }
}

#[test]
fn invalid_metric_wal_flush_loaded_and_deleted_physical_rows() {
    for flushed in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lifecycle.db");
        let db = open(&path);
        create(&db, "c", &[], false, &[]);
        db.close();
        rewrite_metric(&path, "HNSW", "NONE");
        let db = open(&path);
        db.insert("", "c", batch(&[1])).unwrap();
        if flushed {
            db.flush_collection("", "c").unwrap();
            sidecars(&path, "HNSW");
        }
        let handle = db.collection("", "c").unwrap();
        handle.write().load().unwrap();
        db.insert("", "c", batch(&[2])).unwrap();
        db.flush_collection("", "c").unwrap();
        // 新 segment 不会导致已 loaded 的 collection 重新检查。
        handle.write().load().unwrap();
        assert_eq!(live_pks(&db, "c", &[1, 2]), [Pk::Int64(1), Pk::Int64(2)]);
        handle.write().release();
        assert!(load_error(&db, "c").contains("unsupported metric 'NONE'"));
        db.delete("", "c", &[Pk::Int64(1), Pk::Int64(2)]).unwrap();
        db.flush_collection("", "c").unwrap();
        assert_eq!(handle.read().count(), 0);
        assert!(load_error(&db, "c").contains("unsupported metric 'NONE'"));
        db.close();
    }
}

#[cfg(unix)]
#[test]
fn metric_metadata_follows_symlinks_and_does_not_read_present_content() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("links.db");
    let db = open(&path);
    create(&db, "c", &[1], true, &[]);
    db.close();
    rewrite_metric(&path, "AUTOINDEX", "BM25");
    let files = sidecars(&path, "AUTOINDEX");
    let db = open(&path);
    let handle = db.collection("", "c").unwrap();
    std::fs::remove_file(&files[0]).unwrap();
    let target = root.path().join("target.idx");
    std::fs::write(&target, b"present").unwrap();
    symlink(&target, &files[0]).unwrap();
    handle.write().load().unwrap();
    handle.write().release();
    std::fs::remove_file(&target).unwrap();
    assert!(load_error(&db, "c").contains("unsupported metric 'BM25'"));
    std::fs::remove_file(&files[0]).unwrap();
    std::fs::create_dir(&files[0]).unwrap();
    handle.write().load().unwrap();
    db.close();
}

#[test]
fn metadata_errors_are_reported_and_normal_metrics_do_not_stat_sidecars() {
    for (kind, metric, expected_error) in [
        ("HNSW", "NONE", true),
        ("HNSW", "L2", false),
        ("FLAT", "NONE", false),
        ("BRUTE_FORCE", "BM25", false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("metadata.db");
        let db = open(&path);
        create(&db, "c", &[1], true, &[]);
        db.close();
        rewrite_metric(&path, kind, metric);
        let db = open(&path);
        let handle = db.collection("", "c").unwrap();
        std::fs::write(
            path.join("collections/c/partitions/_default/indexes"),
            b"directory replaced after open",
        )
        .unwrap();
        let result = handle.write().load();
        if expected_error {
            assert!(matches!(result, Err(EngineError::Storage(_))));
            assert_eq!(handle.read().load_state(), LoadState::Released);
        } else {
            result.unwrap();
        }
        db.close();
    }
}

#[test]
fn retired_load_handle_cannot_stat_a_recreated_collection() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("retired.db");
    let db = open(&path);
    create(&db, "c", &[1], true, &[]);
    let old = db.collection("", "c").unwrap();
    db.drop_collection("", "c").unwrap();
    create(&db, "c", &[2], true, &[]);
    assert!(matches!(
        old.write().load(),
        Err(EngineError::CollectionNotFound(_))
    ));
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn confirmed_sidecar_deletion_before_manifest_failure_blocks_next_metric_load() {
    use milvus_lite_core::storage::fs_commit::{CommitPhase, inject_manifest_panic_once};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("commit-failure.db");
    let db = open(&path);
    create(&db, "c", &[1], true, &[]);
    db.close();
    rewrite_metric(&path, "HNSW", "NONE");
    let files = sidecars(&path, "HNSW");
    let db = open(&path);
    let handle = db.collection("", "c").unwrap();
    handle.write().load().unwrap();
    handle.write().release();
    let before = std::fs::read(manifest_path(&path, "c")).unwrap();
    inject_manifest_panic_once(CommitPhase::BeforeReplace);
    assert!(db.drop_index("", "c", "v", "").is_err());
    assert_eq!(std::fs::read(manifest_path(&path, "c")).unwrap(), before);
    assert!(files.iter().all(|p| !p.exists()));
    assert_eq!(
        load_error(&db, "c"),
        "unsupported metric 'NONE'; expected COSINE / L2 / IP"
    );
    assert_eq!(handle.read().load_state(), LoadState::Released);
    db.drop_index("", "c", "v", "").unwrap();
    handle.write().load().unwrap();
    assert_eq!(live_pks(&db, "c", &[1]), [Pk::Int64(1)]);
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn compaction_new_source_requires_a_sidecar_on_the_next_metric_load() {
    use milvus_lite_core::ColumnData;
    use milvus_lite_core::storage::{arrow_data, parquet_file};
    use std::sync::mpsc;
    use std::time::Duration;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("compact.db");
    let db = open(&path);
    create(&db, "c", &[1], true, &[]);
    db.close();
    let file = manifest_path(&path, "c");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let parent = path.join("collections/c/partitions/_default");
    for i in 2..=4 {
        let other = format!("data/data_{i:06}_{i:06}.parquet");
        let batch = arrow_data::data_batch(
            &schema(),
            &[i],
            &[
                ColumnData::Int64(vec![i as i64]),
                ColumnData::FloatVector {
                    dim: 2,
                    values: vec![1., 0.],
                },
            ],
            None,
        )
        .unwrap();
        parquet_file::write_new(&parent.join(&other), &batch).unwrap();
        value["partitions"]["_default"]["data_files"]
            .as_array_mut()
            .unwrap()
            .push(other.into());
    }
    value["index_specs"]["v"]["index_type"] = "HNSW".into();
    value["index_specs"]["v"]["metric_type"] = "BM25".into();
    value["current_seq"] = 4.into();
    std::fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
    let db = open(&path);
    let (entered, arrived) = mpsc::channel();
    let (resume, continued) = mpsc::channel();
    db.pause_compaction_once(
        milvus_lite_core::CompactionPausePoint::BeforePrepare,
        entered,
        continued,
    );
    let handle = db.collection("", "c").unwrap();
    arrived.recv_timeout(Duration::from_secs(15)).unwrap();
    sidecars(&path, "HNSW");
    handle.write().load().unwrap();
    resume.send(()).unwrap();
    let report = db
        .wait_for_maintenance("", "c", Duration::from_secs(15))
        .unwrap();
    assert_eq!(report.failed, 0, "{report:?}");
    assert_eq!(data_files(&path, "c"), 1);
    handle.write().load().unwrap();
    handle.write().release();
    assert_eq!(
        load_error(&db, "c"),
        "unsupported metric 'BM25'; expected COSINE / L2 / IP"
    );
    db.drop_index("", "c", "v", "").unwrap();
    handle.write().load().unwrap();
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn load_drop_interleaving_preserves_the_observed_metric_state() {
    use milvus_lite_core::storage::index_intent::{before_commit_once, before_delete_once};
    for load_first in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("interleaving.db");
        let db = open(&path);
        create(&db, "c", &[1], true, &[]);
        db.close();
        rewrite_metric(&path, "HNSW", "NONE");
        let files = sidecars(&path, "HNSW");
        let db = open(&path);
        let handle = db.collection("", "c").unwrap();
        let other = handle.clone();
        if load_first {
            before_delete_once(move || other.write().load().unwrap());
        } else {
            before_commit_once(move || {
                assert!(
                    other
                        .write()
                        .load()
                        .unwrap_err()
                        .to_string()
                        .contains("unsupported metric 'NONE'")
                );
            });
        }
        let result = db.drop_index("", "c", "v", "");
        assert!(files.iter().all(|p| !p.exists()));
        if load_first {
            assert!(matches!(result, Err(EngineError::SchemaValidation(_))));
            assert_eq!(handle.read().load_state(), LoadState::Loaded);
            // 已 loaded 的幂等请求仍成功；下一次 release 后检查缺失侧车。
            handle.write().load().unwrap();
            handle.write().release();
            assert!(load_error(&db, "c").contains("unsupported metric 'NONE'"));
            db.drop_index("", "c", "v", "").unwrap();
        } else {
            result.unwrap();
        }
        assert!(handle.read().indexes().is_empty());
        assert_eq!(handle.read().load_state(), LoadState::Loaded);
        db.close();
    }
}
