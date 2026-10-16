//! ARRAY 父/子空值、列复制、切片及 WAL/segment/合并的生命周期。
#![allow(clippy::unwrap_used)]
use milvus_lite_core::storage::arrow_data;
use milvus_lite_core::{
    CollectionSchema, ColumnData, Compute, DataDir, DataType, Db, FieldSchema, InputValues,
    InsertBatch, Pk,
};
use serde_json::json;

fn schema() -> CollectionSchema {
    let mut pk = FieldSchema::new("id", DataType::Int64);
    pk.is_primary = true;
    let mut v = FieldSchema::new("v", DataType::FloatVector);
    v.dim = Some(2);
    let mut fields = vec![pk, v];
    for (name, dt) in [
        ("b", DataType::Bool),
        ("i8", DataType::Int8),
        ("i16", DataType::Int16),
        ("i32", DataType::Int32),
        ("i64", DataType::Int64),
        ("f", DataType::Float),
        ("d", DataType::Double),
        ("s", DataType::VarChar),
    ] {
        let mut f = FieldSchema::new(name, DataType::Array);
        f.element_type = Some(dt);
        f.max_capacity = Some(4);
        f.nullable = true;
        fields.push(f);
    }
    let mut default = FieldSchema::new("default", DataType::Array);
    default.element_type = Some(DataType::Int64);
    default.default_value = Some(json!([]));
    fields.push(default);
    CollectionSchema {
        fields,
        enable_dynamic_field: false,
        properties: vec![],
    }
}
fn batch(start: i64) -> InsertBatch {
    let mut columns = vec![
        (
            "id".into(),
            InputValues::Int(vec![start, start + 1, start + 2]),
        ),
        (
            "v".into(),
            InputValues::FloatVector {
                dim: 2,
                values: vec![1., 0., 1., 0., 1., 0.],
            },
        ),
    ];
    for (name, good) in [
        ("b", json!(true)),
        ("i8", json!(7)),
        ("i16", json!(7)),
        ("i32", json!(7)),
        ("i64", json!(7)),
        ("f", json!(1.5)),
        ("d", json!(1.5)),
        ("s", json!("abc")),
    ] {
        columns.push((
            name.into(),
            InputValues::Array(vec![
                None,
                Some(InputValues::Int(vec![])),
                Some(InputValues::JsonValues(vec![json!(null), good])),
            ]),
        ));
    }
    InsertBatch {
        num_rows: 3,
        columns,
    }
}
#[test]
fn arrays_roundtrip_slices_gathers_defaults_and_old_snapshots() {
    let root = tempfile::tempdir().unwrap();
    let compute = Compute::new(std::num::NonZeroUsize::MIN).unwrap();
    let db = Db::new(DataDir::open(root.path()).unwrap(), compute).unwrap();
    let s = schema();
    db.create_collection("", "c", s.clone()).unwrap();
    db.insert("", "c", batch(1)).unwrap();
    let old = db.collection("", "c").unwrap().read().read_snapshot();
    let rows = old.query("", &["*".into()], None, 0).unwrap().rows;
    let columns: Vec<_> = s
        .fields
        .iter()
        .map(|f| {
            rows.fields
                .iter()
                .find(|(n, _)| n == &f.name)
                .unwrap()
                .1
                .clone()
        })
        .collect();
    let arrow = arrow_data::data_batch(&s, &[1, 2, 3], &columns, None).unwrap();
    let sliced = arrow.slice(1, 2);
    let decoded = arrow_data::decode_data_batch(&s, &sliced).unwrap();
    for col in &decoded.columns[2..10] {
        assert!(col.validity().is_none());
        let ColumnData::Array(a) = col else {
            panic!("array")
        };
        assert_eq!(a.offsets, vec![0, 0, 2]);
        assert!(a.values.is_null(0));
        assert!(!a.values.is_null(1));
    }
    for part in 0..4 {
        if part > 0 {
            db.insert("", "c", batch(part * 3 + 1)).unwrap();
        }
        db.flush_collection("", "c").unwrap();
    }
    db.close();
    let m: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join("collections/c/manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        m["partitions"]["_default"]["data_files"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let db = Db::new(
        DataDir::open(root.path()).unwrap(),
        Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
    )
    .unwrap();
    let c = db.collection("", "c").unwrap();
    c.write().load().unwrap();
    let read = c.read().read_snapshot();
    assert_eq!(read.count_filtered("i8 is null").unwrap(), 4);
    assert_eq!(read.count_filtered("array_length(i8)==0").unwrap(), 8);
    assert_eq!(read.count_filtered("array_contains(i8,7)").unwrap(), 4);
    assert_eq!(read.count_filtered("array_length(default)==0").unwrap(), 12);
    db.upsert_with_pk_type(
        "",
        "c",
        InsertBatch {
            num_rows: 1,
            columns: vec![
                ("id".into(), InputValues::Int(vec![3])),
                ("i8".into(), InputValues::Null(1)),
            ],
        },
    )
    .unwrap();
    db.delete("", "c", &[Pk::Int64(6)]).unwrap();
    assert_eq!(old.count_filtered("array_contains(i8,7)").unwrap(), 1);
    assert_eq!(read.count_filtered("array_contains(i8,7)").unwrap(), 4);
    let current = c.read().read_snapshot();
    assert_eq!(current.count_filtered("array_contains(i8,7)").unwrap(), 2);
    assert_eq!(current.count_filtered("i8 is null").unwrap(), 5);
    db.close();
}
#[test]
fn array_nan_membership_keeps_python_identity_without_changing_comparison() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::new(
        DataDir::open(root.path()).unwrap(),
        Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
    )
    .unwrap();
    db.create_collection("", "c", schema()).unwrap();
    let mut b = batch(1);
    b.columns.iter_mut().find(|(n, _)| n == "d").unwrap().1 = InputValues::Array(vec![
        Some(InputValues::Float(vec![f64::NAN])),
        Some(InputValues::Float(vec![f64::INFINITY])),
        Some(InputValues::JsonValues(vec![json!(null)])),
    ]);
    db.insert("", "c", b).unwrap();
    for flushed in [false, true] {
        if flushed {
            db.flush_collection("", "c").unwrap();
        }
        let read = db.collection("", "c").unwrap().read().read_snapshot();
        assert_eq!(read.count_filtered("array_contains(d,d[0])").unwrap(), 3);
        assert_eq!(read.count_filtered("d[0] == d[0]").unwrap(), 1);
        assert_eq!(
            read.count_filtered("array_length(d)==1 and d==d").unwrap(),
            3
        );
    }
    db.close();
}

#[test]
fn array_schema_defaults_and_business_errors_match_336_reference_cases() {
    use milvus_lite_core::Collection;
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/array_v321.json")).unwrap();
    for case in fixture["business"].as_array().unwrap() {
        let dt = match case["element_type"].as_str().unwrap() {
            "BOOL" => DataType::Bool,
            "INT8" => DataType::Int8,
            "INT16" => DataType::Int16,
            "INT32" => DataType::Int32,
            "INT64" => DataType::Int64,
            "FLOAT" => DataType::Float,
            "DOUBLE" => DataType::Double,
            "VARCHAR" => DataType::VarChar,
            _ => panic!("type"),
        };
        let mut s = schema();
        s.fields.truncate(2);
        let mut x = FieldSchema::new("x", DataType::Array);
        x.element_type = Some(dt);
        x.max_capacity = Some(2);
        x.nullable = case["nullable"].as_bool().unwrap();
        x.default_value = (!case["default_value"].is_null()).then(|| case["default_value"].clone());
        s.fields.push(x);
        let mut c = Collection::new("c", s).unwrap();
        let mut input = batch(1);
        input.num_rows = 1;
        input.columns.truncate(2);
        input.columns[0].1 = InputValues::Int(vec![1]);
        input.columns[1].1 = InputValues::FloatVector {
            dim: 2,
            values: vec![1., 0.],
        };
        if case["input"] != "missing" {
            input.columns.push((
                "x".into(),
                InputValues::JsonValues(vec![case["input_value"].clone()]),
            ));
        }
        let outcome = c.insert(input);
        let expected = if case["validate"]["ok"] == false {
            Some(case["validate"]["error"].as_str().unwrap())
        } else if case["batch"]["ok"] == false {
            Some(case["batch"]["error"].as_str().unwrap())
        } else {
            None
        };
        if let Some(expected) = expected {
            assert_eq!(outcome.unwrap_err().to_string(), expected, "{case}");
            assert_eq!(c.count(), 0);
            continue;
        }
        outcome.unwrap();
        let rows = c
            .read_snapshot()
            .query("", &["x".into()], None, 0)
            .unwrap()
            .rows;
        let col = &rows.fields[0].1;
        let actual = if col.is_null(0) {
            serde_json::Value::Null
        } else {
            let ColumnData::Array(a) = col.values() else {
                panic!("array")
            };
            serde_json::Value::Array(
                a.range(0)
                    .map(|i| {
                        if a.values.is_null(i) {
                            serde_json::Value::Null
                        } else {
                            match a.values.values() {
                                ColumnData::Bool(v) => json!(v[i]),
                                ColumnData::Int8(v) => json!(v[i]),
                                ColumnData::Int16(v) => json!(v[i]),
                                ColumnData::Int32(v) => json!(v[i]),
                                ColumnData::Int64(v) => json!(v[i]),
                                ColumnData::Float(v) => json!(v[i]),
                                ColumnData::Double(v) => json!(v[i]),
                                ColumnData::VarChar(v) => json!(v[i]),
                                _ => panic!("child"),
                            }
                        }
                    })
                    .collect(),
            )
        };
        assert_eq!(actual, case["batch"]["value"]["values"][0], "{case}");
    }
}

#[test]
fn corrupt_array_offsets_and_child_type_are_rejected_before_arrow_build() {
    use milvus_lite_core::data::ArrayData;
    let mut s = schema();
    s.fields.truncate(3);
    for (offsets, values) in [
        (vec![0, 2, 1], ColumnData::Bool(vec![true])),
        (vec![1, 1, 1], ColumnData::Bool(vec![true])),
        (vec![0, 0, 2], ColumnData::Bool(vec![true])),
        (vec![0, 0, 1], ColumnData::Int64(vec![7])),
    ] {
        let a = ColumnData::Array(ArrayData {
            element_type: DataType::Bool,
            element_name: "item".into(),
            offsets,
            values: Box::new(values),
        });
        assert!(
            arrow_data::data_batch(
                &s,
                &[1, 2],
                &[
                    ColumnData::Int64(vec![1, 2]),
                    ColumnData::FloatVector {
                        dim: 2,
                        values: vec![1., 0., 1., 0.]
                    },
                    a
                ],
                None
            )
            .is_err()
        );
    }
}
