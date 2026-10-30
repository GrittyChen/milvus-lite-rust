//! 0023 A 改名：判定顺序与零写入、别名改指与 `schema.json`、改名后的 load 状态、重开时按意图收尾、
//! 改名未完成状态，以及各阶段的故障与进程退出。gRPC 码与文案、磁盘字节与 3.x 的逐行对照见差分测试。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, EngineError, FieldSchema, IndexRequest,
    InputValues, InsertBatch, LoadState, Pk,
};

const INTENT: &str = "milvus-lite-rust.rename-intent.json";

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

fn count(db: &Db, name: &str) -> usize {
    db.collection("", name).unwrap().read().count()
}

fn schema_name(root: &Path, name: &str) -> String {
    let text =
        std::fs::read_to_string(root.join("collections").join(name).join("schema.json")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value["collection_name"].as_str().unwrap().to_owned()
}

fn aliases(root: &Path) -> Option<String> {
    std::fs::read_to_string(root.join("aliases.json")).ok()
}

fn write_intent(root: &Path, old: &str, new: &str) {
    std::fs::write(
        root.join(INTENT),
        format!(
            r#"{{"format_version":1,"operation":"rename_collection","old":"{old}","new":"{new}"}}"#
        ),
    )
    .unwrap();
}

/// 数据目录下全部路径与文件字节，用于断言零写入。
fn tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut entries = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if path.is_dir() {
                entries.insert(relative, None);
                stack.push(path);
            } else {
                entries.insert(relative, Some(std::fs::read(&path).unwrap()));
            }
        }
    }
    entries
}

fn unfinished(result: Result<(), EngineError>) -> String {
    let error = result.unwrap_err().to_string();
    assert!(error.contains("rename is unfinished"), "{error}");
    error
}

#[test]
fn rename_moves_the_collection_and_repoints_aliases_like_3x() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1, 2])).unwrap();
    db.flush_collection("", "c").unwrap();
    db.insert("", "c", batch(&[3])).unwrap();
    db.create_collection("", "o", schema()).unwrap();
    for (target, alias) in [("c", "a"), ("c", "b"), ("o", "z")] {
        db.create_alias("", target, alias).unwrap();
    }

    // 旧名可以是别名；`new_db` 为空或等于当前数据库时照常改名。
    db.rename_collection("", "a", "d", "default").unwrap();
    assert!(matches!(
        db.collection("", "c"),
        Err(EngineError::CollectionNotFound(name)) if name == "c"
    ));
    assert_eq!(count(&db, "d"), 3);
    assert_eq!(count(&db, "a"), 3);
    assert_eq!(
        aliases(root.path()).unwrap(),
        "{\n  \"a\": \"d\",\n  \"b\": \"d\",\n  \"z\": \"o\"\n}"
    );
    assert_eq!(schema_name(root.path(), "d"), "d");
    assert!(!root.path().join(INTENT).exists());
    assert!(!root.path().join("collections/c").exists());
    let names: Vec<_> = db
        .list_collections("")
        .unwrap()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(names, ["d", "o"]);

    db.insert("", "d", batch(&[4])).unwrap();
    db.flush_collection("", "d").unwrap();
    db.rename_collection("default", "d", "e", "").unwrap();
    db.insert("", "e", batch(&[5])).unwrap();
    db.close();
    let db = open(root.path());
    assert_eq!(count(&db, "e"), 5);
    assert_eq!(db.describe_alias("", "b").unwrap(), "e");
    // 旧名可以再次使用。
    db.create_collection("", "c", schema()).unwrap();
    assert_eq!(count(&db, "c"), 0);
    db.close();

    // 3.x 改名总会写出 `aliases.json`，没有别名时为 `{}`。
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    assert!(aliases(root.path()).is_none());
    db.rename_collection("", "c", "d", "").unwrap();
    assert_eq!(aliases(root.path()).unwrap(), "{}");
    db.close();
}

#[test]
fn rename_checks_follow_3x_order_and_write_nothing_on_rejection() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    for name in ["c", "o"] {
        db.create_collection("", name, schema()).unwrap();
    }
    db.close();
    std::fs::write(
        root.path().join("aliases.json"),
        br#"{"al":"o","ghost":"missing"}"#,
    )
    .unwrap();
    std::fs::create_dir(root.path().join("collections/residue")).unwrap();
    std::fs::write(root.path().join("collections/plain"), b"x").unwrap();
    let db = open(root.path());
    let before = tree(root.path());

    // 跨数据库在旧名判定之前拒绝（取舍 4）。
    for (old, new_db) in [("nope", "other"), ("c", "other"), ("c", "Default")] {
        assert!(matches!(
            db.rename_collection("", old, "x", new_db),
            Err(EngineError::Unsupported {
                feature: "cross-database rename",
                ..
            })
        ));
    }
    // 旧名不存在先于新名校验；非法的旧名与 3.x 相同视为不存在。
    for old in ["nope", "a/b", "", "..", "ghost"] {
        let expected = if old == "ghost" { "missing" } else { old };
        assert!(
            matches!(
                db.rename_collection("", old, "bad/new", ""),
                Err(EngineError::CollectionNotFound(ref name)) if name == expected
            ),
            "{old}"
        );
    }
    assert_eq!(
        db.rename_collection("", "c", "a/b", "")
            .unwrap_err()
            .to_string(),
        "invalid collection name: 'a/b'"
    );
    assert_eq!(
        db.rename_collection("", "c", "", "")
            .unwrap_err()
            .to_string(),
        "collection name must not be empty"
    );
    // 新名是集合、别名（含悬空别名）、与旧名相同、残留目录或文件：一律「已存在」，不覆盖。
    let mut taken = vec!["o", "al", "ghost", "c", "residue", "plain"];
    if root.path().join("collections/C").exists() {
        taken.push("C");
    }
    for new in taken {
        assert_eq!(
            db.rename_collection("", "c", new, "")
                .unwrap_err()
                .to_string(),
            format!("collection '{new}' already exists")
        );
    }
    assert_eq!(tree(root.path()), before);
    assert_eq!(count(&db, "c"), 0);
    db.close();
}

#[test]
fn schema_shape_is_checked_before_changes_and_out_of_scope_schemas_still_rename() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("collections/c/schema.json");
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.close();
    let original = std::fs::read_to_string(&path).unwrap();

    for broken in [
        "[]".to_owned(),
        original.replace("\"collection_name\": \"c\"", "\"collection_name\": 1"),
    ] {
        std::fs::write(&path, &broken).unwrap();
        let db = open(root.path());
        let before = tree(root.path());
        let error = db.rename_collection("", "c", "d", "").unwrap_err();
        assert!(matches!(error, EngineError::Storage(_)), "{error}");
        assert_eq!(tree(root.path()), before);
        db.close();
    }

    // 0006 第 14 项的范围外 schema：只改原始 JSON 的 `collection_name`，其余原样保留。
    let out_of_scope = original.replace("\"properties\": {}", "\"properties\": {\"n\": 1}");
    assert_ne!(out_of_scope, original);
    std::fs::write(&path, &out_of_scope).unwrap();
    let db = open(root.path());
    assert!(matches!(
        db.collection("", "c"),
        Err(EngineError::Unsupported { .. })
    ));
    db.rename_collection("", "c", "d", "").unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("collections/d/schema.json")).unwrap(),
        original
            .replace("\"collection_name\": \"c\"", "\"collection_name\": \"d\"")
            .replace("\"properties\": {}", "\"properties\": {\n    \"n\": 1\n  }")
    );
    assert!(matches!(
        db.collection("", "d"),
        Err(EngineError::Unsupported { .. })
    ));
    db.close();
}

#[test]
fn renamed_collections_reopen_released_when_they_have_index_definitions() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    for name in ["c", "plain"] {
        db.create_collection("", name, schema()).unwrap();
        db.insert("", name, batch(&[1])).unwrap();
    }
    db.create_index(
        "",
        "c",
        "v",
        IndexRequest {
            index_type: "FLAT".to_owned(),
            metric_type: Some("L2".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    db.collection("", "c").unwrap().write().load().unwrap();
    assert_eq!(
        db.collection("", "c").unwrap().read().load_state(),
        LoadState::Loaded
    );
    db.rename_collection("", "c", "d", "").unwrap();
    db.rename_collection("", "plain", "p2", "").unwrap();
    let states: BTreeMap<_, _> = db.list_collections("").unwrap().into_iter().collect();
    assert_eq!(states["d"], LoadState::Released);
    assert_eq!(states["p2"], LoadState::Loaded);
    let renamed = db.collection("", "d").unwrap();
    assert!(matches!(
        renamed.read().require_loaded(),
        Err(EngineError::CollectionNotLoaded(name)) if name == "d"
    ));
    renamed.write().load().unwrap();
    assert_eq!(
        renamed.read().get(&[Pk::Int64(1)], &[]).unwrap().rows.pks,
        [Pk::Int64(1)]
    );
    drop(renamed);
    db.close();

    // 从未打开的集合直接按目录改名。
    let db = open(root.path());
    db.rename_collection("", "d", "e", "").unwrap();
    assert_eq!(
        db.list_collections("").unwrap()[0],
        ("e".to_owned(), LoadState::Released)
    );
    assert_eq!(count(&db, "e"), 1);
    db.close();
}

#[test]
fn reopen_settles_rename_intents_by_exact_names() {
    // 新名存在、旧名不存在：补写 `schema.json` 的名字与别名，删除意图。
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1])).unwrap();
    db.create_alias("", "c", "a").unwrap();
    db.close();
    std::fs::rename(
        root.path().join("collections/c"),
        root.path().join("collections/d"),
    )
    .unwrap();
    write_intent(root.path(), "c", "d");
    let db = open(root.path());
    assert!(!root.path().join(INTENT).exists());
    assert_eq!(schema_name(root.path(), "d"), "d");
    assert_eq!(aliases(root.path()).unwrap(), "{\n  \"a\": \"d\"\n}");
    assert_eq!(count(&db, "a"), 1);
    db.create_collection("", "c", schema()).unwrap();
    db.close();

    // 旧名存在（目录改名之前退出）、两者都存在、两者的路径都不存在：只删除意图。
    for (old, new) in [("c", "x"), ("c", "d"), ("gone", "never")] {
        write_intent(root.path(), old, new);
        let mut before = tree(root.path());
        before.remove(Path::new(INTENT));
        let db = open(root.path());
        db.close();
        assert_eq!(tree(root.path()), before, "{old} -> {new}");
    }
}

#[test]
fn unfinished_renames_block_namespace_changes_until_a_retry_succeeds() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1])).unwrap();
    db.close();

    // 精确名字都不存在、但新名路径存在：需人工处理；意图损坏同样。
    std::fs::create_dir(root.path().join("collections/y")).unwrap();
    write_intent(root.path(), "x", "y");
    for fix in ["residue", "corrupt"] {
        if fix == "corrupt" {
            std::fs::write(root.path().join(INTENT), b"{").unwrap();
        }
        let db = open(root.path());
        assert!(root.path().join(INTENT).exists());
        let error = unfinished(db.create_collection("", "n", schema()));
        assert!(error.contains(INTENT), "{error}");
        unfinished(db.drop_collection("", "c"));
        unfinished(db.rename_collection("", "c", "n", ""));
        unfinished(db.create_alias("", "c", "a"));
        unfinished(db.alter_alias("", "c", "a"));
        unfinished(db.drop_alias("", "a"));
        // 数据读写与属性修改照常。
        db.insert("", "c", batch(&[2])).unwrap();
        db.flush_collection("", "c").unwrap();
        db.alter_collection("", "c", &[("k".into(), "v".into())], &[])
            .unwrap();
        assert_eq!(count(&db, "c"), 2);
        db.delete("", "c", &[Pk::Int64(2)]).unwrap();
        // 运行期间修好之后，下一次受限操作的重试成功。
        if fix == "residue" {
            std::fs::remove_dir(root.path().join("collections/y")).unwrap();
        } else {
            write_intent(root.path(), "c", "n");
        }
        db.create_collection("", "n", schema()).unwrap();
        assert!(!root.path().join(INTENT).exists());
        db.drop_collection("", "n").unwrap();
        db.close();
        write_intent(root.path(), "x", "y");
    }
}

#[cfg(feature = "fault-injection")]
mod faults {
    use super::*;
    use milvus_lite_core::db::{
        DirectoryFault, DirectoryOperation, DirectoryPoint, RenamePausePoint,
        inject_directory_fault_once, pause_rename_once,
    };
    use milvus_lite_core::storage::runtime::{
        RetainedWalSyncFault, inject_retained_wal_sync_failure_once,
    };

    fn inject(point: DirectoryPoint, fault: DirectoryFault) {
        inject_directory_fault_once(DirectoryOperation::Rename, point, fault);
    }

    /// 已打开、带未落盘 data 与 delta WAL 的 `c`，别名 `a` 指向它。
    fn seeded(root: &Path) -> std::sync::Arc<Db> {
        let db = open(root);
        db.create_collection("", "c", schema()).unwrap();
        db.insert("", "c", batch(&[1, 2])).unwrap();
        db.flush_collection("", "c").unwrap();
        db.insert("", "c", batch(&[3])).unwrap();
        db.delete("", "c", &[Pk::Int64(1)]).unwrap();
        db.create_alias("", "c", "a").unwrap();
        db.create_collection("", "other", schema()).unwrap();
        db
    }

    /// 改名之前的状态：`c` 可读写，旧数据与之后的写入在重开后都在，`d` 与意图都不存在。
    fn assert_before(root: &Path, db: std::sync::Arc<Db>) {
        assert!(!root.join(INTENT).exists());
        assert!(!root.join("collections/d").exists());
        assert_eq!(db.describe_alias("", "a").unwrap(), "c");
        db.insert("", "c", batch(&[4])).unwrap();
        db.flush_collection("", "c").unwrap();
        db.insert("", "c", batch(&[5])).unwrap();
        assert_eq!(count(&db, "c"), 4);
        db.close();
        let db = open(root);
        assert_eq!(count(&db, "c"), 4);
        assert_eq!(schema_name(root, "c"), "c");
        db.close();
    }

    /// 改名之后的状态：`d` 有全部数据，别名与 `schema.json` 已一致，意图已删除。
    fn assert_after(root: &Path, db: &Db) {
        assert!(!root.join(INTENT).exists());
        assert!(!root.join("collections/c").exists());
        assert_eq!(schema_name(root, "d"), "d");
        assert_eq!(aliases(root).unwrap(), "{\n  \"a\": \"d\"\n}");
        assert_eq!(count(db, "a"), 2);
    }

    #[test]
    fn wal_sync_and_directory_failures_before_the_commit_leave_the_old_collection_writable() {
        for fault in [
            RetainedWalSyncFault::Data,
            RetainedWalSyncFault::Delta,
            RetainedWalSyncFault::Directory,
        ] {
            let root = tempfile::tempdir().unwrap();
            let db = seeded(root.path());
            inject_retained_wal_sync_failure_once(fault);
            assert!(db.rename_collection("", "c", "d", "").is_err());
            assert_before(root.path(), db);
        }
        for (point, fault) in [
            (DirectoryPoint::Intent, DirectoryFault::Error),
            (DirectoryPoint::Intent, DirectoryFault::Panic),
            (DirectoryPoint::IntentAfterRename, DirectoryFault::Error),
            (DirectoryPoint::BeforeRename, DirectoryFault::Error),
            (DirectoryPoint::BeforeRename, DirectoryFault::Panic),
            (DirectoryPoint::NameVerification, DirectoryFault::Error),
        ] {
            let root = tempfile::tempdir().unwrap();
            let db = seeded(root.path());
            inject(point, fault);
            let error = db.rename_collection("", "c", "d", "").unwrap_err();
            assert!(
                matches!(error, EngineError::Storage(_)),
                "{point:?}: {error}"
            );
            assert!(root.path().join("collections/c/schema.json").exists());
            // 其余受限操作照常：没有进入改名未完成状态。
            db.create_alias("", "c", "b").unwrap();
            db.drop_alias("", "b").unwrap();
            assert_before(root.path(), db);
        }
    }

    #[test]
    fn intent_cleanup_and_rollback_failures_follow_the_frozen_states() {
        // 目录改名失败而意图删除也失败：改名未完成，下一次受限操作重试删除。
        for point in [
            DirectoryPoint::IntentRemove,
            DirectoryPoint::IntentRemoveAfter,
        ] {
            let root = tempfile::tempdir().unwrap();
            let db = seeded(root.path());
            inject(DirectoryPoint::BeforeRename, DirectoryFault::Error);
            inject(point, DirectoryFault::Error);
            let error = db
                .rename_collection("", "c", "d", "")
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("rename intent cleanup is pending"),
                "{error}"
            );
            db.insert("", "c", batch(&[9])).unwrap();
            db.delete("", "c", &[Pk::Int64(9)]).unwrap();
            inject(DirectoryPoint::IntentRemove, DirectoryFault::Error);
            unfinished(db.create_alias("", "c", "b"));
            db.create_alias("", "c", "b").unwrap();
            db.drop_alias("", "b").unwrap();
            assert_before(root.path(), db);
        }

        // 核对不通过而改回也失败：整库写入封锁，意图保留，重开后按「旧名存在」删除意图。
        let root = tempfile::tempdir().unwrap();
        let db = seeded(root.path());
        inject(DirectoryPoint::NameVerification, DirectoryFault::Error);
        inject(DirectoryPoint::Rollback, DirectoryFault::Error);
        let error = db
            .rename_collection("", "c", "d", "")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("rollback durability is unconfirmed"),
            "{error}"
        );
        assert!(root.path().join(INTENT).exists());
        assert!(
            db.insert("", "other", batch(&[1]))
                .unwrap_err()
                .to_string()
                .contains("blocked")
        );
        db.close();
        let db = open(root.path());
        assert!(!root.path().join(INTENT).exists());
        // 改回没有执行：新名目录仍在，旧名不存在；重开按「新名存在、旧名不存在」收尾为改名之后。
        assert_after(root.path(), &db);
        db.close();
    }

    #[test]
    fn failures_after_the_directory_commit_finish_on_retry_or_reopen() {
        // 替换之前失败：改名未完成，数据读写照常，受限操作重试补完。
        for (point, fault) in [
            (DirectoryPoint::Schema, DirectoryFault::Error),
            (DirectoryPoint::Schema, DirectoryFault::Panic),
            (DirectoryPoint::Aliases, DirectoryFault::Error),
            (DirectoryPoint::Aliases, DirectoryFault::Panic),
            (DirectoryPoint::IntentRemove, DirectoryFault::Error),
            (DirectoryPoint::IntentRemoveAfter, DirectoryFault::Error),
        ] {
            let root = tempfile::tempdir().unwrap();
            let db = seeded(root.path());
            inject(point, fault);
            let error = db
                .rename_collection("", "c", "d", "")
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("metadata maintenance is pending"),
                "{point:?}: {error}"
            );
            assert!(matches!(
                db.collection("", "c"),
                Err(EngineError::CollectionNotFound(_))
            ));
            db.insert("", "d", batch(&[7])).unwrap();
            db.flush_collection("", "d").unwrap();
            db.delete("", "d", &[Pk::Int64(7)]).unwrap();
            // 重试仍失败时拒绝，再次重试成功。
            inject(point, DirectoryFault::Error);
            inject(point, DirectoryFault::Error);
            unfinished(db.create_collection("", "n", schema()));
            unfinished(db.drop_collection("", "other"));
            db.create_collection("", "n", schema()).unwrap();
            assert_after(root.path(), &db);
            db.close();
            let db = open(root.path());
            assert_after(root.path(), &db);
            db.close();
        }

        // 替换之后目录同步失败（含退役旧名后的 `collections/` 同步）：整库写入封锁，重开后收尾。
        for point in [
            DirectoryPoint::CollectionsSync,
            DirectoryPoint::SchemaAfterRename,
            DirectoryPoint::AliasesAfterRename,
        ] {
            let root = tempfile::tempdir().unwrap();
            let db = seeded(root.path());
            inject(point, DirectoryFault::Error);
            let error = db
                .rename_collection("", "c", "d", "")
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("durability is unconfirmed"),
                "{point:?}: {error}"
            );
            assert!(root.path().join(INTENT).exists());
            // 未缓存的名字在封锁期间按现有规则报封锁（同 drop 目录同步失败之后），不断言目录状态。
            for result in [
                db.collection("", "c").map(|_| ()),
                db.collection("", "d").map(|_| ()),
                db.insert("", "other", batch(&[1])).map(|_| ()),
                db.create_collection("", "n", schema()),
                db.alter_collection("", "other", &[], &[]),
            ] {
                assert!(
                    result.unwrap_err().to_string().contains("blocked"),
                    "{point:?}"
                );
            }
            db.close();
            let db = open(root.path());
            assert_after(root.path(), &db);
            db.insert("", "d", batch(&[8])).unwrap();
            db.close();
        }
    }

    /// 启动时（`Db::new` 单线程）按意图收尾遇到故障：启动不失败，改名置为未完成，数据读写照常，
    /// 下一次受限操作重试成功；没有故障时的收尾结果与重开一致。
    #[test]
    fn startup_settle_failures_do_not_fail_startup_and_finish_on_the_next_retry() {
        for point in [
            DirectoryPoint::CollectionsSync,
            DirectoryPoint::Schema,
            DirectoryPoint::SchemaAfterRename,
            DirectoryPoint::Aliases,
            DirectoryPoint::AliasesAfterRename,
            DirectoryPoint::IntentRemove,
            DirectoryPoint::IntentRemoveAfter,
        ] {
            let root = tempfile::tempdir().unwrap();
            let db = open(root.path());
            db.create_collection("", "c", schema()).unwrap();
            db.insert("", "c", batch(&[1, 2])).unwrap();
            db.create_alias("", "c", "a").unwrap();
            db.close();
            std::fs::rename(
                root.path().join("collections/c"),
                root.path().join("collections/d"),
            )
            .unwrap();
            write_intent(root.path(), "c", "d");

            inject(point, DirectoryFault::Error);
            let db = open(root.path());
            // 故障确实发生在启动收尾中：意图仍在（只有删除之后的目录同步失败时它已不在）。
            assert_eq!(
                root.path().join(INTENT).exists(),
                point != DirectoryPoint::IntentRemoveAfter,
                "{point:?}"
            );
            // 读写照常（数据读写不依赖元数据收尾）。
            assert_eq!(count(&db, "d"), 2, "{point:?}");
            db.insert("", "d", batch(&[3])).unwrap();
            // 下一次受限操作重试收尾并成功。
            db.create_collection("", "n", schema()).unwrap();
            assert!(!root.path().join(INTENT).exists(), "{point:?}");
            assert_eq!(schema_name(root.path(), "d"), "d", "{point:?}");
            assert_eq!(
                aliases(root.path()).unwrap(),
                "{\n  \"a\": \"d\"\n}",
                "{point:?}"
            );
            assert_eq!(count(&db, "a"), 3, "{point:?}");
            db.close();
            let db = open(root.path());
            assert_eq!(count(&db, "d"), 3, "{point:?}");
            db.close();
        }
    }

    #[test]
    fn requests_during_rename_wait_for_the_exclusive_permit_and_see_the_result() {
        for point in [RenamePausePoint::Retired, RenamePausePoint::SchemaReplaced] {
            let root = tempfile::tempdir().unwrap();
            let db = seeded(root.path());
            db.create_collection("", "gone", schema()).unwrap();
            assert_eq!(count(&db, "other"), 0);
            let (entered_tx, entered) = std::sync::mpsc::channel();
            let (resume, resume_rx) = std::sync::mpsc::channel();
            let renamer = {
                let db = std::sync::Arc::clone(&db);
                std::thread::spawn(move || {
                    pause_rename_once(point, entered_tx, resume_rx);
                    db.rename_collection("", "c", "d", "")
                })
            };
            entered.recv().unwrap();
            // 已打开的其他集合照常可读；旧名、新名的查找与任何写入都等待改名结束。
            assert_eq!(count(&db, "other"), 0);
            let waiters: Vec<_> = [
                Box::new(|db: &Db| db.collection("", "c").map(|_| ()))
                    as Box<dyn Fn(&Db) -> Result<(), EngineError> + Send>,
                Box::new(|db: &Db| {
                    db.collection("", "d")
                        .map(|h| assert_eq!(h.read().count(), 2))
                }),
                Box::new(|db: &Db| db.insert("", "other", batch(&[1])).map(|_| ())),
                Box::new(|db: &Db| db.create_collection("", "n", schema())),
                Box::new(|db: &Db| db.flush_collection("", "other").map(|_| ())),
                Box::new(|db: &Db| db.drop_collection("", "gone")),
            ]
            .into_iter()
            .map(|request| {
                let db = std::sync::Arc::clone(&db);
                std::thread::spawn(move || request(&db))
            })
            .collect();
            std::thread::sleep(std::time::Duration::from_millis(100));
            assert!(
                waiters.iter().all(|waiter| !waiter.is_finished()),
                "{point:?}"
            );
            resume.send(()).unwrap();
            renamer.join().unwrap().unwrap();
            let results: Vec<_> = waiters.into_iter().map(|w| w.join().unwrap()).collect();
            assert!(matches!(
                results[0],
                Err(EngineError::CollectionNotFound(_))
            ));
            for result in &results[1..] {
                assert!(result.is_ok(), "{point:?}: {result:?}");
            }
            assert_after(root.path(), &db);
            db.close();
        }
    }

    #[test]
    fn process_exit_at_each_rename_step_reopens_before_or_after() {
        use std::process::Command;
        let points = [
            ("intent", false),
            ("intent-after", false),
            ("directory", false),
            ("collections-sync", true),
            ("schema", true),
            ("schema-after", true),
            ("aliases", true),
            ("aliases-after", true),
            ("intent-remove", true),
            ("intent-remove-after", true),
        ];
        if let Ok(stage) = std::env::var("MILVUS_LITE_RENAME_CRASH") {
            let path = PathBuf::from(std::env::var("MILVUS_LITE_CRASH_DIR").unwrap());
            let db = seeded(&path);
            let point = match stage.as_str() {
                "intent" => DirectoryPoint::Intent,
                "intent-after" => DirectoryPoint::IntentAfterRename,
                "directory" => DirectoryPoint::BeforeRename,
                "collections-sync" => DirectoryPoint::CollectionsSync,
                "schema" => DirectoryPoint::Schema,
                "schema-after" => DirectoryPoint::SchemaAfterRename,
                "aliases" => DirectoryPoint::Aliases,
                "aliases-after" => DirectoryPoint::AliasesAfterRename,
                "intent-remove" => DirectoryPoint::IntentRemove,
                _ => DirectoryPoint::IntentRemoveAfter,
            };
            inject(point, DirectoryFault::Exit);
            db.rename_collection("", "c", "d", "").unwrap();
            panic!("rename crash injection did not fire");
        }
        // 同 crash_recovery：验证过程放在独立父进程中，避免兄弟子进程短暂继承刚关闭的 LOCK。
        let test = "faults::process_exit_at_each_rename_step_reopens_before_or_after";
        if std::env::var_os("MILVUS_LITE_RENAME_PARENT").is_none() {
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture"])
                .env("MILVUS_LITE_RENAME_PARENT", "1")
                .status()
                .unwrap();
            assert!(
                status.success(),
                "isolated rename recovery parent failed: {status:?}"
            );
            return;
        }
        for (stage, renamed) in points {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("rename-crash.db");
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture"])
                .env("MILVUS_LITE_RENAME_CRASH", stage)
                .env("MILVUS_LITE_CRASH_DIR", &path)
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(88), "{stage}");
            let db = open(&path);
            if renamed {
                assert_after(&path, &db);
                db.close();
            } else {
                assert_eq!(count(&db, "c"), 2, "{stage}");
                assert_before(&path, db);
            }
        }
    }
}
