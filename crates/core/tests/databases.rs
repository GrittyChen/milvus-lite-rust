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
fn timezone_is_rejected_before_any_write_but_existing_values_stay_manageable() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    std::fs::create_dir_all(path.join("databases/d1")).unwrap();
    // 3.x 已写入的 timezone：可以读取、展示与删除，值不校验。
    let file = path.join("databases/d1/database_properties.json");
    std::fs::write(&file, br#"{"timezone": "Asia/Shanghai", "k": "v"}"#).unwrap();
    let db = open(&path);
    assert_eq!(
        describe(&db, "d1").1,
        pairs(&[("timezone", "Asia/Shanghai"), ("k", "v")])
    );
    let before = std::fs::read(&file).unwrap();
    for request in [
        pairs(&[("timezone", "UTC")]),
        pairs(&[("timezone", "Asia/Shanghai")]),
        pairs(&[("other", "x"), ("timezone", "Mars/Base")]),
    ] {
        match db.alter_database("d1", &request, &[]) {
            Err(EngineError::Unsupported { .. }) => {}
            other => panic!("expected Unsupported: {other:?}"),
        }
        assert_eq!(std::fs::read(&file).unwrap(), before);
    }
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
    // timezone 在创建之前返回 Unsupported，零写入。
    for request in [
        pairs(&[("timezone", "UTC")]),
        pairs(&[("a", "1"), ("timezone", "Mars/Base")]),
    ] {
        assert!(matches!(
            db.create_database("tz", &request),
            Err(EngineError::Unsupported { .. })
        ));
    }
    assert!(!path.join("databases/tz").exists());
    assert!(staging_entries(&path).is_empty());
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
