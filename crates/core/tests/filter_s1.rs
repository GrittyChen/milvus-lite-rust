//! S1 在有效位与不可变读视图上的组合；协议真值另由差分套件固定。
#![allow(clippy::unwrap_used)]
use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, EngineError, FieldSchema, InputValues,
    InsertBatch, Pk, QueryVectors, SearchRequest,
};

fn schema() -> CollectionSchema {
    let mut id = FieldSchema::new("id", DataType::Int64);
    id.is_primary = true;
    let mut vector = FieldSchema::new("v", DataType::FloatVector);
    vector.dim = Some(2);
    CollectionSchema {
        fields: vec![id, vector, FieldSchema::new("age", DataType::Int64)],
        enable_dynamic_field: false,
        properties: vec![],
    }
}
fn batch(rows: &[(i64, i64)]) -> InsertBatch {
    InsertBatch {
        num_rows: rows.len(),
        columns: vec![
            (
                "id".into(),
                InputValues::Int(rows.iter().map(|r| r.0).collect()),
            ),
            (
                "v".into(),
                InputValues::FloatVector {
                    dim: 2,
                    values: [1.0, 0.0].repeat(rows.len()),
                },
            ),
            (
                "age".into(),
                InputValues::Int(rows.iter().map(|r| r.1).collect()),
            ),
        ],
    }
}
fn db(path: &std::path::Path) -> std::sync::Arc<Db> {
    Db::new(DataDir::open(path).unwrap(), Compute::for_host().unwrap()).unwrap()
}

#[test]
fn membership_preserves_arrow_cast_selection_and_signed_zero_without_changing_comparisons() {
    let root = tempfile::tempdir().unwrap();
    let engine = db(root.path());
    engine.create_collection("", "c", schema()).unwrap();
    engine
        .insert(
            "",
            "c",
            batch(&[
                (0, 0),
                (1, 1),
                (9_007_199_254_740_993, 2),
                (i64::MIN, 3),
                (i64::MAX, 4),
                (BELOW_2_63, 5),
            ]),
        )
        .unwrap();
    for flush in [false, true] {
        if flush {
            engine.flush_collection("", "c").unwrap();
        }
        let snapshot = engine.collection("", "c").unwrap().read().read_snapshot();
        // None 表示报错：比较走 double 的安全转换，宽整数报错；membership 整数化则不转列。
        // 2^63 不能无损转成 int64，value_set 改走 double，宽整数列同样报错。
        for (expr, expected) in [
            ("id in [-0.0]", Some(vec![Pk::Int64(0)])),
            ("id in [-0.0, 1]", Some(vec![Pk::Int64(0), Pk::Int64(1)])),
            ("id in [-0.0, 9007199254740992.0]", Some(vec![Pk::Int64(0)])),
            ("id in [-0.0, 9223372036854775808.0]", None),
            ("id in [9223372036854775808.0]", None),
            (
                "id in [9223372036854774784.0]",
                Some(vec![Pk::Int64(BELOW_2_63)]),
            ),
            (
                "id in [-9223372036854775808.0]",
                Some(vec![Pk::Int64(i64::MIN)]),
            ),
            (
                "id in [9223372036854775807]",
                Some(vec![Pk::Int64(i64::MAX)]),
            ),
            ("id == -0.0", None),
        ] {
            let result = snapshot.query(expr, &[], None, 0);
            match expected {
                Some(pks) => assert_eq!(result.unwrap().rows.pks, pks, "{expr}"),
                None => assert!(
                    matches!(&result, Err(EngineError::InvalidArgument(m)) if m == "Integer value 9007199254740993 not in range: -9007199254740992 to 9007199254740992"),
                    "{expr}: {result:?}"
                ),
            }
        }
        assert!(matches!(
            snapshot.query("id in [-0.0, 1.5]", &[], None, 0),
            Err(EngineError::InvalidArgument(_))
        ));
    }
    engine.create_collection("", "small", schema()).unwrap();
    engine
        .insert("", "small", batch(&[(0, 0), (1, 1)]))
        .unwrap();
    let snapshot = engine
        .collection("", "small")
        .unwrap()
        .read()
        .read_snapshot();
    assert!(
        snapshot
            .query("id in [-0.0, 1.5]", &[], None, 0)
            .unwrap()
            .rows
            .is_empty()
    );
    assert_eq!(
        snapshot
            .query("id in [0.0, 1.5]", &[], None, 0)
            .unwrap()
            .rows
            .pks,
        [Pk::Int64(0)]
    );
    assert_eq!(
        snapshot.query("id == -0.0", &[], None, 0).unwrap().rows.pks,
        [Pk::Int64(0)]
    );
    // 小整数列可安全转为 double：2^63 不命中任何行，-0.0 按位查集合也不命中 0。
    for expr in [
        "id in [9223372036854775808.0]",
        "id in [-0.0, 9223372036854775808.0]",
    ] {
        assert!(
            snapshot.query(expr, &[], None, 0).unwrap().rows.is_empty(),
            "{expr}"
        );
    }
    engine.close();
}

/// 小于 2^63 的最大 double，可以无损转成 int64。
const BELOW_2_63: i64 = 9_223_372_036_854_774_784;

#[test]
fn out_of_range_float_membership_delete_leaves_rows_sequence_and_wal_unchanged() {
    const WIDE: &str =
        "Integer value 9007199254740993 not in range: -9007199254740992 to 9007199254740992";
    let root = tempfile::tempdir().unwrap();
    let mut engine = db(root.path());
    engine.create_collection("", "c", schema()).unwrap();
    engine
        .insert(
            "",
            "c",
            batch(&[(9_007_199_254_740_993, 1), (i64::MAX - 1, 2), (i64::MAX, 3)]),
        )
        .unwrap();
    let dir = root.path().join("collections/c");
    let rows = |engine: &Db| {
        engine
            .collection("", "c")
            .unwrap()
            .read()
            .read_snapshot()
            .query("", &[], None, 0)
            .unwrap()
    };
    let wal_files = || {
        let mut names: Vec<_> = std::fs::read_dir(dir.join("wal"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    // float(i64::MAX - 1) 也是 2^63；活跃、已提交与重开后都不能命中或删除 i64::MAX。
    for stage in ["active", "committed", "reopened"] {
        match stage {
            "committed" => engine.flush_collection("", "c").unwrap(),
            "reopened" => {
                engine.close();
                engine = db(root.path());
                engine.collection("", "c").unwrap().write().load();
            }
            _ => {}
        }
        assert!(
            matches!(
                engine.collection("", "c").unwrap().read().read_snapshot().query("id in [9223372036854775808.0]", &[], None, 0),
                Err(EngineError::InvalidArgument(m)) if m == WIDE
            ),
            "{stage}"
        );
        let before_rows = rows(&engine);
        let before_wal = wal_files();
        let before_data = (stage == "active")
            .then(|| std::fs::read(dir.join("wal/wal_data_000001.arrow")).unwrap());
        let before_manifest =
            (stage != "active").then(|| std::fs::read(dir.join("manifest.json")).unwrap());
        for expr in [
            "id in [9223372036854775808.0]",
            "id in [9.223372036854776e18]",
        ] {
            assert!(
                matches!(
                    engine.delete_expr("", "c", expr, ""),
                    Err(EngineError::InvalidArgument(m)) if m == WIDE
                ),
                "{stage} {expr}"
            );
            assert_eq!(rows(&engine), before_rows, "{stage} {expr}");
            assert_eq!(wal_files(), before_wal, "{stage} {expr}");
            if let Some(data) = &before_data {
                assert_eq!(
                    &std::fs::read(dir.join("wal/wal_data_000001.arrow")).unwrap(),
                    data
                );
            }
            if let Some(manifest) = &before_manifest {
                assert_eq!(&std::fs::read(dir.join("manifest.json")).unwrap(), manifest);
            }
        }
        // 重开会按磁盘状态重建 next_seq，所以在重开前确认活跃与已提交阶段没有在内存中占用序号：
        // 3 行之后的探针写入取得 4 号；重开后的失败删除同样不占用，下一次写入取得 5 号。
        if stage != "active" {
            let (pk, seq) = if stage == "committed" { (6, 4) } else { (7, 5) };
            engine.insert("", "c", batch(&[(pk, 4)])).unwrap();
            engine.flush_collection("", "c").unwrap();
            let manifest: serde_json::Value =
                serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
            assert_eq!(manifest["current_seq"], serde_json::json!(seq), "{stage}");
        }
    }
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
        vec![
            Pk::Int64(9_007_199_254_740_993),
            Pk::Int64(i64::MAX - 1),
            Pk::Int64(i64::MAX),
            Pk::Int64(6),
            Pk::Int64(7)
        ]
    );
    reopened.close();
}

#[test]
fn query_uses_source_order_and_old_filtered_views_survive_mutation_and_drop() {
    let root = tempfile::tempdir().unwrap();
    let engine = db(root.path());
    engine.create_collection("", "c", schema()).unwrap();
    engine
        .insert("", "c", batch(&[(3, 3), (1, 1), (2, 2)]))
        .unwrap();
    engine.insert("", "c", batch(&[(3, 4)])).unwrap();
    engine.delete("", "c", &[Pk::Int64(1)]).unwrap();
    engine.insert("", "c", batch(&[(1, 5)])).unwrap();
    let snapshot = engine.collection("", "c").unwrap().read().read_snapshot();
    let result = snapshot.query("age>=2 and age<6", &[], Some(2), 1).unwrap();
    assert_eq!(result.rows.pks, [Pk::Int64(2), Pk::Int64(1)]);
    engine.flush_collection("", "c").unwrap();
    engine.delete_expr("", "c", "age>=4", "").unwrap();
    engine.drop_collection("", "c").unwrap();
    engine.create_collection("", "c", schema()).unwrap();
    engine.insert("", "c", batch(&[(9, 9)])).unwrap();
    assert_eq!(
        snapshot.query("age>=2 and age<6", &[], Some(2), 1).unwrap(),
        result
    );
    assert_eq!(snapshot.count_filtered("age>=4").unwrap(), 2);
    let new = engine.collection("", "c").unwrap().read().read_snapshot();
    assert_eq!(
        new.query("age>=0", &[], None, 0).unwrap().rows.pks,
        [Pk::Int64(9)]
    );
    engine.close();
}

#[test]
fn float_comparisons_validate_all_live_operands_and_never_inspect_dead_integer_values() {
    let root = tempfile::tempdir().unwrap();
    let engine = db(root.path());
    engine.create_collection("", "c", schema()).unwrap();
    engine
        .insert("", "c", batch(&[(1, 9_007_199_254_740_993), (2, 2)]))
        .unwrap();
    engine.flush_collection("", "c").unwrap();
    let snapshot = engine.collection("", "c").unwrap().read().read_snapshot();
    assert!(matches!(
        snapshot.query("id<0 and age>1.0", &[], None, 0),
        Err(EngineError::InvalidArgument(_))
    ));
    engine.delete("", "c", &[Pk::Int64(1)]).unwrap();
    let snapshot = engine.collection("", "c").unwrap().read().read_snapshot();
    assert_eq!(
        snapshot.query("age>1.0", &[], None, 0).unwrap().rows.pks,
        [Pk::Int64(2)]
    );
    let result = snapshot
        .search_filtered(
            SearchRequest {
                anns_field: None,
                vectors: QueryVectors::Dense(vec![vec![1.0, 0.0]]),
                metric_type: Some("L2".into()),
                limit: 3,
                offset: 0,
                output_fields: vec![],
            },
            engine.compute(),
            "age>1.0",
        )
        .unwrap();
    assert_eq!(result.hits[0].rows.pks, [Pk::Int64(2)]);
    assert!(
        matches!(engine.delete_expr("","c","age>1.0", ""),Ok((pks,false)) if pks==[Pk::Int64(2)])
    );
    engine.close();
}

#[test]
fn rejected_filter_delete_never_changes_sequence_wal_or_rows() {
    let root = tempfile::tempdir().unwrap();
    let engine = db(root.path());
    engine.create_collection("", "c", schema()).unwrap();
    engine.insert("", "c", batch(&[(1, 1), (2, 2)])).unwrap();
    engine.flush_collection("", "c").unwrap();
    let manifest_path = root.path().join("collections/c/manifest.json");
    let before = std::fs::read(&manifest_path).unwrap();
    for expr in [
        "age = 1",
        "age == 'bad'",
        "ag > 0",
        "age > 9223372036854775808",
    ] {
        assert!(engine.delete_expr("", "c", expr, "").is_err());
        assert_eq!(engine.collection("", "c").unwrap().read().count(), 2);
        assert_eq!(std::fs::read(&manifest_path).unwrap(), before);
        assert_eq!(
            std::fs::read_dir(root.path().join("collections/c/wal"))
                .unwrap()
                .count(),
            0
        );
    }
    assert!(
        engine
            .delete_expr("", "c", "age<0", "p1")
            .unwrap()
            .0
            .is_empty()
    );
    engine.delete_expr("", "c", "age>=2", "").unwrap();
    engine.close();
    let reopened = db(root.path());
    assert_eq!(reopened.collection("", "c").unwrap().read().count(), 1);
    reopened.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn delete_preserves_storage_retry_priority_before_parsing_or_evaluating_filters() {
    use milvus_lite_core::storage::runtime::{
        RetainedWalSyncFault, hold_flush_retry_window, inject_retained_wal_sync_failure_once,
    };
    let root = tempfile::tempdir().unwrap();
    let engine = db(root.path());
    engine.create_collection("", "c", schema()).unwrap();
    engine.insert("", "c", batch(&[(1, 1), (2, 2)])).unwrap();
    inject_retained_wal_sync_failure_once(RetainedWalSyncFault::Data);
    assert!(engine.flush_collection("", "c").is_err());
    let window = hold_flush_retry_window();
    for expr in ["id in [bad]", "age = 1", "age >= 1"] {
        let error = engine.delete_expr("", "c", expr, "").unwrap_err();
        assert!(
            error.to_string().contains("flush retry is throttled"),
            "{error}"
        );
        assert_eq!(engine.collection("", "c").unwrap().read().count(), 2);
    }
    drop(window);
    engine.flush_collection("", "c").unwrap();
    assert_eq!(
        engine.delete_expr("", "c", "age>=1", "").unwrap().0,
        [Pk::Int64(1), Pk::Int64(2)]
    );
    engine.close();
}
