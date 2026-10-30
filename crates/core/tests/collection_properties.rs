//! 0023 A AlterCollection：属性合并顺序、`schema.json` 原始 JSON 的规范重写、`timezone` 与范围外 schema 的零写入、
//! 持久化失败语义。gRPC 码与文案、磁盘字节与 3.x 的逐行对照见差分测试。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, EngineError, FieldSchema, InputValues,
    InsertBatch,
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

fn open(path: &Path) -> std::sync::Arc<Db> {
    Db::new(DataDir::open(path).unwrap(), compute()).unwrap()
}

fn schema_path(root: &Path, name: &str) -> PathBuf {
    root.join("collections").join(name).join("schema.json")
}

/// 字节与 inode：inode 不变说明没有发生替换。
fn file_state(path: &Path) -> (Vec<u8>, u64) {
    (
        std::fs::read(path).unwrap(),
        std::fs::metadata(path).unwrap().ino(),
    )
}

fn properties(db: &Db, name: &str) -> Vec<(String, String)> {
    db.collection("", name)
        .unwrap()
        .read()
        .schema()
        .properties
        .clone()
}

fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn keys(items: &[&str]) -> Vec<String> {
    items.iter().map(|k| (*k).to_owned()).collect()
}

#[test]
fn alter_merges_in_3x_order_and_reopens_in_file_order() {
    let root = tempfile::tempdir().unwrap();
    let path = schema_path(root.path(), "c");
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1, 2])).unwrap();

    db.alter_collection(
        "",
        "c",
        &pairs(&[("zeta", "1"), ("alpha", "2"), ("mid", "3")]),
        &[],
    )
    .unwrap();
    assert_eq!(
        properties(&db, "c"),
        pairs(&[("zeta", "1"), ("alpha", "2"), ("mid", "3")])
    );
    // 已有键原位替换；先删后设，被删的键重新设置时追加在末尾；删除不存在的键忽略。
    db.alter_collection("", "c", &pairs(&[("alpha", "20")]), &[])
        .unwrap();
    db.alter_collection(
        "",
        "c",
        &pairs(&[("zeta", "9")]),
        &keys(&["zeta", "missing"]),
    )
    .unwrap();
    assert_eq!(
        properties(&db, "c"),
        pairs(&[("alpha", "20"), ("mid", "3"), ("zeta", "9")])
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains(
            "\"properties\": {\n    \"alpha\": \"20\",\n    \"mid\": \"3\",\n    \"zeta\": \"9\"\n  },"
        ),
        "{text}"
    );

    // 规范字节相同则不写。
    let before = file_state(&path);
    db.alter_collection("", "c", &pairs(&[("mid", "3")]), &keys(&["missing"]))
        .unwrap();
    db.alter_collection("", "c", &[], &[]).unwrap();
    assert_eq!(file_state(&path), before);

    // 经别名修改目标集合；不存在的集合报目标名。
    db.create_alias("", "c", "a").unwrap();
    db.alter_collection("", "a", &pairs(&[("k", "v")]), &[])
        .unwrap();
    assert_eq!(
        properties(&db, "c").last().unwrap(),
        &pairs(&[("k", "v")])[0]
    );
    assert!(matches!(
        db.alter_collection("", "nope", &pairs(&[("k", "v")]), &[]),
        Err(EngineError::CollectionNotFound(name)) if name == "nope"
    ));
    assert_eq!(db.collection("", "c").unwrap().read().count(), 2);
    db.close();

    // 重开后按文件中的顺序（3.x 排序写出）读回。
    let db = open(root.path());
    assert_eq!(
        properties(&db, "c"),
        pairs(&[("alpha", "20"), ("k", "v"), ("mid", "3"), ("zeta", "9")])
    );
    db.alter_collection("", "c", &[], &keys(&["alpha", "k", "mid", "zeta"]))
        .unwrap();
    assert!(properties(&db, "c").is_empty());
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("\"properties\": {},")
    );
    assert_eq!(db.collection("", "c").unwrap().read().count(), 2);
    db.close();
}

#[test]
fn timezone_is_rejected_after_existence_with_zero_writes() {
    let root = tempfile::tempdir().unwrap();
    let path = schema_path(root.path(), "c");
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.alter_collection("", "c", &pairs(&[("keep", "1")]), &[])
        .unwrap();
    let before = file_state(&path);

    assert!(matches!(
        db.alter_collection("", "nope", &pairs(&[("timezone", "Mars/Base")]), &[]),
        Err(EngineError::CollectionNotFound(_))
    ));
    for request in [
        pairs(&[("timezone", "UTC")]),
        pairs(&[("a", "1"), ("timezone", "Mars/Base")]),
    ] {
        // 混在普通属性里也不先写普通属性；删除同名键也不能绕过。
        for delete in [keys(&[]), keys(&["timezone", "keep"])] {
            assert!(matches!(
                db.alter_collection("", "c", &request, &delete),
                Err(EngineError::Unsupported {
                    feature: "timezone property",
                    ..
                })
            ));
        }
    }
    assert_eq!(properties(&db, "c"), pairs(&[("keep", "1")]));
    assert_eq!(file_state(&path), before);
    // 只删除 `timezone` 键不受影响。
    db.alter_collection("", "c", &[], &keys(&["timezone"]))
        .unwrap();
    assert_eq!(file_state(&path), before);
    db.close();
}

#[test]
fn unsorted_or_renamed_schema_files_are_rewritten_canonically() {
    let root = tempfile::tempdir().unwrap();
    let path = schema_path(root.path(), "c");
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.close();
    let canonical = std::fs::read(&path).unwrap();

    // 0.3.0rc3 按结构体字段顺序写出、未排序；名字与目录不同的文件也能打开（取舍 3）。
    let value: serde_json::Value = serde_json::from_slice(&canonical).unwrap();
    let mut reordered = serde_json::Map::new();
    for (key, item) in value.as_object().unwrap().iter().rev() {
        let item = if key == "collection_name" {
            serde_json::Value::String("other".into())
        } else {
            item.clone()
        };
        reordered.insert(key.clone(), item);
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&reordered).unwrap()).unwrap();

    let db = open(root.path());
    let before = file_state(&path);
    db.alter_collection("", "c", &[], &[]).unwrap();
    let after = file_state(&path);
    assert_ne!(after.1, before.1);
    assert_eq!(after.0, canonical);
    db.close();
}

#[test]
fn out_of_scope_schemas_are_unsupported_without_writes() {
    let root = tempfile::tempdir().unwrap();
    let path = schema_path(root.path(), "c");
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.close();
    // 非字符串属性与 3.x 写入的 `timezone` 都是 0006 第 14 项的范围外 schema。
    for properties in [r#"{"n": 1}"#, r#"{"timezone": "UTC"}"#] {
        let text = std::fs::read_to_string(&path).unwrap();
        let start = text.find("\"properties\": ").unwrap();
        let end = start + text[start..].find(['}']).unwrap() + 1;
        std::fs::write(
            &path,
            format!(
                "{}\"properties\": {properties}{}",
                &text[..start],
                &text[end..]
            ),
        )
        .unwrap();
        let db = open(root.path());
        let before = file_state(&path);
        for delete in [keys(&[]), keys(&["n", "timezone"])] {
            assert!(matches!(
                db.alter_collection("", "c", &pairs(&[("k", "v")]), &delete),
                Err(EngineError::Unsupported { .. })
            ));
        }
        assert_eq!(file_state(&path), before);
        db.close();
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn alter_failures_before_replace_change_nothing_and_after_replace_seal_writes() {
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
    };
    for point in [DirectoryPoint::BeforeRename, DirectoryPoint::AfterRename] {
        for fault in [DirectoryFault::Error, DirectoryFault::Panic] {
            let root = tempfile::tempdir().unwrap();
            let path = schema_path(root.path(), "c");
            let db = open(root.path());
            for name in ["c", "other"] {
                db.create_collection("", name, schema()).unwrap();
            }
            db.alter_collection("", "c", &pairs(&[("kept", "1")]), &[])
                .unwrap();
            let before = std::fs::read(&path).unwrap();
            inject_directory_fault_once(DirectoryOperation::Alter, point, fault);
            let error = db
                .alter_collection("", "c", &pairs(&[("new", "2")]), &keys(&["kept"]))
                .unwrap_err()
                .to_string();
            let replaced = point == DirectoryPoint::AfterRename;
            assert!(error.contains("storage error"), "{error}");
            assert_eq!(error.contains("durability is unconfirmed"), replaced);
            // 内存与磁盘一致：替换之前失败两者都不变，替换之后两者都已更新。
            assert_eq!(std::fs::read(&path).unwrap() != before, replaced);
            let expected = if replaced {
                pairs(&[("new", "2")])
            } else {
                pairs(&[("kept", "1")])
            };
            assert_eq!(properties(&db, "c"), expected);
            let write = db.insert("", "other", batch(&[1]));
            let alter = db.alter_collection("", "other", &pairs(&[("x", "y")]), &[]);
            if replaced {
                // 0007：改名之后目录同步未确认，本服务实例的写入全部封锁，已打开集合照常可读。
                for result in [write.map(|_| ()), alter] {
                    assert!(
                        result.unwrap_err().to_string().contains("blocked"),
                        "writes must be sealed"
                    );
                }
            } else {
                write.unwrap();
                alter.unwrap();
            }
            assert!(
                std::fs::read_dir(path.parent().unwrap())
                    .unwrap()
                    .all(|entry| !entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .contains(".tmp"))
            );
            db.close();
            let db = open(root.path());
            assert_eq!(properties(&db, "c"), expected);
            db.close();
        }
    }
}
