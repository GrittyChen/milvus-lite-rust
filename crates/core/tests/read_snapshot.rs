//! 0007「读」：快照钉住列数据与有效位，写入、发布、删除不等待锁外读者。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, mpsc};
use std::time::Duration;

use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, FieldSchema, IndexRequest, InputValues,
    InsertBatch, Pk, QueryVectors, SearchRequest,
};

fn schema() -> CollectionSchema {
    let mut id = FieldSchema::new("id", DataType::Int64);
    id.is_primary = true;
    let mut vector = FieldSchema::new("v", DataType::FloatVector);
    vector.dim = Some(2);
    CollectionSchema {
        fields: vec![id, vector, FieldSchema::new("json", DataType::Json)],
        enable_dynamic_field: true,
        properties: Vec::new(),
    }
}

fn batch(ids: &[i64], value: f32) -> InsertBatch {
    InsertBatch {
        num_rows: ids.len(),
        columns: vec![
            ("id".into(), InputValues::Int(ids.to_vec())),
            (
                "v".into(),
                InputValues::FloatVector {
                    dim: 2,
                    values: [value, 0.0].repeat(ids.len()),
                },
            ),
            (
                "json".into(),
                InputValues::Json(vec![br#"{"wide":9223372036854775808}"#.to_vec(); ids.len()]),
            ),
            (
                "tag".into(),
                InputValues::Str(vec![format!("v{value}"); ids.len()]),
            ),
        ],
    }
}

fn request() -> SearchRequest {
    SearchRequest {
        anns_field: None,
        vectors: QueryVectors::Dense(vec![vec![1.0, 0.0]; 9]),
        metric_type: Some("L2".into()),
        limit: 10,
        offset: 0,
        output_fields: vec!["*".into()],
    }
}

#[test]
fn varchar_get_pins_remapped_blocks_and_keeps_duplicate_input_order() {
    for committed in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let db = Db::new(
            DataDir::open(root.path()).unwrap(),
            Compute::for_host().unwrap(),
        );
        let mut schema = schema();
        schema.fields[0].data_type = DataType::VarChar;
        schema.fields[0].max_length = Some(32);
        db.create_collection("", "c", schema).unwrap();
        let strings = |ids: &[&str], value| {
            let mut batch = batch(&vec![0; ids.len()], value);
            batch.columns[0].1 = InputValues::Str(ids.iter().map(|s| (*s).to_owned()).collect());
            batch
        };
        // 两个命中位于不同块，且请求顺序与块顺序相反，必须重映射下标。
        db.insert("", "c", strings(&["三", "unused"], 1.0)).unwrap();
        db.insert("", "c", strings(&["é"], 2.0)).unwrap();
        if committed {
            db.flush_collection("", "c").unwrap();
        }
        let handle = db.collection("", "c").unwrap();
        let pks: Vec<_> = ["é", "missing", "三", "é"]
            .into_iter()
            .map(|s| Pk::VarChar(s.into()))
            .collect();
        let fields = vec!["*".into()];
        let expected = handle.read().get(&pks, &fields).unwrap();
        assert_eq!(
            expected.rows.pks,
            [pks[0].clone(), pks[2].clone(), pks[0].clone()]
        );
        let prepared = handle.read().prepare_get(&pks, &fields).unwrap();
        db.insert("", "c", strings(&["é"], 7.0)).unwrap();
        db.delete("", "c", &[Pk::VarChar("三".into())]).unwrap();
        db.flush_collection("", "c").unwrap();
        let current = handle.read().get(&pks, &fields).unwrap();
        assert_eq!(current.rows.pks, [pks[0].clone(), pks[0].clone()]);
        assert_ne!(current, expected);
        db.drop_collection("", "c").unwrap();
        assert_eq!(prepared.read(), expected);
        db.close();
    }
}

#[test]
fn locked_snapshot_acquisition_then_unlocked_read_survives_mutation_publication_and_drop() {
    // 分别钉住活跃代与已提交数据；重复请求主键与缺失主键同时验收 Get。
    for committed in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::new(
            DataDir::open(root.path()).unwrap(),
            Compute::for_host().unwrap(),
        ));
        db.create_collection("", "c", schema()).unwrap();
        db.insert("", "c", batch(&[3, 1, 2], 1.0)).unwrap();
        if committed {
            db.flush_collection("", "c").unwrap();
        }
        let handle = db.collection("", "c").unwrap();
        let fields = vec!["*".into()];
        let pks = [Pk::Int64(2), Pk::Int64(999), Pk::Int64(3), Pk::Int64(2)];
        let expected_search = handle.read().search(request(), db.compute()).unwrap();
        let expected_get = handle.read().get(&pks, &fields).unwrap();
        let mut default_request = request();
        default_request.metric_type = None;
        let expected_default = handle
            .read()
            .search(default_request.clone(), db.compute())
            .unwrap();
        assert_eq!(
            expected_search.hits[0].rows.pks,
            [Pk::Int64(3), Pk::Int64(1), Pk::Int64(2)]
        );
        assert_eq!(
            expected_get.rows.pks,
            [Pk::Int64(2), Pk::Int64(3), Pk::Int64(2)]
        );
        let (ready_tx, ready_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let reader_db = Arc::clone(&db);
        let reader = std::thread::spawn(move || {
            let (search, get) = {
                let c = handle.read();
                (c.read_snapshot(), c.prepare_get(&pks, &fields).unwrap())
            };
            ready_tx.send(()).unwrap();
            resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            assert_eq!(
                search.search(request(), reader_db.compute()).unwrap(),
                expected_search
            );
            assert_eq!(get.read(), expected_get);
            assert_eq!(
                search.search(default_request, reader_db.compute()).unwrap(),
                expected_default
            );
        });
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let writer_db = Arc::clone(&db);
        let (done_tx, done_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            writer_db.insert("", "c", batch(&[3], 4.0)).unwrap();
            writer_db.delete("", "c", &[Pk::Int64(1)]).unwrap();
            writer_db
                .upsert_with_pk_type("", "c", batch(&[2], 5.0))
                .unwrap();
            writer_db.insert("", "c", batch(&[1], 6.0)).unwrap();
            writer_db.flush_collection("", "c").unwrap();
            writer_db
                .create_index(
                    "",
                    "c",
                    "v",
                    IndexRequest {
                        index_type: "FLAT".into(),
                        metric_type: Some("IP".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            writer_db.collection("", "c").unwrap().write().release();
            writer_db.drop_collection("", "c").unwrap();
            writer_db.create_collection("", "c", schema()).unwrap();
            writer_db.insert("", "c", batch(&[99], 9.0)).unwrap();
            done_tx.send(()).unwrap();
        });
        // 读者仍暂停；超时就代表写路径被它持有的锁挡住，先释放读者再报告失败。
        let completed = done_rx.recv_timeout(Duration::from_secs(10));
        resume_tx.send(()).unwrap();
        reader.join().unwrap();
        writer.join().unwrap();
        completed.unwrap();
        assert_eq!(db.collection("", "c").unwrap().read().count(), 1);
        db.close();
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn pending_generation_snapshot_survives_pre_and_post_commit_failure_then_retry() {
    use milvus_lite_core::storage::fs_commit::inject_manifest_dir_sync_failure_once;
    use milvus_lite_core::storage::runtime::{
        RetainedWalSyncFault, inject_retained_wal_sync_failure_once,
    };
    for post_commit in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let db = Db::new(
            DataDir::open(root.path()).unwrap(),
            Compute::for_host().unwrap(),
        );
        db.create_collection("", "c", schema()).unwrap();
        db.insert("", "c", batch(&[3, 1, 2], 1.0)).unwrap();
        let handle = db.collection("", "c").unwrap();
        let expected = handle.read().search(request(), db.compute()).unwrap();
        if post_commit {
            inject_manifest_dir_sync_failure_once();
        } else {
            inject_retained_wal_sync_failure_once(RetainedWalSyncFault::Data);
        }
        let error = db.flush_collection("", "c").unwrap_err().to_string();
        assert!(
            error.contains(if post_commit {
                "manifest was replaced, but directory durability is unconfirmed"
            } else {
                "BeforeReplace"
            }),
            "{error}"
        );
        let snapshot = handle.read().read_snapshot();
        assert_eq!(snapshot.search(request(), db.compute()).unwrap(), expected);
        db.flush_collection("", "c").unwrap();
        db.insert("", "c", batch(&[3], 7.0)).unwrap();
        db.delete("", "c", &[Pk::Int64(1)]).unwrap();
        db.flush_collection("", "c").unwrap();
        assert_eq!(snapshot.search(request(), db.compute()).unwrap(), expected);
        assert_ne!(
            handle.read().search(request(), db.compute()).unwrap(),
            expected
        );
        db.close();
    }
}
