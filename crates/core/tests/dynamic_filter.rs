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
    handle.write().load().unwrap();
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
    handle.write().load().unwrap();
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
    handle.write().load().unwrap();
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
fn out_of_range_float_membership_never_matches_int64_max_or_deletes_it() {
    let root = tempfile::tempdir().unwrap();
    let mut engine = db(root.path());
    engine.create_collection("", "c", schema()).unwrap();
    // 宽整数让 value_set 的 double 路径转换失败，进入 Python 语义：int 与 float 精确比较。
    let below = 9_223_372_036_854_774_784_i64;
    engine
        .insert(
            "",
            "c",
            batch(
                1,
                &[
                    json!(0),
                    json!(i64::MAX),
                    json!(9_007_199_254_740_993_i64),
                    json!(below),
                ],
            ),
        )
        .unwrap();
    let dir = root.path().join("collections/c");
    let check = |engine: &Db| {
        let snapshot = engine.collection("", "c").unwrap().read().read_snapshot();
        for (expr, expected) in [
            ("x in [9223372036854775808.0]", 0),
            ("x in [9.223372036854776e18]", 0),
            ("x in [-0.0, 9223372036854775808.0]", 1),
            ("x in [9223372036854774784.0]", 1),
            ("x in [9223372036854775807]", 1),
        ] {
            assert_eq!(snapshot.count_filtered(expr).unwrap(), expected, "{expr}");
        }
        assert_eq!(
            snapshot
                .query("x in [-0.0, 9223372036854775808.0]", &[], None, 0)
                .unwrap()
                .rows
                .pks,
            vec![Pk::Int64(1)]
        );
    };
    let wal_files = || {
        let mut names: Vec<_> = std::fs::read_dir(dir.join("wal"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    for stage in ["active", "committed", "reopened"] {
        match stage {
            "committed" => engine.flush_collection("", "c").unwrap(),
            "reopened" => {
                engine.close();
                engine = db(root.path());
                engine.collection("", "c").unwrap().write().load().unwrap();
            }
            _ => {}
        }
        check(&engine);
        let rows = engine
            .collection("", "c")
            .unwrap()
            .read()
            .read_snapshot()
            .query("", &[], None, 0)
            .unwrap();
        let before_wal = wal_files();
        let before_data = (stage == "active")
            .then(|| std::fs::read(dir.join("wal/wal_data_000001.arrow")).unwrap());
        let before_manifest =
            (stage != "active").then(|| std::fs::read(dir.join("manifest.json")).unwrap());
        assert!(
            engine
                .delete_expr("", "c", "x in [9223372036854775808.0]", "")
                .unwrap()
                .0
                .is_empty(),
            "{stage}"
        );
        assert_eq!(
            engine
                .collection("", "c")
                .unwrap()
                .read()
                .read_snapshot()
                .query("", &[], None, 0)
                .unwrap(),
            rows,
            "{stage}"
        );
        assert_eq!(wal_files(), before_wal, "{stage}");
        if let Some(data) = &before_data {
            assert_eq!(
                &std::fs::read(dir.join("wal/wal_data_000001.arrow")).unwrap(),
                data
            );
        }
        if let Some(manifest) = &before_manifest {
            assert_eq!(&std::fs::read(dir.join("manifest.json")).unwrap(), manifest);
        }
        // 重开会按磁盘状态重建 next_seq，所以在重开前确认活跃与已提交阶段没有在内存中占用序号：
        // 4 行之后的探针写入取得 5 号；重开后的零匹配删除同样不占用，下一次写入取得 6 号。
        if stage != "active" {
            let (id, seq) = if stage == "committed" { (5, 5) } else { (6, 6) };
            engine.insert("", "c", batch(id, &[json!(1)])).unwrap();
            engine.flush_collection("", "c").unwrap();
            let manifest: Value =
                serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
            assert_eq!(manifest["current_seq"], json!(seq), "{stage}");
        }
    }
    // 只有 ±2^53 内的整数时不转入 Python：与小整数主键列一样按 double 位模式查集合，-0.0 不命中 0。
    engine.create_collection("", "small", schema()).unwrap();
    engine
        .insert("", "small", batch(1, &[json!(0), json!(1), json!(2)]))
        .unwrap();
    let small = engine
        .collection("", "small")
        .unwrap()
        .read()
        .read_snapshot();
    for (expr, expected) in [
        ("x in [9223372036854775808.0]", 0),
        ("x in [-0.0, 9223372036854775808.0]", 0),
        ("x in [-0.0]", 1),
    ] {
        assert_eq!(small.count_filtered(expr).unwrap(), expected, "{expr}");
    }
    engine.close();
    let reopened = db(root.path());
    reopened
        .collection("", "c")
        .unwrap()
        .write()
        .load()
        .unwrap();
    check(&reopened);
    reopened.close();
}
