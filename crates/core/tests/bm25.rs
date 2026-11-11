//! 0031 第 1 步：BM25 function 的 schema 校验、分词器构造、写入时生成输出字段与持久化。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use milvus_lite_core::{
    Collection, CollectionSchema, ColumnData, Compute, DataDir, DataType, Db, EngineError,
    FieldSchema, Function, InputValues, InsertBatch, Pk,
};
use serde_json::{Value, json};

fn open(path: &Path) -> Arc<Db> {
    Db::new(
        DataDir::open(path).unwrap(),
        Compute::new(std::num::NonZeroUsize::MIN).unwrap(),
    )
    .unwrap()
}

fn bm25(name: &str, input: &[&str], output: &[&str]) -> Function {
    Function {
        name: name.into(),
        function_type: 1,
        input_field_names: input.iter().map(|s| (*s).to_owned()).collect(),
        output_field_names: output.iter().map(|s| (*s).to_owned()).collect(),
        params: serde_json::Map::new(),
    }
}

fn text(analyzer_params: Option<Value>) -> FieldSchema {
    FieldSchema {
        max_length: Some(200),
        enable_analyzer: true,
        nullable: true,
        analyzer_params,
        ..FieldSchema::new("text", DataType::VarChar)
    }
}

fn schema(fields: Vec<FieldSchema>, functions: Vec<Function>) -> CollectionSchema {
    let mut all = vec![FieldSchema {
        is_primary: true,
        ..FieldSchema::new("id", DataType::Int64)
    }];
    all.extend(fields);
    CollectionSchema {
        fields: all,
        enable_dynamic_field: true,
        properties: Vec::new(),
        functions,
    }
}

fn basic(analyzer_params: Option<Value>) -> CollectionSchema {
    schema(
        vec![
            text(analyzer_params),
            FieldSchema::new("sp", DataType::SparseFloatVector),
        ],
        vec![bm25("bm", &["text"], &["sp"])],
    )
}

fn error(schema: CollectionSchema) -> String {
    Collection::new("c", schema).unwrap_err().to_string()
}

fn fnv(term: &str) -> u32 {
    term.bytes().fold(0x811c_9dc5, |h: u32, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

fn pack(pairs: &[(u32, f32)]) -> Vec<u8> {
    pairs
        .iter()
        .flat_map(|(d, w)| d.to_le_bytes().into_iter().chain(w.to_le_bytes()))
        .collect()
}

/// 每个主键的（文本，输出字节）。
fn stored(db: &Db) -> Vec<(i64, Option<String>, Vec<u8>)> {
    let handle = db.collection("", "c").unwrap();
    let rows = handle
        .read()
        .read_snapshot()
        .query("", &["text".into(), "sp".into()], None, 0)
        .unwrap()
        .rows;
    let column = |name: &str| &rows.fields.iter().find(|(n, _)| n == name).unwrap().1;
    let ColumnData::SparseFloatVector(sp) = column("sp").values() else {
        panic!("sparse column lost its type");
    };
    let texts = column("text");
    let ColumnData::VarChar(values) = texts.values() else {
        panic!("text column lost its type");
    };
    let mut out: Vec<_> = rows
        .pks
        .iter()
        .enumerate()
        .map(|(row, pk)| {
            let Pk::Int64(id) = pk else { panic!("{pk:?}") };
            (
                *id,
                (!texts.is_null(row)).then(|| values[row].clone()),
                sp.row(row).to_vec(),
            )
        })
        .collect();
    out.sort();
    out
}

fn texts(ids: &[i64], values: &[Option<&str>]) -> InsertBatch {
    let validity: Vec<bool> = values.iter().map(Option::is_some).collect();
    InsertBatch {
        num_rows: ids.len(),
        columns: vec![
            ("id".into(), InputValues::Int(ids.to_vec())),
            (
                "text".into(),
                InputValues::Nullable {
                    values: Box::new(InputValues::Str(
                        values.iter().map(|v| v.unwrap_or("").to_owned()).collect(),
                    )),
                    validity: milvus_lite_core::data::Validity::from_bools(&validity),
                },
            ),
        ],
    }
}

/// 3.x `validate_schema` 末尾的函数校验，按顺序报第一个错误。
#[test]
fn function_validation_follows_3x_order_and_messages() {
    let sp = || FieldSchema::new("sp", DataType::SparseFloatVector);
    let int_text = FieldSchema::new("text", DataType::Int64);
    let plain_text = FieldSchema {
        enable_analyzer: false,
        ..text(None)
    };
    for (fields, functions, message) in [
        (
            vec![text(None), sp()],
            vec![
                bm25("bm", &["text"], &["sp"]),
                bm25("bm", &["text"], &["sp"]),
            ],
            "duplicate function name 'bm'",
        ),
        (
            vec![text(None), sp()],
            vec![bm25("a", &["text"], &["sp"]), bm25("b", &["text"], &["sp"])],
            "function output field 'sp' is used by multiple functions",
        ),
        (
            vec![text(None), sp()],
            vec![bm25("bm", &[], &["sp"])],
            "BM25 function 'bm' requires exactly one input field",
        ),
        (
            vec![text(None), sp()],
            vec![bm25("bm", &["nope"], &["sp"])],
            "BM25 function 'bm' input field 'nope' not found in schema",
        ),
        (
            vec![int_text, sp()],
            vec![bm25("bm", &["text"], &["sp"])],
            "BM25 function 'bm' input field 'text' must be VARCHAR, got INT64",
        ),
        (
            vec![plain_text, sp()],
            vec![bm25("bm", &["text"], &["sp"])],
            "BM25 function 'bm' input field 'text' must have enable_analyzer=True",
        ),
        (
            vec![text(None), sp()],
            vec![bm25("bm", &["text"], &["sp", "id"])],
            "BM25 function 'bm' requires exactly one output field",
        ),
        (
            vec![text(None), sp()],
            vec![bm25("bm", &["text"], &["nope"])],
            "BM25 function 'bm' output field 'nope' not found in schema",
        ),
        (
            vec![
                text(None),
                FieldSchema {
                    dim: Some(2),
                    ..FieldSchema::new("sp", DataType::FloatVector)
                },
            ],
            vec![bm25("bm", &["text"], &["sp"])],
            "BM25 function 'bm' output field 'sp' must be SPARSE_FLOAT_VECTOR, got FLOAT_VECTOR",
        ),
        (
            vec![FieldSchema {
                is_function_output: true,
                ..sp()
            }],
            vec![],
            "sparse function output field 'sp' has no producing Function",
        ),
    ] {
        assert_eq!(
            error(schema(fields, functions)),
            message.to_string(),
            "{message}"
        );
    }
    // 函数校验在最后：主键缺失这类错误先报。
    let no_pk = CollectionSchema {
        fields: vec![text(None), sp()],
        functions: vec![bm25("bm", &["nope"], &["sp"])],
        ..Default::default()
    };
    assert_eq!(error(no_pk), "schema has no primary key field");
    // 本实现不支持的形状：其他函数类型，以及标了输出却没有函数产出的非稀疏字段。
    let mut embedding = basic(None);
    embedding.functions[0].function_type = 2;
    assert!(matches!(
        Collection::new("c", embedding),
        Err(EngineError::Unsupported { .. })
    ));
    let marked = schema(
        vec![
            FieldSchema {
                is_function_output: true,
                ..FieldSchema::new("t", DataType::Int64)
            },
            FieldSchema::new("sp", DataType::SparseFloatVector),
        ],
        vec![],
    );
    assert!(matches!(
        Collection::new("c", marked),
        Err(EngineError::Unsupported { .. })
    ));
}

/// 分词器在函数校验之后构造；假值原值只在 BM25 输入字段上报错。
#[test]
fn analyzer_params_are_checked_after_functions_with_raw_values() {
    let mut falsy = basic(None);
    falsy.fields[1].unsaved_analyzer_params = Some(json!(0));
    assert_eq!(error(falsy.clone()), "'int' object has no attribute 'get'");
    // 函数校验的错误先报。
    falsy.functions.push(bm25("bm", &["text"], &["sp"]));
    assert_eq!(error(falsy), "duplicate function name 'bm'");
    assert_eq!(
        error(basic(Some(json!({"tokenizer": "english"})))),
        "unknown tokenizer type: 'english' (supported: 'standard', 'jieba')"
    );
    assert!(matches!(
        Collection::new("c", basic(Some(json!({"tokenizer": "jieba"})))),
        Err(EngineError::Unsupported { .. })
    ));
    assert!(Collection::new("c", basic(Some(json!({"tokenizer": []})))).is_ok());
}

/// 写入时生成：小写、停用词、按词项哈希计数并按维度排序；null 与缺失的文本生成空向量，
/// 即使文本之后补成默认值。字节经 WAL、flush 与重开保持不变。
#[test]
fn outputs_are_generated_before_defaults_and_survive_reopen() {
    let root = tempfile::tempdir().unwrap();
    let mut schema = basic(Some(
        json!({"filter": [{"type": "stop", "stop_words": ["the", 1]}]}),
    ));
    schema.fields[1].default_value = Some(json!("dflt word"));
    let db = open(root.path());
    db.create_collection("", "c", schema).unwrap();
    db.insert(
        "",
        "c",
        texts(
            &[1, 2, 3],
            &[Some("The fox, THE fox and 1 Dog"), None, Some("")],
        ),
    )
    .unwrap();
    db.insert(
        "",
        "c",
        InsertBatch {
            num_rows: 1,
            columns: vec![("id".into(), InputValues::Int(vec![4]))],
        },
    )
    .unwrap();
    let mut fox = [(fnv("fox"), 2.0), (fnv("and"), 1.0), (fnv("dog"), 1.0)];
    fox.sort_by_key(|p| p.0);
    let expected = vec![
        (1, Some("The fox, THE fox and 1 Dog".to_owned()), pack(&fox)),
        (2, Some("dflt word".to_owned()), Vec::new()),
        (3, Some(String::new()), Vec::new()),
        (4, Some("dflt word".to_owned()), Vec::new()),
    ];
    assert_eq!(stored(&db), expected);
    db.flush_collection("", "c").unwrap();
    db.close();
    drop(db);
    let db = open(root.path());
    assert_eq!(stored(&db), expected);
    let handle = db.collection("", "c").unwrap();
    let schema = handle.read().schema().clone();
    assert!(schema.fields[2].is_function_output);
    assert_eq!(schema.functions, vec![bm25("bm", &["text"], &["sp"])]);
    db.close();
}

/// 输出字段出现在请求里（列或 `$meta` 的键）时整批拒绝，先于逐行校验；upsert 同样。
/// Upsert 不提供文本时按请求前的文本重新生成。
#[test]
fn outputs_are_rejected_in_requests_and_regenerated_on_upsert() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", basic(None)).unwrap();
    db.insert("", "c", texts(&[1, 2], &[Some("a b"), Some("c")]))
        .unwrap();
    let generated = "BM25 output field 'sp' is generated; provide its source text instead";
    let with_column = InsertBatch {
        num_rows: 1,
        columns: vec![(
            "sp".into(),
            InputValues::SparseFloatVector(vec![vec![(1, 1.0)]]),
        )],
    };
    assert_eq!(
        db.insert("", "c", with_column.clone())
            .unwrap_err()
            .to_string(),
        generated
    );
    assert_eq!(
        db.upsert_with_pk_type("", "c", with_column)
            .err()
            .unwrap()
            .to_string(),
        generated
    );
    let with_meta = InsertBatch {
        num_rows: 2,
        columns: vec![
            ("id".into(), InputValues::Int(vec![7, 8])),
            (
                "text".into(),
                InputValues::Str(vec!["x".into(), "y".into()]),
            ),
            (
                "$meta".into(),
                InputValues::Json(vec![b"{\"k\": 1}".to_vec(), b"{\"sp\": 1}".to_vec()]),
            ),
        ],
    };
    assert_eq!(
        db.insert("", "c", with_meta).unwrap_err().to_string(),
        generated
    );
    // 只改 id 1 的 tag 一类字段：这里没有其他字段，只提供主键，文本取旧值。
    db.upsert_with_pk_type(
        "",
        "c",
        InsertBatch {
            num_rows: 2,
            columns: vec![("id".into(), InputValues::Int(vec![1, 9]))],
        },
    )
    .unwrap();
    db.upsert_with_pk_type("", "c", texts(&[2], &[Some("c c")]))
        .unwrap();
    let mut ab = [(fnv("a"), 1.0), (fnv("b"), 1.0)];
    ab.sort_by_key(|p| p.0);
    assert_eq!(
        stored(&db),
        vec![
            (1, Some("a b".to_owned()), pack(&ab)),
            (2, Some("c c".to_owned()), pack(&[(fnv("c"), 2.0)])),
            (9, None, Vec::new()),
        ]
    );
    db.close();
}

/// 手改的 `schema.json`：BM25 输入字段上的假值 `analyzer_params` 使集合打不开（3.x 打开时 `create_analyzer`
/// 报错），其他字段上的假值照 0029 当作没有设置；该集合仍可 drop。
#[test]
fn falsy_analyzer_params_in_schema_file_only_break_bm25_inputs() {
    let root = tempfile::tempdir().unwrap();
    let mut with_other = basic(None);
    with_other.fields.push(FieldSchema {
        max_length: Some(8),
        enable_analyzer: true,
        ..FieldSchema::new("other", DataType::VarChar)
    });
    let db = open(root.path());
    db.create_collection("", "c", with_other).unwrap();
    db.close();
    drop(db);
    let path = root.path().join("collections/c/schema.json");
    let edit = |field: &str| {
        let mut value = milvus_lite_core::json_text::value(&std::fs::read(&path).unwrap()).unwrap();
        for f in value["fields"].as_array_mut().unwrap() {
            f.as_object_mut().unwrap().remove("analyzer_params");
            if f["name"] == field {
                f["analyzer_params"] = json!(0);
            }
        }
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    };
    edit("other");
    let db = open(root.path());
    let handle = db.collection("", "c").unwrap();
    assert_eq!(handle.read().schema().fields[3].analyzer_params, None);
    drop(handle);
    db.close();
    drop(db);
    edit("text");
    let db = open(root.path());
    assert!(db.collection("", "c").is_err());
    db.drop_collection("", "c").unwrap();
    db.close();
}

/// 分区键集合走按桶写入的路径：写入、partial upsert（不带文本保留旧向量、带文本重新生成、新主键）、
/// 输出字段出现在列或 `$meta` 里时拒绝；flush 与重开后不变。
#[test]
fn partition_key_writes_generate_and_reject_outputs() {
    let root = tempfile::tempdir().unwrap();
    let mut schema = basic(None);
    schema.fields.push(FieldSchema {
        max_length: Some(8),
        is_partition_key: true,
        ..FieldSchema::new("tag", DataType::VarChar)
    });
    let db = open(root.path());
    db.create_collection("", "c", schema).unwrap();
    let with_tags = |mut batch: InsertBatch, tags: &[&str]| {
        batch.columns.push((
            "tag".into(),
            InputValues::Str(tags.iter().map(|t| (*t).to_owned()).collect()),
        ));
        batch
    };
    db.insert(
        "",
        "c",
        with_tags(texts(&[1, 2], &[Some("a b a"), Some("c")]), &["x", "y"]),
    )
    .unwrap();
    let generated = "BM25 output field 'sp' is generated; provide its source text instead";
    let mut with_column = with_tags(texts(&[3], &[Some("d")]), &["x"]);
    with_column.columns.push((
        "sp".into(),
        InputValues::SparseFloatVector(vec![vec![(1, 1.0)]]),
    ));
    assert_eq!(
        db.insert("", "c", with_column.clone())
            .unwrap_err()
            .to_string(),
        generated
    );
    assert_eq!(
        db.upsert_with_pk_type("", "c", with_column)
            .err()
            .unwrap()
            .to_string(),
        generated
    );
    let mut with_meta = with_tags(texts(&[3], &[Some("d")]), &["x"]);
    with_meta.columns.push((
        "$meta".into(),
        InputValues::Json(vec![b"{\"sp\": 1}".to_vec()]),
    ));
    assert_eq!(
        db.insert("", "c", with_meta.clone())
            .unwrap_err()
            .to_string(),
        generated
    );
    assert_eq!(
        db.upsert_with_pk_type("", "c", with_meta)
            .err()
            .unwrap()
            .to_string(),
        generated
    );
    // 只给主键：1 保留旧文本与旧向量；4 是新主键，没有文本列，生成空向量。
    db.upsert_with_pk_type(
        "",
        "c",
        InsertBatch {
            num_rows: 1,
            columns: vec![("id".into(), InputValues::Int(vec![1]))],
        },
    )
    .unwrap();
    db.upsert_with_pk_type(
        "",
        "c",
        InsertBatch {
            num_rows: 1,
            columns: vec![
                ("id".into(), InputValues::Int(vec![4])),
                ("tag".into(), InputValues::Str(vec!["z".into()])),
            ],
        },
    )
    .unwrap();
    db.upsert_with_pk_type("", "c", texts(&[2], &[Some("e e")]))
        .unwrap();
    let mut ab = [(fnv("a"), 2.0), (fnv("b"), 1.0)];
    ab.sort_by_key(|p| p.0);
    let expected = vec![
        (1, Some("a b a".to_owned()), pack(&ab)),
        (2, Some("e e".to_owned()), pack(&[(fnv("e"), 2.0)])),
        (4, None, Vec::new()),
    ];
    assert_eq!(stored(&db), expected);
    db.flush_collection("", "c").unwrap();
    db.close();
    drop(db);
    let db = open(root.path());
    assert_eq!(stored(&db), expected);
    db.close();
}

fn bm25_index(metric: &str, params: Value) -> milvus_lite_core::IndexRequest {
    milvus_lite_core::IndexRequest {
        index_type: "sparse_inverted_index".into(),
        metric_type: Some(metric.into()),
        params: params.as_object().unwrap().clone(),
        search_params: serde_json::Map::new(),
    }
}

fn search_error(
    db: &Db,
    vectors: milvus_lite_core::QueryVectors,
    metric_type: Option<&str>,
    params: Value,
) -> EngineError {
    let handle = db.collection("", "c").unwrap();
    let snapshot = handle.read().read_search_snapshot(&[], Some("sp")).unwrap();
    snapshot
        .search_filtered_with_range(
            milvus_lite_core::SearchRequest {
                anns_field: Some("sp".into()),
                vectors,
                metric_type: metric_type.map(str::to_owned),
                limit: 10,
                offset: 0,
                output_fields: Vec::new(),
                params: params.as_object().unwrap().clone(),
                negative_top_k: false,
            },
            db.compute(),
            "",
            None,
        )
        .unwrap_err()
}

/// 0031 第 2 步：BM25 输出字段的期望度量是 BM25（建索引、load、检索三处同一套校验），查询只能是文本，
/// k1 / b 在定义与检索请求里都按 3.x `_validate_sparse_tuning` 校验，且先于查询种类。
#[test]
fn bm25_metric_tuning_and_query_kind_follow_3x() {
    use milvus_lite_core::QueryVectors;
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", basic(None)).unwrap();
    assert_eq!(
        db.create_index("", "c", "sp", bm25_index("IP", json!({})))
            .unwrap_err()
            .to_string(),
        "sparse field 'sp' requires SPARSE_INVERTED_INDEX with metric BM25; got 'SPARSE_INVERTED_INDEX'/'IP'. \
         For an existing index, release, drop, and recreate it with the allowed metric."
    );
    assert_eq!(
        db.create_index("", "c", "sp", bm25_index("BM25", json!({"bm25_k1": "x"})))
            .unwrap_err()
            .to_string(),
        "field 'sp': invalid bm25_k1"
    );
    db.create_index(
        "",
        "c",
        "sp",
        bm25_index("bm25", json!({"bm25_k1": 1.2, "bm25_b": "0.5"})),
    )
    .unwrap();

    let sparse = || QueryVectors::Sparse(vec![vec![(1, 1.0)]]);
    let text = || QueryVectors::Text(vec!["fox".into()]);
    for (metric, got) in [("ip", "'IP'"), ("", "''")] {
        assert_eq!(
            search_error(&db, text(), Some(metric), json!({})).to_string(),
            format!("sparse field 'sp' requires metric BM25, got {got}")
        );
    }
    let kind = "BM25 field 'sp' requires a text query, not a sparse vector";
    assert_eq!(
        search_error(&db, sparse(), None, json!({})).to_string(),
        kind
    );
    assert_eq!(
        search_error(&db, QueryVectors::Dense(vec![vec![1.0]]), None, json!({})).to_string(),
        kind
    );
    // 检索请求的调参项先于查询种类校验；它们只校验、不参与打分。
    assert_eq!(
        search_error(&db, sparse(), Some("BM25"), json!({"bm25_b": 2})).to_string(),
        "field 'sp': invalid bm25_b"
    );
    assert_eq!(
        search_error(&db, sparse(), None, json!({"bm25_k1": true})).to_string(),
        "field 'sp': invalid bm25_k1"
    );
    db.close();
    drop(db);

    // load 用同一套校验拒绝其他版本写下的定义。
    for (key, value, message) in [
        (
            "metric_type",
            json!("IP"),
            "sparse field 'sp' requires SPARSE_INVERTED_INDEX with metric BM25; got 'SPARSE_INVERTED_INDEX'/'IP'. \
             For an existing index, release, drop, and recreate it with the allowed metric.",
        ),
        (
            "search_params",
            json!({"bm25_k1": 0}),
            "field 'sp': invalid bm25_k1",
        ),
    ] {
        let file = root.path().join("collections/c/manifest.json");
        let original = std::fs::read(&file).unwrap();
        let mut manifest: Value = milvus_lite_core::json_text::value(&original).unwrap();
        manifest["index_specs"]["sp"][key] = value;
        std::fs::write(&file, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let db = open(root.path());
        let handle = db.collection("", "c").unwrap();
        assert_eq!(handle.write().load().unwrap_err().to_string(), message);
        db.close();
        drop(db);
        std::fs::write(&file, original).unwrap();
    }
}

fn tagged(rows: &[(i64, i64, &str)]) -> InsertBatch {
    InsertBatch {
        num_rows: rows.len(),
        columns: vec![
            (
                "id".into(),
                InputValues::Int(rows.iter().map(|r| r.0).collect()),
            ),
            (
                "tag".into(),
                InputValues::Int(rows.iter().map(|r| r.1).collect()),
            ),
            (
                "text".into(),
                InputValues::Str(rows.iter().map(|r| r.2.to_owned()).collect()),
            ),
        ],
    }
}

fn tagged_schema() -> CollectionSchema {
    schema(
        vec![
            FieldSchema::new("tag", DataType::Int64),
            text(None),
            FieldSchema::new("sp", DataType::SparseFloatVector),
        ],
        vec![bm25("bm", &["text"], &["sp"])],
    )
}

/// 每条查询的 (id, 对外距离的 float32 位模式)。
fn text_search(db: &Db, name: &str, texts: &[&str], expr: &str) -> Vec<Vec<(i64, String)>> {
    let handle = db.collection("", name).unwrap();
    let snapshot = handle.read().read_search_snapshot(&[], Some("sp")).unwrap();
    let result = snapshot
        .search_filtered_with_range(
            milvus_lite_core::SearchRequest {
                anns_field: Some("sp".into()),
                vectors: milvus_lite_core::QueryVectors::Text(
                    texts.iter().map(|t| (*t).to_owned()).collect(),
                ),
                metric_type: None,
                limit: 10,
                offset: 0,
                output_fields: Vec::new(),
                params: serde_json::Map::new(),
                negative_top_k: false,
            },
            db.compute(),
            expr,
            None,
        )
        .unwrap();
    result
        .hits
        .iter()
        .map(|hits| {
            hits.rows
                .pks
                .iter()
                .zip(&hits.scores)
                .map(|(pk, score)| {
                    let Pk::Int64(id) = pk else { panic!("{pk:?}") };
                    (*id, format!("{:08x}", score.to_bits()))
                })
                .collect()
        })
        .collect()
}

fn hits(pairs: &[(i64, &str)]) -> Vec<(i64, String)> {
    pairs
        .iter()
        .map(|(id, bits)| (*id, (*bits).to_owned()))
        .collect()
}

/// 0031 第 3 步：期望的距离位模式取自 3.x（`1320d26`，NumPy 2.5.3）的 pymilvus 检索结果（小端 float32 的大端写法）。
/// MemTable 只统计存活且通过过滤的行，所以加过滤后分数变化；segment 统计全部物理行，过滤与删除都不改变分数。
/// 并列时先比首个命中的查询词序号，再比行号。
#[test]
fn bm25_scores_match_3x_in_memtable_and_segments() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "a", tagged_schema()).unwrap();
    db.insert(
        "",
        "a",
        tagged(&[
            (0, 0, "dog"),
            (1, 1, "fox"),
            (2, 0, "fox dog dog cat"),
            (3, 1, "the quick brown fox"),
        ]),
    )
    .unwrap();
    let all = hits(&[
        (2, "bf8e3408"),
        (0, "bf731387"),
        (1, "befa293b"),
        (3, "be8fcb1a"),
    ]);
    assert_eq!(
        text_search(&db, "a", &["fox dog", "dog fox", "cat"], ""),
        vec![all.clone(), all.clone(), hits(&[(2, "bf72b0ca")])]
    );
    assert_eq!(
        text_search(&db, "a", &["fox dog"], "tag == 0"),
        vec![hits(&[(2, "bf439e4f"), (0, "be7fbfed")])]
    );
    db.flush_collection("", "a").unwrap();
    assert_eq!(text_search(&db, "a", &["fox dog"], ""), vec![all.clone()]);
    assert_eq!(
        text_search(&db, "a", &["fox dog"], "tag == 0"),
        vec![all[..2].to_vec()]
    );
    db.delete("", "a", &[Pk::Int64(2)]).unwrap();
    assert_eq!(
        text_search(&db, "a", &["fox dog"], ""),
        vec![all[1..].to_vec()]
    );
    // 空文本与不在任何行里的词：没有命中。
    assert_eq!(
        text_search(&db, "a", &["", "zebra"], ""),
        vec![vec![], vec![]]
    );

    db.create_collection("", "t", tagged_schema()).unwrap();
    db.insert(
        "",
        "t",
        tagged(&[(0, 0, "dog"), (1, 0, "fox"), (2, 0, "cat cat")]),
    )
    .unwrap();
    let tie = vec![
        hits(&[(1, "bf8d75e0"), (0, "bf8d75e0")]),
        hits(&[(0, "bf8d75e0"), (1, "bf8d75e0")]),
    ];
    assert_eq!(text_search(&db, "t", &["fox dog", "dog fox"], ""), tie);
    db.flush_collection("", "t").unwrap();
    assert_eq!(text_search(&db, "t", &["fox dog", "dog fox"], ""), tie);
}

/// 合法但使 float32 溢出的 k1（1e308）：分数是 NaN，照常返回（3.x 实测距离位模式为 0xffc00000）。
#[test]
fn bm25_overflowing_k1_returns_nan_scores() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "k", tagged_schema()).unwrap();
    db.create_index(
        "",
        "k",
        "sp",
        bm25_index("BM25", json!({"bm25_k1": 1e308, "bm25_b": 0})),
    )
    .unwrap();
    db.insert("", "k", tagged(&[(0, 0, "fox fox")])).unwrap();
    let got = text_search(&db, "k", &["fox"], "");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].len(), 1);
    assert!(f32::from_bits(u32::from_str_radix(&got[0][0].1, 16).unwrap()).is_nan());
}

/// k1 / b 只取索引定义的 `params`；期望的距离位模式取自 3.x（`1320d26`，NumPy 2）。`tf·(k1+1)` 在 float64 中算出
/// 再转 float32，改成 float32 相乘会差 1 ULP，这组数据能区分。
#[test]
fn bm25_tuning_from_index_definition_matches_3x() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    let docs = [
        "a",
        "a a",
        "a a a b",
        "a a a a c c",
        "a a a a a d d d",
        "a b c d e f g",
        "b b b b b b b",
    ];
    for (name, k1, b, expected) in [
        (
            "k",
            1.2,
            0.6,
            [
                hits(&[
                    (5, "c03e2ed8"),
                    (4, "c0001c6d"),
                    (3, "bfefaa3c"),
                    (6, "bfbffb8f"),
                    (2, "bf9c7956"),
                    (1, "bea8fdf6"),
                    (0, "be900492"),
                ]),
                hits(&[
                    (4, "c05423b1"),
                    (5, "c0326ead"),
                    (6, "bfbffb8f"),
                    (2, "bf6273b8"),
                ]),
            ],
        ),
        (
            "j",
            2.3,
            0.3,
            [
                hits(&[
                    (5, "c0467ac3"),
                    (4, "c01d66b4"),
                    (3, "c009f3f8"),
                    (6, "bfff4151"),
                    (2, "bfa167d4"),
                    (1, "beb48f48"),
                    (0, "be7f553c"),
                ]),
                hits(&[
                    (4, "c080fa03"),
                    (5, "c03a375e"),
                    (6, "bfff4151"),
                    (2, "bf5cddad"),
                ]),
            ],
        ),
    ] {
        db.create_collection("", name, tagged_schema()).unwrap();
        db.create_index(
            "",
            name,
            "sp",
            bm25_index("BM25", json!({"bm25_k1": k1, "bm25_b": b})),
        )
        .unwrap();
        let rows: Vec<(i64, i64, &str)> = docs
            .iter()
            .enumerate()
            .map(|(i, t)| (i as i64, 0, *t))
            .collect();
        db.insert("", name, tagged(&rows)).unwrap();
        assert_eq!(
            text_search(&db, name, &["a b c d", "b d d"], ""),
            expected.to_vec()
        );
    }
}
