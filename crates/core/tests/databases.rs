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

fn properties_file(root: &Path) -> Option<String> {
    std::fs::read_to_string(root.join("database_properties.json")).ok()
}

fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn describe(db: &Db, database: &str) -> (String, Vec<(String, String)>) {
    db.describe_database(database).unwrap()
}

#[test]
fn list_databases_is_the_sorted_registry_including_default() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    for name in ["d1", "Zdb", ".hidden", "库"] {
        std::fs::create_dir_all(path.join("databases").join(name)).unwrap();
    }
    let db = open(&path);
    // 码点序：`.` < 大写 < 小写 < 非 ASCII；`default` 与其他库一同排序。
    assert_eq!(
        db.list_databases(),
        [".hidden", "Zdb", "d1", "default", "库"]
    );
    db.close();
}

#[test]
fn describe_and_alter_follow_the_reference_merge_rules() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    std::fs::create_dir_all(path.join("databases/d1")).unwrap();
    let db = open(&path);
    assert_eq!(describe(&db, ""), ("default".to_owned(), vec![]));
    assert_eq!(describe(&db, "d1").0, "d1");
    missing(db.describe_database("dx"), "dx");
    missing(db.describe_database("D1"), "D1");
    missing(db.alter_database("dx", &pairs(&[("a", "b")]), &[]), "dx");
    // 存在性先于 timezone 的判定。
    missing(
        db.alter_database("dx", &pairs(&[("timezone", "UTC")]), &[]),
        "dx",
    );

    // 没有变化不写文件：默认库的属性文件保持不存在。
    db.alter_database("", &[], &[]).unwrap();
    db.alter_database("", &[], &["nokey".to_owned()]).unwrap();
    assert_eq!(properties_file(&path), None);

    db.alter_database("", &pairs(&[("zeta", "1"), ("alpha", "2")]), &[])
        .unwrap();
    assert_eq!(
        properties_file(&path).unwrap(),
        "{\n  \"alpha\": \"2\",\n  \"zeta\": \"1\"\n}"
    );
    // 进程内按插入顺序，落盘按键序。
    assert_eq!(describe(&db, "").1, pairs(&[("zeta", "1"), ("alpha", "2")]));
    // 设为现值、删除后重设为原值：没有变化，不重写、顺序不变。
    let before = file_state(&path.join("database_properties.json"));
    db.alter_database("", &pairs(&[("zeta", "1")]), &[])
        .unwrap();
    db.alter_database("", &pairs(&[("zeta", "1")]), &["zeta".to_owned()])
        .unwrap();
    assert_eq!(file_state(&path.join("database_properties.json")), before);
    assert_eq!(describe(&db, "").1, pairs(&[("zeta", "1"), ("alpha", "2")]));
    // 同一请求先删后设同一键且值不同：该键移到末尾。
    db.alter_database("", &pairs(&[("zeta", "9")]), &["zeta".to_owned()])
        .unwrap();
    assert_eq!(describe(&db, "").1, pairs(&[("alpha", "2"), ("zeta", "9")]));

    // 各库的属性互不影响，写在各自的根目录。
    db.alter_database("d1", &pairs(&[("k", "v")]), &[]).unwrap();
    assert_eq!(describe(&db, "d1").1, pairs(&[("k", "v")]));
    assert_eq!(describe(&db, "").1, pairs(&[("alpha", "2"), ("zeta", "9")]));
    assert_eq!(
        properties_file(&path.join("databases/d1")).unwrap(),
        "{\n  \"k\": \"v\"\n}"
    );
    db.close();
    drop(db);

    // 重开后从文件读回，顺序即文件的键序。
    let db = open(&path);
    assert_eq!(describe(&db, "").1, pairs(&[("alpha", "2"), ("zeta", "9")]));
    assert_eq!(describe(&db, "d1").1, pairs(&[("k", "v")]));
    db.close();
}

#[test]
fn timezone_is_validated_before_any_write_and_existing_values_stay_manageable() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    std::fs::create_dir_all(path.join("databases/d1")).unwrap();
    // 3.x 已写入的 timezone：可以读取、展示、修改与删除。
    let file = path.join("databases/d1/database_properties.json");
    std::fs::write(&file, br#"{"timezone": "Asia/Shanghai", "k": "v"}"#).unwrap();
    let db = open(&path);
    assert_eq!(
        describe(&db, "d1").1,
        pairs(&[("timezone", "Asia/Shanghai"), ("k", "v")])
    );
    let before = std::fs::read(&file).unwrap();
    // 坏值（含混在普通属性里）报 6，零写入。
    for request in [
        pairs(&[("timezone", "Mars/Base")]),
        pairs(&[("other", "x"), ("timezone", "Mars/Base")]),
        pairs(&[("timezone", "")]),
    ] {
        match db.alter_database("d1", &request, &[]) {
            Err(EngineError::SchemaValidation(_)) => {}
            other => panic!("expected SchemaValidation: {other:?}"),
        }
        assert_eq!(std::fs::read(&file).unwrap(), before);
    }
    // 没有变化的请求不写文件。
    db.alter_database("d1", &pairs(&[("timezone", "Asia/Shanghai")]), &[])
        .unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), before);
    // 合法值可以修改，读取到的是最新值，重开后仍在。
    db.alter_database("d1", &pairs(&[("timezone", "UTC")]), &[])
        .unwrap();
    assert_eq!(db.database_timezone("d1").unwrap().as_deref(), Some("UTC"));
    db.alter_database("d1", &pairs(&[("timezone", "Asia/Shanghai")]), &[])
        .unwrap();
    assert_eq!(properties_file(&path), None);
    db.alter_database("d1", &[], &["timezone".to_owned()])
        .unwrap();
    assert_eq!(describe(&db, "d1").1, pairs(&[("k", "v")]));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "{\n  \"k\": \"v\"\n}"
    );
    db.close();
}

#[test]
fn unknown_timezone_in_the_file_fails_describe_and_alter_like_3x() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    std::fs::create_dir_all(path.join("databases/d1")).unwrap();
    let file = path.join("databases/d1/database_properties.json");
    std::fs::write(&file, br#"{"timezone": "Mars/Base", "k": "v"}"#).unwrap();
    let db = open(&path);
    let before = std::fs::read(&file).unwrap();
    assert!(matches!(
        db.describe_database("d1"),
        Err(EngineError::SchemaValidation(m)) if m == "unknown timezone 'Mars/Base'"
    ));
    // 3.x 加载属性就失败，所以任何 Alter 都报 6，包括删除或改成合法值的请求；文件保持原样。
    let delete = ["timezone".to_owned()];
    for (request, delete_keys) in [
        (pairs(&[("other", "x")]), &[][..]),
        (vec![], &[][..]),
        (pairs(&[("timezone", "UTC")]), &[][..]),
        (vec![], &delete[..]),
    ] {
        assert!(matches!(
            db.alter_database("d1", &request, delete_keys),
            Err(EngineError::SchemaValidation(_))
        ));
    }
    assert_eq!(std::fs::read(&file).unwrap(), before);
    // 3.x 校验不过的加载不缓存：文件被修正后，同一个进程里的下一次调用重新读取。
    std::fs::write(&file, br#"{"timezone": "UTC", "k": "v"}"#).unwrap();
    let (_, properties) = db.describe_database("d1").unwrap();
    assert_eq!(properties, pairs(&[("timezone", "UTC"), ("k", "v")]));
    db.close();
}

#[test]
fn unreadable_property_files_fail_describe_and_alter_only() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    let databases = path.join("databases");
    for (name, text) in [
        ("p_bad", &b"{"[..]),
        ("p_nonstr", br#"{"a": 1, "b": true}"#),
        ("p_list", b"[]"),
        ("p_unsorted", br#"{"z": "1", "a": "2"}"#),
    ] {
        std::fs::create_dir_all(databases.join(name)).unwrap();
        std::fs::write(databases.join(name).join("database_properties.json"), text).unwrap();
    }
    let db = open(&path);
    for name in ["p_bad", "p_nonstr"] {
        let file = databases.join(name).join("database_properties.json");
        let before = std::fs::read(&file).unwrap();
        assert!(matches!(
            db.describe_database(name),
            Err(EngineError::Storage(_))
        ));
        assert!(matches!(
            db.alter_database(name, &pairs(&[("n", "v")]), &[]),
            Err(EngineError::Storage(_))
        ));
        assert_eq!(std::fs::read(&file).unwrap(), before, "{name}");
        // 其余操作不读属性文件：集合照常创建、列出与删除。
        db.create_collection(name, "c", schema()).unwrap();
        assert_eq!(names(&db, name), ["c"]);
        db.drop_collection(name, "c").unwrap();
    }
    // 失败不缓存：修好文件后下一次读取成功。
    std::fs::write(
        databases.join("p_bad/database_properties.json"),
        br#"{"a": "1"}"#,
    )
    .unwrap();
    assert_eq!(describe(&db, "p_bad").1, pairs(&[("a", "1")]));
    // 顶层不是对象按空表处理，Alter 写出新文件；文件中的键序保留到下一次写出。
    assert!(describe(&db, "p_list").1.is_empty());
    db.alter_database("p_list", &pairs(&[("n", "v")]), &[])
        .unwrap();
    assert_eq!(
        properties_file(&databases.join("p_list")).unwrap(),
        "{\n  \"n\": \"v\"\n}"
    );
    assert_eq!(
        describe(&db, "p_unsorted").1,
        pairs(&[("z", "1"), ("a", "2")])
    );
    db.alter_database("p_unsorted", &pairs(&[("new", "v")]), &[])
        .unwrap();
    assert_eq!(
        properties_file(&databases.join("p_unsorted")).unwrap(),
        "{\n  \"a\": \"2\",\n  \"new\": \"v\",\n  \"z\": \"1\"\n}"
    );
    assert_eq!(
        describe(&db, "p_unsorted").1,
        pairs(&[("z", "1"), ("a", "2"), ("new", "v")])
    );
    db.close();
}

/// 字节与 inode：inode 不变说明没有发生替换。
fn file_state(path: &Path) -> (Vec<u8>, Option<u64>) {
    #[cfg(unix)]
    let inode = {
        use std::os::unix::fs::MetadataExt;
        Some(std::fs::metadata(path).unwrap().ino())
    };
    #[cfg(not(unix))]
    let inode = None;
    (std::fs::read(path).unwrap(), inode)
}

#[cfg(feature = "fault-injection")]
#[test]
fn property_write_failures_before_replace_change_nothing_and_after_replace_seal_writes() {
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
    };
    for database in ["", "d1"] {
        for point in [DirectoryPoint::BeforeRename, DirectoryPoint::AfterRename] {
            for fault in [DirectoryFault::Error, DirectoryFault::Panic] {
                let root = tempfile::tempdir().unwrap();
                let path = root.path().join("m.db");
                std::fs::create_dir_all(path.join("databases/d1")).unwrap();
                let db = open(&path);
                db.create_collection("", "c", schema()).unwrap();
                db.alter_database(database, &pairs(&[("kept", "1")]), &[])
                    .unwrap();
                let dir = if database.is_empty() {
                    path.clone()
                } else {
                    path.join("databases/d1")
                };
                let before = properties_file(&dir);
                inject_directory_fault_once(DirectoryOperation::DatabaseProperties, point, fault);
                let error = db
                    .alter_database(database, &pairs(&[("new", "2")]), &[])
                    .unwrap_err()
                    .to_string();
                let replaced = point == DirectoryPoint::AfterRename;
                assert!(error.contains("storage error"), "{error}");
                assert_eq!(error.contains("durability is unconfirmed"), replaced);
                // 缓存与磁盘一致：替换之前两者都不变，替换之后两者都已更新。
                assert_eq!(properties_file(&dir) != before, replaced);
                assert_eq!(
                    describe(&db, database).1.iter().any(|(k, _)| k == "new"),
                    replaced
                );
                let write = db.insert("", "c", batch(&[1]));
                if replaced {
                    // 0007：替换之后目录同步未确认，写入全部封锁，读照常。
                    assert!(write.unwrap_err().to_string().contains("blocked"));
                    assert!(
                        db.alter_database(database, &[], &["kept".to_owned()])
                            .unwrap_err()
                            .to_string()
                            .contains("blocked")
                    );
                } else {
                    write.unwrap();
                    db.alter_database(database, &pairs(&[("new", "2")]), &[])
                        .unwrap();
                }
                assert!(std::fs::read_dir(&dir).unwrap().all(|entry| {
                    !entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .contains(".tmp")
                }));
                db.close();
                drop(db);
                let db = open(&path);
                assert!(
                    describe(&db, database).1.iter().any(|(k, _)| k == "new"),
                    "the file holds the change after reopen (replaced={replaced}) or the retry"
                );
                db.close();
            }
        }
    }
}

/// 进程在 `database_properties.json` 替换前后退出：重开只见替换之前或之后的完整文件，没有残留临时文件。
#[cfg(feature = "fault-injection")]
#[test]
fn process_exit_around_the_property_replace_reopens_before_or_after() {
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
    };
    use std::process::Command;
    let test = "process_exit_around_the_property_replace_reopens_before_or_after";
    if let Ok(stage) = std::env::var("MILVUS_LITE_PROPERTY_CRASH") {
        let path = std::path::PathBuf::from(std::env::var("MILVUS_LITE_CRASH_DIR").unwrap());
        std::fs::create_dir_all(path.join("databases/d1")).unwrap();
        let db = open(&path);
        db.alter_database("d1", &pairs(&[("kept", "1")]), &[])
            .unwrap();
        let point = if stage == "before" {
            DirectoryPoint::BeforeRename
        } else {
            DirectoryPoint::AfterRename
        };
        inject_directory_fault_once(
            DirectoryOperation::DatabaseProperties,
            point,
            DirectoryFault::Exit,
        );
        db.alter_database("d1", &pairs(&[("new", "2")]), &[])
            .unwrap();
        panic!("property crash injection did not fire");
    }
    // 验证放在独立父进程中，避免兄弟子进程短暂继承刚关闭的 LOCK（同别名的进程退出用例）。
    if std::env::var_os("MILVUS_LITE_PROPERTY_PARENT").is_none() {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("MILVUS_LITE_PROPERTY_PARENT", "1")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "isolated recovery parent failed: {status:?}"
        );
        return;
    }
    for (stage, replaced) in [("before", false), ("after", true)] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("property-crash.db");
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("MILVUS_LITE_PROPERTY_CRASH", stage)
            .env("MILVUS_LITE_CRASH_DIR", &path)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(88), "{stage}");
        let db = open(&path);
        let (_, properties) = describe(&db, "d1");
        assert!(
            properties.contains(&("kept".to_owned(), "1".to_owned())),
            "{stage}"
        );
        assert_eq!(
            properties.contains(&("new".to_owned(), "2".to_owned())),
            replaced,
            "{stage}"
        );
        db.alter_database("d1", &pairs(&[("later", "3")]), &[])
            .unwrap();
        db.close();
        assert!(
            std::fs::read_dir(path.join("databases/d1"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".tmp")),
            "{stage}"
        );
    }
}

fn case_insensitive(dir: &Path) -> bool {
    let probe = dir.join("Case-Probe");
    std::fs::create_dir(&probe).unwrap();
    let insensitive = dir.join("case-probe").exists();
    std::fs::remove_dir(&probe).unwrap();
    insensitive
}

fn staging_entries(path: &Path) -> Vec<String> {
    match std::fs::read_dir(path.join(".database-staging")) {
        Ok(entries) => entries
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn already_exists(result: Result<(), EngineError>, name: &str) {
    match result {
        Err(EngineError::DatabaseAlreadyExists(found)) => assert_eq!(found, name),
        other => panic!("expected DatabaseAlreadyExists for {name}: {other:?}"),
    }
}

#[test]
fn create_database_builds_the_3x_layout_and_registers_it() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    let db = open(&path);
    // 没有 databases/ 与 staging 的数据目录：创建时补建。
    assert!(!path.join("databases").exists());
    db.create_database("d1", &pairs(&[("zz", "2"), ("aa", ""), ("键", "值")]))
        .unwrap();
    db.create_database("d2", &[]).unwrap();
    let d1 = path.join("databases/d1");
    assert!(d1.join("collections").is_dir());
    assert_eq!(
        properties_file(&d1).unwrap(),
        "{\n  \"aa\": \"\",\n  \"zz\": \"2\",\n  \"键\": \"值\"\n}"
    );
    assert_eq!(properties_file(&path.join("databases/d2")).unwrap(), "{}");
    assert!(!d1.join("aliases.json").exists());
    assert!(staging_entries(&path).is_empty());
    assert_eq!(db.list_databases(), ["d1", "d2", "default"]);
    // 进程内按请求顺序返回，各库互不影响；新库立即可用。
    assert_eq!(
        describe(&db, "d1").1,
        pairs(&[("zz", "2"), ("aa", ""), ("键", "值")])
    );
    assert!(describe(&db, "d2").1.is_empty());
    db.create_collection("d1", "c", schema()).unwrap();
    db.insert("d1", "c", batch(&[1, 2])).unwrap();
    db.create_alias("d1", "c", "a").unwrap();
    assert_eq!(names(&db, "d1"), ["c"]);
    assert!(names(&db, "d2").is_empty());
    assert!(names(&db, "").is_empty());
    db.alter_database("d1", &pairs(&[("new", "v")]), &[])
        .unwrap();
    db.close();
    drop(db);

    // 重开后各库仍在，属性读回为文件的键序。
    let db = open(&path);
    assert_eq!(db.list_databases(), ["d1", "d2", "default"]);
    assert_eq!(
        describe(&db, "d1").1,
        pairs(&[("aa", ""), ("new", "v"), ("zz", "2"), ("键", "值")])
    );
    assert_eq!(names(&db, "d1"), ["c"]);
    assert!(db.collection("d1", "a").is_ok());
    db.close();
}

#[test]
fn create_database_checks_name_then_existence_then_properties() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    let db = open(&path);
    db.create_database("d1", &[]).unwrap();
    // 空名即 default，已存在；名字校验先于存在判定，存在判定先于属性。
    already_exists(db.create_database("", &[]), "default");
    already_exists(db.create_database("default", &[]), "default");
    already_exists(db.create_database("d1", &[]), "d1");
    already_exists(
        db.create_database("d1", &pairs(&[("timezone", "UTC")])),
        "d1",
    );
    for name in ["a/b", "a\\b", ".", "..", "a\0b", &"x".repeat(256)] {
        assert!(
            matches!(
                db.create_database(name, &pairs(&[("timezone", "UTC")])),
                Err(EngineError::InvalidArgument(_))
            ),
            "{name:?}"
        );
    }
    assert_eq!(
        db.create_database(&"x".repeat(256), &[])
            .unwrap_err()
            .to_string(),
        "database name too long (256 bytes, max 255)"
    );
    // 坏的 timezone 在创建之前报 6，零写入；合法值原样保存。
    for (request, message) in [
        (
            pairs(&[("a", "1"), ("timezone", "Mars/Base")]),
            "unknown timezone 'Mars/Base'",
        ),
        (
            pairs(&[("timezone", "")]),
            "timezone property must be a non-empty string",
        ),
    ] {
        match db.create_database("tz", &request) {
            Err(EngineError::SchemaValidation(m)) => assert_eq!(m, message),
            other => panic!("{other:?}"),
        }
    }
    assert!(!path.join("databases/tz").exists());
    assert!(staging_entries(&path).is_empty());
    db.create_database("tz", &pairs(&[("timezone", "utc")]))
        .unwrap();
    assert_eq!(describe(&db, "tz").1, pairs(&[("timezone", "utc")]));
    assert_eq!(db.database_timezone("tz").unwrap().as_deref(), Some("utc"));
    db.drop_database("tz").unwrap();
    // 空格、冒号、非 ASCII、255 字节都是合法名字。
    for name in [" sp ", "a:b", "库一", &"y".repeat(255)] {
        db.create_database(name, &[]).unwrap();
    }
    // 路径被普通文件占用：已存在，不覆盖。
    std::fs::write(path.join("databases/afile"), b"keep").unwrap();
    already_exists(db.create_database("afile", &[]), "afile");
    assert_eq!(
        std::fs::read(path.join("databases/afile")).unwrap(),
        b"keep"
    );
    missing(db.describe_database("afile"), "afile");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.path().join("nowhere"), path.join("databases/dangling"))
            .unwrap();
        already_exists(db.create_database("dangling", &[]), "dangling");
    }
    // 只差大小写的名字：大小写不敏感的文件系统上路径已存在，报已存在；敏感时是另一个库。
    if case_insensitive(root.path()) {
        already_exists(db.create_database("D1", &[]), "D1");
        missing(db.describe_database("D1"), "D1");
    } else {
        db.create_database("D1", &[]).unwrap();
    }
    assert!(staging_entries(&path).is_empty());
    db.close();
}

#[cfg(unix)]
#[test]
fn create_database_follows_a_symlinked_databases_root() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    let elsewhere = root.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::create_dir_all(&path).unwrap();
    std::os::unix::fs::symlink(&elsewhere, path.join("databases")).unwrap();
    let db = open(&path);
    db.create_database("d1", &pairs(&[("k", "v")])).unwrap();
    assert!(elsewhere.join("d1/collections").is_dir());
    assert_eq!(db.list_databases(), ["d1", "default"]);
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn create_database_failures_before_the_rename_change_nothing() {
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
    };
    for fault in [DirectoryFault::Error, DirectoryFault::Panic] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("m.db");
        let db = open(&path);
        inject_directory_fault_once(
            DirectoryOperation::CreateDatabase,
            DirectoryPoint::BeforeRename,
            fault,
        );
        let error = db.create_database("d1", &pairs(&[("k", "v")])).unwrap_err();
        assert!(error.to_string().contains("storage error"), "{error}");
        assert!(!path.join("databases/d1").exists());
        assert!(staging_entries(&path).is_empty());
        assert_eq!(db.list_databases(), ["default"]);
        // 写入没有被封锁，同名创建照常成功。
        db.create_collection("", "c", schema()).unwrap();
        db.create_database("d1", &pairs(&[("k", "v")])).unwrap();
        assert_eq!(describe(&db, "d1").1, pairs(&[("k", "v")]));
        db.close();
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn create_database_failures_after_the_rename_publish_and_seal_writes() {
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
    };
    for point in [
        DirectoryPoint::AfterRename,
        DirectoryPoint::CollectionsSync,
        DirectoryPoint::StagingSync,
    ] {
        for fault in [DirectoryFault::Error, DirectoryFault::Panic] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("m.db");
            let db = open(&path);
            db.create_collection("", "c", schema()).unwrap();
            inject_directory_fault_once(DirectoryOperation::CreateDatabase, point, fault);
            let error = db.create_database("d1", &pairs(&[("k", "v")])).unwrap_err();
            assert!(
                error.to_string().contains("durability is unconfirmed"),
                "{point:?}: {error}"
            );
            // 提交点已过：库已发布且完整，写入全部封锁，读照常。
            assert_eq!(db.list_databases(), ["d1", "default"], "{point:?}");
            assert_eq!(describe(&db, "d1").1, pairs(&[("k", "v")]));
            assert!(path.join("databases/d1/collections").is_dir());
            for result in [
                db.insert("", "c", batch(&[1])).map(|_| ()),
                db.create_database("d2", &[]),
                db.alter_database("d1", &pairs(&[("n", "1")]), &[]),
            ] {
                assert!(result.unwrap_err().to_string().contains("blocked"));
            }
            assert!(db.collection("", "c").is_ok());
            db.close();
            drop(db);
            let db = open(&path);
            assert_eq!(describe(&db, "d1").1, pairs(&[("k", "v")]));
            db.create_collection("d1", "c", schema()).unwrap();
            db.close();
        }
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn create_database_name_verification_failure_rolls_back_or_seals() {
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
    };
    // 核对不通过：改回 staging 并删除，状态不变，写入不封锁。
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    let db = open(&path);
    inject_directory_fault_once(
        DirectoryOperation::CreateDatabase,
        DirectoryPoint::NameVerification,
        DirectoryFault::Error,
    );
    let error = db.create_database("d1", &[]).unwrap_err().to_string();
    assert!(error.contains("normalized by the filesystem"), "{error}");
    assert!(!path.join("databases/d1").exists());
    assert!(staging_entries(&path).is_empty());
    assert_eq!(db.list_databases(), ["default"]);
    db.create_database("d1", &[]).unwrap();
    db.close();
    drop(db);

    // 核对不通过且改回也失败：整库写入封锁，目录留在原处，重开后是完整的库。
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    let db = open(&path);
    for point in [DirectoryPoint::NameVerification, DirectoryPoint::Rollback] {
        inject_directory_fault_once(
            DirectoryOperation::CreateDatabase,
            point,
            DirectoryFault::Error,
        );
    }
    let error = db
        .create_database("d1", &pairs(&[("k", "v")]))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("rollback durability is unconfirmed"),
        "{error}"
    );
    assert_eq!(db.list_databases(), ["default"]);
    assert!(
        db.create_collection("", "c", schema())
            .unwrap_err()
            .to_string()
            .contains("blocked")
    );
    db.close();
    drop(db);
    let db = open(&path);
    assert_eq!(describe(&db, "d1").1, pairs(&[("k", "v")]));
    db.close();
}

/// 进程在改名进 `databases/` 前后退出：重开只有「未创建」或「已创建且完整」，staging 残留被清空。
#[cfg(feature = "fault-injection")]
#[test]
fn process_exit_around_database_creation_reopens_before_or_after() {
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
    };
    use std::process::Command;
    let test = "process_exit_around_database_creation_reopens_before_or_after";
    if let Ok(stage) = std::env::var("MILVUS_LITE_DB_CREATE_CRASH") {
        let path = std::path::PathBuf::from(std::env::var("MILVUS_LITE_CRASH_DIR").unwrap());
        let db = open(&path);
        let point = if stage == "before" {
            DirectoryPoint::BeforeRename
        } else {
            DirectoryPoint::AfterRename
        };
        inject_directory_fault_once(
            DirectoryOperation::CreateDatabase,
            point,
            DirectoryFault::Exit,
        );
        db.create_database("d1", &pairs(&[("k", "v")])).unwrap();
        panic!("database creation crash injection did not fire");
    }
    if std::env::var_os("MILVUS_LITE_DB_CREATE_PARENT").is_none() {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("MILVUS_LITE_DB_CREATE_PARENT", "1")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "isolated recovery parent failed: {status:?}"
        );
        return;
    }
    for (stage, created) in [("before", false), ("after", true)] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("create-crash.db");
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("MILVUS_LITE_DB_CREATE_CRASH", stage)
            .env("MILVUS_LITE_CRASH_DIR", &path)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(88), "{stage}");
        let db = open(&path);
        assert!(staging_entries(&path).is_empty(), "{stage}");
        if created {
            assert_eq!(db.list_databases(), ["d1", "default"], "{stage}");
            assert_eq!(describe(&db, "d1").1, pairs(&[("k", "v")]), "{stage}");
            assert!(path.join("databases/d1/collections").is_dir());
            db.create_collection("d1", "c", schema()).unwrap();
        } else {
            assert_eq!(db.list_databases(), ["default"], "{stage}");
            assert!(!path.join("databases/d1").exists());
            db.create_database("d1", &[]).unwrap();
        }
        db.close();
    }
}

fn not_empty(result: Result<(), EngineError>, name: &str) {
    match result {
        Err(EngineError::DatabaseNotEmpty(found)) => assert_eq!(found, name),
        other => panic!("expected DatabaseNotEmpty for {name}: {other:?}"),
    }
}

#[test]
fn drop_database_checks_default_then_existence_then_emptiness() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    let db = open(&path);
    for name in ["", "default"] {
        match db.drop_database(name) {
            Err(EngineError::InvalidArgument(reason)) => {
                assert_eq!(reason, "cannot drop default database");
            }
            other => panic!("expected InvalidArgument: {other:?}"),
        }
    }
    for name in ["a/b", ".", &"x".repeat(256)] {
        assert!(matches!(
            db.drop_database(name),
            Err(EngineError::InvalidArgument(_))
        ));
    }
    missing(db.drop_database("nope"), "nope");

    db.create_database("d1", &pairs(&[("k", "v")])).unwrap();
    db.create_database("empty", &[]).unwrap();
    db.create_collection("d1", "c", schema()).unwrap();
    db.insert("d1", "c", batch(&[1])).unwrap();
    assert_eq!(
        db.drop_database("d1").unwrap_err().to_string(),
        "database 'd1' is not empty"
    );
    not_empty(db.drop_database("d1"), "d1");
    assert_eq!(names(&db, "d1"), ["c"]);
    db.drop_database("empty").unwrap();
    assert!(!path.join("databases/empty").exists());
    assert!(staging_entries(&path).is_empty());
    assert_eq!(db.list_databases(), ["d1", "default"]);
    missing(db.describe_database("empty"), "empty");
    missing(db.list_collections("empty"), "empty");
    // 集合删除后库即为空，可以删除；databases/ 本身保留。
    db.drop_collection("d1", "c").unwrap();
    db.drop_database("d1").unwrap();
    assert_eq!(db.list_databases(), ["default"]);
    assert!(path.join("databases").is_dir());
    assert!(staging_entries(&path).is_empty());

    // 删除之后同名重建得到全新的库：属性、别名、集合都不残留。
    db.create_database("d1", &[]).unwrap();
    assert!(describe(&db, "d1").1.is_empty());
    assert!(names(&db, "d1").is_empty());
    assert!(db.list_aliases("d1", "").unwrap().1.is_empty());
    db.create_collection("d1", "c", schema()).unwrap();
    db.close();
    drop(db);
    let db = open(&path);
    assert_eq!(db.list_databases(), ["d1", "default"]);
    db.close();
}

#[test]
fn drop_database_ignores_residue_and_damaged_metadata_but_removes_everything() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    let d1 = path.join("databases/d1");
    // 残留目录（无 schema.json）、悬空别名、损坏的属性与别名文件、改名意图都不阻止删除。
    std::fs::create_dir_all(d1.join("collections/residue")).unwrap();
    std::fs::write(d1.join("aliases.json"), br#"{"dangling": "nope"}"#).unwrap();
    std::fs::write(d1.join("database_properties.json"), b"{").unwrap();
    std::fs::write(d1.join("milvus-lite-rust.rename-intent.json"), b"not json").unwrap();
    std::fs::write(d1.join("collections/stray-file"), b"x").unwrap();
    let db = open(&path);
    db.drop_database("d1").unwrap();
    assert!(!d1.exists());
    assert!(staging_entries(&path).is_empty());
    assert_eq!(db.list_databases(), ["default"]);
    // 默认库不受影响。
    db.create_collection("", "c", schema()).unwrap();
    db.close();
}

#[test]
fn drop_database_uses_exact_names() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    let db = open(&path);
    db.create_database("d1", &[]).unwrap();
    // 只差大小写的名字：按精确名字报不存在，不删除另一个库（大小写不敏感的文件系统上 3.x 会删除）。
    if case_insensitive(root.path()) {
        missing(db.drop_database("D1"), "D1");
        assert!(path.join("databases/d1").is_dir());
        assert_eq!(db.list_databases(), ["d1", "default"]);
    }
    db.close();
}

#[cfg(unix)]
#[test]
fn drop_database_never_follows_a_symlinked_database() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    let databases = path.join("databases");
    std::fs::create_dir_all(&databases).unwrap();
    let empty = root.path().join("elsewhere-empty");
    let full = root.path().join("elsewhere-full");
    std::fs::create_dir_all(empty.join("collections")).unwrap();
    std::fs::write(empty.join("keep.txt"), b"keep").unwrap();
    std::fs::create_dir_all(full.join("collections/c")).unwrap();
    std::fs::write(full.join("collections/c/schema.json"), b"{}").unwrap();
    std::os::unix::fs::symlink(&empty, databases.join("empty")).unwrap();
    std::os::unix::fs::symlink(&full, databases.join("full")).unwrap();
    let db = open(&path);
    // 空库：不跟随删除，报存储错误，链接与目标都保留；非空：与目录形式的库一样报非空。
    assert!(matches!(
        db.drop_database("empty"),
        Err(EngineError::Storage(_))
    ));
    assert!(
        std::fs::symlink_metadata(databases.join("empty"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(empty.join("keep.txt").is_file());
    not_empty(db.drop_database("full"), "full");
    assert_eq!(db.list_databases(), ["default", "empty", "full"]);
    assert!(staging_entries(&path).is_empty());
    db.close();
}

#[test]
fn concurrent_drop_and_create_collection_never_leave_a_collection_in_a_missing_database() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    let db = open(&path);
    for round in 0..30 {
        let name = format!("r{round}");
        db.create_database(&name, &[]).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let dropper = {
            let (db, name, barrier) = (db.clone(), name.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                db.drop_database(&name)
            })
        };
        let creator = {
            let (db, name, barrier) = (db.clone(), name.clone(), barrier);
            std::thread::spawn(move || {
                barrier.wait();
                db.create_collection(&name, "c", schema())
            })
        };
        let dropped = dropper.join().unwrap();
        let created = creator.join().unwrap();
        match (dropped, created) {
            // 先创建集合：库非空，删除失败。
            (Err(EngineError::DatabaseNotEmpty(_)), Ok(())) => {
                assert_eq!(names(&db, &name), ["c"], "round {round}");
                db.drop_collection(&name, "c").unwrap();
                db.drop_database(&name).unwrap();
            }
            // 先删除库：创建集合报数据库不存在。
            (Ok(()), Err(EngineError::DatabaseNotFound(_))) => {}
            other => panic!("round {round}: unexpected outcome {other:?}"),
        }
        assert!(
            !path.join("databases").join(&name).exists(),
            "round {round}"
        );
        missing(db.describe_database(&name), &name);
    }
    assert!(staging_entries(&path).is_empty());
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn drop_database_failure_before_the_rename_changes_nothing() {
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
    };
    for fault in [DirectoryFault::Error, DirectoryFault::Panic] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("m.db");
        let db = open(&path);
        db.create_database("d1", &pairs(&[("k", "v")])).unwrap();
        inject_directory_fault_once(
            DirectoryOperation::DropDatabase,
            DirectoryPoint::BeforeRename,
            fault,
        );
        let error = db.drop_database("d1").unwrap_err();
        assert!(error.to_string().contains("storage error"), "{error}");
        assert_eq!(db.list_databases(), ["d1", "default"]);
        assert_eq!(describe(&db, "d1").1, pairs(&[("k", "v")]));
        assert!(staging_entries(&path).is_empty());
        // 写入没有被封锁，重试成功。
        db.create_collection("d1", "c", schema()).unwrap();
        db.drop_collection("d1", "c").unwrap();
        db.drop_database("d1").unwrap();
        db.close();
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn drop_database_failures_after_the_rename_remove_the_database_and_seal_writes() {
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
    };
    for point in [
        DirectoryPoint::AfterRename,
        DirectoryPoint::CollectionsSync,
        DirectoryPoint::StagingSync,
    ] {
        for fault in [DirectoryFault::Error, DirectoryFault::Panic] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("m.db");
            let db = open(&path);
            db.create_collection("", "c", schema()).unwrap();
            db.create_database("d1", &[]).unwrap();
            inject_directory_fault_once(DirectoryOperation::DropDatabase, point, fault);
            let error = db.drop_database("d1").unwrap_err();
            assert!(
                error.to_string().contains("durability is unconfirmed"),
                "{point:?}: {error}"
            );
            // 提交点已过：库已不存在，写入全部封锁，读照常。
            assert_eq!(db.list_databases(), ["default"], "{point:?}");
            missing(db.describe_database("d1"), "d1");
            for result in [
                db.insert("", "c", batch(&[1])).map(|_| ()),
                db.create_database("d1", &[]),
                db.drop_database("d1"),
            ] {
                assert!(result.is_err());
            }
            assert!(db.collection("", "c").is_ok());
            db.close();
            drop(db);
            // 重开：库不存在，staging 中的残留被清空。
            let db = open(&path);
            assert_eq!(db.list_databases(), ["default"], "{point:?}");
            assert!(staging_entries(&path).is_empty());
            db.create_database("d1", &[]).unwrap();
            db.close();
        }
    }
}

/// 进程在改名进 staging 前后退出：重开只有「未删除」或「已删除」，staging 残留被清空。
#[cfg(feature = "fault-injection")]
#[test]
fn process_exit_around_database_drop_reopens_before_or_after() {
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
    };
    use std::process::Command;
    let test = "process_exit_around_database_drop_reopens_before_or_after";
    if let Ok(stage) = std::env::var("MILVUS_LITE_DB_DROP_CRASH") {
        let path = std::path::PathBuf::from(std::env::var("MILVUS_LITE_CRASH_DIR").unwrap());
        let db = open(&path);
        db.create_database("d1", &pairs(&[("k", "v")])).unwrap();
        let point = if stage == "before" {
            DirectoryPoint::BeforeRename
        } else {
            DirectoryPoint::AfterRename
        };
        inject_directory_fault_once(
            DirectoryOperation::DropDatabase,
            point,
            DirectoryFault::Exit,
        );
        db.drop_database("d1").unwrap();
        panic!("database drop crash injection did not fire");
    }
    if std::env::var_os("MILVUS_LITE_DB_DROP_PARENT").is_none() {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("MILVUS_LITE_DB_DROP_PARENT", "1")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "isolated recovery parent failed: {status:?}"
        );
        return;
    }
    for (stage, dropped) in [("before", false), ("after", true)] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("drop-crash.db");
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("MILVUS_LITE_DB_DROP_CRASH", stage)
            .env("MILVUS_LITE_CRASH_DIR", &path)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(88), "{stage}");
        let db = open(&path);
        assert!(staging_entries(&path).is_empty(), "{stage}");
        if dropped {
            assert_eq!(db.list_databases(), ["default"], "{stage}");
            assert!(!path.join("databases/d1").exists());
        } else {
            assert_eq!(db.list_databases(), ["d1", "default"], "{stage}");
            assert_eq!(describe(&db, "d1").1, pairs(&[("k", "v")]), "{stage}");
        }
        // 两种结果之后创建与删除都照常。
        db.create_database("again", &[]).unwrap();
        db.drop_database("again").unwrap();
        db.close();
    }
}

/// 清空、改名与别名写入在非默认库内进程退出：意图文件在 `databases/<db>/` 与集合目录，重开后只有「操作前」或
/// 「操作后」，意图被收尾（0023 B：每个库各有别名、改名意图与清空意图）。
#[cfg(feature = "fault-injection")]
#[test]
fn process_exit_in_a_non_default_database_settles_management_intents() {
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, inject_directory_fault_once,
    };
    use std::process::Command;
    let test = "process_exit_in_a_non_default_database_settles_management_intents";
    // （操作:阶段，是否已生效）
    let stages = [
        ("rename:directory", false),
        ("rename:collections-sync", true),
        ("rename:aliases-after", true),
        ("truncate:intent-after", false),
        ("truncate:rebuild", true),
        ("truncate:files", true),
        ("alias:before", false),
        ("alias:after", true),
    ];
    if let Ok(stage) = std::env::var("MILVUS_LITE_DB_MGMT_CRASH") {
        let path = std::path::PathBuf::from(std::env::var("MILVUS_LITE_CRASH_DIR").unwrap());
        let db = open(&path);
        db.create_database("d1", &[]).unwrap();
        db.create_collection("d1", "c", schema()).unwrap();
        db.insert("d1", "c", batch(&[1, 2])).unwrap();
        db.create_alias("d1", "c", "a").unwrap();
        db.flush_collection("d1", "c").unwrap();
        let (operation, point) = match stage.as_str() {
            "rename:directory" => (DirectoryOperation::Rename, DirectoryPoint::BeforeRename),
            "rename:collections-sync" => {
                (DirectoryOperation::Rename, DirectoryPoint::CollectionsSync)
            }
            "rename:aliases-after" => (
                DirectoryOperation::Rename,
                DirectoryPoint::AliasesAfterRename,
            ),
            "truncate:intent-after" => (
                DirectoryOperation::Truncate,
                DirectoryPoint::IntentAfterRename,
            ),
            "truncate:rebuild" => (DirectoryOperation::Truncate, DirectoryPoint::Rebuild),
            "truncate:files" => (DirectoryOperation::Truncate, DirectoryPoint::Files),
            "alias:before" => (DirectoryOperation::Alias, DirectoryPoint::Aliases),
            _ => (
                DirectoryOperation::Alias,
                DirectoryPoint::AliasesAfterRename,
            ),
        };
        inject_directory_fault_once(operation, point, DirectoryFault::Exit);
        match stage.split(':').next().unwrap() {
            "rename" => db.rename_collection("d1", "c", "d", "").unwrap(),
            "truncate" => db.truncate_collection("d1", "c").unwrap(),
            _ => db.create_alias("d1", "c", "b").unwrap(),
        }
        panic!("management crash injection did not fire");
    }
    if std::env::var_os("MILVUS_LITE_DB_MGMT_PARENT").is_none() {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("MILVUS_LITE_DB_MGMT_PARENT", "1")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "isolated recovery parent failed: {status:?}"
        );
        return;
    }
    for (stage, effective) in stages {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("mgmt-crash.db");
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("MILVUS_LITE_DB_MGMT_CRASH", stage)
            .env("MILVUS_LITE_CRASH_DIR", &path)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(88), "{stage}");
        let db = open(&path);
        let d1 = path.join("databases/d1");
        // 数据库根目录上的改名意图在打开时已收尾；默认库不受影响。
        assert!(
            !d1.join("milvus-lite-rust.rename-intent.json").exists(),
            "{stage}"
        );
        assert!(names(&db, "").is_empty(), "{stage}");
        match stage.split(':').next().unwrap() {
            "rename" => {
                let expected = if effective { "d" } else { "c" };
                assert_eq!(names(&db, "d1"), [expected], "{stage}");
                assert_eq!(db.describe_alias("d1", "a").unwrap(), expected, "{stage}");
                assert_eq!(
                    db.collection("d1", "a").unwrap().read().count(),
                    2,
                    "{stage}"
                );
                let schema_text = std::fs::read_to_string(
                    d1.join("collections").join(expected).join("schema.json"),
                )
                .unwrap();
                assert!(
                    schema_text.contains(&format!("\"collection_name\": \"{expected}\"")),
                    "{stage}"
                );
            }
            "truncate" => {
                assert_eq!(names(&db, "d1"), ["c"], "{stage}");
                // 清空意图在集合首次访问时收尾。
                let count = db.collection("d1", "c").unwrap().read().count();
                assert_eq!(count, if effective { 0 } else { 2 }, "{stage}");
                assert!(
                    !d1.join("collections/c/milvus-lite-rust.truncate-intent.json")
                        .exists(),
                    "{stage}"
                );
                db.insert("d1", "c", batch(&[9])).unwrap();
                assert_eq!(db.describe_alias("d1", "a").unwrap(), "c", "{stage}");
            }
            _ => {
                assert_eq!(db.describe_alias("d1", "a").unwrap(), "c", "{stage}");
                assert_eq!(db.describe_alias("d1", "b").is_ok(), effective, "{stage}");
                db.create_alias("d1", "c", "later").unwrap();
            }
        }
        assert!(!path.join("aliases.json").exists(), "{stage}");
        db.close();
    }
}

#[test]
fn concurrent_creates_of_one_name_succeed_exactly_once() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    let db = open(&path);
    for round in 0..20 {
        let name = format!("r{round}");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let workers: Vec<_> = (0..4)
            .map(|worker| {
                let (db, name, barrier) = (db.clone(), name.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    db.create_database(&name, &pairs(&[("winner", &worker.to_string())]))
                })
            })
            .collect();
        let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "round {round}"
        );
        for result in results.into_iter().filter(Result::is_err) {
            assert!(matches!(result, Err(EngineError::DatabaseAlreadyExists(_))));
        }
        // 缓存与文件都来自同一个胜者。
        let (_, cached) = describe(&db, &name);
        let on_disk = properties_file(&path.join("databases").join(&name)).unwrap();
        assert_eq!(
            on_disk,
            format!("{{\n  \"winner\": \"{}\"\n}}", cached[0].1)
        );
    }
    assert!(staging_entries(&path).is_empty());
    assert_eq!(db.list_databases().len(), 21);
    db.close();
}

#[test]
fn concurrent_alters_are_serialized_and_the_file_matches_the_cache() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    let db = open(&path);
    db.create_database("d1", &[]).unwrap();
    let workers: Vec<_> = (0..8)
        .map(|worker| {
            let db = db.clone();
            std::thread::spawn(move || {
                for step in 0..5 {
                    let key = format!("k{worker}-{step}");
                    db.alter_database("d1", &pairs(&[(&key, "v")]), &[])
                        .unwrap();
                    assert!(describe(&db, "d1").1.iter().any(|(k, _)| *k == key));
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    let (_, cached) = describe(&db, "d1");
    assert_eq!(cached.len(), 40);
    db.close();
    drop(db);
    let db = open(&path);
    let mut reread = describe(&db, "d1").1;
    let mut expected = cached;
    reread.sort();
    expected.sort();
    assert_eq!(reread, expected);
    db.close();
}

/// 他库内的集合 create / drop / 改名 / 写入与数据库的创建、删除并发：独占许可使两者互斥，互不丢失、互不报错。
#[test]
fn database_lifecycle_does_not_disturb_activity_in_other_databases() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    let db = open(&path);
    db.create_database("busy", &[]).unwrap();
    db.create_collection("busy", "base", schema()).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = {
        let (db, stop) = (db.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut rounds = 0_i64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                db.create_collection("busy", "tmp", schema()).unwrap();
                db.insert("busy", "tmp", batch(&[rounds])).unwrap();
                db.rename_collection("busy", "tmp", "moved", "").unwrap();
                db.insert("busy", "base", batch(&[rounds])).unwrap();
                db.drop_collection("busy", "moved").unwrap();
                rounds += 1;
            }
            rounds
        })
    };
    for round in 0..40 {
        let name = format!("life{round}");
        db.create_database(&name, &pairs(&[("round", &round.to_string())]))
            .unwrap();
        db.alter_database(&name, &pairs(&[("k", "v")]), &[])
            .unwrap();
        db.drop_database(&name).unwrap();
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(worker.join().unwrap() > 0);
    assert_eq!(db.list_databases(), ["busy", "default"]);
    assert_eq!(names(&db, "busy"), ["base"]);
    assert!(staging_entries(&path).is_empty());
    db.close();
}

/// 读请求在 Drop 的提交点附近取得旧状态：只会得到数据库不存在、删除前的属性，或（属性尚未加载时）空属性，
/// 不会报存储错误或 panic（3.x 无锁，同类竞态；验收记录说明该行为）。
#[test]
fn describe_racing_a_drop_never_fails_with_a_storage_error() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    for round in 0..30 {
        let db = open(&path);
        let name = format!("race{round}");
        db.create_database(&name, &pairs(&[("k", "v")])).unwrap();
        db.close();
        drop(db);
        // 重开后属性缓存为空，读者在 Drop 窗口内要从文件加载。
        let db = open(&path);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let reader = {
            let (db, name, barrier) = (db.clone(), name.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                (0..50)
                    .map(|_| db.describe_database(&name))
                    .collect::<Vec<_>>()
            })
        };
        barrier.wait();
        db.drop_database(&name).unwrap();
        for result in reader.join().unwrap() {
            match result {
                Ok((found, properties)) => {
                    assert_eq!(found, name);
                    assert!(properties.is_empty() || properties == pairs(&[("k", "v")]));
                }
                Err(EngineError::DatabaseNotFound(found)) => assert_eq!(found, name),
                Err(other) => panic!("round {round}: unexpected {other:?}"),
            }
        }
        db.close();
    }
}
