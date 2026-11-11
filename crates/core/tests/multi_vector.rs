//! 0028：多向量字段的范数、写入检查与持久化。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use milvus_lite_core::{
    CollectionSchema, ColumnData, Compute, DataDir, DataType, Db, FieldSchema, InputValues,
    InsertBatch, Pk, QueryVectors, SearchRequest,
};

fn open(path: &Path) -> Arc<Db> {
    Db::new(
        DataDir::open(path).unwrap(),
        Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
    )
    .unwrap()
}

fn vector(name: &str, dim: i64) -> FieldSchema {
    FieldSchema {
        dim: Some(dim),
        ..FieldSchema::new(name, DataType::FloatVector)
    }
}

fn schema(key: bool) -> CollectionSchema {
    let mut fields = vec![
        FieldSchema {
            is_primary: true,
            ..FieldSchema::new("id", DataType::Int64)
        },
        vector("a", 2),
        vector("b", 3),
    ];
    if key {
        fields.push(FieldSchema {
            is_partition_key: true,
            ..FieldSchema::new("k", DataType::Int64)
        });
    }
    CollectionSchema {
        fields,
        enable_dynamic_field: false,
        properties: Vec::new(),
    }
}

fn floats(dim: usize, values: Vec<f32>) -> InputValues {
    InputValues::FloatVector { dim, values }
}

fn rows(ids: &[i64], b: InputValues) -> InsertBatch {
    InsertBatch {
        num_rows: ids.len(),
        columns: vec![
            ("id".into(), InputValues::Int(ids.to_vec())),
            ("a".into(), floats(2, [1.0, 0.0].repeat(ids.len()))),
            ("b".into(), b),
        ],
    }
}

/// `b` 的第 id 行。相似度被截断到 1 以内，查询必须和它们不共线（见 `cosine_on_b`），错误的范数才能
/// 反映在分数上：`b` 比 `a` 长，用 `a` 的范数算出的相似度大于真值。
fn b_values(ids: &[i64]) -> Vec<f32> {
    ids.iter()
        .flat_map(|id| [3.0 * *id as f32, 0.0, 4.0 * *id as f32])
        .collect()
}

fn cosine_on_b(db: &Db) -> Vec<(Pk, f32)> {
    let handle = db.collection("", "c").unwrap();
    let result = handle
        .read()
        .read_snapshot()
        .search(
            SearchRequest {
                anns_field: Some("b".into()),
                vectors: QueryVectors::Dense(vec![vec![1.0, 0.0, 0.0]]),
                metric_type: Some("COSINE".into()),
                limit: 10,
                offset: 0,
                output_fields: Vec::new(),
                params: Default::default(),
                negative_top_k: false,
            },
            db.compute(),
        )
        .unwrap();
    let hits = result.hits.into_iter().next().unwrap();
    hits.rows.pks.into_iter().zip(hits.scores).collect()
}

/// 每行的 `b` 都是 `[3k, 0, 4k]`，与查询 `[1, 0, 0]` 的相似度都是 0.6，与长度无关。
fn assert_cosine(db: &Db, ids: &[i64]) {
    let got = cosine_on_b(db);
    assert_eq!(got.len(), ids.len(), "{got:?}");
    for (pk, score) in &got {
        let Pk::Int64(id) = pk else {
            panic!("unexpected key {pk:?}")
        };
        assert!(ids.contains(id), "{pk:?}");
        assert!((score - 0.6).abs() < 1e-5, "{pk:?} scored {score}");
    }
}

#[test]
fn cosine_on_a_later_field_uses_its_own_norms_through_every_storage_stage() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(false)).unwrap();
    db.insert("", "c", rows(&[1, 2], floats(3, b_values(&[1, 2]))))
        .unwrap();
    assert_cosine(&db, &[1, 2]);

    db.flush_collection("", "c").unwrap();
    assert_cosine(&db, &[1, 2]);

    // 每个文件两行：合并时第 0 行以外的范数才会暴露切片错误。
    for first in [3, 5, 7] {
        let ids = [first, first + 1];
        db.insert("", "c", rows(&ids, floats(3, b_values(&ids))))
            .unwrap();
        db.flush_collection("", "c").unwrap();
    }
    let ids = [1, 2, 3, 4, 5, 6, 7, 8];
    assert_cosine(&db, &ids);

    // 正常关闭会合并已提交文件；合并产物的范数在内存里，重开后是重新计算的，所以要在这里读。
    db.close();
    assert_cosine(&db, &ids);
    let manifest = milvus_lite_core::storage::manifest::Manifest::read(
        &root.path().join("collections/c/manifest.json"),
    )
    .unwrap();
    assert_eq!(manifest.partitions["_default"].data_files.len(), 1);

    let reopened = open(root.path());
    assert_cosine(&reopened, &ids);
    reopened.close();
}

#[test]
fn cosine_on_a_later_field_survives_wal_recovery_and_partition_keys() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(true)).unwrap();
    let ids = [1, 2, 3, 4];
    let mut batch = rows(&ids, floats(3, b_values(&ids)));
    batch
        .columns
        .push(("k".into(), InputValues::Int(vec![10, 20, 30, 40])));
    db.insert("", "c", batch).unwrap();
    assert_cosine(&db, &ids);
    db.flush_collection("", "c").unwrap();
    assert_cosine(&db, &ids);
    db.close();
    let reopened = open(root.path());
    assert_cosine(&reopened, &ids);
    reopened.close();
}

fn message(error: milvus_lite_core::EngineError) -> String {
    error.to_string()
}

fn keyed(mut batch: InsertBatch, key: bool) -> InsertBatch {
    if key {
        batch
            .columns
            .push(("k".into(), InputValues::Int(vec![10; batch.num_rows])));
    }
    batch
}

#[test]
fn later_vector_errors_cover_insert_and_keep_earlier_validation_first() {
    for key in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        db.create_collection("", "c", schema(key)).unwrap();
        let insert = |batch| message(db.insert("", "c", keyed(batch, key)).unwrap_err());

        assert_eq!(
            insert(rows(&[1], floats(2, vec![1.0, 0.0]))),
            "Length of item not correct: expected 3 but got array of size 2",
            "key={key}"
        );
        assert_eq!(
            insert(rows(&[1], InputValues::Int(vec![1]))),
            "vector field 'b' must be list/tuple, got int",
            "key={key}"
        );
        let mut missing = rows(&[1], floats(3, b_values(&[1])));
        missing.columns.retain(|(name, _)| name != "b");
        assert_eq!(
            insert(missing),
            "vector field 'b' missing or None",
            "key={key}"
        );
        assert_eq!(
            insert(rows(&[1], InputValues::Null(1))),
            "vector field 'b' missing or None",
            "key={key}"
        );

        // 第一个向量字段的错误先于其余向量字段。
        let mut both = rows(&[1], floats(2, vec![1.0, 0.0]));
        both.columns[1].1 = floats(3, vec![1.0, 0.0, 0.0]);
        assert_eq!(
            insert(both),
            "vector field 'a' expected dim 2, got 3",
            "key={key}"
        );

        let handle = db.collection("", "c").unwrap();
        assert!(
            handle
                .read()
                .get(&[Pk::Int64(1)], &[])
                .unwrap()
                .rows
                .pks
                .is_empty(),
            "key={key}"
        );
        drop(handle);
        db.close();
    }
}

#[test]
fn type_and_dim_errors_outrank_a_missing_later_vector() {
    let mut fields = schema(false);
    fields.fields.push(vector("c", 4));
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", fields).unwrap();
    let mut batch = rows(&[1], floats(3, b_values(&[1])));
    batch.columns.retain(|(name, _)| name != "b");
    batch.columns.push(("c".into(), floats(2, vec![1.0, 2.0])));
    assert_eq!(
        message(db.insert("", "c", batch).unwrap_err()),
        "Length of item not correct: expected 4 but got array of size 2"
    );
    db.close();
}

fn upsert(db: &Db, batch: InsertBatch) -> Result<Vec<Pk>, milvus_lite_core::EngineError> {
    db.upsert_partition_with_pk_type("", "c", batch, "")
        .map(|outcome| outcome.pks)
}

fn only_b(ids: &[i64], values: Vec<f32>) -> InsertBatch {
    InsertBatch {
        num_rows: ids.len(),
        columns: vec![
            ("id".into(), InputValues::Int(ids.to_vec())),
            ("b".into(), floats(3, values)),
        ],
    }
}

fn stored_b(db: &Db, id: i64) -> Vec<f32> {
    let handle = db.collection("", "c").unwrap();
    let got = handle
        .read()
        .get(&[Pk::Int64(id)], &["b".to_owned()])
        .unwrap();
    let (_, column) = got
        .rows
        .fields
        .into_iter()
        .find(|(name, _)| name == "b")
        .unwrap();
    let ColumnData::FloatVector { values, .. } = column else {
        panic!("b is not a vector column")
    };
    values
}

#[test]
fn upsert_merges_later_vectors_and_rejects_new_rows_without_them() {
    for key in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        db.create_collection("", "c", schema(key)).unwrap();
        let mut seed = rows(&[1], floats(3, b_values(&[1])));
        if key {
            seed.columns.push(("k".into(), InputValues::Int(vec![10])));
        }
        db.insert("", "c", seed).unwrap();

        // 已有行只改第一个向量：其余向量沿用旧值。
        let mut only_a = InsertBatch {
            num_rows: 1,
            columns: vec![
                ("id".into(), InputValues::Int(vec![1])),
                ("a".into(), floats(2, vec![0.0, 1.0])),
            ],
        };
        if key {
            only_a
                .columns
                .push(("k".into(), InputValues::Int(vec![10])));
        }
        upsert(&db, only_a).unwrap();
        assert_eq!(stored_b(&db, 1), b_values(&[1]), "key={key}");

        // 已有行只改第二个向量。
        let mut new_b = only_b(&[1], vec![9.0, 9.0, 9.0]);
        if key {
            new_b.columns.push(("k".into(), InputValues::Int(vec![10])));
        }
        upsert(&db, new_b).unwrap();
        assert_eq!(stored_b(&db, 1), vec![9.0, 9.0, 9.0], "key={key}");

        // 维度错的行在合并后拒绝，不能 panic。
        let mut bad_dim = only_b(&[1], vec![1.0, 2.0, 3.0]);
        bad_dim.columns[1].1 = floats(2, vec![1.0, 2.0]);
        if key {
            bad_dim
                .columns
                .push(("k".into(), InputValues::Int(vec![10])));
        }
        assert_eq!(
            message(upsert(&db, bad_dim).unwrap_err()),
            "Length of item not correct: expected 3 but got array of size 2",
            "key={key}"
        );

        // 新行缺第二个向量：3.x 接受并在 flush 后读取出错，这里拒绝（0028）。
        let mut new_row = InsertBatch {
            num_rows: 1,
            columns: vec![
                ("id".into(), InputValues::Int(vec![50])),
                ("a".into(), floats(2, vec![1.0, 0.0])),
            ],
        };
        if key {
            new_row
                .columns
                .push(("k".into(), InputValues::Int(vec![10])));
        }
        assert_eq!(
            message(upsert(&db, new_row).unwrap_err()),
            "vector field 'b' missing or None",
            "key={key}"
        );
        let handle = db.collection("", "c").unwrap();
        assert!(
            handle
                .read()
                .get(&[Pk::Int64(50)], &[])
                .unwrap()
                .rows
                .pks
                .is_empty(),
            "key={key}"
        );
        drop(handle);
        db.close();
    }
}

#[test]
fn upsert_reports_earlier_row_validation_before_a_later_vector_problem() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(false)).unwrap();
    db.insert("", "c", rows(&[1], floats(3, b_values(&[1]))))
        .unwrap();
    // 第 0 行合并旧值后只有第二个向量维度错；第 1 行是新行，缺第一个向量。3.x 先校验完全部行，报第 1 行。
    let batch = InsertBatch {
        num_rows: 2,
        columns: vec![
            ("id".into(), InputValues::Int(vec![1, 2])),
            ("b".into(), floats(2, vec![1.0, 2.0, 3.0, 4.0])),
        ],
    };
    assert_eq!(
        message(upsert(&db, batch).unwrap_err()),
        "vector field 'a' missing or None"
    );
    db.close();
}
