//! GEOMETRY（0034 第 1 步）：校验过的 WKT 原文列的写入、落盘（Arrow `string`）、恢复、合并与 schema 规则。
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use arrow_schema::DataType as ArrowType;
use milvus_lite_core::data::Validity;
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

/// `geo` 可空、没有默认值；`g2` 可空且有默认值。
fn schema() -> CollectionSchema {
    let mut id = FieldSchema::new("id", DataType::Int64);
    id.is_primary = true;
    let mut vector = FieldSchema::new("v", DataType::FloatVector);
    vector.dim = Some(2);
    let mut geo = FieldSchema::new("geo", DataType::Geometry);
    geo.nullable = true;
    let mut g2 = FieldSchema::new("g2", DataType::Geometry);
    g2.nullable = true;
    g2.default_value = Some(json!("POINT(9 9)"));
    CollectionSchema {
        functions: Vec::new(),
        fields: vec![id, vector, geo, g2],
        enable_dynamic_field: false,
        properties: vec![],
    }
}

fn nullable(values: &[Option<&str>]) -> InputValues {
    InputValues::Nullable {
        values: Box::new(InputValues::Str(
            values.iter().map(|v| v.unwrap_or("").to_owned()).collect(),
        )),
        validity: Validity::from_bools(&values.iter().map(Option::is_some).collect::<Vec<_>>()),
    }
}

fn batch(ids: &[i64], geo: &[Option<&str>], g2: &[Option<&str>]) -> InsertBatch {
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
            ("geo".into(), nullable(geo)),
            ("g2".into(), nullable(g2)),
        ],
    }
}

type Row = (i64, Option<String>, Option<String>);

fn rows(db: &Db) -> Vec<Row> {
    let handle = db.collection("", "c").unwrap();
    handle.write().load().unwrap();
    let snapshot = handle.read().read_snapshot();
    let result = snapshot
        .query("", &["geo".into(), "g2".into()], None, 0)
        .unwrap();
    let column = |name: &str, row: usize| {
        let column = &result
            .rows
            .fields
            .iter()
            .find(|(n, _)| n == name)
            .unwrap()
            .1;
        let ColumnData::VarChar(values) = column.values() else {
            panic!("GEOMETRY is stored as a string column");
        };
        (!column.is_null(row)).then(|| values[row].clone())
    };
    let mut out: Vec<Row> = (0..result.rows.pks.len())
        .map(|row| {
            let pk = match &result.rows.pks[row] {
                milvus_lite_core::Pk::Int64(v) => *v,
                other => panic!("{other:?}"),
            };
            (pk, column("geo", row), column("g2", row))
        })
        .collect();
    out.sort();
    out
}

fn row(id: i64, geo: Option<&str>, g2: &str) -> Row {
    (id, geo.map(str::to_owned), Some(g2.to_owned()))
}

#[test]
fn original_text_survives_wal_flush_compaction_and_reopen_as_arrow_strings() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    // 原文不规范化；有默认值的列，显式空值取默认值（3.x：nullable 加默认值时也一样）。
    let spaced = "  point ( 1e1   -2.5 )  ";
    let polygon = "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))";
    db.insert(
        "",
        "c",
        batch(
            &[1, 2, 3],
            &[Some(spaced), None, Some(polygon)],
            &[Some("PO\u{130}NT(\u{661} 1_0)\n"), None, Some("POINT(2 2)")],
        ),
    )
    .unwrap();
    let expected = vec![
        row(1, Some(spaced), "PO\u{130}NT(\u{661} 1_0)\n"),
        row(2, None, "POINT(9 9)"),
        row(3, Some(polygon), "POINT(2 2)"),
    ];
    assert_eq!(rows(&db), expected);
    let wal = std::fs::read_dir(root.path().join("collections/c/wal"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|e| e == "arrow"))
        .unwrap();
    let reader =
        arrow_ipc::reader::StreamReader::try_new(std::fs::File::open(wal).unwrap(), None).unwrap();
    for name in ["geo", "g2"] {
        let wal_schema = reader.schema();
        let field = wal_schema.field_with_name(name).unwrap();
        assert_eq!(field.data_type(), &ArrowType::Utf8);
        assert!(field.is_nullable());
    }
    for number in 0..4 {
        if number > 0 {
            db.insert(
                "",
                "c",
                batch(&[10 + number], &[Some("POINT(1 1)")], &[Some("POINT(3 3)")]),
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
    assert_eq!(
        builder.schema().field_with_name("geo").unwrap().data_type(),
        &ArrowType::Utf8
    );
    // 3.x 的 schema.json 把类型写成 "geometry"。
    let saved = std::fs::read_to_string(root.path().join("collections/c/schema.json")).unwrap();
    assert_eq!(saved.matches("\"dtype\": \"geometry\"").count(), 2);
    let reopened = open(root.path());
    let mut all = expected;
    all.extend((1..4).map(|n| row(10 + n, Some("POINT(1 1)"), "POINT(3 3)")));
    assert_eq!(rows(&reopened), all);
}

#[test]
fn invalid_wkt_rejects_the_whole_batch_without_writes() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1], &[Some("POINT(1 1)")], &[None]))
        .unwrap();
    for (bad, repr) in [
        ("not-wkt", "'not-wkt'"),
        ("", "''"),
        ("POINT(inf 1)", "'POINT(inf 1)'"),
        ("LINESTRING(0 0,1 1)", "'LINESTRING(0 0,1 1)'"),
        (
            "POLYGON((0 0,2 2,2 0,0 2,0 0))",
            "'POLYGON((0 0,2 2,2 0,0 2,0 0))'",
        ),
    ] {
        for field in ["geo", "g2"] {
            let (geo, g2) = if field == "geo" {
                (Some(bad), Some("POINT(1 1)"))
            } else {
                (Some("POINT(1 1)"), Some(bad))
            };
            let error = db
                .insert(
                    "",
                    "c",
                    batch(&[2, 3], &[Some("POINT(5 5)"), geo], &[None, g2]),
                )
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("field '{field}' value {repr} does not match dtype DataType.GEOMETRY")
            );
            assert!(
                matches!(error, EngineError::SchemaValidation(_)),
                "{error:?}"
            );
        }
    }
    let expected = vec![row(1, Some("POINT(1 1)"), "POINT(9 9)")];
    assert_eq!(rows(&db), expected);
    db.close();
    assert_eq!(rows(&open(root.path())), expected);
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
    // 主键与分区键不接受 GEOMETRY（3.x 服务端的文案）。
    let error = make(&|s| {
        s.fields[0] = {
            let mut f = FieldSchema::new("id", DataType::Geometry);
            f.is_primary = true;
            f
        }
    })
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "primary key 'id' must be VARCHAR or INT64, got DataType.GEOMETRY"
    );
    let error = make(&|s| {
        s.fields[2].nullable = false;
        s.fields[2].is_partition_key = true;
    })
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "partition key 'geo' must be INT64 or VARCHAR, got GEOMETRY"
    );
    // ARRAY 的 GEOMETRY 元素保持 UNIMPLEMENTED。
    let error = make(&|s| {
        let mut a = FieldSchema::new("a", DataType::Array);
        a.element_type = Some(DataType::Geometry);
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
    // 默认值：不是字符串或不是合法 WKT 都报同一条；JSON null 等于没有默认值。
    for value in [
        json!(5),
        json!(true),
        json!("nope"),
        json!("POINT(1)"),
        json!(""),
    ] {
        let error = make(&|s| s.fields[3].default_value = Some(value.clone())).unwrap_err();
        assert_eq!(
            error.to_string(),
            "GEOMETRY field 'g2' default_value is invalid"
        );
    }
    make(&|s| s.fields[3].default_value = Some(json!("  polygon((0 0,1 0,1 1,0 0)) "))).unwrap();
    let described = db.collection("", "x").unwrap().read().schema().clone();
    assert_eq!(
        described.fields[3].default_value,
        Some(json!("  polygon((0 0,1 0,1 1,0 0)) "))
    );
}

/// 第 1 步还没有 `geometry` 语义类型：引用 GEOMETRY 字段的过滤一律 UNIMPLEMENTED，不按字符串列求值，
/// 也不报类型错误。
#[test]
fn filters_that_reference_a_geometry_field_stay_unimplemented() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1], &[Some("POINT(1 1)")], &[None]))
        .unwrap();
    let handle = db.collection("", "c").unwrap();
    handle.write().load().unwrap();
    let snapshot = handle.read().read_snapshot();
    for expr in [
        "geo is null",
        "geo is not null",
        "geo == geo",
        "geo == g2",
        "geo == 1",
        "geo == 'POINT(1 1)'",
        "geo in ['POINT(1 1)']",
        "geo like 'P%'",
        "text_match(geo, 'point')",
        "ST_ISVALID(geo)",
        "ST_WITHIN(geo, 'POINT(1 1)')",
        "ST_DWITHIN(g2, 'POINT(1 1)', 5)",
        "id > 0 and geo is null",
        "id > 0 or not (g2 == geo)",
        "ST_ISVALID(id)",
    ] {
        let error = snapshot.query(expr, &[], None, 0).unwrap_err();
        assert!(
            matches!(
                &error,
                EngineError::Unsupported { feature: "filter expressions", reason }
                    if reason == "geometry predicates are not supported"
            ),
            "{expr}: {error:?}"
        );
    }
    // 不引用 GEOMETRY 字段的过滤不受影响。
    assert_eq!(
        snapshot
            .query("id > 0", &[], None, 0)
            .unwrap()
            .rows
            .pks
            .len(),
        1
    );
}

/// 3.x 能建出 `ARRAY<GEOMETRY>` 并经原始 gRPC 写入；这样的集合在这里打不开，但不妨碍 drop 与同目录的
/// 其他集合。
#[test]
fn collections_with_geometry_array_elements_from_3x_cannot_be_opened_but_can_be_dropped() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    let mut with_array = schema();
    let mut a = FieldSchema::new("a", DataType::Array);
    a.element_type = Some(DataType::VarChar);
    a.max_capacity = Some(4);
    a.nullable = true;
    with_array.fields.push(a);
    db.create_collection("", "arr", with_array).unwrap();
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(&[1], &[Some("POINT(1 1)")], &[None]))
        .unwrap();
    db.close();
    // 3.x 的 `list<string>` 元素列与 VARCHAR 元素相同，区别只在 schema.json 的 element_type。
    let path = root.path().join("collections/arr/schema.json");
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.matches("\"element_type\": \"varchar\"").count(), 1);
    std::fs::write(
        &path,
        text.replace(
            "\"element_type\": \"varchar\"",
            "\"element_type\": \"geometry\"",
        ),
    )
    .unwrap();
    let db = open(root.path());
    let error = db.collection("", "arr").map(|_| ()).unwrap_err();
    assert!(
        matches!(&error, EngineError::Unsupported { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("ARRAY element type"), "{error}");
    assert_eq!(rows(&db), vec![row(1, Some("POINT(1 1)"), "POINT(9 9)")]);
    db.drop_collection("", "arr").unwrap();
    assert!(!root.path().join("collections/arr").exists());
}
