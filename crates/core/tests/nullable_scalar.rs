//! 标量空值贯通列、快照、WAL、合并与默认值；参照真值另由组件与差分固定。
#![allow(clippy::unwrap_used)]

use milvus_lite_core::data::Validity;
use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, FieldSchema, InputValues, InsertBatch, Pk,
    QueryVectors, SearchRequest,
};
use serde_json::json;
use std::sync::Arc;

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
    let mut x = FieldSchema::new("x", DataType::Int64);
    x.nullable = true;
    let mut flag = FieldSchema::new("flag", DataType::Bool);
    flag.default_value = Some(json!(false));
    let mut title = FieldSchema::new("title", DataType::VarChar);
    title.default_value = Some(json!(""));
    let mut j = FieldSchema::new("j", DataType::Json);
    j.nullable = true;
    CollectionSchema {
        fields: vec![id, vector, x, flag, title, j],
        enable_dynamic_field: false,
        properties: vec![],
    }
}
fn batch(start: i64) -> InsertBatch {
    InsertBatch {
        num_rows: 3,
        columns: vec![
            (
                "id".into(),
                InputValues::Int(vec![start, start + 1, start + 2]),
            ),
            (
                "v".into(),
                InputValues::FloatVector {
                    dim: 2,
                    values: vec![1.0, 0.0, 1.0, 0.0, 1.0, 0.0],
                },
            ),
            (
                "x".into(),
                InputValues::Nullable {
                    values: Box::new(InputValues::Int(vec![999, 7, 0])),
                    validity: Validity::from_bools(&[false, true, true]),
                },
            ),
            (
                "j".into(),
                InputValues::Nullable {
                    values: Box::new(InputValues::Json(vec![
                        b"null".to_vec(),
                        br#"{"k":1}"#.to_vec(),
                        b"null".to_vec(),
                    ])),
                    validity: Validity::from_bools(&[false, true, false]),
                },
            ),
        ],
    }
}
#[test]
fn nulls_defaults_snapshots_and_real_compaction_survive_reopen() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(1)).unwrap();
    let old = db.collection("", "c").unwrap().read().read_snapshot();
    assert_eq!(old.count_filtered("x is null").unwrap(), 1);
    assert_eq!(old.count_filtered("x not in [7]").unwrap(), 2);
    let rows = old
        .query(
            "",
            &["x".into(), "flag".into(), "title".into(), "j".into()],
            None,
            0,
        )
        .unwrap()
        .rows;
    let x = &rows.fields.iter().find(|(name, _)| name == "x").unwrap().1;
    assert!(x.is_null(0));
    assert!(!x.is_null(1));
    assert!(
        rows.fields
            .iter()
            .find(|(name, _)| name == "flag")
            .unwrap()
            .1
            .validity()
            .is_none()
    );
    for number in 0..4 {
        if number != 0 {
            db.insert("", "c", batch(1 + number * 3)).unwrap();
        }
        db.flush_collection("", "c").unwrap();
    }
    db.close();
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join("collections/c/manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        manifest["partitions"]["_default"]["data_files"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let reopened = open(root.path());
    let handle = reopened.collection("", "c").unwrap();
    handle.write().load().unwrap();
    let current = handle.read().read_snapshot();
    assert_eq!(current.count_filtered("x is null").unwrap(), 4);
    assert_eq!(
        current
            .count_filtered("flag == false and title == ''")
            .unwrap(),
        12
    );
    assert_eq!(current.count_filtered("j is null").unwrap(), 8);
    assert_eq!(old.count_filtered("x is null").unwrap(), 1);
    let result = current
        .search_filtered(
            SearchRequest {
                anns_field: None,
                vectors: QueryVectors::Dense(vec![vec![1.0, 0.0]]),
                metric_type: Some("L2".into()),
                limit: 20,
                offset: 0,
                output_fields: vec!["x".into()],
            },
            reopened.compute(),
            "x is null",
        )
        .unwrap();
    assert_eq!(
        result.hits[0].rows.pks,
        vec![Pk::Int64(1), Pk::Int64(4), Pk::Int64(7), Pk::Int64(10)]
    );
    assert!(result.hits[0].rows.fields[0].1.is_null(0));
    reopened.close();
}

#[test]
fn partial_upsert_keeps_absent_columns_and_resolves_explicit_null_defaults() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    db.insert("", "c", batch(1)).unwrap();
    db.upsert_with_pk_type(
        "",
        "c",
        InsertBatch {
            num_rows: 1,
            columns: vec![
                ("id".into(), InputValues::Int(vec![2])),
                ("x".into(), InputValues::Null(1)),
                ("flag".into(), InputValues::Null(1)),
            ],
        },
    )
    .unwrap();
    let snapshot = db.collection("", "c").unwrap().read().read_snapshot();
    assert_eq!(snapshot.count_filtered("x is null").unwrap(), 2);
    assert_eq!(
        snapshot
            .count_filtered("flag == false and title == ''")
            .unwrap(),
        3
    );
    db.close();
}

#[test]
fn auto_primary_nulls_are_assigned_and_null_primary_without_auto_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    let mut s = schema();
    s.fields[0].auto_id = true;
    s.fields[0].default_value = Some(json!("invalid but unused auto-id default"));
    db.create_collection("", "c", s).unwrap();
    let mut input = batch(1);
    input.columns[0].1 = InputValues::Nullable {
        values: Box::new(InputValues::Int(vec![100, 101, 102])),
        validity: Validity::from_bools(&[false, true, false]),
    };
    assert_eq!(
        db.insert("", "c", input).unwrap(),
        vec![Pk::Int64(1), Pk::Int64(101), Pk::Int64(3)]
    );
    db.create_collection("", "plain", schema()).unwrap();
    let mut input = batch(1);
    input.columns[0].1 = InputValues::Null(3);
    assert_eq!(
        db.insert("", "plain", input).unwrap_err().to_string(),
        "primary key 'id' missing or None"
    );
    db.close();
}
