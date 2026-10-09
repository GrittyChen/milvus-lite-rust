//! 数据库层的行为用例，期望值取自 3.x（v3.2.1）的 `db.py` 与实测。

// 辅助函数不带 #[test]，clippy 的 allow-unwrap-in-tests 覆盖不到；测试中失败即应 panic。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataDirError, DataType, Db, EngineError, FieldSchema,
    LoadState,
};

fn compute() -> Compute {
    Compute::new(std::num::NonZeroUsize::new(2).unwrap()).unwrap()
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

fn db() -> (tempfile::TempDir, std::sync::Arc<Db>) {
    let root = tempfile::tempdir().unwrap();
    let dir = DataDir::open(root.path()).unwrap();
    (root, Db::new(dir, compute()).unwrap())
}

#[test]
fn lifecycle() {
    let (_root, db) = db();
    for name in ["qc", "_x", "C", "a", "1z", "中文"] {
        db.create_collection("", name, schema()).unwrap();
    }
    // 码点序，与 3.x 的 sorted(names) 相同。
    let names: Vec<String> = db
        .list_collections("")
        .unwrap()
        .into_iter()
        .map(|(n, state)| {
            assert_eq!(state, LoadState::Loaded);
            n
        })
        .collect();
    assert_eq!(names, ["1z", "C", "_x", "a", "qc", "中文"]);
    assert!(db.collection("", "qc").is_ok());
    let e = db.create_collection("", "qc", schema()).unwrap_err();
    assert!(matches!(e, EngineError::CollectionAlreadyExists(_)));
    assert_eq!(e.to_string(), "collection 'qc' already exists");

    db.drop_collection("", "qc").unwrap();
    db.drop_collection("", "qc").unwrap();
    db.drop_collection("", "").unwrap();
    assert!(db.collection("", "qc").is_err());
    let e = db.collection("", "qc").unwrap_err();
    assert!(matches!(e, EngineError::CollectionNotFound(_)));
    assert_eq!(e.to_string(), "collection 'qc' does not exist");
    // 同名重建得到全新的 collection。
    db.create_collection("", "qc", schema()).unwrap();
    assert_eq!(db.collection("", "qc").unwrap().read().count(), 0);
}

#[test]
fn names_are_case_sensitive() {
    let (root, db) = db();
    db.create_collection("", "Case", schema()).unwrap();
    // 不敏感卷将两种拼写映射到同一物理目录；敏感卷允许两个独立的名字。
    if root.path().join("collections/case").exists() {
        assert!(matches!(
            db.create_collection("default", "case", schema()),
            Err(EngineError::CollectionAlreadyExists(_))
        ));
    } else {
        db.create_collection("default", "case", schema()).unwrap();
        assert!(db.collection("", "Case").is_ok());
        assert!(db.collection("", "case").is_ok());
        db.drop_collection("", "case").unwrap();
        assert!(db.collection("", "Case").is_ok());
        assert!(db.collection("", "case").is_err());
    }
    assert!(db.collection("", "CASE").is_err());
}

#[test]
fn data_only_reference_directory_accepts_first_index_and_first_empty_flush() {
    use milvus_lite_core::IndexRequest;
    use milvus_lite_core::storage::manifest::Manifest;

    for index_first in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let collection = root.path().join("collections/c");
        let partition = collection.join("partitions/_default");
        std::fs::create_dir_all(partition.join("data")).unwrap();
        std::fs::write(
            collection.join("schema.json"),
            include_bytes!("fixtures/schema_3x.json"),
        )
        .unwrap();
        let filename = "data/data_000001_000002.parquet";
        std::fs::write(
            partition.join(filename),
            include_bytes!("fixtures/data_3x.parquet"),
        )
        .unwrap();
        let mut manifest = Manifest {
            current_seq: 2,
            active_wal_number: Some(2),
            ..Manifest::default()
        };
        manifest
            .partitions
            .get_mut("_default")
            .unwrap()
            .data_files
            .push(filename.into());
        std::fs::write(
            collection.join("manifest.json"),
            manifest.to_bytes().unwrap(),
        )
        .unwrap();
        let db = Db::new(DataDir::open(root.path()).unwrap(), compute()).unwrap();
        assert!(!partition.join("delta").exists());
        if index_first {
            db.create_index(
                "",
                "c",
                "vector",
                IndexRequest {
                    index_type: "FLAT".into(),
                    metric_type: Some("L2".into()),
                    ..IndexRequest::default()
                },
            )
            .unwrap();
            // 建定义不需要伪造一个未被引用的 delta 目录。
            assert!(!partition.join("delta").exists());
        } else {
            let before = std::fs::read(collection.join("manifest.json")).unwrap();
            db.flush_collection("", "c").unwrap();
            // 没有活跃写入或残留 WAL 时，Db 的显式 Flush 合法地不产生提交。
            assert_eq!(
                std::fs::read(collection.join("manifest.json")).unwrap(),
                before
            );
            assert!(!partition.join("delta").exists());
            // 另固定真正空代提交原语：它沿用先创建 data/delta、再同步继承输入的顺序。
            milvus_lite_core::storage::flush::flush_generation(
                &collection,
                &manifest,
                2,
                2,
                None,
                None,
            )
            .unwrap();
            assert!(partition.join("delta").is_dir());
        }
        assert_eq!(db.collection("", "c").unwrap().read().count(), 2);
        assert_eq!(
            std::fs::read(partition.join(filename)).unwrap(),
            include_bytes!("fixtures/data_3x.parquet")
        );
        db.close();
        let reopened = Db::new(DataDir::open(root.path()).unwrap(), compute()).unwrap();
        assert_eq!(reopened.collection("", "c").unwrap().read().count(), 2);
        reopened.close();
    }
}

#[test]
fn name_validation_matches_3x() {
    let (_root, db) = db();
    let err = |name: &str| db.create_collection("", name, schema()).unwrap_err();
    let e = err("");
    assert!(matches!(e, EngineError::InvalidArgument(_)));
    assert_eq!(e.to_string(), "collection name must not be empty");
    assert_eq!(err("a/b").to_string(), "invalid collection name: 'a/b'");
    assert_eq!(err("..").to_string(), "invalid collection name: '..'");
    assert_eq!(
        err(&"x".repeat(256)).to_string(),
        "collection name too long (256 bytes, max 255)"
    );
    db.create_collection("", &"x".repeat(255), schema())
        .unwrap();
}

#[test]
fn schema_errors_take_precedence_over_duplicate_names() {
    let (_root, db) = db();
    db.create_collection("", "c", schema()).unwrap();
    let bad = CollectionSchema {
        fields: vec![],
        enable_dynamic_field: false,
        properties: Vec::new(),
    };
    assert_eq!(
        db.create_collection("", "c", bad).unwrap_err().to_string(),
        "schema has no fields"
    );
}

#[test]
fn only_the_default_database_exists() {
    let (_root, db) = db();
    let e = db.create_collection("foo", "c", schema()).unwrap_err();
    assert!(matches!(e, EngineError::DatabaseNotFound(_)));
    assert_eq!(e.to_string(), "database 'foo' does not exist");
    // 3.x 先校验库名，非法库名是 ValueError。
    for (name, msg) in [
        ("a/b", "invalid database name: 'a/b'".to_owned()),
        ("a\0b", "invalid database name: 'a\\x00b'".to_owned()),
        (
            &"d".repeat(256),
            "database name too long (256 bytes, max 255)".to_owned(),
        ),
    ] {
        let e = db.collection(name, "c").unwrap_err();
        assert!(matches!(e, EngineError::InvalidArgument(_)), "{e}");
        assert_eq!(e.to_string(), msg);
    }
    // 数据库先于 collection 名检查。
    assert!(matches!(
        db.create_collection("foo", "", schema()).unwrap_err(),
        EngineError::DatabaseNotFound(_)
    ));
    assert!(db.drop_collection("foo", "c").is_err());
    assert!(db.list_collections("foo").is_err());
}

#[test]
fn timezone_property_is_unsupported() {
    let (_root, db) = db();
    let mut s = schema();
    s.properties = vec![
        ("a".to_owned(), "1".to_owned()),
        ("timezone".to_owned(), "UTC".to_owned()),
    ];
    let e = db.create_collection("", "c", s).unwrap_err();
    assert!(matches!(e, EngineError::Unsupported { .. }), "{e}");
    // 3.x 先报「没有字段」，再看 timezone。
    let empty = CollectionSchema {
        properties: vec![("timezone".to_owned(), "UTC".to_owned())],
        ..CollectionSchema::default()
    };
    assert_eq!(
        db.create_collection("", "c", empty)
            .unwrap_err()
            .to_string(),
        "schema has no fields"
    );
}

fn lock_is_free(path: &std::path::Path) -> bool {
    match DataDir::open(path) {
        Ok(_) => true,
        Err(DataDirError::Locked { .. }) => false,
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn close_waits_for_in_flight_tasks_before_releasing_the_lock() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::new(DataDir::open(root.path()).unwrap(), compute()).unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (finish_tx, finish_rx) = std::sync::mpsc::channel::<()>();
    let task = {
        let db = Arc::clone(&db);
        std::thread::spawn(move || {
            let _task = db.enter().unwrap();
            started_tx.send(()).unwrap();
            finish_rx.recv().unwrap();
        })
    };
    started_rx.recv().unwrap();
    let closer = {
        let db = Arc::clone(&db);
        std::thread::spawn(move || db.close())
    };
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !closer.is_finished(),
        "close must wait for the in-flight task"
    );
    assert!(!lock_is_free(root.path()));
    assert!(matches!(db.enter().map(|_| ()), Err(EngineError::Closed)));
    finish_tx.send(()).unwrap();
    task.join().unwrap();
    closer.join().unwrap();
    assert!(lock_is_free(root.path()));
    db.close();
}

#[test]
fn close_nowait_syncs_in_the_background_before_releasing_the_lock() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::new(DataDir::open(root.path()).unwrap(), compute()).unwrap();
    let task = db.enter().unwrap();
    db.close_nowait();
    assert!(!lock_is_free(root.path()));
    assert_eq!(
        db.enter().err().map(|e| e.to_string()).as_deref(),
        Some("MilvusLite is closed")
    );
    drop(task);
    db.close();
    assert!(lock_is_free(root.path()));

    let root = tempfile::tempdir().unwrap();
    let db = Db::new(DataDir::open(root.path()).unwrap(), compute()).unwrap();
    db.close_nowait();
    db.close();
    assert!(lock_is_free(root.path()));
}

/// 0005「验证」的显式持锁场景：生产路径已在取得快照后放锁，
/// 本用例保留锁内计算时排队的写入与后到读者都能完成的不死锁证明。
/// 计算任务不拿锁，所以即使写入排在读锁之后、又挡住后到的读者，持锁的检索也能算完并放锁。
#[test]
fn search_with_a_queued_writer_does_not_deadlock() {
    use milvus_lite_core::{InputValues, InsertBatch, QueryVectors, SearchRequest};

    let (_root, db) = db();
    let dim = 64;
    let schema = CollectionSchema {
        fields: vec![
            FieldSchema {
                is_primary: true,
                ..FieldSchema::new("id", DataType::Int64)
            },
            FieldSchema {
                dim: Some(dim),
                ..FieldSchema::new("v", DataType::FloatVector)
            },
        ],
        enable_dynamic_field: false,
        properties: Vec::new(),
    };
    db.create_collection("", "c", schema).unwrap();
    let rows = |start: i64, n: usize| InsertBatch {
        num_rows: n,
        columns: vec![
            (
                "id".to_owned(),
                InputValues::Int((start..start + n as i64).collect()),
            ),
            (
                "v".to_owned(),
                InputValues::FloatVector {
                    dim: dim as usize,
                    values: (0..n * dim as usize).map(|i| (i % 7) as f32).collect(),
                },
            ),
        ],
    };
    let handle = db.collection("", "c").unwrap();
    db.insert("", "c", rows(0, 20_000)).unwrap();
    let search = |nq: usize| SearchRequest {
        anns_field: None,
        vectors: QueryVectors::Dense(vec![vec![1.0; dim as usize]; nq]),
        metric_type: Some("L2".to_owned()),
        limit: 10,
        offset: 0,
        output_fields: Vec::new(),
    };
    let db = Arc::new(db);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (db, handle) = (&db, &handle);
    std::thread::scope(|s| {
        let done = done_tx.clone();
        s.spawn(move || {
            let c = handle.read();
            held_tx.send(()).unwrap();
            c.search(search(64), db.compute()).unwrap();
            drop(c);
            done.send("long search").unwrap();
        });
        held_rx.recv().unwrap();
        let done = done_tx.clone();
        s.spawn(move || {
            db.insert("", "c", rows(20_000, 10)).unwrap();
            done.send("insert").unwrap();
        });
        std::thread::sleep(Duration::from_millis(20));
        let done = done_tx;
        s.spawn(move || {
            handle.read().search(search(1), db.compute()).unwrap();
            done.send("later search").unwrap();
        });
        let mut finished: Vec<&str> = (0..3)
            .map(|_| {
                done_rx
                    .recv_timeout(Duration::from_secs(60))
                    .expect("deadlock")
            })
            .collect();
        finished.sort_unstable();
        assert_eq!(finished, ["insert", "later search", "long search"]);
    });
    assert_eq!(handle.read().count(), 20_010);
}
