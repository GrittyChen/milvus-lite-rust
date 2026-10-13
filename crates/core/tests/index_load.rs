//! 存量索引定义的 load 判定：3.x 自身建不成的向量索引定义，在有行的已提交 segment 存在时
//! load 失败并保持 released（0002「结论」）。3.x 的 `create_index` 不校验向量字段的
//! index_type，这类定义只会出现在 3.x 写的目录中，这里直接改写 manifest 模拟。

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
