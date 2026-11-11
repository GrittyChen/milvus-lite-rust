//! 0030：稀疏向量字段的写入校验、存储与 partial upsert（检索见各自的测试）。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use milvus_lite_core::data::Validity;
use milvus_lite_core::{
    CollectionSchema, ColumnData, Compute, DataDir, DataType, Db, FieldSchema, InputValues,
    InsertBatch, Pk,
};

fn open(path: &Path) -> Arc<Db> {
    Db::new(
        DataDir::open(path).unwrap(),
        Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
    )
    .unwrap()
}

fn schema(dense: bool, nullable: bool, dynamic: bool) -> CollectionSchema {
    let mut fields = vec![FieldSchema {
        is_primary: true,
        ..FieldSchema::new("id", DataType::Int64)
    }];
    if dense {
        fields.push(FieldSchema {
            dim: Some(2),
            ..FieldSchema::new("d", DataType::FloatVector)
        });
    }
    fields.push(FieldSchema {
        nullable,
        ..FieldSchema::new("sp", DataType::SparseFloatVector)
    });
    CollectionSchema {
        fields,
        enable_dynamic_field: dynamic,
        properties: Vec::new(),
    }
}

fn pack(pairs: &[(u32, f32)]) -> Vec<u8> {
    pairs
        .iter()
        .flat_map(|(d, w)| d.to_le_bytes().into_iter().chain(w.to_le_bytes()))
        .collect()
}

fn batch(ids: &[i64], dense: bool, sparse: InputValues) -> InsertBatch {
    let mut columns = vec![("id".into(), InputValues::Int(ids.to_vec()))];
    if dense {
        columns.push((
            "d".into(),
            InputValues::FloatVector {
                dim: 2,
                values: [1.0, 0.0].repeat(ids.len()),
            },
        ));
    }
    columns.push(("sp".into(), sparse));
    InsertBatch {
        num_rows: ids.len(),
        columns,
    }
}

/// 每个主键的稀疏值：None 是空值。
fn stored(db: &Db) -> Vec<(i64, Option<Vec<u8>>)> {
    let handle = db.collection("", "c").unwrap();
    let rows = handle
        .read()
        .read_snapshot()
        .query("", &["sp".into()], None, 0)
        .unwrap()
        .rows;
    let column = &rows.fields.iter().find(|(name, _)| name == "sp").unwrap().1;
    let ColumnData::SparseFloatVector(values) = column.values() else {
        panic!("sparse column lost its type: {column:?}");
    };
    let mut out: Vec<_> = rows
        .pks
        .iter()
        .enumerate()
        .map(|(row, pk)| {
            let Pk::Int64(id) = pk else { panic!("{pk:?}") };
            (
                *id,
                (!column.is_null(row)).then(|| values.row(row).to_vec()),
            )
        })
        .collect();
    out.sort();
    out
}

/// 写入值按 3.x `normalize_sparse_vector` 去掉零权重（含 -0.0），再按 `sparse_to_bytes` 排序存储；
/// 字节经 WAL、flush、合并与重开保持不变。
#[test]
fn writes_store_normalized_sorted_bytes_through_every_storage_stage() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(false, false, false))
        .unwrap();
    let expected = vec![
        (1, Some(pack(&[(2, 2.0), (9, 1.0)]))),
        (2, Some(pack(&[(3, 1.0)]))),
        (3, Some(pack(&[(0, 0.5), (4_294_967_294, 7.0)]))),
    ];
    db.insert(
        "",
        "c",
        batch(
            &[1, 2, 3],
            false,
            InputValues::SparseFloatVector(vec![
                vec![(9, 1.0), (2, 2.0)],
                vec![(1, -0.0), (3, 1.0), (7, 0.0)],
                vec![(4_294_967_294, 7.0), (0, 0.5)],
            ]),
        ),
    )
    .unwrap();
    assert_eq!(stored(&db), expected);
    db.flush_collection("", "c").unwrap();
    db.insert(
        "",
        "c",
        batch(
            &[4],
            false,
            InputValues::SparseFloatVector(vec![vec![(1, 1.0)]]),
        ),
    )
    .unwrap();
    db.flush_collection("", "c").unwrap();
    let mut all = expected.clone();
    all.push((4, Some(pack(&[(1, 1.0)]))));
    assert_eq!(stored(&db), all);
    db.close();
    let reopened = open(root.path());
    assert_eq!(stored(&reopened), all);
    reopened.close();
}

fn message(db: &Db, batch: InsertBatch) -> String {
    db.insert("", "c", batch).unwrap_err().to_string()
}

/// 3.x `validate_record` 的稀疏分支：缺失与 None 都报「missing」，值的错误带字段前缀（code 6）；
/// 稠密向量的检查在前，只有稀疏字段时没有稠密检查。
#[test]
fn write_validation_matches_3x_messages_and_order() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(true, false, false))
        .unwrap();
    let sparse = |rows: Vec<Vec<(u32, f32)>>| InputValues::SparseFloatVector(rows);
    let prefix = "sparse vector field 'sp': ";
    for (rows, expected) in [
        (
            vec![vec![]],
            "sparse vector must contain at least one nonzero float32 weight",
        ),
        (
            vec![vec![(1, 0.0)]],
            "sparse vector must contain at least one nonzero float32 weight",
        ),
        (
            vec![vec![(1, -1.0)]],
            "sparse weight at dimension 1 must be finite, non-negative float32",
        ),
        (
            vec![vec![(1, f32::NAN)]],
            "sparse weight at dimension 1 must be finite, non-negative float32",
        ),
        (
            vec![vec![(u32::MAX, 1.0)]],
            "sparse dimension 4294967295 must be non-negative and less than 2^32-1",
        ),
    ] {
        assert_eq!(
            message(&db, batch(&[1], true, sparse(rows))),
            format!("{prefix}{expected}")
        );
    }
    assert_eq!(
        message(&db, batch(&[1], true, InputValues::Int(vec![5]))),
        format!("{prefix}sparse vector must be a dict, got int")
    );
    assert_eq!(
        message(
            &db,
            batch(
                &[1],
                true,
                InputValues::JsonValues(vec![serde_json::json!({"1": 0.5})])
            )
        ),
        format!("{prefix}sparse dimension '1' must be int")
    );
    for missing in [
        InputValues::Null(1),
        InputValues::JsonValues(vec![serde_json::Value::Null]),
    ] {
        assert_eq!(
            message(&db, batch(&[1], true, missing)),
            "sparse vector field 'sp' missing and not nullable / no default"
        );
    }
    let mut without = batch(&[1], true, sparse(vec![vec![(1, 1.0)]]));
    without.columns.retain(|(name, _)| name != "sp");
    assert_eq!(
        message(&db, without),
        "sparse vector field 'sp' missing and not nullable / no default"
    );
    // 稠密向量的存在性检查先于稀疏字段的值检查。
    let mut dense_missing = batch(&[1], true, sparse(vec![vec![]]));
    dense_missing.columns.retain(|(name, _)| name != "d");
    assert_eq!(
        message(&db, dense_missing),
        "vector field 'd' missing or None"
    );
    // 稀疏 FieldData 写进稠密字段：值是 dict。
    let mut wrong = batch(&[1], true, sparse(vec![vec![(1, 1.0)]]));
    wrong.columns[1].1 = sparse(vec![vec![(1, 1.0)]]);
    assert_eq!(
        message(&db, wrong),
        "vector field 'd' must be list/tuple, got dict"
    );
    assert_eq!(db.collection("", "c").unwrap().read().count(), 0);
}

/// 不在 schema 里的稀疏列作为动态字段：3.x 用 `json.dumps` 把整数键写成字符串。
#[test]
fn sparse_dynamic_column_is_a_string_keyed_object() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(false, false, true))
        .unwrap();
    let mut rows = batch(
        &[1],
        false,
        InputValues::SparseFloatVector(vec![vec![(1, 1.0)]]),
    );
    rows.columns.push((
        "extra".into(),
        InputValues::SparseFloatVector(vec![vec![(9, 0.5), (2, 0.25)]]),
    ));
    db.insert("", "c", rows).unwrap();
    let handle = db.collection("", "c").unwrap();
    let result = handle
        .read()
        .read_snapshot()
        .query("", &["extra".into()], None, 0)
        .unwrap();
    assert_eq!(
        result.rows.meta.unwrap()[0]["extra"],
        serde_json::json!({"9": 0.5, "2": 0.25})
    );
}

/// 3.x 允许 nullable 稀疏字段，引擎接口可以写 null（gRPC 写不进，见 0030）；空值经 flush 与重开保持。
#[test]
fn nullable_sparse_accepts_engine_nulls_and_keeps_them() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(true, true, false))
        .unwrap();
    db.insert(
        "",
        "c",
        batch(
            &[1, 2],
            true,
            InputValues::Nullable {
                values: Box::new(InputValues::SparseFloatVector(vec![vec![(1, 1.0)], vec![]])),
                validity: Validity::from_bools(&[true, false]),
            },
        ),
    )
    .unwrap();
    db.insert("", "c", batch(&[3], true, InputValues::Null(1)))
        .unwrap();
    let mut without = batch(&[4], true, InputValues::Null(1));
    without.columns.retain(|(name, _)| name != "sp");
    db.insert("", "c", without).unwrap();
    let expected = vec![
        (1, Some(pack(&[(1, 1.0)]))),
        (2, None),
        (3, None),
        (4, None),
    ];
    assert_eq!(stored(&db), expected);
    db.flush_collection("", "c").unwrap();
    db.close();
    let reopened = open(root.path());
    assert_eq!(stored(&reopened), expected);
    reopened.close();
}

/// partial upsert 继承旧行的稀疏值与空值；提供了的字段不读旧值。
#[test]
fn partial_upsert_inherits_sparse_values_from_every_source() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(true, true, false))
        .unwrap();
    db.insert(
        "",
        "c",
        batch(
            &[1, 2],
            true,
            InputValues::Nullable {
                values: Box::new(InputValues::SparseFloatVector(vec![vec![(4, 2.0)], vec![]])),
                validity: Validity::from_bools(&[true, false]),
            },
        ),
    )
    .unwrap();
    let patch = |ids: &[i64]| InsertBatch {
        num_rows: ids.len(),
        columns: vec![
            ("id".into(), InputValues::Int(ids.to_vec())),
            (
                "d".into(),
                InputValues::FloatVector {
                    dim: 2,
                    values: [0.0, 1.0].repeat(ids.len()),
                },
            ),
        ],
    };
    let expected = vec![(1, Some(pack(&[(4, 2.0)]))), (2, None)];
    db.upsert_with_pk_type("", "c", patch(&[1, 2])).unwrap();
    assert_eq!(stored(&db), expected);
    db.flush_collection("", "c").unwrap();
    db.upsert_with_pk_type("", "c", patch(&[1, 2])).unwrap();
    assert_eq!(stored(&db), expected);
    let mut replace = patch(&[1]);
    replace.columns.push((
        "sp".into(),
        InputValues::SparseFloatVector(vec![vec![(8, 1.0)]]),
    ));
    db.upsert_with_pk_type("", "c", replace).unwrap();
    assert_eq!(stored(&db), vec![(1, Some(pack(&[(8, 1.0)]))), (2, None)]);
}

/// 只有稀疏向量字段时没有默认检索字段（3.x `_resolve_anns_field`）。
#[test]
fn sparse_only_collection_has_no_default_anns_field() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(false, false, false))
        .unwrap();
    let handle = db.collection("", "c").unwrap();
    assert_eq!(
        handle
            .read()
            .resolve_search_metric(None, None)
            .unwrap_err()
            .to_string(),
        "collection has no FLOAT_VECTOR field; specify anns_field explicitly for sparse search"
    );
}

/// 过滤中引用稀疏字段：3.x 的诊断文案（含位置标记）；IS NULL 同样拒绝。
#[test]
fn filters_reject_sparse_fields_with_3x_diagnostics() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(true, true, false))
        .unwrap();
    let handle = db.collection("", "c").unwrap();
    let snapshot = handle.read().read_snapshot();
    for (expr, marker) in [
        ("sp == 1", "  sp == 1\n  ^^"),
        ("sp is null", "  sp is null\n  ^^"),
    ] {
        assert_eq!(
            snapshot.count_filtered(expr).unwrap_err().to_string(),
            format!(
                "field 'sp' of type sparse_float_vector cannot be used in scalar filter expressions at column 1\n{marker}"
            )
        );
    }
}

/// 引擎接口给出的重复维度按 dict 语义合并（后者覆盖、保留首次位置），与 gRPC 解码一致。
#[test]
fn engine_input_duplicates_merge_like_a_python_dict() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(false, false, false))
        .unwrap();
    db.insert(
        "",
        "c",
        batch(
            &[1, 2],
            false,
            InputValues::SparseFloatVector(vec![
                vec![(5, 1.0), (1, 2.0), (5, 3.0)],
                vec![(1, -1.0), (1, 4.0)],
            ]),
        ),
    )
    .unwrap();
    assert_eq!(
        stored(&db),
        vec![
            (1, Some(pack(&[(1, 2.0), (5, 3.0)]))),
            (2, Some(pack(&[(1, 4.0)])))
        ]
    );
}

/// nullable 稀疏行经正常关闭时的合并与重开后保持（只有 WAL 的情形由互通测试的进程退出覆盖）。
#[test]
fn nullable_rows_survive_compaction_and_reopen() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(false, true, false))
        .unwrap();
    let mut expected = Vec::new();
    for id in 1..=4 {
        let null = id % 2 == 0;
        db.insert(
            "",
            "c",
            batch(
                &[id],
                false,
                if null {
                    InputValues::Null(1)
                } else {
                    InputValues::SparseFloatVector(vec![vec![(id as u32, 1.0)]])
                },
            ),
        )
        .unwrap();
        db.flush_collection("", "c").unwrap();
        expected.push((id, (!null).then(|| pack(&[(id as u32, 1.0)]))));
    }
    db.close();
    let manifest = milvus_lite_core::storage::manifest::Manifest::read(
        &root.path().join("collections/c/manifest.json"),
    )
    .unwrap();
    assert_eq!(manifest.partitions["_default"].data_files.len(), 1);
    let reopened = open(root.path());
    assert_eq!(stored(&reopened), expected);
    reopened.close();
}

fn sparse_index(metric: &str, params: serde_json::Value) -> milvus_lite_core::IndexRequest {
    milvus_lite_core::IndexRequest {
        index_type: "SPARSE_INVERTED_INDEX".into(),
        metric_type: Some(metric.into()),
        params: params.as_object().unwrap().clone(),
        search_params: serde_json::Map::new(),
    }
}

/// 3.x `load()` 不论有没有数据都校验稀疏字段的定义；失败时停在 released，可以 release / drop 后重建。
/// 这种定义只可能来自其他版本写的目录，这里改写 manifest 构造。
#[test]
fn load_rejects_stored_sparse_definitions_3x_cannot_use() {
    for (field, value, message) in [
        (
            "metric_type",
            serde_json::json!("BM25"),
            "sparse field 'sp' requires SPARSE_INVERTED_INDEX with metric IP; got 'SPARSE_INVERTED_INDEX'/'BM25'. \
             For an existing index, release, drop, and recreate it with the allowed metric.",
        ),
        (
            "build_params",
            serde_json::json!({"drop_ratio_build": 0.3}),
            "field 'sp': only drop_ratio_build=0 is supported",
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        db.create_collection("", "c", schema(true, false, false))
            .unwrap();
        db.create_index("", "c", "sp", sparse_index("IP", serde_json::json!({})))
            .unwrap();
        db.close();
        drop(db);
        let file = root.path().join("collections/c/manifest.json");
        let mut manifest: serde_json::Value =
            milvus_lite_core::json_text::value(&std::fs::read(&file).unwrap()).unwrap();
        manifest["index_specs"]["sp"][field] = value;
        std::fs::write(&file, serde_json::to_vec(&manifest).unwrap()).unwrap();

        let db = open(root.path());
        let handle = db.collection("", "c").unwrap();
        let error = handle.write().load().unwrap_err();
        assert!(
            matches!(error, milvus_lite_core::EngineError::SchemaValidation(_)),
            "{error:?}"
        );
        assert_eq!(error.to_string(), message);
        assert_eq!(
            handle.read().load_state(),
            milvus_lite_core::LoadState::Released
        );
        db.drop_index("", "c", "sp", "").unwrap();
        handle.write().load().unwrap();
        db.close();
    }
}

/// 稀疏字段上的合法定义：调参项为 0 的形式都接受，describe 原样回显；无索引时也能 load。
#[test]
fn sparse_index_definitions_round_trip() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(false, false, false))
        .unwrap();
    db.create_index(
        "",
        "c",
        "sp",
        sparse_index(
            "ip",
            serde_json::json!({"drop_ratio_build": "0", "inverted_index_algo": "taat_naive"}),
        ),
    )
    .unwrap();
    let error = db
        .create_index("", "c", "sp", sparse_index("IP", serde_json::json!({})))
        .unwrap_err();
    assert!(matches!(
        error,
        milvus_lite_core::EngineError::IndexAlreadyExists(_)
    ));
    db.close();
    let db = open(root.path());
    let handle = db.collection("", "c").unwrap();
    handle.write().load().unwrap();
    let spec = handle.read().index("sp").unwrap().clone();
    assert_eq!(spec.metric_type, "IP");
    assert_eq!(
        serde_json::Value::Object(spec.params),
        serde_json::json!({"drop_ratio_build": "0", "inverted_index_algo": "taat_naive"})
    );
    db.close();
}

/// 3.x `resolve_search_metric` 的稀疏分支：只有未给度量时放行，空串与其他值都报错；大小写不敏感。
#[test]
fn sparse_search_metric_accepts_only_ip() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(true, false, false))
        .unwrap();
    let resolve = |metric: Option<&str>| {
        db.collection("", "c")
            .unwrap()
            .read()
            .resolve_search_metric(Some("sp"), metric)
    };
    for metric in [None, Some("IP"), Some("ip")] {
        assert_eq!(resolve(metric).unwrap(), milvus_lite_core::Metric::Ip);
    }
    for (metric, shown) in [("", "''"), ("l2", "'L2'"), ("BM25", "'BM25'")] {
        assert_eq!(
            resolve(Some(metric)).unwrap_err().to_string(),
            format!("sparse field 'sp' requires metric IP, got {shown}")
        );
    }
    db.create_index("", "c", "sp", sparse_index("IP", serde_json::json!({})))
        .unwrap();
    assert_eq!(
        resolve(Some("")).unwrap_err().to_string(),
        "sparse field 'sp' requires metric IP, got ''"
    );
}

/// 有标量字段与动态字段的稀疏集合，供检索用例过滤。
fn scalar_schema() -> CollectionSchema {
    CollectionSchema {
        fields: vec![
            FieldSchema {
                is_primary: true,
                ..FieldSchema::new("id", DataType::Int64)
            },
            FieldSchema::new("sp", DataType::SparseFloatVector),
            FieldSchema::new("n", DataType::Int64),
        ],
        enable_dynamic_field: true,
        properties: Vec::new(),
    }
}

fn scalar_rows(ids: &[i64], rows: Vec<Vec<(u32, f32)>>, x: serde_json::Value) -> InsertBatch {
    InsertBatch {
        num_rows: ids.len(),
        columns: vec![
            ("id".into(), InputValues::Int(ids.to_vec())),
            ("sp".into(), InputValues::SparseFloatVector(rows)),
            ("n".into(), InputValues::Int(ids.to_vec())),
            ("x".into(), InputValues::JsonValues(vec![x; ids.len()])),
        ],
    }
}

/// 检索的可选部分；`params` 为空时是 `{}`。
#[derive(Default)]
struct Options<'a> {
    expr: &'a str,
    params: Option<serde_json::Value>,
    negative_top_k: bool,
    range: Option<milvus_lite_core::SearchRange>,
    partitions: Vec<String>,
}

/// 一条稀疏查询的 (主键, 对外分数)。
fn search_with(
    db: &Db,
    name: &str,
    query: &[(u32, f32)],
    limit: usize,
    options: Options,
) -> Result<Vec<(i64, f32)>, milvus_lite_core::EngineError> {
    let handle = db.collection("", name).unwrap();
    let snapshot = handle
        .read()
        .read_search_snapshot(&options.partitions, Some("sp"))?;
    let result = snapshot.search_filtered_with_range(
        milvus_lite_core::SearchRequest {
            anns_field: Some("sp".into()),
            vectors: milvus_lite_core::QueryVectors::Sparse(vec![query.to_vec()]),
            metric_type: None,
            limit,
            offset: 0,
            output_fields: Vec::new(),
            params: options
                .params
                .map(|p| p.as_object().unwrap().clone())
                .unwrap_or_default(),
            negative_top_k: options.negative_top_k,
        },
        db.compute(),
        options.expr,
        options.range,
    )?;
    let hits = result.hits.into_iter().next().unwrap();
    Ok(hits
        .rows
        .pks
        .iter()
        .zip(hits.scores)
        .map(|(pk, score)| {
            let Pk::Int64(id) = pk else { panic!("{pk:?}") };
            (*id, score)
        })
        .collect())
}

fn search(db: &Db, name: &str, query: &[(u32, f32)], limit: usize) -> Vec<(i64, f32)> {
    search_with(db, name, query, limit, Options::default()).unwrap()
}

fn ids(hits: &[(i64, f32)]) -> Vec<i64> {
    hits.iter().map(|(id, _)| *id).collect()
}

fn insert_sparse(db: &Db, name: &str, rows: &[(i64, &[(u32, f32)])]) {
    let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
    db.insert(
        "",
        name,
        batch(
            &ids,
            false,
            InputValues::SparseFloatVector(rows.iter().map(|(_, pairs)| pairs.to_vec()).collect()),
        ),
    )
    .unwrap();
}

/// 3.x 的次序（sparse_probe4 / 5 / 11 实测）：先比 float32 分数，再比数据源先后（已提交 segment 在前、
/// MemTable 在后），源内按 double 分数降序，double 也相同时保持插入 dict 的先后，即先比首个命中的查询维度
/// 序号（按查询给出的顺序，不排序），再比行号。
#[test]
fn search_ties_follow_3x_source_double_first_hit_and_row_order() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    let eps = 2f32.powi(-12);
    // double 为 1 + 2^-24、float32 舍入为 1 的行，跨源时仍排在数据源先后之后。
    db.create_collection("", "t", schema(false, false, false))
        .unwrap();
    insert_sparse(&db, "t", &[(10, &[(0, 1.0)]), (11, &[(0, 1.0), (1, eps)])]);
    db.flush_collection("", "t").unwrap();
    insert_sparse(&db, "t", &[(1, &[(0, 1.0)]), (2, &[(0, 1.0), (1, eps)])]);
    let query = [(0, 1.0), (1, eps)];
    let got = search(&db, "t", &query, 10);
    assert_eq!(ids(&got), vec![11, 10, 2, 1]);
    assert!(
        got.iter()
            .all(|(_, score)| score.to_bits() == 1f32.to_bits())
    );
    assert_eq!(ids(&search(&db, "t", &query, 1)), vec![11]);
    assert_eq!(ids(&search(&db, "t", &query, 3)), vec![11, 10, 2]);
    // 乘积在 double 中也被吸收（1 + 2^-60 == 1）时，退回到行号。
    let tiny = 2f32.powi(-30);
    db.create_collection("", "t2", schema(false, false, false))
        .unwrap();
    insert_sparse(
        &db,
        "t2",
        &[(10, &[(0, 1.0)]), (11, &[(0, 1.0), (1, tiny)])],
    );
    db.flush_collection("", "t2").unwrap();
    insert_sparse(&db, "t2", &[(1, &[(0, 1.0)]), (2, &[(0, 1.0), (1, tiny)])]);
    assert_eq!(
        ids(&search(&db, "t2", &[(0, 1.0), (1, tiny)], 10)),
        vec![10, 11, 1, 2]
    );
    // 原始 RPC 可以给出未排序的查询（pymilvus 会排序）：行 11、2 先被查询的第一个维度命中。
    assert_eq!(
        ids(&search(&db, "t2", &[(1, tiny), (0, 1.0)], 10)),
        vec![11, 10, 2, 1]
    );

    // 源内：行号小的 double 分数低；查询维度的先后不影响只有一个维度命中的行。
    db.create_collection("", "w", schema(false, false, false))
        .unwrap();
    insert_sparse(
        &db,
        "w",
        &[
            (1, &[(0, 1.0)]),
            (2, &[(0, 1.0), (1, eps)]),
            (3, &[(1, eps), (0, 1.0)]),
        ],
    );
    // 首个命中的查询维度：行 1 只有维度 5，行 2 只有维度 0，行 3 两个都有（分数同为 1）。
    db.create_collection("", "f", schema(false, false, false))
        .unwrap();
    insert_sparse(
        &db,
        "f",
        &[
            (1, &[(5, 1.0)]),
            (2, &[(0, 1.0)]),
            (3, &[(0, 0.5), (5, 0.5)]),
        ],
    );
    for _ in ["memtable", "segment"] {
        for q in [[(0, 1.0), (1, eps)], [(1, eps), (0, 1.0)]] {
            assert_eq!(ids(&search(&db, "w", &q, 10)), vec![2, 3, 1]);
        }
        assert_eq!(ids(&search(&db, "w", &query, 1)), vec![2]);
        assert_eq!(
            ids(&search(&db, "f", &[(0, 1.0), (5, 1.0)], 10)),
            vec![2, 3, 1]
        );
        assert_eq!(
            ids(&search(&db, "f", &[(5, 1.0), (0, 1.0)], 10)),
            vec![1, 3, 2]
        );
        db.flush_collection("", "w").unwrap();
        db.flush_collection("", "f").unwrap();
    }
}

/// float32 下溢为 0 的乘积在 double 中仍为正，行照样返回（对外分数 0.0）；排序与范围检索看 double
/// 之前先看 float32（sparse_probe5 实测）。
#[test]
fn underflowing_products_are_returned_with_zero_scores() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "u", schema(false, false, false))
        .unwrap();
    insert_sparse(
        &db,
        "u",
        &[
            (1, &[(0, 1e-30)]),
            (2, &[(0, 1.0)]),
            (3, &[(0, 1e-30), (1, 1e-30)]),
            (4, &[(1, 1e-30)]),
            (5, &[(0, 1e-38)]),
        ],
    );
    let range = |radius, range_filter| {
        Some(milvus_lite_core::SearchRange {
            radius: Some(radius),
            range_filter,
        })
    };
    for _ in ["memtable", "segment"] {
        let got = search(&db, "u", &[(0, 1e-30)], 10);
        assert_eq!(ids(&got), vec![2, 1, 3, 5]);
        assert_eq!(got[0].1.to_bits(), 0x0da2_4260);
        assert!(got[1..].iter().all(|(_, score)| score.to_bits() == 0));
        assert_eq!(
            ids(&search(&db, "u", &[(0, 1e-30), (1, 1e-30)], 10)),
            vec![2, 3, 1, 4, 5]
        );
        let within = |radius, filter| {
            let options = Options {
                range: range(radius, filter),
                ..Options::default()
            };
            ids(&search_with(&db, "u", &[(0, 1e-30)], 10, options).unwrap())
        };
        assert_eq!(within(0.0, None), vec![2]);
        assert_eq!(within(-1e-45, Some(1.0)), vec![2, 1, 3, 5]);
        db.flush_collection("", "u").unwrap();
    }
}

const NEGATIVE_TOP_K: &str = "top_k must be a non-negative integer";

/// 3.x 只在打分时检查 top_k：segment 过滤后还有有效行才打分，MemTable 只要有行就打分（不看过滤）。
#[test]
fn negative_top_k_fails_only_where_3x_scores_a_source() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", scalar_schema()).unwrap();
    let negative = |expr: &str| {
        let options = Options {
            expr,
            negative_top_k: true,
            ..Options::default()
        };
        search_with(&db, "c", &[(1, 1.0)], 0, options)
    };
    let fails = |result: Result<Vec<(i64, f32)>, milvus_lite_core::EngineError>| {
        let error = result.unwrap_err();
        assert!(
            matches!(&error, milvus_lite_core::EngineError::InvalidArgument(m) if m == NEGATIVE_TOP_K),
            "{error:?}"
        );
    };
    assert_eq!(negative("").unwrap(), Vec::new());
    db.insert(
        "",
        "c",
        scalar_rows(&[1, 2], vec![vec![(1, 1.0)]; 2], serde_json::json!(1)),
    )
    .unwrap();
    fails(negative(""));
    fails(negative("id < 0"));
    db.flush_collection("", "c").unwrap();
    fails(negative(""));
    assert_eq!(negative("id < 0").unwrap(), Vec::new());
    db.delete("", "c", &[Pk::Int64(1), Pk::Int64(2)]).unwrap();
    assert_eq!(negative("").unwrap(), Vec::new());
    db.flush_collection("", "c").unwrap();
    assert_eq!(negative("").unwrap(), Vec::new());
}

/// MemTable 上的过滤按 3.x 的 Python 逐行求值（`_eval_row`），segment 上按 Arrow 路线：宽整数比较在
/// MemTable 上只是不命中，在 segment 上报 OverflowError；动态字段跨类型比较在 MemTable 上只是不命中
/// （sparse_probe10 实测）。
#[test]
fn memtable_filters_use_the_python_route() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", scalar_schema()).unwrap();
    let filtered = |expr: &str| {
        let options = Options {
            expr,
            ..Options::default()
        };
        search_with(&db, "c", &[(1, 1.0)], 10, options)
    };
    let wide = "n > 99999999999999999999";
    db.insert(
        "",
        "c",
        scalar_rows(
            &[1, 2],
            vec![vec![(1, 1.0)], vec![(1, 2.0)]],
            serde_json::json!(1),
        ),
    )
    .unwrap();
    assert_eq!(filtered(wide).unwrap(), Vec::new());
    assert_eq!(filtered("x > '0'").unwrap(), Vec::new());
    db.flush_collection("", "c").unwrap();
    let error = filtered(wide).unwrap_err();
    assert_eq!(
        error.to_string(),
        "Python int too large to convert to C long"
    );
    // segment 中的整数与 MemTable 中的字符串混在一起：只有字符串行命中。
    db.insert(
        "",
        "c",
        scalar_rows(&[3], vec![vec![(1, 3.0)]], serde_json::json!("5")),
    )
    .unwrap();
    assert_eq!(ids(&filtered("x > '2'").unwrap()), vec![3]);
    assert_eq!(ids(&filtered("x > 0").unwrap()), vec![2, 1]);
}

/// 把 segment 文件里主键为 `id` 的行的稀疏字节换成 `bytes`，模拟其他版本写下的坏行（本实现的写入会拒绝）。
fn poison_segment_row(root: &Path, name: &str, partition: &str, id: i64, bytes: &[u8]) {
    use arrow_array::{Array, BinaryArray, Int64Array, RecordBatch};
    let dir = root.join(format!("collections/{name}/partitions/{partition}/data"));
    let mut poisoned = false;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "parquet") {
            continue;
        }
        let builder = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
            std::fs::File::open(&path).unwrap(),
        )
        .unwrap();
        let schema = builder.schema().clone();
        let batches: Vec<RecordBatch> = builder.build().unwrap().map(|b| b.unwrap()).collect();
        let mut changed = false;
        let batches: Vec<RecordBatch> = batches
            .into_iter()
            .map(|batch| {
                let pks = batch
                    .column_by_name("id")
                    .unwrap()
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .unwrap()
                    .clone();
                let position = schema.index_of("sp").unwrap();
                let old = batch
                    .column(position)
                    .as_any()
                    .downcast_ref::<BinaryArray>()
                    .unwrap()
                    .clone();
                let rows: Vec<Option<&[u8]>> = (0..old.len())
                    .map(|row| {
                        if pks.value(row) == id {
                            changed = true;
                            Some(bytes)
                        } else {
                            old.is_valid(row).then(|| old.value(row))
                        }
                    })
                    .collect();
                let mut columns = batch.columns().to_vec();
                columns[position] = Arc::new(BinaryArray::from(rows));
                RecordBatch::try_new(schema.clone(), columns).unwrap()
            })
            .collect();
        if changed {
            let mut writer = parquet::arrow::ArrowWriter::try_new(
                std::fs::File::create(&path).unwrap(),
                schema,
                None,
            )
            .unwrap();
            for batch in &batches {
                writer.write(batch).unwrap();
            }
            writer.close().unwrap();
            poisoned = true;
        }
    }
    assert!(poisoned, "row {id} not found in any segment of {name}");
}

/// 3.x `_search_sparse` 的错误先后：调参项 → 查询 → 过滤编译 → 逐个 segment（建倒排 → 过滤）→ MemTable。
/// 建倒排解码并规范化全部物理行，坏行被删除或被过滤掉也照样报错（code 1，原始 `ValueError` 文案）；
/// 排在坏 segment 之前的 segment 先报过滤错误（sparse_probe4 实测坏行，错误先后见 3.x 源码）。
#[test]
fn bad_segment_rows_fail_in_3x_error_order() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", scalar_schema()).unwrap();
    db.insert(
        "",
        "c",
        scalar_rows(&[1], vec![vec![(1, 1.0)]], serde_json::json!(1)),
    )
    .unwrap();
    db.flush_collection("", "c").unwrap();
    db.insert(
        "",
        "c",
        scalar_rows(
            &[2, 3],
            vec![vec![(1, 2.0)], vec![(1, 3.0)]],
            serde_json::json!(1),
        ),
    )
    .unwrap();
    db.flush_collection("", "c").unwrap();
    db.close();
    drop(db);
    poison_segment_row(root.path(), "c", "_default", 2, &pack(&[(1, -1.0)]));
    let db = open(root.path());
    db.collection("", "c").unwrap().write().load().unwrap();
    db.insert(
        "",
        "c",
        scalar_rows(&[4], vec![vec![(1, 4.0)]], serde_json::json!(1)),
    )
    .unwrap();
    let attempt = |query: &[(u32, f32)], expr: &str, params: serde_json::Value| {
        let options = Options {
            expr,
            params: Some(params),
            ..Options::default()
        };
        search_with(&db, "c", query, 10, options)
            .unwrap_err()
            .to_string()
    };
    let bad_row = "sparse weight at dimension 1 must be finite, non-negative float32";
    let q = [(1, 1.0)];
    assert_eq!(attempt(&q, "", serde_json::json!({})), bad_row);
    assert_eq!(
        attempt(&q, "", serde_json::json!({"drop_ratio_search": 0.1})),
        "field 'sp': only drop_ratio_search=0 is supported"
    );
    assert_eq!(
        attempt(&[(1, -2.0)], "", serde_json::json!({})),
        "sparse query field 'sp': sparse weight at dimension 1 must be finite, non-negative float32"
    );
    assert!(attempt(&q, "n > 'a'", serde_json::json!({})).starts_with("comparison '>' between"),);
    // 第一个 segment 正常建好后先求值过滤，宽整数在 segment 上报错，轮不到第二个 segment 的坏行。
    assert_eq!(
        attempt(&q, "n > 99999999999999999999", serde_json::json!({})),
        "Python int too large to convert to C long"
    );
    // 坏行被过滤掉、被删除都不影响建倒排时的报错。
    assert_eq!(attempt(&q, "id != 2", serde_json::json!({})), bad_row);
    db.delete("", "c", &[Pk::Int64(2)]).unwrap();
    assert_eq!(attempt(&q, "", serde_json::json!({})), bad_row);
    // 失败的构建不缓存：坏行仍在，再次检索仍报同样的错。
    assert_eq!(attempt(&q, "", serde_json::json!({})), bad_row);
    db.close();
}

/// 3.x `_search_sparse` 的分区语义与稠密检索不同：先在全部分区上判版本，再按 `partition_names` 剪枝，
/// 被其他分区新版本遮住的旧版本不露出；选中分区里的 segment 即使没有有效行也建倒排，坏行照样报错，
/// 未选中分区的 segment 不建（独立评审的随机差分与 sparse 分区探针实测）。
#[test]
fn partition_selection_resolves_versions_across_all_partitions() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(false, false, false))
        .unwrap();
    db.create_partition("", "c", "p1").unwrap();
    insert_sparse(&db, "c", &[(1, &[(1, 1.0)]), (2, &[(1, 2.0)])]);
    db.flush_collection("", "c").unwrap();
    db.upsert_partition_with_pk_type(
        "",
        "c",
        batch(
            &[1],
            false,
            InputValues::SparseFloatVector(vec![vec![(1, 5.0)]]),
        ),
        "p1",
    )
    .unwrap();
    let in_partitions = |names: &[&str]| {
        let options = Options {
            partitions: names.iter().map(|name| (*name).to_owned()).collect(),
            ..Options::default()
        };
        search_with(&db, "c", &[(1, 1.0)], 10, options).unwrap()
    };
    for _ in ["memtable", "segment"] {
        assert_eq!(in_partitions(&["_default"]), vec![(2, 2.0)]);
        assert_eq!(in_partitions(&["p1"]), vec![(1, 5.0)]);
        assert_eq!(in_partitions(&["_default", "p1"]), vec![(1, 5.0), (2, 2.0)]);
        assert_eq!(in_partitions(&[]), vec![(1, 5.0), (2, 2.0)]);
        db.flush_collection("", "c").unwrap();
    }

    // p1 的 segment 里有坏行，且 p1 的行全部失效。
    db.create_collection("", "b", schema(false, false, false))
        .unwrap();
    db.create_partition("", "b", "p1").unwrap();
    for (id, partition) in [(1, "p1"), (2, "_default")] {
        db.insert_partition_with_pk_type(
            "",
            "b",
            batch(
                &[id],
                false,
                InputValues::SparseFloatVector(vec![vec![(1, 1.0)]]),
            ),
            partition,
        )
        .unwrap();
        db.flush_collection("", "b").unwrap();
    }
    db.close();
    drop(db);
    poison_segment_row(root.path(), "b", "p1", 1, &pack(&[(1, -1.0)]));
    let db = open(root.path());
    db.collection("", "b").unwrap().write().load().unwrap();
    db.delete("", "b", &[Pk::Int64(1)]).unwrap();
    let in_partitions = |names: &[&str]| {
        let options = Options {
            partitions: names.iter().map(|name| (*name).to_owned()).collect(),
            ..Options::default()
        };
        search_with(&db, "b", &[(1, 1.0)], 10, options)
    };
    let bad_row = "sparse weight at dimension 1 must be finite, non-negative float32";
    assert_eq!(in_partitions(&["p1"]).unwrap_err().to_string(), bad_row);
    assert_eq!(in_partitions(&[]).unwrap_err().to_string(), bad_row);
    assert_eq!(in_partitions(&["_default"]).unwrap(), vec![(2, 1.0)]);
    db.close();
}

/// 已登记的差异（0030）：指定其他分区、尚未 flush 的删除，3.x 的稀疏检索在 flush 前仍返回该行；
/// 本实现与稠密检索、与 flush 之后一样按主键全局屏蔽。
#[test]
fn partition_scoped_pending_deletes_hide_the_key_everywhere() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(false, false, false))
        .unwrap();
    db.create_partition("", "c", "p1").unwrap();
    db.create_partition("", "c", "p2").unwrap();
    db.insert_partition_with_pk_type(
        "",
        "c",
        batch(
            &[1, 2],
            false,
            InputValues::SparseFloatVector(vec![vec![(1, 1.0)], vec![(1, 2.0)]]),
        ),
        "p2",
    )
    .unwrap();
    db.flush_collection("", "c").unwrap();
    db.delete_expr("", "c", "id in [1]", "p1").unwrap();
    let in_partitions = |names: &[&str]| {
        let options = Options {
            partitions: names.iter().map(|name| (*name).to_owned()).collect(),
            ..Options::default()
        };
        search_with(&db, "c", &[(1, 1.0)], 10, options).unwrap()
    };
    for _ in ["pending", "flushed"] {
        assert_eq!(in_partitions(&[]), vec![(2, 2.0)]);
        assert_eq!(in_partitions(&["p2"]), vec![(2, 2.0)]);
        db.flush_collection("", "c").unwrap();
    }
}

/// 查询分段并行：同一快照交给不同线程数的计算池，每条查询的主键与分数位模式都相同（含跨源同分）。
#[test]
fn query_batches_score_the_same_on_any_pool_size() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(false, false, false))
        .unwrap();
    let row = |id: i64| -> Vec<(u32, f32)> {
        (0..4)
            .map(|k| {
                (
                    ((id * 7 + k * 13) % 23) as u32,
                    ((id + k) % 5 + 1) as f32 / 4.0,
                )
            })
            .collect()
    };
    for batch in [0..40, 40..80] {
        let ids: Vec<i64> = batch.collect();
        let rows = ids.iter().map(|id| row(*id)).collect();
        db.insert(
            "",
            "c",
            self::batch(&ids, false, InputValues::SparseFloatVector(rows)),
        )
        .unwrap();
        db.flush_collection("", "c").unwrap();
    }
    let ids: Vec<i64> = (80..100).collect();
    let rows = ids.iter().map(|id| row(*id)).collect();
    db.insert(
        "",
        "c",
        batch(&ids, false, InputValues::SparseFloatVector(rows)),
    )
    .unwrap();
    let queries: Vec<Vec<(u32, f32)>> = (0..7).map(|q| row(q * 3 + 1)).collect();
    let snapshot = db.collection("", "c").unwrap().read().read_snapshot();
    let run = |threads: usize| {
        let compute = Compute::new(std::num::NonZeroUsize::new(threads).unwrap()).unwrap();
        let result = snapshot
            .search(
                milvus_lite_core::SearchRequest {
                    anns_field: Some("sp".into()),
                    vectors: milvus_lite_core::QueryVectors::Sparse(queries.clone()),
                    metric_type: None,
                    limit: 15,
                    offset: 0,
                    output_fields: Vec::new(),
                    params: Default::default(),
                    negative_top_k: false,
                },
                &compute,
            )
            .unwrap();
        result
            .hits
            .into_iter()
            .map(|hits| {
                let bits: Vec<u32> = hits.scores.iter().map(|s| s.to_bits()).collect();
                (hits.rows.pks, bits)
            })
            .collect::<Vec<_>>()
    };
    let single = run(1);
    assert_eq!(single.len(), 7);
    assert!(single.iter().all(|(pks, _)| pks.len() == 15));
    for threads in [2, 3, 4, 8] {
        assert_eq!(run(threads), single, "{threads} threads");
    }
}
