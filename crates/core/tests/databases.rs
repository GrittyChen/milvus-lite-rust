//! 数据库注册表：打开时枚举 `databases/`，已有的非默认库按 3.x 使用（0023 B）。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, EngineError, FieldSchema, InputValues,
    InsertBatch,
};

fn compute() -> Compute {
    Compute::new(std::num::NonZeroUsize::new(2).unwrap()).unwrap()
}

fn open(path: &Path) -> std::sync::Arc<Db> {
    Db::new(DataDir::open(path).unwrap(), compute()).unwrap()
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

fn names(db: &Db, database: &str) -> Vec<String> {
    db.list_collections(database)
        .unwrap()
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

fn missing(result: Result<impl Sized, EngineError>, name: &str) {
    match result {
        Err(EngineError::DatabaseNotFound(found)) => assert_eq!(found, name),
        Err(other) => panic!("expected DatabaseNotFound for {name}: {other:?}"),
        Ok(_) => panic!("expected DatabaseNotFound for {name}"),
    }
}

#[test]
fn existing_databases_are_registered_at_open_and_usable() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    let databases = path.join("databases");
    // 带 collections/ 的库、没有 collections/ 与属性文件的手工空库、隐藏目录；普通文件不是数据库。
    std::fs::create_dir_all(databases.join("d1/collections")).unwrap();
    std::fs::create_dir_all(databases.join("hand")).unwrap();
    std::fs::create_dir_all(databases.join(".hidden")).unwrap();
    std::fs::write(databases.join("file"), b"").unwrap();
    let db = open(&path);

    for database in ["", "default", "d1", "hand", ".hidden"] {
        assert!(names(&db, database).is_empty(), "{database}");
    }
    // 按精确名字查注册表：未知名、普通文件、只差大小写的名字都是不存在，非法名字是参数错误。
    for name in ["d2", "file", "D1", "HAND"] {
        missing(db.list_collections(name), name);
    }
    assert!(matches!(
        db.list_collections("bad/name"),
        Err(EngineError::InvalidArgument(_))
    ));

    // 同名集合与别名在各库互不影响，数据落在各库自己的根目录。
    db.create_collection("d1", "c", schema()).unwrap();
    db.insert("d1", "c", batch(&[1, 2])).unwrap();
    db.create_collection("hand", "c", schema()).unwrap();
    db.create_collection("", "c", schema()).unwrap();
    db.create_alias("d1", "c", "a").unwrap();
    assert!(databases.join("d1/collections/c/schema.json").is_file());
    assert!(databases.join("hand/collections/c/schema.json").is_file());
    assert!(path.join("collections/c/schema.json").is_file());
    assert!(databases.join("d1/aliases.json").is_file());
    assert!(!path.join("aliases.json").exists());
    assert!(!databases.join("hand/aliases.json").exists());
    assert_eq!(db.list_aliases("d1", "").unwrap().1, vec!["a".to_owned()]);
    assert!(db.list_aliases("", "").unwrap().1.is_empty());
    assert!(db.collection("d1", "a").is_ok());
    assert!(matches!(
        db.collection("", "a"),
        Err(EngineError::CollectionNotFound(_))
    ));
    db.drop_collection("d1", "c").unwrap();
    assert_eq!(names(&db, "hand"), ["c"]);
    assert_eq!(names(&db, ""), ["c"]);
    assert!(names(&db, "d1").is_empty());
    db.create_collection("d1", "keep", schema()).unwrap();
    db.insert("d1", "keep", batch(&[7])).unwrap();
    db.close();
    drop(db);

    let db = open(&path);
    assert_eq!(names(&db, "d1"), ["keep"]);
    assert_eq!(names(&db, "hand"), ["c"]);
    assert!(db.collection("d1", "keep").is_ok());
    db.close();
}

#[test]
fn databases_default_directory_is_not_a_second_default() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    std::fs::create_dir_all(path.join("databases/default/collections")).unwrap();
    let db = open(&path);
    db.create_collection("default", "c", schema()).unwrap();
    // 默认库来自数据根；手工的 databases/default 不被登记，也不接收集合。
    assert!(path.join("collections/c/schema.json").is_file());
    assert!(!path.join("databases/default/collections/c").exists());
    assert_eq!(names(&db, ""), ["c"]);
    db.close();
}

#[test]
fn databases_path_must_be_a_directory() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("databases"), b"").unwrap();
    assert!(matches!(
        Db::new(DataDir::open(&path).unwrap(), compute()),
        Err(EngineError::Storage(_))
    ));
}

#[cfg(unix)]
#[test]
fn dangling_or_looping_databases_link_fails_the_open() {
    let root = tempfile::tempdir().unwrap();
    for name in ["dangling", "loop"] {
        let path = root.path().join(format!("{name}.db"));
        std::fs::create_dir_all(&path).unwrap();
        let link = path.join("databases");
        let target = if name == "loop" {
            link.clone()
        } else {
            root.path().join("nowhere")
        };
        std::os::unix::fs::symlink(target, &link).unwrap();
        assert!(
            matches!(
                Db::new(DataDir::open(&path).unwrap(), compute()),
                Err(EngineError::Storage(_))
            ),
            "{name}"
        );
    }
}

#[cfg(unix)]
#[test]
fn symlinked_entries_follow_isdir_and_unreadable_roots_fail_the_open() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    let databases = path.join("databases");
    std::fs::create_dir_all(&databases).unwrap();
    let elsewhere = root.path().join("elsewhere");
    std::fs::create_dir_all(elsewhere.join("collections")).unwrap();
    symlink(&elsewhere, databases.join("linked")).unwrap();
    symlink(root.path().join("nowhere"), databases.join("dangling")).unwrap();
    let db = open(&path);
    // 3.x 的 isdir 跟随符号链接：指向目录的链接是数据库，悬空链接不是。
    assert!(names(&db, "linked").is_empty());
    missing(db.list_collections("dangling"), "dangling");
    db.create_collection("linked", "c", schema()).unwrap();
    assert!(elsewhere.join("collections/c/schema.json").is_file());
    db.close();
    drop(db);

    // databases/ 本身是指向目录的符号链接：与 3.x 一样跟随，其中的库照常登记。
    let moved = root.path().join("moved-databases");
    std::fs::rename(&databases, &moved).unwrap();
    symlink(&moved, &databases).unwrap();
    let db = open(&path);
    assert_eq!(names(&db, "linked"), ["c"]);
    db.close();
    drop(db);
    std::fs::remove_file(&databases).unwrap();
    std::fs::rename(&moved, &databases).unwrap();

    // 无法枚举 databases/ 使打开失败（3.x 启动时 makedirs 不要求可读，本实现更严格）。
    std::fs::set_permissions(&databases, std::fs::Permissions::from_mode(0o300)).unwrap();
    let readable = std::fs::read_dir(&databases).is_ok();
    let result = Db::new(DataDir::open(&path).unwrap(), compute());
    std::fs::set_permissions(&databases, std::fs::Permissions::from_mode(0o755)).unwrap();
    if readable {
        // root 不受目录权限限制。
        result.unwrap().close();
    } else {
        assert!(matches!(result, Err(EngineError::Storage(_))));
    }
}

#[test]
fn open_sweeps_replace_temps_in_every_database_root() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    let d1 = path.join("databases/d1");
    std::fs::create_dir_all(d1.join("collections")).unwrap();
    let residue = [
        "aliases.json.tmp.1.2",
        "database_properties.json.tmp.3.4",
        "milvus-lite-rust.rename-intent.json.tmp.5.6",
    ];
    // 3.x 的固定名与近似名不是本实现的残留，保留。
    let kept = [
        "aliases.json.tmp",
        "aliases.json.tmp.x.1",
        "other.json.tmp.1.2",
    ];
    for name in residue.iter().chain(&kept) {
        std::fs::write(d1.join(name), b"x").unwrap();
    }
    std::fs::write(path.join("database_properties.json.tmp.7.8"), b"x").unwrap();
    let db = open(&path);
    for name in residue {
        assert!(!d1.join(name).exists(), "{name}");
    }
    for name in kept {
        assert!(d1.join(name).exists(), "{name}");
    }
    assert!(!path.join("database_properties.json.tmp.7.8").exists());
    db.close();
}

#[test]
fn open_settles_a_rename_intent_in_a_non_default_database() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    {
        let db = open(&path);
        std::fs::create_dir_all(path.join("databases/d1/collections")).unwrap();
        db.close();
    }
    let db = open(&path);
    db.create_collection("d1", "a", schema()).unwrap();
    db.close();
    drop(db);
    // 目录改名之前退出：旧名存在、新名不存在，重开时删除意图，集合保持原名。
    let intent = path.join("databases/d1/milvus-lite-rust.rename-intent.json");
    std::fs::write(
        &intent,
        br#"{"format_version": 1, "operation": "rename_collection", "old": "a", "new": "b"}"#,
    )
    .unwrap();
    let db = open(&path);
    assert!(!intent.exists());
    assert_eq!(names(&db, "d1"), ["a"]);
    db.close();
}
