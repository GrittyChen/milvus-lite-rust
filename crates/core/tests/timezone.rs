//! 时区（0025 第 3 步）：请求 → 集合 → 数据库的优先级、数据库时区的按需读取与即时生效、
//! naive 默认值的换算与并发改动。过滤与写入的逐项对照 3.x 见 tests/differential。
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use milvus_lite_core::data::Validity;
use milvus_lite_core::filter::ParsedFilter;
use milvus_lite_core::timezone::TimezoneSource;
use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, EngineError, FieldSchema, InputValues,
    InsertBatch, Pk, QueryVectors, SearchRequest,
};
use serde_json::json;

fn open(path: &std::path::Path) -> Arc<Db> {
    Db::new(
        DataDir::open(path).unwrap(),
        Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
    )
    .unwrap()
}

fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn schema(properties: &[(&str, &str)], default: Option<serde_json::Value>) -> CollectionSchema {
    let mut id = FieldSchema::new("id", DataType::Int64);
    id.is_primary = true;
    let mut vector = FieldSchema::new("v", DataType::FloatVector);
    vector.dim = Some(2);
    let mut ts = FieldSchema::new("ts", DataType::Timestamptz);
    ts.nullable = true;
    ts.default_value = default;
    CollectionSchema {
        fields: vec![id, vector, ts],
        enable_dynamic_field: false,
        properties: pairs(properties),
    }
}

/// `ts` 取值：第 i 行为 `2025-01-01T00:00:00Z` 加 i 小时。
fn insert_hours(db: &Db, database: &str, count: i64) {
    let ids: Vec<i64> = (0..count).collect();
    let n = ids.len();
    db.insert(
        database,
        "c",
        InsertBatch {
            num_rows: n,
            columns: vec![
                ("id".into(), InputValues::Int(ids.clone())),
                (
                    "v".into(),
                    InputValues::FloatVector {
                        dim: 2,
                        values: vec![0.0; n * 2],
                    },
                ),
                (
                    "ts".into(),
                    InputValues::Nullable {
                        values: Box::new(InputValues::Int(
                            ids.iter()
                                .map(|i| 1_735_689_600_000_000 + i * 3_600_000_000)
                                .collect(),
                        )),
                        validity: Validity::from_bools(&vec![true; n]),
                    },
                ),
            ],
        },
    )
    .unwrap();
}

fn ids_with(
    db: &Db,
    database: &str,
    expr: &str,
    timezone: &TimezoneSource<'_>,
) -> Result<Vec<i64>, EngineError> {
    let handle = db.collection(database, "c").unwrap();
    handle.write().load().unwrap();
    let snapshot = handle.read().read_snapshot();
    let parsed = ParsedFilter::parse_with(expr, timezone)?;
    let mut ids: Vec<i64> = snapshot
        .query_parsed(&parsed, &[], None, 0)?
        .rows
        .pks
        .iter()
        .map(|pk| match pk {
            Pk::Int64(v) => *v,
            other => panic!("{other:?}"),
        })
        .collect();
    ids.sort_unstable();
    Ok(ids)
}

fn remaining(db: &Db, database: &str) -> Vec<i64> {
    ids_with(db, database, "id >= 0", &TimezoneSource::none()).unwrap()
}

#[test]
fn naive_literals_use_request_then_collection_then_database_zone() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_database("d", &pairs(&[("timezone", "Asia/Tokyo")]))
        .unwrap();
    db.create_collection("d", "c", schema(&[], None)).unwrap();
    insert_hours(&db, "d", 4);
    let database = || db.database_timezone("d");
    // 行 1 是 2025-01-01T01:00Z：东京 10:00、上海 09:00、UTC 01:00。
    let expr = "ts == ISO '2025-01-01T10:00:00'";
    let source = |request: Option<&str>, collection: Option<&str>| {
        TimezoneSource::new(
            request.map(str::to_owned),
            collection.map(str::to_owned),
            &database,
        )
    };
    assert_eq!(ids_with(&db, "d", expr, &source(None, None)).unwrap(), [1]);
    // 集合优先于数据库，请求优先于集合。
    assert_eq!(
        ids_with(&db, "d", expr, &source(None, Some("Asia/Shanghai"))).unwrap(),
        [2]
    );
    assert_eq!(
        ids_with(
            &db,
            "d",
            "ts == ISO '2025-01-01T09:00:00'",
            &source(None, Some("Asia/Shanghai"))
        )
        .unwrap(),
        [1]
    );
    assert_eq!(
        ids_with(
            &db,
            "d",
            "ts == ISO '2025-01-01T01:00:00'",
            &source(Some("UTC"), Some("Asia/Shanghai"))
        )
        .unwrap(),
        [1]
    );
    // 请求时区先于表达式校验：坏名字报 6，即使表达式本身也有语法错误。
    for expr in [expr, "id >"] {
        assert!(matches!(
            ids_with(&db, "d", expr, &source(Some("Mars/Base"), None)),
            Err(EngineError::SchemaValidation(m)) if m == "unknown timezone 'Mars/Base'"
        ));
    }
    // 没有配置任何时区时 3.x 的错误（位置是字符串字面量）。
    db.create_database("plain", &[]).unwrap();
    let none = || db.database_timezone("plain");
    let message = ids_with(&db, "d", expr, &TimezoneSource::new(None, None, &none))
        .unwrap_err()
        .to_string();
    assert!(
        message.starts_with(
            "TIMESTAMPTZ value must include a timezone offset or use a configured timezone at column"
        ),
        "{message}"
    );
}

#[test]
fn delete_expr_uses_collection_then_database_zone_and_changes_apply_immediately() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_database("d", &pairs(&[("timezone", "Asia/Tokyo")]))
        .unwrap();
    db.create_collection("d", "c", schema(&[], None)).unwrap();
    insert_hours(&db, "d", 6);
    // 数据库时区东京：行 1（01:00Z）是 10:00。
    let (deleted, _) = db
        .delete_expr("d", "c", "ts == ISO '2025-01-01T10:00:00'", "")
        .unwrap();
    assert_eq!(deleted, [Pk::Int64(1)]);
    // 改数据库时区立即影响已打开的集合：UTC 下 02:00 是行 2。
    db.alter_database("d", &pairs(&[("timezone", "UTC")]), &[])
        .unwrap();
    let (deleted, _) = db
        .delete_expr("d", "c", "ts == ISO '2025-01-01T02:00:00'", "")
        .unwrap();
    assert_eq!(deleted, [Pk::Int64(2)]);
    // 集合自己的时区优先。
    db.alter_collection("d", "c", &pairs(&[("timezone", "Asia/Shanghai")]), &[])
        .unwrap();
    let (deleted, _) = db
        .delete_expr("d", "c", "ts == ISO '2025-01-01T11:00:00'", "")
        .unwrap();
    assert_eq!(deleted, [Pk::Int64(2 + 1)]);
    // 删除属性后退回数据库，再删除数据库属性后 naive 字面量报错且不删除任何行。
    db.alter_collection("d", "c", &[], &["timezone".to_owned()])
        .unwrap();
    db.alter_database("d", &[], &["timezone".to_owned()])
        .unwrap();
    let before = remaining(&db, "d");
    assert!(matches!(
        db.delete_expr("d", "c", "ts == ISO '2025-01-01T04:00:00'", ""),
        Err(EngineError::SchemaValidation(m)) if m.starts_with("TIMESTAMPTZ value must include")
    ));
    assert_eq!(remaining(&db, "d"), before);
    // 带偏移的字面量不需要时区。
    let (deleted, _) = db
        .delete_expr("d", "c", "ts == ISO '2025-01-01T04:00:00Z'", "")
        .unwrap();
    assert_eq!(deleted, [Pk::Int64(4)]);
    db.close();
}

#[test]
fn naive_defaults_are_normalized_once_by_collection_then_database_zone() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_database("d", &pairs(&[("timezone", "Europe/London")]))
        .unwrap();
    let default = Some(json!("2025-07-01T12:00:00"));
    // 数据库时区（伦敦夏令时 +1）：11:00Z。
    db.create_collection("d", "c", schema(&[], default.clone()))
        .unwrap();
    // 集合时区优先（上海 +8）：04:00Z。
    db.create_collection(
        "d",
        "c2",
        schema(&[("timezone", "Asia/Shanghai")], default.clone()),
    )
    .unwrap();
    let described = |name: &str| {
        db.collection("d", name).unwrap().read().schema().fields[2]
            .default_value
            .clone()
    };
    assert_eq!(described("c"), Some(json!(1_751_367_600_000_000_i64)));
    assert_eq!(described("c2"), Some(json!(1_751_342_400_000_000_i64)));
    // 之后改时区不改变已保存的 schema。
    db.alter_database("d", &pairs(&[("timezone", "UTC")]), &[])
        .unwrap();
    assert_eq!(described("c"), Some(json!(1_751_367_600_000_000_i64)));
    db.close();
    let db = open(root.path());
    assert_eq!(
        db.collection("d", "c").unwrap().read().schema().fields[2].default_value,
        Some(json!(1_751_367_600_000_000_i64))
    );
    // 没有任何时区：创建失败（6），带偏移的字符串不受影响。
    db.alter_database("d", &[], &["timezone".to_owned()])
        .unwrap();
    assert!(matches!(
        db.create_collection("d", "c3", schema(&[], default)),
        Err(EngineError::SchemaValidation(m)) if m.starts_with("TIMESTAMPTZ value must include")
    ));
    assert!(db.collection("d", "c3").is_err());
    db.create_collection("d", "c4", schema(&[], Some(json!("2025-07-01T12:00:00Z"))))
        .unwrap();
    db.close();
}

#[test]
fn unreadable_database_properties_only_fail_operations_that_need_the_zone() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    std::fs::create_dir_all(path.join("databases/bad/collections")).unwrap();
    std::fs::write(path.join("databases/bad/database_properties.json"), b"{").unwrap();
    let db = open(&path);
    // 没有 naive 默认值时创建不读数据库属性。
    db.create_collection("bad", "c", schema(&[], Some(json!(5))))
        .unwrap();
    insert_hours(&db, "bad", 3);
    // 不需要时区的过滤、带偏移的字面量与带集合时区的 naive 字面量照常。
    let database = || db.database_timezone("bad");
    let source = TimezoneSource::new(None, None, &database);
    assert_eq!(
        ids_with(&db, "bad", "ts >= ISO '2025-01-01T01:00:00Z'", &source).unwrap(),
        [1, 2]
    );
    assert_eq!(ids_with(&db, "bad", "id >= 1", &source).unwrap(), [1, 2]);
    let with_collection = TimezoneSource::new(None, Some("UTC".into()), &database);
    assert_eq!(
        ids_with(
            &db,
            "bad",
            "ts == ISO '2025-01-01T01:00:00'",
            &with_collection
        )
        .unwrap(),
        [1]
    );
    // 需要数据库时区时报存储错误，零写入。
    assert!(matches!(
        ids_with(&db, "bad", "ts == ISO '2025-01-01T01:00:00'", &source),
        Err(EngineError::Storage(_))
    ));
    let before = remaining(&db, "bad");
    assert!(matches!(
        db.delete_expr("bad", "c", "ts == ISO '2025-01-01T01:00:00'", ""),
        Err(EngineError::Storage(_))
    ));
    assert_eq!(remaining(&db, "bad"), before);
    assert!(matches!(
        db.create_collection(
            "bad",
            "naive",
            schema(&[], Some(json!("2025-07-01T12:00:00")))
        ),
        Err(EngineError::Storage(_))
    ));
    assert!(db.collection("bad", "naive").is_err());
    // 集合时区使数据库属性不再被需要。
    db.create_collection(
        "bad",
        "naive_ok",
        schema(&[("timezone", "UTC")], Some(json!("2025-07-01T12:00:00"))),
    )
    .unwrap();
    // 只有 Describe / Alter 读取损坏的属性文件，失败不缓存。
    assert!(db.database_timezone("bad").is_err());
    std::fs::write(
        path.join("databases/bad/database_properties.json"),
        br#"{"timezone": "UTC"}"#,
    )
    .unwrap();
    assert_eq!(db.database_timezone("bad").unwrap().as_deref(), Some("UTC"));
    db.close();
}

#[test]
fn invalid_stored_zone_fails_every_zone_read_and_is_not_cached() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("m.db");
    std::fs::create_dir_all(path.join("databases/bad/collections")).unwrap();
    let file = path.join("databases/bad/database_properties.json");
    let db = open(&path);
    // 3.x 加载属性就校验 timezone：未知名字、空串与路径形态的读取都失败，文案与 Describe / Alter 相同。
    for (text, expected) in [
        (
            &br#"{"timezone": "Mars/Base"}"#[..],
            "unknown timezone 'Mars/Base'",
        ),
        (
            &br#"{"timezone": ""}"#[..],
            "timezone property must be a non-empty string",
        ),
    ] {
        std::fs::write(&file, text).unwrap();
        assert!(matches!(
            db.database_timezone("bad"),
            Err(EngineError::SchemaValidation(m)) if m == expected
        ));
        assert!(matches!(
            db.describe_database("bad"),
            Err(EngineError::SchemaValidation(m)) if m == expected
        ));
    }
    // 失败不缓存：文件修正后，下一次时区读取（不经过 Describe / Alter）直接成功。
    std::fs::write(&file, br#"{"timezone": "Mars/Base"}"#).unwrap();
    assert!(db.database_timezone("bad").is_err());
    std::fs::write(&file, br#"{"timezone": "UTC"}"#).unwrap();
    assert_eq!(db.database_timezone("bad").unwrap().as_deref(), Some("UTC"));
    // 删除属性后恢复为未配置。
    std::fs::write(&file, b"{}").unwrap();
    db.alter_database("bad", &[], &["timezone".to_owned()])
        .unwrap();
    assert_eq!(db.database_timezone("bad").unwrap(), None);
    db.close();
}

#[test]
fn concurrent_database_zone_changes_never_deadlock_or_tear() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_database("d", &pairs(&[("timezone", "UTC")]))
        .unwrap();
    db.create_collection("d", "c", schema(&[], None)).unwrap();
    insert_hours(&db, "d", 4);
    let stop = Arc::new(AtomicBool::new(false));
    let altering = {
        let (db, stop) = (Arc::clone(&db), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut flip = false;
            while !stop.load(Ordering::Relaxed) {
                let zone = if flip { "Asia/Tokyo" } else { "UTC" };
                db.alter_database("d", &pairs(&[("timezone", zone)]), &[])
                    .unwrap();
                flip = !flip;
            }
        })
    };
    let readers: Vec<_> = (0..3)
        .map(|n| {
            let db = Arc::clone(&db);
            std::thread::spawn(move || {
                for i in 0..150 {
                    let database = || db.database_timezone("d");
                    let source = TimezoneSource::new(None, None, &database);
                    // 每次解析都看到完整的改前或改后的值：东京 10:00 与 UTC 01:00 是同一行。
                    let tokyo =
                        ids_with(&db, "d", "ts == ISO '2025-01-01T10:00:00'", &source).unwrap();
                    let utc =
                        ids_with(&db, "d", "ts == ISO '2025-01-01T01:00:00'", &source).unwrap();
                    assert!(tokyo.len() <= 1 && utc.len() <= 1);
                    // Delete（不带请求时区）与 Search 同样每次读取当前的数据库时区；没有行命中，数据保持不变。
                    let (deleted, _) = db
                        .delete_expr("d", "c", "ts == ISO '2100-01-01T00:00:00'", "")
                        .unwrap();
                    assert!(deleted.is_empty());
                    let handle = db.collection("d", "c").unwrap();
                    let snapshot = handle.read().read_snapshot();
                    let hits = snapshot
                        .search_grouped_in(
                            SearchRequest {
                                anns_field: None,
                                vectors: QueryVectors::Dense(vec![vec![0.0, 0.0]]),
                                metric_type: None,
                                limit: 10,
                                offset: 0,
                                output_fields: vec![],
                                params: Default::default(),
                                negative_top_k: false,
                            },
                            db.compute(),
                            "ts == ISO '2025-01-01T10:00:00'",
                            None,
                            None,
                            &source,
                        )
                        .unwrap();
                    assert!(hits.hits[0].rows.pks.len() <= 1);
                    assert!(matches!(
                        db.database_timezone("d").unwrap().as_deref(),
                        Some("UTC" | "Asia/Tokyo")
                    ));
                    if n == 0 && i % 25 == 0 {
                        let name = format!("c{i}");
                        db.create_collection(
                            "d",
                            &name,
                            schema(&[], Some(json!("2025-07-01T12:00:00"))),
                        )
                        .unwrap();
                    }
                }
            })
        })
        .collect();
    for reader in readers {
        reader.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    altering.join().unwrap();
    // 创建时换算出的默认值只会是两个时区之一的结果，不会撕裂。
    let handle = db.collection("d", "c0").unwrap();
    let default = handle.read().schema().fields[2].default_value.clone();
    assert!(
        default == Some(json!(1_751_371_200_000_000_i64))
            || default == Some(json!(1_751_338_800_000_000_i64)),
        "{default:?}"
    );
    db.close();
}

/// 期望值由 `tests/differential/generate_timezone_fixtures.py`（CPython 3.12 zoneinfo）生成。
#[test]
fn conversions_match_the_cpython_zoneinfo_fixture() {
    use milvus_lite_core::timestamptz::{format_iso_z, parse};
    use milvus_lite_core::timezone::resolve;
    let fixture: serde_json::Value =
        milvus_lite_core::json_text::value_str(include_str!("fixtures/timezone_v321.json"))
            .unwrap();
    let none = || Ok(None);
    let mut mismatches = Vec::new();
    for case in fixture["local"].as_array().unwrap() {
        let (zone, text, expected) = (
            case[0].as_str().unwrap(),
            case[1].as_str().unwrap(),
            case[2].as_str().unwrap(),
        );
        let source = TimezoneSource::new(Some(zone.to_owned()), None, &none);
        let actual = match source.parse_timestamp(text) {
            Ok(micros) => format_iso_z(micros).unwrap(),
            Err(EngineError::InvalidArgument(message)) => message,
            Err(other) => panic!("{zone} {text}: {other:?}"),
        };
        if actual != expected {
            mismatches.push(format!("local {zone} {text}: {actual} != {expected}"));
        }
    }
    for case in fixture["components"].as_array().unwrap() {
        let (zone, text) = (case[0].as_str().unwrap(), case[1].as_str().unwrap());
        let micros = parse(text).unwrap();
        let actual = match resolve(zone).unwrap().components(micros) {
            Ok(c) => Some(vec![
                c.year,
                c.month,
                c.day,
                c.hour,
                c.minute,
                c.second,
                c.microsecond,
            ]),
            Err(EngineError::InvalidArgument(m)) if m == "date value out of range" => None,
            Err(other) => panic!("{zone} {text}: {other:?}"),
        };
        let expected: Option<Vec<i64>> = case[2]
            .as_array()
            .map(|a| a.iter().map(|v| v.as_i64().unwrap()).collect());
        if actual != expected {
            mismatches.push(format!("utc {zone} {text}: {actual:?} != {expected:?}"));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} mismatches:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}
