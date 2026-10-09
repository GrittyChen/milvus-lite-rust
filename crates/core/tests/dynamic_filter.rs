//! 顶层动态过滤在数据源、快照与持久化生命周期中的组合。
#![allow(clippy::unwrap_used)]

use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, EngineError, FieldSchema, InputValues,
    InsertBatch, Pk,
};
use serde_json::{Value, json};

fn schema() -> CollectionSchema {
    CollectionSchema {
        fields: vec![
            FieldSchema {
                is_primary: true,
                ..FieldSchema::new("id", DataType::Int64)
            },
            FieldSchema {
                dim: Some(2),
                ..FieldSchema::new("v", DataType::FloatVector)
            },
            FieldSchema::new("age", DataType::Int64),
        ],
        enable_dynamic_field: true,
        properties: vec![],
    }
}
fn db(path: &std::path::Path) -> std::sync::Arc<Db> {
    Db::new(DataDir::open(path).unwrap(), Compute::for_host().unwrap()).unwrap()
}
fn batch(start: i64, values: &[Value]) -> InsertBatch {
    InsertBatch {
        num_rows: values.len(),
        columns: vec![
            (
                "id".into(),
                InputValues::Int((start..start + values.len() as i64).collect()),
            ),
            ("age".into(), InputValues::Int(vec![1; values.len()])),
            (
                "v".into(),
                InputValues::FloatVector {
                    dim: 2,
                    values: vec![1.0; values.len() * 2],
                },
            ),
            (
                "$meta".into(),
                InputValues::Json(
                    values
                        .iter()
                        .map(|v| json!({"x":v}).to_string().into_bytes())
                        .collect(),
                ),
            ),
        ],
    }
}

#[test]
fn dynamic_type_binding_spans_all_chunks_in_one_active_source() {
    let root = tempfile::tempdir().unwrap();
    let engine = db(root.path());
    engine.create_collection("", "c", schema()).unwrap();
    engine
        .insert("", "c", batch(0, &vec![json!(0.0); 32]))
        .unwrap();
    engine
        .insert("", "c", batch(32, &[json!("other")]))
        .unwrap();
    let snapshot = engine.collection("", "c").unwrap().read().read_snapshot();
    assert_eq!(snapshot.count_filtered("x in [-0.0]").unwrap(), 32);
    engine.flush_collection("", "c").unwrap();
    engine.insert("", "c", batch(40, &[json!(0.0)])).unwrap();
    assert_eq!(
        engine
            .collection("", "c")
            .unwrap()
            .read()
            .read_snapshot()
            .count_filtered("x in [-0.0]")
            .unwrap(),
        32
    );
    engine.drop_collection("", "c").unwrap();
    assert_eq!(snapshot.count_filtered("x in [-0.0]").unwrap(), 32);
    engine.close();
}

#[test]
fn committed_source_type_binding_survives_parquet_batch_splits_and_reopen() {
    let root = tempfile::tempdir().unwrap();
    let engine = db(root.path());
    engine.create_collection("", "c", schema()).unwrap();
    let mut values = vec![json!(0.0); 20_000];
    values.push(json!("other"));
    engine.insert("", "c", batch(0, &values)).unwrap();
    engine.flush_collection("", "c").unwrap();
    engine.close();
    let reopened = db(root.path());
    let handle = reopened.collection("", "c").unwrap();
    handle.write().load();
    let snapshot = handle.read().read_snapshot();
    assert_eq!(snapshot.count_filtered("x in [-0.0]").unwrap(), 20_000);
    reopened.close();
}

#[test]
fn failed_dynamic_delete_preserves_sequence_wal_and_rows() {
    let root = tempfile::tempdir().unwrap();
    let engine = db(root.path());
    engine.create_collection("", "c", schema()).unwrap();
    engine
        .insert("", "c", batch(1, &[json!([1]), json!(1)]))
        .unwrap();
    let dir = root.path().join("collections/c");
    let wal = dir.join("wal/wal_data_000001.arrow");
    let before = std::fs::read(&wal).unwrap();
    let read_manifest = || match std::fs::read(dir.join("manifest.json")) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("manifest read failed: {error}"),
    };
    let manifest = read_manifest();
    let snapshot = engine.collection("", "c").unwrap().read().read_snapshot();
    let rows = snapshot.query("", &[], None, 0).unwrap();
    assert!(
        matches!(engine.delete_expr("","c","x in [1]",""),Err(EngineError::InvalidArgument(message)) if message=="unhashable type: 'list'")
    );
    assert_eq!(std::fs::read(&wal).unwrap(), before);
    assert_eq!(read_manifest(), manifest);
    assert_eq!(
        engine
            .collection("", "c")
            .unwrap()
            .read()
            .read_snapshot()
            .query("", &[], None, 0)
            .unwrap(),
        rows
    );
    engine.insert("", "c", batch(3, &[json!(2)])).unwrap();
    engine.flush_collection("", "c").unwrap();
    assert_eq!(snapshot.query("", &[], None, 0).unwrap(), rows);
    engine.close();
    let reopened = db(root.path());
    let handle = reopened.collection("", "c").unwrap();
    handle.write().load();
    assert_eq!(
        handle
            .read()
            .read_snapshot()
            .query("", &[], None, 0)
            .unwrap()
            .rows
            .pks,
        vec![Pk::Int64(1), Pk::Int64(2), Pk::Int64(3)]
    );
    reopened.close();
}

#[test]
fn unknown_ordering_cannot_commit_a_partial_delete_or_consume_sequence() {
    let root = tempfile::tempdir().unwrap();
    let engine = db(root.path());
    engine.create_collection("", "c", schema()).unwrap();
    let mut input = batch(1, &[json!(1), json!("b")]);
    input.columns.last_mut().unwrap().1 = InputValues::Json(vec![
        json!({"x":1,"y":[1]}).to_string().into_bytes(),
        json!({"x":"b","y":1}).to_string().into_bytes(),
    ]);
    engine.insert("", "c", input).unwrap();
    let dir = root.path().join("collections/c");
    let wal = dir.join("wal/wal_data_000001.arrow");
    let before = std::fs::read(&wal).unwrap();
    let snapshot = engine.collection("", "c").unwrap().read().read_snapshot();
    let rows = snapshot.query("", &[], None, 0).unwrap();
    assert!(
        matches!(engine.delete_expr("","c","x < 'c' and y in [1]",""),Err(EngineError::InvalidArgument(message)) if message=="unhashable type: 'list'")
    );
    assert_eq!(std::fs::read(&wal).unwrap(), before);
    assert!(!dir.join("manifest.json").exists());
    assert!(!dir.join("wal/wal_delta_000001.arrow").exists());
    assert_eq!(
        engine
            .collection("", "c")
            .unwrap()
            .read()
            .read_snapshot()
            .query("", &[], None, 0)
            .unwrap(),
        rows
    );
    engine.insert("", "c", batch(3, &[json!(2)])).unwrap();
    engine.flush_collection("", "c").unwrap();
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["current_seq"], json!(3));
    engine.close();
    let reopened = db(root.path());
    let handle = reopened.collection("", "c").unwrap();
    handle.write().load();
    assert_eq!(
        handle
            .read()
            .read_snapshot()
            .query("", &[], None, 0)
            .unwrap()
            .rows
            .pks,
        vec![Pk::Int64(1), Pk::Int64(2), Pk::Int64(3)]
    );
    reopened.close();
}
