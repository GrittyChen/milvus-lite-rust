//! 0023 A 别名：判定顺序、全入口解析、悬空别名、缓存与持久化。gRPC 码与文案的逐行对照见差分测试。

#![allow(clippy::unwrap_used, clippy::expect_used)]

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

fn open(path: &std::path::Path) -> std::sync::Arc<Db> {
    Db::new(DataDir::open(path).unwrap(), compute()).unwrap()
}

fn aliases_file(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path.join("aliases.json")).ok()
}

#[test]
fn alias_checks_follow_3x_order_and_store_the_resolved_target() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    for name in ["c1", "c2"] {
        db.create_collection("", name, schema()).unwrap();
    }
    std::fs::create_dir_all(root.path().join("collections/residue")).unwrap();

    for bad in ["a/b", "a\\b", ".", "..", "x\0y"] {
        // 非法别名配不存在的目标仍报非法名：名字校验先于目标存在。
        for target in ["c1", "nope"] {
            assert_eq!(
                db.create_alias("", target, bad).unwrap_err().to_string(),
                format!(
                    "invalid collection name: {}",
                    milvus_lite_core::error::py_repr(bad)
                )
            );
        }
    }
    assert_eq!(
        db.create_alias("", "c1", "").unwrap_err().to_string(),
        "collection name must not be empty"
    );
    assert_eq!(
        db.create_alias("", "c1", &"a".repeat(256))
            .unwrap_err()
            .to_string(),
        "collection name too long (256 bytes, max 255)"
    );
    for missing in ["nope", "residue", "a/b", "../c1"] {
        assert!(matches!(
            db.create_alias("", missing, "a1"),
            Err(EngineError::CollectionNotFound(name)) if name == missing
        ));
    }
    assert!(matches!(
        db.create_alias("", "c1", "c2"),
        Err(EngineError::AliasConflictsWithCollection(name)) if name == "c2"
    ));
    // 没有 schema.json 的残留目录不构成冲突（3.x 相同）。
    db.create_alias("", "c1", "residue").unwrap();
    db.create_alias("", "c1", "a1").unwrap();
    for target in ["c1", "c2"] {
        assert!(matches!(
            db.create_alias("", target, "a1"),
            Err(EngineError::AliasAlreadyExists(name)) if name == "a1"
        ));
    }
    // 目标可以是别名，保存的是解析后的真实集合名，只解析一层。
    db.create_alias("", "a1", "a2").unwrap();
    assert_eq!(db.describe_alias("", "a2").unwrap(), "c1");
    assert_eq!(
        aliases_file(root.path()).unwrap(),
        "{\n  \"a1\": \"c1\",\n  \"a2\": \"c1\",\n  \"residue\": \"c1\"\n}"
    );

    // AlterAlias：先判别名存在（不校验名字），再判目标。
    for missing in ["zz", "a/b"] {
        assert!(matches!(
            db.alter_alias("", "nope", missing),
            Err(EngineError::AliasNotFound(name)) if name == missing
        ));
    }
    assert!(matches!(
        db.alter_alias("", "nope", "a1"),
        Err(EngineError::CollectionNotFound(name)) if name == "nope"
    ));
    db.alter_alias("", "c2", "a1").unwrap();
    db.alter_alias("", "a1", "a2").unwrap();
    assert_eq!(db.describe_alias("", "a2").unwrap(), "c2");
    assert!(matches!(
        db.describe_alias("", "zz"),
        Err(EngineError::AliasNotFound(name)) if name == "zz"
    ));

    assert_eq!(
        db.list_aliases("", "").unwrap(),
        (
            String::new(),
            vec!["a1".into(), "a2".into(), "residue".into()]
        )
    );
    assert_eq!(
        db.list_aliases("", "a1").unwrap(),
        ("c2".into(), vec!["a1".into(), "a2".into()])
    );
    assert_eq!(
        db.list_aliases("", "nope").unwrap(),
        ("nope".into(), Vec::new())
    );

    // DropAlias 不校验名字，不存在即成功且不写文件。
    let before = aliases_file(root.path());
    db.drop_alias("", "zz").unwrap();
    db.drop_alias("", "a/b").unwrap();
    assert_eq!(aliases_file(root.path()), before);
    db.drop_alias("", "residue").unwrap();
    assert_eq!(
        db.list_aliases("", "").unwrap().1,
        ["a1".to_owned(), "a2".to_owned()]
    );
    // 以别名为名新建集合报已存在；ShowCollections 只列真实集合。
    assert!(matches!(
        db.create_collection("", "a1", schema()),
        Err(EngineError::CollectionAlreadyExists(name)) if name == "a1"
    ));
    let names: Vec<_> = db
        .list_collections("")
        .unwrap()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(names, ["c1", "c2"]);
    db.close();
}

#[test]
fn every_collection_entry_resolves_aliases_to_the_target() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.create_alias("", "c", "a").unwrap();

    db.insert("", "a", batch(&[1, 2, 3])).unwrap();
    db.upsert_with_pk_type("", "a", batch(&[3])).unwrap();
    db.delete("", "a", &[Pk::Int64(1)]).unwrap();
    db.delete_expr("", "a", "id in [2]", "").unwrap();
    db.flush_collection("", "a").unwrap();
    db.create_partition("", "a", "p").unwrap();
    assert!(db.has_partition("", "a", "p").unwrap());
    assert_eq!(
        db.list_partitions("", "a").unwrap().0,
        ["_default".to_owned(), "p".to_owned()]
    );
    db.insert_partition_with_pk_type("", "a", batch(&[7]), "p")
        .unwrap();
    db.drop_partition("", "a", "p").unwrap();
    db.create_index(
        "",
        "a",
        "v",
        IndexRequest {
            index_type: "FLAT".to_owned(),
            metric_type: Some("L2".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();

    let via_alias = db.collection("", "a").unwrap();
    let direct = db.collection("", "c").unwrap();
    assert!(std::sync::Arc::ptr_eq(&via_alias, &direct));
    let collection = via_alias.read();
    assert_eq!(collection.name(), "c");
    assert_eq!(collection.count(), 1);
    assert_eq!(
        collection.get(&[Pk::Int64(3)], &[]).unwrap().rows.pks,
        [Pk::Int64(3)]
    );
    assert_eq!(collection.index("v").unwrap().metric_type, "L2");
    drop(collection);
    via_alias.write().release();
    db.drop_index("", "a", "v", "").unwrap();
    assert!(direct.read().index("v").is_none());

    // 悬空别名：错误文案用目标名（3.x 相同）。
    db.close();
    std::fs::write(
        root.path().join("aliases.json"),
        br#"{"a":"c","ghost":"missing"}"#,
    )
    .unwrap();
    let db = open(root.path());
    for result in [
        db.collection("", "ghost").map(|_| ()),
        db.insert("", "ghost", batch(&[9])).map(|_| ()),
        db.delete_expr("", "ghost", "id in [1]", "").map(|_| ()),
        db.flush_collection("", "ghost"),
        db.create_partition("", "ghost", "p"),
        db.drop_partition("", "ghost", "p"),
    ] {
        assert!(
            matches!(&result, Err(EngineError::CollectionNotFound(name)) if name == "missing"),
            "{result:?}"
        );
    }
    assert_eq!(
        db.collection("", "a").unwrap().read().load_state(),
        LoadState::Loaded
    );
    db.close();
}

#[test]
fn drop_through_alias_removes_target_and_all_its_aliases() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    for name in ["c", "other"] {
        db.create_collection("", name, schema()).unwrap();
    }
    for (target, alias) in [("c", "a1"), ("c", "a2"), ("other", "keep")] {
        db.create_alias("", target, alias).unwrap();
    }
    db.drop_collection("", "a1").unwrap();
    assert!(!root.path().join("collections/c").exists());
    assert!(root.path().join("collections/other").exists());
    assert_eq!(db.list_aliases("", "").unwrap().1, ["keep".to_owned()]);
    assert_eq!(
        aliases_file(root.path()).unwrap(),
        "{\n  \"keep\": \"other\"\n}"
    );
    // 别名已删，同名再 drop 按普通名字处理，不存在即成功。
    db.drop_collection("", "a1").unwrap();
    db.create_collection("", "c", schema()).unwrap();
    assert!(db.list_aliases("", "c").unwrap().1.is_empty());
    db.close();
}

#[test]
fn alias_rpcs_keep_dangling_entries_that_create_and_drop_prune() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.close();
    std::fs::write(
        root.path().join("aliases.json"),
        br#"{"ghost":"missing","real":"c"}"#,
    )
    .unwrap();
    let db = open(root.path());
    assert_eq!(db.describe_alias("", "ghost").unwrap(), "missing");
    db.create_alias("", "c", "new").unwrap();
    db.alter_alias("", "c", "real").unwrap();
    db.drop_alias("", "new").unwrap();
    assert_eq!(
        aliases_file(root.path()).unwrap(),
        "{\n  \"ghost\": \"missing\",\n  \"real\": \"c\"\n}"
    );
    // create 沿用 0007：改名之前剔除全部悬空别名。
    db.create_collection("", "missing", schema()).unwrap();
    assert_eq!(db.list_aliases("", "").unwrap().1, ["real".to_owned()]);
    db.close();
}

#[test]
fn unparseable_aliases_fail_resolution_without_writes_and_are_retried() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.close();
    let path = root.path().join("aliases.json");
    std::fs::write(&path, b"{").unwrap();
    let db = open(root.path());
    let results = [
        db.collection("", "c").map(|_| ()),
        db.insert("", "c", batch(&[1])).map(|_| ()),
        db.flush_collection("", "c"),
        db.drop_collection("", "c"),
        db.create_collection("", "n", schema()),
        db.create_alias("", "c", "a"),
        db.alter_alias("", "c", "a"),
        db.drop_alias("", "a"),
        db.describe_alias("", "a").map(|_| ()),
        db.list_aliases("", "").map(|_| ()),
        db.check_aliases(""),
    ];
    for result in results {
        assert!(
            matches!(&result, Err(EngineError::Storage(_))),
            "{result:?}"
        );
    }
    assert_eq!(std::fs::read(&path).unwrap(), b"{");
    assert!(root.path().join("collections/c/schema.json").exists());
    assert!(!root.path().join("collections/n").exists());
    // 失败不缓存：修复文件后同一个服务实例即可继续。
    std::fs::write(&path, b"[]").unwrap();
    db.check_aliases("").unwrap();
    db.create_alias("", "c", "a").unwrap();
    assert_eq!(db.collection("", "a").unwrap().read().name(), "c");
    db.close();
}

#[test]
fn aliases_survive_reopen_in_3x_sorted_layout() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "target", schema()).unwrap();
    for alias in ["z_alias", "m_alias", "A_alias"] {
        db.create_alias("", "target", alias).unwrap();
    }
    assert_eq!(
        std::fs::read(root.path().join("aliases.json")).unwrap(),
        include_bytes!("fixtures/aliases_3x.json")
    );
    db.close();
    let db = open(root.path());
    assert_eq!(
        db.list_aliases("", "target").unwrap().1,
        ["A_alias", "m_alias", "z_alias"]
    );
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn alias_write_failures_before_replace_change_nothing_and_after_replace_seal_writes() {
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
    };
    for point in [DirectoryPoint::Aliases, DirectoryPoint::AliasesAfterRename] {
        for fault in [DirectoryFault::Error, DirectoryFault::Panic] {
            let root = tempfile::tempdir().unwrap();
            let db = open(root.path());
            for name in ["c", "other"] {
                db.create_collection("", name, schema()).unwrap();
            }
            db.create_alias("", "c", "kept").unwrap();
            let before = aliases_file(root.path());
            inject_directory_fault_once(DirectoryOperation::Alias, point, fault);
            let error = db.create_alias("", "c", "new").unwrap_err().to_string();
            let replaced = point == DirectoryPoint::AliasesAfterRename;
            assert!(error.contains("storage error"), "{error}");
            assert_eq!(error.contains("durability is unconfirmed"), replaced);
            // 缓存与磁盘一致：替换之前失败两者都不变，替换之后两者都已更新。
            assert_eq!(aliases_file(root.path()) != before, replaced);
            assert_eq!(db.describe_alias("", "new").is_ok(), replaced);
            assert_eq!(db.collection("", "c").unwrap().read().count(), 0);
            let write = db.insert("", "other", batch(&[1]));
            let alias_write = db.drop_alias("", "kept");
            if replaced {
                // 0007：改名之后目录同步未确认，本服务实例的写入全部封锁，已打开集合照常可读。
                for result in [write.map(|_| ()), alias_write] {
                    assert!(
                        result.unwrap_err().to_string().contains("blocked"),
                        "writes must be sealed"
                    );
                }
                assert_eq!(db.collection("", "new").unwrap().read().name(), "c");
            } else {
                write.unwrap();
                alias_write.unwrap();
            }
            assert!(std::fs::read_dir(root.path()).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".tmp")
            }));
            db.close();
            let db = open(root.path());
            assert_eq!(db.describe_alias("", "new").is_ok(), replaced);
            assert_eq!(db.describe_alias("", "kept").is_ok(), replaced);
            db.close();
        }
    }
}
