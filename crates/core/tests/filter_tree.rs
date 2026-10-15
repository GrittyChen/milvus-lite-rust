//! 新布尔树在来源、快照、失败删除与重开上的组合，协议真值另由差分套件固定。
#![allow(clippy::unwrap_used)]

use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, EngineError, FieldSchema, InputValues,
    InsertBatch, Pk, QueryVectors, SearchRequest,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn open(path: &Path) -> Arc<Db> {
    Db::new(DataDir::open(path).unwrap(), Compute::for_host().unwrap()).unwrap()
}
fn schema() -> CollectionSchema {
    let mut id = FieldSchema::new("id", DataType::Int64);
    id.is_primary = true;
    let mut vector = FieldSchema::new("v", DataType::FloatVector);
    vector.dim = Some(2);
    CollectionSchema {
        fields: vec![
            id,
            vector,
            FieldSchema::new("age", DataType::Int64),
            FieldSchema::new("j", DataType::Json),
        ],
        enable_dynamic_field: true,
        properties: vec![],
    }
}
fn insert(db: &Db, start: i64, meta: &[Value]) {
    db.insert(
        "",
        "c",
        InsertBatch {
            num_rows: meta.len(),
            columns: vec![
                (
                    "id".into(),
                    InputValues::Int((start..start + meta.len() as i64).collect()),
                ),
                ("age".into(), InputValues::Int(vec![1; meta.len()])),
                (
                    "v".into(),
                    InputValues::FloatVector {
                        dim: 2,
                        values: vec![1.0; meta.len() * 2],
                    },
                ),
                (
                    "j".into(),
                    InputValues::Json(vec![
                        b"{\"a\":1,\"arr\":[1,true,null]}".to_vec();
                        meta.len()
                    ]),
                ),
                (
                    "$meta".into(),
                    InputValues::Json(
                        meta.iter()
                            .map(|value| value.to_string().into_bytes())
                            .collect(),
                    ),
                ),
            ],
        },
    )
    .unwrap();
}
fn files(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut work = vec![path.to_path_buf()];
    while let Some(dir) = work.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                work.push(entry.path());
            } else {
                out.insert(
                    entry.path().strip_prefix(path).unwrap().to_owned(),
                    std::fs::read(entry.path()).unwrap(),
                );
            }
        }
    }
    out
}
#[test]
fn whole_source_hybrid_fallback_preserves_chunks_old_snapshots_and_reopen() {
    let root = tempfile::tempdir().unwrap();
    let mut db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    insert(&db, 0, &vec![json!({"x":0.0}); 32]);
    insert(&db, 32, &[json!({"x":"other"})]);
    let expr = "x in [-0.0] or id < 0";
    let old = db.collection("", "c").unwrap().read().read_snapshot();
    assert_eq!(old.count_filtered(expr).unwrap(), 32);
    db.flush_collection("", "c").unwrap();
    insert(&db, 40, &[json!({"x":0.0})]);
    assert_eq!(
        db.collection("", "c")
            .unwrap()
            .read()
            .read_snapshot()
            .count_filtered(expr)
            .unwrap(),
        32
    );
    db.delete("", "c", &[Pk::Int64(0)]).unwrap();
    assert_eq!(old.count_filtered(expr).unwrap(), 32);
    db.close();
    db = open(root.path());
    db.collection("", "c").unwrap().write().load().unwrap();
    assert_eq!(
        db.collection("", "c")
            .unwrap()
            .read()
            .read_snapshot()
            .count_filtered(expr)
            .unwrap(),
        31
    );
    assert_eq!(old.count_filtered(expr).unwrap(), 32);
    db.close();
}
#[test]
fn late_source_failure_never_publishes_masks_or_writes_a_delete() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    insert(&db, 1, &[json!({"x":1})]);
    db.flush_collection("", "c").unwrap();
    insert(&db, 2, &[json!({"x":[1]})]);
    let snapshot = db.collection("", "c").unwrap().read().read_snapshot();
    let before_rows = snapshot.query("", &[], None, 0).unwrap();
    let before_files = files(root.path());
    let expr = "id == 1 or (j['a'] == 1 and x in [1])";
    for result in [
        snapshot.count_filtered(expr).map(|_| ()),
        snapshot.query(expr, &[], None, 0).map(|_| ()),
        snapshot
            .search_filtered(
                SearchRequest {
                    anns_field: None,
                    vectors: QueryVectors::Dense(vec![vec![1.0, 1.0]]),
                    metric_type: Some("L2".into()),
                    limit: 2,
                    offset: 0,
                    output_fields: vec![],
                },
                db.compute(),
                expr,
            )
            .map(|_| ()),
        db.delete_expr("", "c", expr, "").map(|_| ()),
    ] {
        assert!(
            matches!(result,Err(EngineError::InvalidArgument(ref text)) if text=="unhashable type: 'list'")
        );
    }
    assert_eq!(files(root.path()), before_files);
    assert_eq!(snapshot.query("", &[], None, 0).unwrap(), before_rows);
    assert_eq!(
        db.collection("", "c")
            .unwrap()
            .read()
            .read_snapshot()
            .query("", &[], None, 0)
            .unwrap(),
        before_rows
    );
    insert(&db, 3, &[json!({"x":2})]);
    db.flush_collection("", "c").unwrap();
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(root.path().join("collections/c/manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["current_seq"], 3);
    db.close();
}
#[test]
fn routing_short_circuit_unknown_and_overflow_are_independent() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    insert(
        &db,
        1,
        &[
            json!({}),
            json!({"x":7}),
            json!({"x":"a"}),
            json!({"x":null}),
        ],
    );
    let snapshot = db.collection("", "c").unwrap().read().read_snapshot();
    assert!(
        snapshot
            .count_filtered("true or age == 9223372036854775808")
            .is_err()
    );
    assert_eq!(
        snapshot
            .count_filtered("true or j['a'] == 9223372036854775808")
            .unwrap(),
        4
    );
    assert_eq!(
        snapshot
            .count_filtered("true or json_contains(j['arr'], [1])")
            .unwrap(),
        4
    );
    assert_eq!(
        snapshot
            .count_filtered("false and json_contains(j['arr'], [1])")
            .unwrap(),
        0
    );
    assert!(
        matches!(snapshot.count_filtered("j['a'] == 1 and json_contains(j['arr'], [1])"),Err(EngineError::InvalidArgument(ref text)) if text=="unknown AST node: ListLit")
    );
    assert_eq!(
        snapshot
            .query("not (j['a'] == x)", &[], None, 0)
            .unwrap()
            .rows
            .pks,
        vec![Pk::Int64(2), Pk::Int64(3)]
    );
    assert_eq!(
        snapshot
            .query("not (x like 'a%') and j['a'] == 1", &[], None, 0)
            .unwrap()
            .rows
            .pks,
        vec![Pk::Int64(2)]
    );
    assert_eq!(
        snapshot
            .count_filtered("not json_contains(j['missing'],1)")
            .unwrap(),
        4
    );
    // Arrow 的 C long 溢出不属于 hybrid 可回退错误，不能由后续 Python 短路遮蔽。
    assert!(
        snapshot
            .count_filtered("true or y == 9223372036854775808")
            .is_err()
    );
    assert_eq!(
        snapshot
            .count_filtered("true or x == 9223372036854775808")
            .unwrap(),
        4
    );
    db.close();
}
#[test]
fn reserved_contains_target_reads_the_existing_sequence_without_weakening_checks() {
    let root = tempfile::tempdir().unwrap();
    let mut db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    insert(&db, 1, &[json!({}), json!({})]);
    for committed in [false, true] {
        if committed {
            db.flush_collection("", "c").unwrap();
        }
        let snapshot = db.collection("", "c").unwrap().read().read_snapshot();
        assert_eq!(
            snapshot
                .query("json_contains(j['arr'],_seq)", &[], None, 0)
                .unwrap()
                .rows
                .pks,
            vec![Pk::Int64(1)]
        );
        assert!(snapshot.count_filtered("_seq == 1").is_err());
        assert_eq!(
            snapshot
                .count_filtered("json_contains(j['arr'],ghost)")
                .unwrap(),
            2
        );
        assert_eq!(
            snapshot
                .count_filtered("json_contains(j['arr'],_partition)")
                .unwrap(),
            2
        );
    }
    db.close();
    db = open(root.path());
    db.collection("", "c").unwrap().write().load().unwrap();
    assert_eq!(
        db.collection("", "c")
            .unwrap()
            .read()
            .read_snapshot()
            .query("json_contains(j['arr'],_seq)", &[], None, 0)
            .unwrap()
            .rows
            .pks,
        vec![Pk::Int64(1)]
    );
    insert(&db, 1, &[json!({})]);
    assert_eq!(
        db.collection("", "c")
            .unwrap()
            .read()
            .read_snapshot()
            .count_filtered("json_contains(j['arr'],_seq)")
            .unwrap(),
        0
    );
    db.close();
}
#[test]
fn deep_string_json_limits_do_not_depend_on_serdes_storage_limit() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    for (i, depth) in [128, 129, 9997, 9998].into_iter().enumerate() {
        insert(
            &db,
            i as i64 + 1,
            &[json!({"blob":format!("{}0{}","[".repeat(depth),"]".repeat(depth))})],
        );
    }
    let snapshot = db.collection("", "c").unwrap().read().read_snapshot();
    assert_eq!(
        snapshot
            .count_filtered("id < 4 and blob[0] is not null")
            .unwrap(),
        3
    );
    assert_eq!(
        snapshot.count_filtered("true or blob[0] is null").unwrap(),
        4
    );
    assert!(
        matches!(snapshot.count_filtered("blob[0] is null"),Err(EngineError::InvalidArgument(ref text)) if text=="maximum recursion depth exceeded while decoding a JSON array from a unicode string")
    );
    db.close();
}

#[cfg(feature = "fault-injection")]
#[test]
fn frozen_source_binding_keeps_new_active_rows_separate() {
    use std::sync::mpsc;
    use std::time::Duration;
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    insert(&db, 0, &vec![json!({"x":0.0}); 32]);
    insert(&db, 32, &[json!({"x":"other"})]);
    let old = db.collection("", "c").unwrap().read().read_snapshot();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let writer = Arc::clone(&db);
    let flush = std::thread::spawn(move || {
        let _flight = writer.enter().unwrap();
        milvus_lite_core::storage::runtime::pause_flush_before_commit_once(entered_tx, resume_rx);
        writer.flush_collection("", "c").unwrap();
    });
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    insert(&db, 40, &[json!({"x":0.0})]);
    let frozen = db.collection("", "c").unwrap().read().read_snapshot();
    assert_eq!(frozen.count_filtered("x in [-0.0] or id < 0").unwrap(), 32);
    resume_tx.send(()).unwrap();
    flush.join().unwrap();
    assert_eq!(old.count_filtered("x in [-0.0] or id < 0").unwrap(), 32);
    assert_eq!(frozen.count_filtered("x in [-0.0] or id < 0").unwrap(), 32);
    db.close();
}

#[test]
fn long_boolean_trees_evaluate_and_drop_without_using_the_host_stack() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema()).unwrap();
    insert(&db, 1, &[json!({})]);
    let snapshot = db.collection("", "c").unwrap().read().read_snapshot();
    let expression = vec!["true"; 10000].join(" or ");
    assert_eq!(snapshot.count_filtered(&expression).unwrap(), 1);
    assert_eq!(
        snapshot
            .count_filtered(&format!("{expression} or json_contains(j['arr'],[1])"))
            .unwrap(),
        1
    );
    let expression = vec!["id == 1"; 10000].join(" and ");
    assert_eq!(snapshot.count_filtered(&expression).unwrap(), 1);
    db.close();
}

#[test]
fn portable_integer_membership_policy_also_protects_schema_not_in_and_float_columns() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    let mut definition = schema();
    definition
        .fields
        .push(FieldSchema::new("d", DataType::Double));
    db.create_collection("", "c", definition).unwrap();
    db.insert(
        "",
        "c",
        InsertBatch {
            num_rows: 2,
            columns: vec![
                ("id".into(), InputValues::Int(vec![1, 2])),
                ("age".into(), InputValues::Int(vec![i64::MAX, 0])),
                (
                    "v".into(),
                    InputValues::FloatVector {
                        dim: 2,
                        values: vec![1.0; 4],
                    },
                ),
                ("j".into(), InputValues::Json(vec![b"{}".to_vec(); 2])),
                (
                    "d".into(),
                    InputValues::Float(vec![9_223_372_036_854_775_808.0, 0.0]),
                ),
                (
                    "$meta".into(),
                    InputValues::Json(vec![
                        json!({"x":i64::MAX}).to_string().into_bytes(),
                        b"{}".to_vec(),
                    ]),
                ),
            ],
        },
    )
    .unwrap();
    let snapshot = db.collection("", "c").unwrap().read().read_snapshot();
    let before = files(root.path());
    for expr in [
        "age in [9223372036854775808.0]",
        "age not in [9223372036854775808.0]",
        "d in [9223372036854775807]",
        "d not in [9223372036854775807]",
    ] {
        assert!(matches!(
            snapshot.count_filtered(expr),
            Err(EngineError::InvalidArgument(_))
        ));
        assert!(matches!(
            db.delete_expr("", "c", expr, ""),
            Err(EngineError::InvalidArgument(_))
        ));
        assert_eq!(files(root.path()), before);
        assert_eq!(db.collection("", "c").unwrap().read().count(), 2);
    }
    // 安全转换失败使整源回退 Python，缺失行保持 Unknown，不被 not in 误选。
    assert_eq!(
        snapshot
            .query("x not in [9223372036854775808.0]", &[], None, 0)
            .unwrap()
            .rows
            .pks,
        vec![Pk::Int64(1)]
    );
    db.close();
}
