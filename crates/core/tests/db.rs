//! 数据库层的行为用例，期望值取自 3.x（v3.2.1）的 `db.py` 与实测。

// 辅助函数不带 #[test]，clippy 的 allow-unwrap-in-tests 覆盖不到；测试中失败即应 panic。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use milvus_lite_core::{CollectionSchema, DataDir, DataType, Db, EngineError, FieldSchema};

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
    }
}

fn db() -> (tempfile::TempDir, Db) {
    let root = tempfile::tempdir().unwrap();
    let dir = DataDir::open(root.path()).unwrap();
    (root, Db::new(dir))
}

#[test]
fn lifecycle() {
    let (_root, db) = db();
    for name in ["qc", "_x", "C", "a", "1z", "中文"] {
        db.create_collection(name, schema()).unwrap();
    }
    // 码点序，与 3.x 的 sorted(names) 相同。
    assert_eq!(db.list_collections(), ["1z", "C", "_x", "a", "qc", "中文"]);
    assert!(db.has_collection("qc"));
    let e = db.create_collection("qc", schema()).unwrap_err();
    assert!(matches!(e, EngineError::CollectionAlreadyExists(_)));
    assert_eq!(e.to_string(), "collection 'qc' already exists");

    db.drop_collection("qc");
    db.drop_collection("qc");
    db.drop_collection("");
    assert!(!db.has_collection("qc"));
    let e = db.collection("qc").unwrap_err();
    assert!(matches!(e, EngineError::CollectionNotFound(_)));
    assert_eq!(e.to_string(), "collection 'qc' does not exist");
    // 同名重建得到全新的 collection。
    db.create_collection("qc", schema()).unwrap();
    assert_eq!(db.collection("qc").unwrap().read().count(), 0);
}

#[test]
fn names_are_case_sensitive() {
    // 0004「决策」：3.x 在 macOS 上因文件系统不区分大小写而冲突，本实现按原样区分。
    let (_root, db) = db();
    db.create_collection("Case", schema()).unwrap();
    db.create_collection("case", schema()).unwrap();
    assert!(!db.has_collection("CASE"));
}

#[test]
fn name_validation_matches_3x() {
    let (_root, db) = db();
    let err = |name: &str| db.create_collection(name, schema()).unwrap_err();
    let e = err("");
    assert!(matches!(e, EngineError::InvalidArgument(_)));
    assert_eq!(e.to_string(), "collection name must not be empty");
    assert_eq!(err("a/b").to_string(), "invalid collection name: 'a/b'");
    assert_eq!(err("..").to_string(), "invalid collection name: '..'");
    assert_eq!(
        err(&"x".repeat(256)).to_string(),
        "collection name too long (256 bytes, max 255)"
    );
    db.create_collection(&"x".repeat(255), schema()).unwrap();
}

#[test]
fn schema_errors_take_precedence_over_duplicate_names() {
    let (_root, db) = db();
    db.create_collection("c", schema()).unwrap();
    let bad = CollectionSchema {
        fields: vec![],
        enable_dynamic_field: false,
    };
    assert_eq!(
        db.create_collection("c", bad).unwrap_err().to_string(),
        "schema has no fields"
    );
}
