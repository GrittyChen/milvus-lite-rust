//! TIMESTAMPTZ（0025 第 1 步）：UTC 微秒列的写入、落盘（Arrow `timestamp[us, tz=UTC]`）、恢复、合并与 schema 规则。
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use arrow_schema::{DataType as ArrowType, TimeUnit};
use milvus_lite_core::data::Validity;
use milvus_lite_core::timestamptz::{MAX_MICROS, MIN_MICROS};
use milvus_lite_core::{
    CollectionSchema, ColumnData, Compute, DataDir, DataType, Db, EngineError, FieldSchema,
    InputValues, InsertBatch,
};
use serde_json::json;

fn open(path: &std::path::Path) -> Arc<Db> {
    Db::new(
        DataDir::open(path).unwrap(),
        Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
    )
    .unwrap()
}

fn schema() -> CollectionSchema {
    let mut id = FieldSchema::new("id", DataType::Int64);
    id.is_primary = true;
    let mut vector = FieldSchema::new("v", DataType::FloatVector);
    vector.dim = Some(2);
    let mut ts = FieldSchema::new("ts", DataType::Timestamptz);
    ts.nullable = true;
    let mut ts2 = FieldSchema::new("ts2", DataType::Timestamptz);
    ts2.default_value = Some(json!(1_735_689_600_000_000_i64));
    CollectionSchema {
        functions: Vec::new(),
        fields: vec![id, vector, ts, ts2],
        enable_dynamic_field: false,
        properties: vec![],
    }
}

fn batch(ids: &[i64], ts: &[Option<i64>], ts2: &[i64]) -> InsertBatch {
    let n = ids.len();
    InsertBatch {
        num_rows: n,
        columns: vec![
            ("id".into(), InputValues::Int(ids.to_vec())),
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
                        ts.iter().map(|v| v.unwrap_or(0)).collect(),
                    )),
                    validity: Validity::from_bools(
                        &ts.iter().map(Option::is_some).collect::<Vec<_>>(),
                    ),
                },
            ),
            ("ts2".into(), InputValues::Int(ts2.to_vec())),
        ],
    }
}

fn rows(db: &Db) -> Vec<(i64, Option<i64>, i64)> {
    let handle = db.collection("", "c").unwrap();
    handle.write().load().unwrap();
    let snapshot = handle.read().read_snapshot();
    let result = snapshot
        .query("", &["ts".into(), "ts2".into()], None, 0)
        .unwrap();
    let column = |name: &str| {
        &result
            .rows
            .fields
            .iter()
            .find(|(n, _)| n == name)
            .unwrap()
            .1
    };
    let (ts, ts2) = (column("ts"), column("ts2"));
    let ColumnData::Int64(ts_values) = ts.values() else {
        panic!("TIMESTAMPTZ is stored as an INT64 column");
    };
    let ColumnData::Int64(ts2_values) = ts2.values() else {
        panic!("TIMESTAMPTZ is stored as an INT64 column");
    };
    let mut out: Vec<_> = (0..result.rows.pks.len())
        .map(|row| {
            let pk = match &result.rows.pks[row] {
                milvus_lite_core::Pk::Int64(v) => *v,
                other => panic!("{other:?}"),
            };
            (
                (pk),
                (!ts.is_null(row)).then_some(ts_values[row]),
                ts2_values[row],
            )
        })
        .collect();
    out.sort_unstable();
    out
}

#[test]
fn values_survive_wal_flush_compaction_and_reopen_in_3x_arrow_types() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    let expected = vec![
        (1, None, 1_735_689_600_000_000),
        (2, Some(MIN_MICROS), MAX_MICROS),
        (3, Some(1_735_776_600_500_000), -1),
    ];
    db.insert(
        "",
        "c",
        batch(
            &[1, 2, 3],
            &[None, Some(MIN_MICROS), Some(1_735_776_600_500_000)],
            &[1_735_689_600_000_000, MAX_MICROS, -1],
        ),
    )
    .unwrap();
    assert_eq!(rows(&db), expected);
    // WAL 的列类型是 3.x 的 timestamp[us, tz=UTC]；nullable 列可空，非空列带 not null。
    let wal = std::fs::read_dir(root.path().join("collections/c/wal"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|e| e == "arrow"))
        .unwrap();
    let reader =
        arrow_ipc::reader::StreamReader::try_new(std::fs::File::open(wal).unwrap(), None).unwrap();
    let wal_schema = reader.schema();
    for (name, nullable) in [("ts", true), ("ts2", false)] {
        let field = wal_schema.field_with_name(name).unwrap();
        assert_eq!(
            field.data_type(),
            &ArrowType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        );
        assert_eq!(field.is_nullable(), nullable);
    }
    for number in 0..4 {
        if number > 0 {
            db.insert(
                "",
                "c",
                batch(&[10 + number], &[Some(number)], &[number * 1_000_000]),
            )
            .unwrap();
        }
        db.flush_collection("", "c").unwrap();
    }
    db.close();
    let parquet = std::fs::read_dir(root.path().join("collections/c/partitions/_default/data"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|e| e == "parquet"))
        .unwrap();
    let builder = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        std::fs::File::open(parquet).unwrap(),
    )
    .unwrap();
    let parquet_schema = builder.schema().clone();
    assert_eq!(
        parquet_schema.field_with_name("ts").unwrap().data_type(),
        &ArrowType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
    );
    let reopened = open(root.path());
    let mut all = expected.clone();
    all.extend((1..4).map(|n| (10 + n, Some(n), n * 1_000_000)));
    all.sort_unstable();
    assert_eq!(rows(&reopened), all);
}

#[test]
fn out_of_range_integers_are_rejected_for_the_whole_batch_without_writes() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1], &[Some(0)], &[0])).unwrap();
    for bad in [MAX_MICROS + 1, MIN_MICROS - 1, i64::MAX, i64::MIN, 1 << 62] {
        for position in 0..2 {
            let (ts, ts2) = if position == 0 {
                (Some(bad), 0)
            } else {
                (Some(0), bad)
            };
            let error = db
                .insert("", "c", batch(&[2, 3], &[Some(5), ts], &[7, ts2]))
                .unwrap_err();
            assert!(
                matches!(&error, EngineError::InvalidArgument(m) if m == "date value out of range"),
                "{error:?}"
            );
        }
    }
    assert_eq!(rows(&db), vec![(1, Some(0), 0)]);
    db.close();
    assert_eq!(rows(&open(root.path())), vec![(1, Some(0), 0)]);
}

#[test]
fn schema_rules_for_the_new_type() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    let make = |edit: &dyn Fn(&mut CollectionSchema)| {
        let mut s = schema();
        edit(&mut s);
        db.create_collection("", "x", s)
    };
    // 主键与分区键不接受 TIMESTAMPTZ（3.x 的文案）。
    let error = make(&|s| {
        s.fields[0] = {
            let mut f = FieldSchema::new("id", DataType::Timestamptz);
            f.is_primary = true;
            f
        }
    })
    .unwrap_err();
    assert!(
        error.to_string().contains("must be VARCHAR or INT64"),
        "{error}"
    );
    let error = make(&|s| s.fields[2].is_partition_key = true).unwrap_err();
    assert!(
        error.to_string().contains("must be INT64 or VARCHAR"),
        "{error}"
    );
    // ARRAY 的 TIMESTAMPTZ 元素保持 UNIMPLEMENTED。
    let error = make(&|s| {
        let mut a = FieldSchema::new("a", DataType::Array);
        a.element_type = Some(DataType::Timestamptz);
        a.max_capacity = Some(2);
        s.fields.push(a);
    })
    .unwrap_err();
    assert!(
        matches!(
            &error,
            EngineError::Unsupported {
                feature: "ARRAY element type",
                ..
            }
        ),
        "{error:?}"
    );
    // 默认值：字符串必须带偏移并换算成微秒；其他取值按 3.x 的 parse_timestamptz 报错。
    for (value, expected) in [
        (json!(true), "TIMESTAMPTZ value must not be bool"),
        (
            json!(1.5),
            "TIMESTAMPTZ value must be ISO string, aware datetime, or int microseconds; got float",
        ),
        (
            json!("2025-01-01T00:00:00"),
            "TIMESTAMPTZ value must include a timezone offset or use a configured timezone",
        ),
        (
            json!("xx"),
            "TIMESTAMPTZ value 'xx' is not a valid ISO 8601 timestamp",
        ),
        (
            json!("0001-01-01T00:00:00+08:00"),
            "date value out of range",
        ),
    ] {
        let error = make(&|s| s.fields[3].default_value = Some(value.clone())).unwrap_err();
        assert_eq!(error.to_string(), expected);
    }
    make(&|s| s.fields[3].default_value = Some(json!("2025-01-01T08:00:00+08:00"))).unwrap();
    let described = db.collection("", "x").unwrap().read().schema().clone();
    assert_eq!(
        described.fields[3].default_value,
        Some(json!(1_735_689_600_000_000_i64))
    );
    // 保留字。
    for name in ["iso", "ISO", "interval", "INTERVAL"] {
        let error = make(&|s| s.fields[2].name = name.into()).unwrap_err();
        assert!(error.to_string().contains("is reserved"), "{error}");
    }
}

fn ids(snapshot: &milvus_lite_core::ReadSnapshot, expr: &str) -> Result<Vec<i64>, EngineError> {
    let result = snapshot.query(expr, &[], None, 0)?;
    let mut ids: Vec<i64> = result
        .rows
        .pks
        .iter()
        .map(|pk| match pk {
            milvus_lite_core::Pk::Int64(v) => *v,
            other => panic!("{other:?}"),
        })
        .collect();
    ids.sort_unstable();
    Ok(ids)
}

#[test]
fn timestamp_filters_match_3x_results() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    // ts：2025-01-01、0001-01-01、9999-12-31T23:59:59.999999、空；ts2：2025-01-02、2024-01-01、2024-01-01、2024-01-01。
    let (jan1, jan2, y2024) = (
        1_735_689_600_000_000,
        1_735_776_000_000_000,
        1_704_067_200_000_000,
    );
    db.insert(
        "",
        "c",
        batch(
            &[1, 2, 3, 4],
            &[Some(jan1), Some(MIN_MICROS), Some(MAX_MICROS), None],
            &[jan2, y2024, y2024, y2024],
        ),
    )
    .unwrap();
    let handle = db.collection("", "c").unwrap();
    handle.write().load().unwrap();
    let snapshot = handle.read().read_snapshot();
    // 期望值与真实 3.x 逐条核对过（见 tests/differential/test_timestamp_filters.py）。
    for (expr, expected) in [
        ("ts >= ISO '2025-01-01T00:00:00Z'", vec![1, 3]),
        ("ts == ISO '2025-01-01T08:00:00+08:00'", vec![1]),
        ("ts != ISO '2025-01-01T00:00:00Z'", vec![2, 3]),
        ("ISO '2025-01-01T00:00:00Z' < ts", vec![3]),
        ("ts >= iso \"2025-01-01T00:00:00Z\"", vec![1, 3]),
        (
            "ts in [ISO '2025-01-01T00:00:00Z', ISO '0001-01-01T00:00:00Z']",
            vec![1, 2],
        ),
        // Arrow 的 is_in 对空值给 false，取反后空值行命中。
        ("ts not in [ISO '2025-01-01T00:00:00Z']", vec![2, 3, 4]),
        ("ts in []", vec![]),
        ("ts not in []", vec![1, 2, 3, 4]),
        ("ts is null", vec![4]),
        ("ts is not null", vec![1, 2, 3]),
        ("ts < ts2", vec![1, 2]),
        ("ts == ts2 or ts is null", vec![4]),
        ("not (ts > ISO '2025-01-01T00:00:00Z')", vec![1, 2]),
        ("ts + INTERVAL 'P1D' == ts2", vec![1]),
        ("INTERVAL 'P1D' + ts == ts2", vec![1]),
        (
            "ts - INTERVAL 'PT1H' < ISO '2025-01-01T00:00:00Z'",
            vec![1, 2],
        ),
        ("ts + INTERVAL 'P1D' + INTERVAL 'P1D' > ts2", vec![1, 3]),
        // 区间微秒向零截断：PT1.9999999S 是 1999999 微秒。
        (
            "ts + INTERVAL 'PT1.9999999S' > ISO '2025-01-01T00:00:01.999998Z'",
            vec![1, 3],
        ),
        // Arrow 的非检查运算按 int64 回绕。
        (
            "ts + INTERVAL 'P106751991D' > ISO '2025-01-01T00:00:00Z'",
            vec![2],
        ),
        (
            "ts - INTERVAL 'P106751991D' < ISO '2025-01-01T00:00:00Z'",
            vec![1, 3],
        ),
        ("INTERVAL 'P1D' < INTERVAL 'P2D'", vec![1, 2, 3, 4]),
        ("INTERVAL 'P1D' == INTERVAL 'p1d'", vec![1, 2, 3, 4]),
        (
            "ISO '2025-01-01T00:00:00Z' == ISO '2025-01-01T08:00:00+08:00'",
            vec![1, 2, 3, 4],
        ),
        ("ts > ISO '9999-12-31T23:59:59.999999Z'", vec![]),
    ] {
        assert_eq!(ids(&snapshot, expr).unwrap(), expected, "{expr}");
    }
    // 类型检查与语法错误沿用 3.x 的文案。
    for (expr, expected) in [
        ("ts > 1", "comparison '>' between incompatible types"),
        ("ts == 'x'", "right side is string"),
        (
            "ts > INTERVAL 'P1D'",
            "left side is timestamptz (field 'ts'), right side is interval",
        ),
        (
            "ts + 1 > ts2",
            "arithmetic '+' requires numeric operands, left side is timestamptz (field 'ts')",
        ),
        (
            "ts - ts > INTERVAL 'P1D'",
            "arithmetic '-' requires numeric operands, left side is timestamptz (field 'ts')",
        ),
        (
            "-ts > ts2",
            "arithmetic '-' requires numeric operands, right side is timestamptz (field 'ts')",
        ),
        (
            "ts + INTERVAL 'P1D'",
            "top-level filter expression must evaluate to bool, got timestamptz",
        ),
        (
            "ts like 'x%'",
            "LIKE requires a string operand, got timestamptz (field 'ts')",
        ),
        (
            "ts in [ISO '2025-01-01T00:00:00Z', 5]",
            "list elements must be of compatible types, got timestamptz and int",
        ),
        (
            "ts in [INTERVAL 'P1D']",
            "list elements must be literals, got 'INTERVAL'",
        ),
        (
            "ts > ISO '2025-01-01T00:00:00'",
            "must include a timezone offset",
        ),
        (
            "ts > ISO '0001-01-01T00:00:00+01:00'",
            "date value out of range",
        ),
        ("ts + INTERVAL 'P1Y' > ts2", "unsupported INTERVAL 'P1Y'"),
        ("ts + INTERVAL 'PT0S' > ts2", "unsupported INTERVAL 'PT0S'"),
        // 超出 int64 的区间在对行求值时才报错（3.x 把标量交给 Arrow 的位置）。
        (
            "ts + INTERVAL 'P106751992D' > ts2",
            "Python int too large to convert to C long",
        ),
        // 恰为 2^63 微秒也超出 int64。
        (
            "ts + INTERVAL 'PT9223372036854.775808S' > ts2",
            "Python int too large to convert to C long",
        ),
    ] {
        let error = ids(&snapshot, expr).unwrap_err().to_string();
        assert!(error.contains(expected), "{expr}: {error}");
    }
    // 四百位数字的区间在解析阶段得到无穷大。
    let huge = format!("ts + INTERVAL 'P{}D' > ts2", "9".repeat(400));
    let error = ids(&snapshot, &huge).unwrap_err().to_string();
    assert!(
        error.contains("cannot convert float infinity to integer"),
        "{error}"
    );
    // 不涉及时间值的算术仍是范围外构造，先于任何语义检查。
    for expr in [
        "id + 1 > 2",
        "-id > 0",
        "id + 1 > 2 and ts > 5",
        "ts > 5 and id + 1 > 2",
        "(id + 1) + ts > 5",
    ] {
        assert!(
            matches!(ids(&snapshot, expr), Err(EngineError::Unsupported { .. })),
            "{expr}"
        );
    }
}

fn without_ts2(ids: &[i64]) -> InsertBatch {
    let mut b = batch(ids, &vec![None; ids.len()], &vec![0; ids.len()]);
    b.columns.retain(|(name, _)| name != "ts2");
    b
}

#[test]
fn out_of_range_defaults_are_accepted_by_create_and_reopen_but_rejected_when_used() {
    // 3.x 在创建时不查整数默认值的范围，含该默认值的 3.x 目录必须能创建、描述与重开。
    for default in [MAX_MICROS + 1, MIN_MICROS - 1] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        let mut s = schema();
        s.fields[3].default_value = Some(json!(default));
        db.create_collection("", "c", s).unwrap();
        db.close();
        let db = open(root.path());
        let described = db.collection("", "c").unwrap().read().schema().clone();
        assert_eq!(described.fields[3].default_value, Some(json!(default)));
        // 使用默认值的写入整批拒绝、不落盘；显式给值的写入照常。
        let error = db.insert("", "c", without_ts2(&[1])).unwrap_err();
        assert!(
            matches!(&error, EngineError::InvalidArgument(m) if m == "date value out of range"),
            "{error:?}"
        );
        assert!(rows(&db).is_empty());
        db.insert("", "c", batch(&[1], &[None], &[7])).unwrap();
        assert_eq!(rows(&db), vec![(1, None, 7)]);
        db.close();
    }
}

#[test]
fn varchar_slot_strings_are_parsed_by_the_engine_like_3x() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    let with = |ts: InputValues| {
        let mut b = batch(&[1], &[None], &[0]);
        for (name, values) in &mut b.columns {
            if name == "ts" {
                *values = ts.clone();
            }
        }
        b
    };
    let strings = |texts: &[&str]| InputValues::Str(texts.iter().map(|t| t.to_string()).collect());
    db.insert(
        "",
        "c",
        with(strings(&["2025-01-01T08:00:00.1234567+08:00"])),
    )
    .unwrap();
    assert_eq!(rows(&db), vec![(1, Some(1_735_689_600_123_456), 0)]);
    // 无偏移、非 ISO、空串按类型不符报 6；带偏移越界报 1。
    for text in ["2025-01-01T00:00:00", "not a time", ""] {
        let error = db.insert("", "c", with(strings(&[text]))).unwrap_err();
        assert!(
            matches!(&error, EngineError::SchemaValidation(m) if m.contains("does not match dtype DataType.TIMESTAMPTZ")),
            "{text}: {error:?}"
        );
    }
    for text in ["0001-01-01T00:00:00+08:00", "9999-12-31T23:59:59-08:00"] {
        let error = db.insert("", "c", with(strings(&[text]))).unwrap_err();
        assert!(
            matches!(&error, EngineError::InvalidArgument(m) if m == "date value out of range"),
            "{text}: {error:?}"
        );
    }
    assert_eq!(rows(&db).len(), 1);
}
