//! 内存引擎的行为用例。期望值取自 3.x（v3.2.1）memtable 路径的实测与源码，
//! 与 3.x 不同之处对应 0004「决策」。

// 辅助函数不带 #[test]，clippy 的 allow-unwrap-in-tests 覆盖不到；测试中失败即应 panic。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use milvus_lite_core::filter::PkLiteral;
use milvus_lite_core::{
    Collection, CollectionSchema, ColumnData, Compute, DataType, EngineError, FieldSchema,
    IndexRequest, InputValues, InsertBatch, LoadState, Pk, QueryVectors, SearchRequest,
};
use serde_json::json;

/// 检索用的计算池：2 个线程，覆盖并行路径。
fn compute() -> Compute {
    Compute::new(std::num::NonZeroUsize::new(2).unwrap()).unwrap()
}

fn pk_field(name: &str, auto_id: bool) -> FieldSchema {
    FieldSchema {
        is_primary: true,
        auto_id,
        ..FieldSchema::new(name, DataType::Int64)
    }
}

fn vec_field(name: &str, dim: i64) -> FieldSchema {
    FieldSchema {
        dim: Some(dim),
        ..FieldSchema::new(name, DataType::FloatVector)
    }
}

/// id INT64 主键、v 二维向量、t VARCHAR(max 5)。
fn basic(dynamic: bool, auto_id: bool) -> Collection {
    let schema = CollectionSchema {
        fields: vec![
            pk_field("id", auto_id),
            vec_field("v", 2),
            FieldSchema {
                max_length: Some(5),
                ..FieldSchema::new("t", DataType::VarChar)
            },
        ],
        enable_dynamic_field: dynamic,
        properties: Vec::new(),
    };
    Collection::new("c", schema).unwrap()
}

fn vectors(rows: &[[f32; 2]]) -> InputValues {
    InputValues::FloatVector {
        dim: 2,
        values: rows.iter().flatten().copied().collect(),
    }
}

fn strs(v: &[&str]) -> InputValues {
    InputValues::Str(v.iter().map(|s| (*s).to_owned()).collect())
}

fn batch(columns: Vec<(&str, InputValues)>) -> InsertBatch {
    let num_rows = columns.first().map_or(0, |(_, v)| v.len());
    InsertBatch {
        num_rows,
        columns: columns
            .into_iter()
            .map(|(n, v)| (n.to_owned(), v))
            .collect(),
    }
}

fn rows(ids: &[i64], vs: &[[f32; 2]], ts: &[&str]) -> InsertBatch {
    batch(vec![
        ("id", InputValues::Int(ids.to_vec())),
        ("v", vectors(vs)),
        ("t", strs(ts)),
    ])
}

fn ints(pks: &[Pk]) -> Vec<i64> {
    pks.iter()
        .map(|p| match p {
            Pk::Int64(i) => *i,
            Pk::VarChar(s) => panic!("unexpected varchar pk {s}"),
        })
        .collect()
}

fn insert_err(c: &mut Collection, b: InsertBatch) -> String {
    c.insert(b).unwrap_err().to_string()
}

fn search(c: &Collection, q: [f32; 2], limit: usize, metric: Option<&str>) -> (Vec<i64>, Vec<f32>) {
    let r = c
        .search(
            SearchRequest {
                anns_field: None,
                vectors: QueryVectors::Dense(vec![q.to_vec()]),
                metric_type: metric.map(str::to_owned),
                limit,
                offset: 0,
                output_fields: vec![],
            },
            &compute(),
        )
        .unwrap();
    let hits = &r.hits[0];
    (ints(&hits.rows.pks), hits.scores.clone())
}

fn flat_index(c: &mut Collection, metric: &str) {
    c.create_index(
        "v",
        IndexRequest {
            index_type: "FLAT".to_owned(),
            metric_type: Some(metric.to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
}

#[test]
fn insert_returns_pks_per_row_and_latest_version_wins() {
    let mut c = basic(false, false);
    let ids = c
        .insert(rows(
            &[5, 3, 5],
            &[[1.0, 0.0], [0.0, 1.0], [2.0, 0.0]],
            &["a", "b", "c"],
        ))
        .unwrap();
    assert_eq!(ints(&ids), [5, 3, 5]);
    assert_eq!(c.count(), 2);
    // 批内重复：前一行被同批的后一行覆盖，不参与检索。
    let (ids, scores) = search(&c, [0.0, 0.0], 10, Some("L2"));
    assert_eq!((ids, scores), (vec![3, 5], vec![1.0, 4.0]));
    c.insert(rows(&[3], &[[9.0, 9.0]], &["d"])).unwrap();
    assert_eq!(c.count(), 2);
    let got = c.get(&[Pk::Int64(3), Pk::Int64(5)], &[]).unwrap();
    assert_eq!(ints(&got.rows.pks), [3, 5]);
    let t = &got.rows.fields.iter().find(|(n, _)| n == "t").unwrap().1;
    assert_eq!(t, &ColumnData::VarChar(vec!["d".into(), "c".into()]));
    // 被覆盖的旧版本不参与检索。
    let (ids, _) = search(&c, [1.0, 0.0], 10, Some("L2"));
    assert_eq!(ids.len(), 2);
}

#[test]
fn get_keeps_request_order_duplicates_and_skips_missing() {
    let mut c = basic(false, false);
    c.insert(rows(&[3, 5], &[[1.0, 0.0], [0.0, 1.0]], &["a", "b"]))
        .unwrap();
    let pks = [Pk::Int64(3), Pk::Int64(3), Pk::Int64(99), Pk::Int64(5)];
    assert_eq!(ints(&c.get(&pks, &[]).unwrap().rows.pks), [3, 3, 5]);
    // 字面量类型与主键类型不符时返回空（3.x 相同）。
    assert!(
        c.get(&[Pk::VarChar("3".into())], &[])
            .unwrap()
            .rows
            .is_empty()
    );
    assert!(c.get(&[], &[]).unwrap().rows.is_empty());
}

#[test]
fn auto_id_is_the_row_seq_and_failed_inserts_consume_nothing() {
    let mut c = basic(false, true);
    let no_pk = |vs: &[[f32; 2]], ts: &[&str]| batch(vec![("v", vectors(vs)), ("t", strs(ts))]);
    assert_eq!(
        ints(&c.insert(no_pk(&[[1.0, 0.0]; 3], &["a", "b", "c"])).unwrap()),
        [1, 2, 3]
    );
    // 0004「决策」：3.x 中失败的 insert 会消耗 auto_id，本实现不消耗。
    insert_err(&mut c, no_pk(&[[1.0, 0.0]; 2], &["toolong", "x"]));
    assert_eq!(
        ints(&c.insert(no_pk(&[[1.0, 0.0]; 2], &["d", "e"])).unwrap()),
        [4, 5]
    );
    // auto_id collection 中用户自带主键时采用之，仍消耗一个 _seq（3.x 相同）。
    assert_eq!(
        ints(&c.insert(rows(&[100], &[[1.0, 0.0]], &["f"])).unwrap()),
        [100]
    );
    assert_eq!(ints(&c.insert(no_pk(&[[1.0, 0.0]], &["g"])).unwrap()), [7]);
    // 每个 collection 独立计数。
    let mut other = basic(false, true);
    assert_eq!(
        ints(&other.insert(no_pk(&[[1.0, 0.0]], &["a"])).unwrap()),
        [1]
    );
}

#[test]
fn insert_errors_match_3x() {
    let mut c = basic(false, false);
    assert_eq!(
        insert_err(
            &mut c,
            batch(vec![("v", vectors(&[[1.0, 0.0]])), ("t", strs(&["a"]))])
        ),
        "primary key 'id' missing or None"
    );
    assert_eq!(
        insert_err(
            &mut c,
            batch(vec![("id", InputValues::Int(vec![1])), ("t", strs(&["a"]))])
        ),
        "vector field 'v' missing or None"
    );
    let dim3 = InputValues::FloatVector {
        dim: 3,
        values: vec![1.0, 2.0, 3.0],
    };
    assert_eq!(
        insert_err(
            &mut c,
            batch(vec![
                ("id", InputValues::Int(vec![1])),
                ("v", dim3),
                ("t", strs(&["a"]))
            ])
        ),
        "vector field 'v' expected dim 2, got 3"
    );
    assert_eq!(
        insert_err(
            &mut c,
            batch(vec![
                ("id", InputValues::Int(vec![1])),
                ("v", vectors(&[[1.0, 0.0]]))
            ])
        ),
        "field 't' missing and not nullable / no default"
    );
    assert_eq!(
        insert_err(&mut c, rows(&[1], &[[1.0, 0.0]], &["toolong"])),
        "VARCHAR field 't' value length 7 exceeds max_length=5"
    );
    assert_eq!(
        insert_err(
            &mut c,
            batch(vec![
                ("id", strs(&["x"])),
                ("v", vectors(&[[1.0, 0.0]])),
                ("t", strs(&["a"]))
            ])
        ),
        "field 'id' value 'x' does not match dtype DataType.INT64"
    );
    let mut extra = rows(&[1], &[[1.0, 0.0]], &["a"]);
    extra.columns.push(("zz".into(), InputValues::Int(vec![1])));
    assert_eq!(
        insert_err(&mut c, extra),
        "record has fields ['zz'] not in schema (enable_dynamic_field is False)"
    );
    assert_eq!(
        insert_err(
            &mut c,
            InsertBatch {
                num_rows: 2,
                columns: vec![("id".into(), InputValues::Int(vec![1]))],
            }
        ),
        "FieldData 'id' has 1 rows, expected 2"
    );
    // 校验全部通过才写入。
    assert_eq!(c.count(), 0);
}

#[test]
fn scalar_type_checks_match_3x() {
    let schema = CollectionSchema {
        fields: vec![
            pk_field("id", false),
            vec_field("v", 2),
            FieldSchema::new("b", DataType::Bool),
            FieldSchema::new("i8", DataType::Int8),
            FieldSchema::new("f", DataType::Float),
        ],
        enable_dynamic_field: false,
        properties: Vec::new(),
    };
    let mut c = Collection::new("s", schema).unwrap();
    let row = |b: InputValues, i8: InputValues, f: InputValues| {
        batch(vec![
            ("id", InputValues::Int(vec![1])),
            ("v", vectors(&[[1.0, 0.0]])),
            ("b", b),
            ("i8", i8),
            ("f", f),
        ])
    };
    assert_eq!(
        insert_err(
            &mut c,
            row(
                InputValues::Int(vec![1]),
                InputValues::Int(vec![1]),
                InputValues::Int(vec![1])
            )
        ),
        "field 'b' value 1 does not match dtype DataType.BOOL"
    );
    assert_eq!(
        insert_err(
            &mut c,
            row(
                InputValues::Bool(vec![true]),
                InputValues::Int(vec![300]),
                InputValues::Int(vec![1])
            )
        ),
        "field 'i8' value 300 does not match dtype DataType.INT8"
    );
    assert_eq!(
        insert_err(
            &mut c,
            row(
                InputValues::Bool(vec![true]),
                InputValues::Float(vec![1.5]),
                InputValues::Int(vec![1])
            )
        ),
        "field 'i8' value 1.5 does not match dtype DataType.INT8"
    );
    // FLOAT 接受整数，以 float32 存储。
    c.insert(row(
        InputValues::Bool(vec![true]),
        InputValues::Int(vec![-128]),
        InputValues::Float(vec![0.1]),
    ))
    .unwrap();
    let got = c.get(&[Pk::Int64(1)], &["f".into()]).unwrap();
    assert_eq!(got.rows.fields[0].1, ColumnData::Float(vec![0.1f32]));
}

#[test]
fn row_major_check_order_matches_3x() {
    // 3.x 逐行校验，每行内依次查主键、向量、schema 顺序的字段、多余字段。
    let schema = CollectionSchema {
        fields: vec![
            pk_field("id", false),
            vec_field("v", 2),
            FieldSchema::new("i8", DataType::Int8),
            FieldSchema {
                max_length: Some(5),
                ..FieldSchema::new("t", DataType::VarChar)
            },
        ],
        enable_dynamic_field: false,
        properties: Vec::new(),
    };
    let mut c = Collection::new("o", schema).unwrap();
    // 第 0 行只有 t 超长，第 1 行在更靠前的 i8 上越界：报第 0 行的错误。
    let b = batch(vec![
        ("id", InputValues::Int(vec![1, 2])),
        ("v", vectors(&[[1.0, 0.0], [0.0, 1.0]])),
        ("i8", InputValues::Int(vec![1, 300])),
        ("t", strs(&["toolong", "a"])),
    ]);
    assert_eq!(
        insert_err(&mut c, b),
        "VARCHAR field 't' value length 7 exceeds max_length=5"
    );
    // 同时缺主键与向量：先报主键；同时缺向量与 t：先报向量。
    assert_eq!(
        insert_err(&mut c, batch(vec![("i8", InputValues::Int(vec![1]))])),
        "primary key 'id' missing or None"
    );
    assert_eq!(
        insert_err(
            &mut c,
            batch(vec![
                ("id", InputValues::Int(vec![1])),
                ("i8", InputValues::Int(vec![1]))
            ])
        ),
        "vector field 'v' missing or None"
    );
}

#[test]
fn errors_new_in_this_build_come_after_3x_checks_of_the_same_row() {
    // 第 0 行既有 3.x 也会拒绝的 VARCHAR 超长，又有本实现才拒绝的非对象 $meta：报前者。
    let mut c = basic(true, false);
    let mut b = rows(&[1], &[[1.0, 0.0]], &["toolong"]);
    b.columns
        .push(("$meta".into(), InputValues::Json(vec![b"[1]".to_vec()])));
    assert_eq!(
        insert_err(&mut c, b),
        "VARCHAR field 't' value length 7 exceeds max_length=5"
    );
}

#[test]
fn decode_level_checks_match_3x() {
    let mut c = basic(false, false);
    let bad_vec = InsertBatch {
        num_rows: 2,
        columns: vec![
            ("id".into(), InputValues::Int(vec![1, 2])),
            (
                "v".into(),
                InputValues::FloatVector {
                    dim: 2,
                    values: vec![1.0; 5],
                },
            ),
            ("t".into(), strs(&["a", "b"])),
        ],
    };
    assert_eq!(
        insert_err(&mut c, bad_vec),
        "FieldData 'v' float_vector data has 5 elements, expected num_rows(2) * dim(2) = 4"
    );
    // num_rows 为 0 时不看列内容（3.x 相同）。
    let empty = InsertBatch {
        num_rows: 0,
        columns: vec![("id".into(), InputValues::Int(vec![1]))],
    };
    assert!(c.insert(empty).unwrap().is_empty());
    assert_eq!(c.count(), 0);
}

#[test]
fn integers_into_float_fields_must_be_exact() {
    // 3.x 由 pyarrow 转换：不能精确表示时报 ArrowInvalid（code 1）。经 gRPC 发送 type=Int64 的 FieldData
    // 写入 FLOAT 字段实测，reason 与此相同；pymilvus 会在客户端先转成 float，走不到这里。
    let schema = CollectionSchema {
        fields: vec![
            pk_field("id", false),
            vec_field("v", 2),
            FieldSchema::new("f", DataType::Float),
            FieldSchema::new("d", DataType::Double),
        ],
        enable_dynamic_field: false,
        properties: Vec::new(),
    };
    let mut c = Collection::new("f", schema).unwrap();
    let row = |id: i64, f: i64, d: i64| {
        batch(vec![
            ("id", InputValues::Int(vec![id])),
            ("v", vectors(&[[1.0, 0.0]])),
            ("f", InputValues::Int(vec![f])),
            ("d", InputValues::Int(vec![d])),
        ])
    };
    let e = c.insert(row(1, 16_777_217, 1)).unwrap_err();
    assert!(matches!(e, EngineError::InvalidArgument(_)));
    assert_eq!(
        e.to_string(),
        "Integer value 16777217 is outside of the range exactly representable by a IEEE 754 single precision value"
    );
    assert_eq!(
        insert_err(&mut c, row(1, 1, 9_007_199_254_740_993)),
        "Integer value 9007199254740993 is outside of the range exactly representable by a IEEE 754 double precision value"
    );
    c.insert(row(1, 16_777_216, 9_007_199_254_740_992)).unwrap();
    assert_eq!(c.count(), 1);
}

#[test]
fn non_json_values_for_json_fields_are_rejected() {
    // 0004「决策」：3.x 接受并存下读取时会出错的值，本实现报类型不符。
    let schema = CollectionSchema {
        fields: vec![
            pk_field("id", false),
            vec_field("v", 2),
            FieldSchema::new("j", DataType::Json),
        ],
        enable_dynamic_field: false,
        properties: Vec::new(),
    };
    let mut c = Collection::new("j", schema).unwrap();
    let b = batch(vec![
        ("id", InputValues::Int(vec![1])),
        ("v", vectors(&[[1.0, 0.0]])),
        ("j", strs(&["hi"])),
    ]);
    assert_eq!(
        insert_err(&mut c, b),
        "field 'j' value 'hi' does not match dtype DataType.JSON"
    );
}

#[test]
fn no_query_vectors_returns_before_any_check() {
    // 3.x：nq 为 0 时在 load 状态与参数检查之前返回空结果。
    let mut c = basic(false, false);
    flat_index(&mut c, "L2");
    c.release();
    let r = c
        .search(
            SearchRequest {
                anns_field: Some("nope".into()),
                vectors: QueryVectors::Dense(vec![]),
                metric_type: Some("bad".into()),
                limit: 1,
                offset: 0,
                output_fields: vec![],
            },
            &compute(),
        )
        .unwrap();
    assert!(r.hits.is_empty());
}

#[test]
fn dynamic_fields_merge_in_column_order() {
    let mut c = basic(true, false);
    let mut b = rows(&[1, 2], &[[1.0, 0.0], [0.0, 1.0]], &["a", "b"]);
    b.columns.push((
        "$meta".into(),
        InputValues::Json(vec![br#"{"k":1,"z":"x"}"#.to_vec(), br#"{}"#.to_vec()]),
    ));
    b.columns.push(("k".into(), InputValues::Int(vec![7, 8])));
    c.insert(b).unwrap();
    let got = c.get(&[Pk::Int64(1), Pk::Int64(2)], &[]).unwrap();
    let meta = got.rows.meta.unwrap();
    assert_eq!(
        serde_json::Value::Object(meta[0].clone()),
        json!({"k": 7, "z": "x"})
    );
    assert_eq!(serde_json::Value::Object(meta[1].clone()), json!({"k": 8}));
    // 显式请求的动态 key 只保留该 key；不存在的 key 不报错。
    let got = c
        .get(&[Pk::Int64(1)], &["z".into(), "nope".into()])
        .unwrap();
    assert!(got.rows.fields.is_empty());
    assert_eq!(
        serde_json::Value::Object(got.rows.meta.unwrap()[0].clone()),
        json!({"z": "x"})
    );
}

#[test]
fn malformed_json_and_meta_are_rejected() {
    // 0004「决策」：3.x 静默存下、丢弃或覆盖，本实现明确报错。
    let mut c = basic(true, false);
    let with_meta = |meta: &[u8]| {
        let mut b = rows(&[1], &[[1.0, 0.0]], &["a"]);
        b.columns
            .push(("$meta".into(), InputValues::Json(vec![meta.to_vec()])));
        b
    };
    assert!(insert_err(&mut c, with_meta(b"[1,2]")).contains("must be a JSON object"));
    assert!(insert_err(&mut c, with_meta(b"not json")).contains("not valid JSON"));
    assert!(
        insert_err(&mut c, with_meta(br#"{"id":999}"#))
            .contains("conflicts with schema field 'id'")
    );
    assert_eq!(c.count(), 0);

    let schema = CollectionSchema {
        fields: vec![
            pk_field("id", false),
            vec_field("v", 2),
            FieldSchema::new("j", DataType::Json),
        ],
        enable_dynamic_field: false,
        properties: Vec::new(),
    };
    let mut j = Collection::new("j", schema).unwrap();
    let json_row = |text: &[u8]| {
        batch(vec![
            ("id", InputValues::Int(vec![1])),
            ("v", vectors(&[[1.0, 0.0]])),
            ("j", InputValues::Json(vec![text.to_vec()])),
        ])
    };
    assert!(insert_err(&mut j, json_row(b"{bad")).contains("not valid JSON"));
    assert_eq!(
        insert_err(&mut j, json_row(b"null")),
        "field 'j' is None but not nullable"
    );
    j.insert(json_row(br#"{"b":1,"a":[true]}"#)).unwrap();
    let got = j.get(&[Pk::Int64(1)], &["j".into()]).unwrap();
    assert_eq!(
        got.rows.fields[0].1,
        ColumnData::Json(vec![br#"{"b":1,"a":[true]}"#.to_vec()])
    );
}

#[test]
fn search_scores_follow_3x_conventions() {
    let mut c = basic(false, false);
    c.insert(rows(
        &[1, 2, 3],
        &[[1.0, 0.0], [0.0, 2.0], [3.0, 4.0]],
        &["a", "b", "c"],
    ))
    .unwrap();
    // 没有索引时默认 COSINE，返回相似度。
    let (ids, scores) = search(&c, [1.0, 0.0], 3, None);
    assert_eq!(ids, [1, 3, 2]);
    assert!(
        (scores[0] - 1.0).abs() < 1e-6 && (scores[1] - 0.6).abs() < 1e-6 && scores[2].abs() < 1e-6
    );
    // L2 返回平方距离，升序。
    let (ids, scores) = search(&c, [0.0, 0.0], 3, Some("L2"));
    assert_eq!((ids, scores), (vec![1, 2, 3], vec![1.0, 4.0, 25.0]));
    // IP 返回点积，降序。
    let (ids, scores) = search(&c, [1.0, 1.0], 2, Some("IP"));
    assert_eq!((ids, scores), (vec![3, 2], vec![7.0, 2.0]));
}

#[test]
fn index_metric_is_the_default_and_request_metric_wins() {
    let mut c = basic(false, false);
    c.insert(rows(&[1, 2], &[[1.0, 0.0], [3.0, 0.0]], &["a", "b"]))
        .unwrap();
    flat_index(&mut c, "L2");
    assert_eq!(c.load_state(), LoadState::Loaded);
    assert_eq!(
        search(&c, [0.0, 0.0], 2, None),
        (vec![1, 2], vec![1.0, 9.0])
    );
    // 0004「决策」：与索引度量不一致时按请求的度量计算。
    assert_eq!(
        search(&c, [1.0, 0.0], 2, Some("IP")),
        (vec![2, 1], vec![3.0, 1.0])
    );
}

#[test]
fn limit_offset_and_multiple_queries() {
    let mut c = basic(false, false);
    c.insert(rows(
        &[1, 2, 3, 4],
        &[[1.0, 0.0], [2.0, 0.0], [3.0, 0.0], [4.0, 0.0]],
        &["a", "b", "c", "d"],
    ))
    .unwrap();
    let req = |offset, limit| SearchRequest {
        anns_field: Some("v".into()),
        vectors: QueryVectors::Dense(vec![vec![0.0, 0.0], vec![5.0, 0.0]]),
        metric_type: Some("L2".into()),
        limit,
        offset,
        output_fields: vec![],
    };
    let r = c.search(req(1, 2), &compute()).unwrap();
    assert_eq!(ints(&r.hits[0].rows.pks), [2, 3]);
    assert_eq!(ints(&r.hits[1].rows.pks), [3, 2]);
    assert_eq!(
        ints(&c.search(req(3, 5), &compute()).unwrap().hits[0].rows.pks),
        [4]
    );
    assert!(
        c.search(req(10, 5), &compute()).unwrap().hits[0]
            .rows
            .is_empty()
    );
    assert!(
        c.search(req(0, 0), &compute()).unwrap().hits[0]
            .rows
            .is_empty()
    );
}

#[test]
fn search_default_output_includes_vector_and_meta_but_not_pk_field() {
    let mut c = basic(true, false);
    let mut b = rows(&[1], &[[1.0, 0.0]], &["a"]);
    b.columns.push(("k".into(), InputValues::Int(vec![1])));
    c.insert(b).unwrap();
    let r = c
        .search(
            SearchRequest {
                anns_field: None,
                vectors: QueryVectors::Dense(vec![vec![1.0, 0.0]]),
                metric_type: None,
                limit: 1,
                offset: 0,
                output_fields: vec![],
            },
            &compute(),
        )
        .unwrap();
    let names: Vec<&str> = r.hits[0]
        .rows
        .fields
        .iter()
        .map(|(n, _)| n.as_str())
        .collect();
    assert_eq!(names, ["v", "t"]);
    assert_eq!(ints(&r.hits[0].rows.pks), [1]);
    assert_eq!(
        serde_json::Value::Object(r.hits[0].rows.meta.clone().unwrap()[0].clone()),
        json!({"k": 1})
    );
}

#[test]
fn search_argument_errors() {
    let mut c = basic(false, false);
    let req = |anns: Option<&str>, metric: Option<&str>, q: Vec<f32>| SearchRequest {
        anns_field: anns.map(str::to_owned),
        vectors: QueryVectors::Dense(vec![q]),
        metric_type: metric.map(str::to_owned),
        limit: 1,
        offset: 0,
        output_fields: vec![],
    };
    let err = |r| c.search(r, &compute()).unwrap_err();
    // 0004「决策」：维度不符与非法度量在空 collection 上同样报错。
    let e = err(req(None, None, vec![1.0, 0.0, 0.0]));
    assert!(matches!(e, EngineError::InvalidArgument(_)));
    assert_eq!(
        e.to_string(),
        "query vector has dim 3, but field 'v' expects dim 2"
    );
    assert_eq!(
        err(req(None, Some("l2"), vec![1.0, 0.0])).to_string(),
        "unsupported metric_type 'l2'; expected one of ('COSINE', 'L2', 'IP')"
    );
    assert_eq!(
        err(req(Some("nope"), None, vec![1.0, 0.0])).to_string(),
        "anns_field 'nope' not found in schema"
    );
    assert_eq!(
        err(req(Some("t"), None, vec![1.0, 0.0])).to_string(),
        "anns_field 't' is not a vector field (dtype=VARCHAR)"
    );
    c.insert(rows(&[1], &[[1.0, 0.0]], &["a"])).unwrap();
}

#[test]
fn load_state_semantics_match_3x() {
    let mut c = basic(false, false);
    c.insert(rows(&[1], &[[1.0, 0.0]], &["a"])).unwrap();
    // 没有索引时 release 不生效。
    c.release();
    assert_eq!(c.load_state(), LoadState::Loaded);
    search(&c, [1.0, 0.0], 1, None);

    flat_index(&mut c, "COSINE");
    c.release();
    assert_eq!(c.load_state(), LoadState::Released);
    let msg = "Collection 'c' is in state 'released'; call load() before search/get/query";
    assert_eq!(c.get(&[Pk::Int64(1)], &[]).unwrap_err().to_string(), msg);
    let e = c
        .search(
            SearchRequest {
                anns_field: None,
                vectors: QueryVectors::Dense(vec![vec![1.0, 0.0]]),
                metric_type: None,
                limit: 1,
                offset: 0,
                output_fields: vec![],
            },
            &compute(),
        )
        .unwrap_err();
    assert_eq!(e.to_string(), msg);
    // count 与 insert 不受 load 状态影响。
    c.insert(rows(&[2], &[[0.0, 1.0]], &["b"])).unwrap();
    assert_eq!(c.count(), 2);
    c.load().unwrap();
    assert_eq!(c.get(&[Pk::Int64(2)], &[]).unwrap().rows.len(), 1);
}

#[test]
fn create_index_errors_match_3x() {
    let mut c = basic(false, false);
    flat_index(&mut c, "L2");
    let again = c.create_index(
        "v",
        IndexRequest {
            index_type: "FLAT".into(),
            metric_type: Some("IP".into()),
            ..Default::default()
        },
    );
    assert_eq!(
        again.unwrap_err().to_string(),
        "index already exists for field 'v'; call drop_index first"
    );
    let unknown = c.create_index("zzz", IndexRequest::default());
    assert_eq!(
        unknown.unwrap_err().to_string(),
        "unknown field 'zzz' for create_index"
    );
    assert_eq!(c.indexes().len(), 1);
}

#[test]
fn varchar_primary_key() {
    let schema = CollectionSchema {
        fields: vec![
            FieldSchema {
                is_primary: true,
                ..FieldSchema::new("pk", DataType::VarChar)
            },
            vec_field("v", 2),
        ],
        enable_dynamic_field: false,
        properties: Vec::new(),
    };
    let mut c = Collection::new("s", schema).unwrap();
    let ids = c
        .insert(batch(vec![
            ("pk", strs(&["a", "b"])),
            ("v", vectors(&[[1.0, 0.0], [0.0, 1.0]])),
        ]))
        .unwrap();
    assert_eq!(ids, [Pk::VarChar("a".into()), Pk::VarChar("b".into())]);
    let got = c.get(&[Pk::VarChar("b".into())], &[]).unwrap();
    assert_eq!(got.rows.pks, [Pk::VarChar("b".into())]);
    assert!(c.get(&[Pk::Int64(1)], &[]).unwrap().rows.is_empty());
}

fn nan_rows() -> InsertBatch {
    rows(
        &[1, 2, 3, 4],
        &[[3.0, 0.0], [f32::NAN, 0.0], [1.0, 0.0], [2.0, 0.0]],
        &["a", "b", "c", "d"],
    )
}

#[test]
fn nan_scores_rank_last_without_panicking() {
    // 3.x：有限值照常排序，NaN 行排在最后。
    let mut c = basic(false, false);
    c.insert(nan_rows()).unwrap();
    for (metric, q, expected) in [
        ("L2", [0.0, 0.0], [3, 4, 1, 2]),
        ("IP", [1.0, 0.0], [1, 4, 3, 2]),
        ("COSINE", [1.0, 0.0], [1, 3, 4, 2]),
    ] {
        let (ids, _) = search(&c, q, 10, Some(metric));
        assert_eq!(ids, expected, "{metric}");
    }
    // 查询向量含 NaN / inf 时全部分数为 NaN，不 panic。
    for q in [[f32::NAN, 0.0], [f32::INFINITY, f32::NEG_INFINITY]] {
        let (ids, _) = search(&c, q, 10, Some("L2"));
        assert_eq!(ids.len(), 4);
    }
}

#[test]
fn dynamic_floats_round_trip_exactly() {
    let mut c = basic(true, false);
    let mut b = rows(&[1], &[[1.0, 0.0]], &["a"]);
    b.columns.push((
        "$meta".into(),
        InputValues::Json(vec![
            br#"{"z":0.9556595384052861,"a":493.21226683922947}"#.to_vec(),
        ]),
    ));
    c.insert(b).unwrap();
    let meta = c.get(&[Pk::Int64(1)], &[]).unwrap().rows.meta.unwrap();
    // 与 3.x 的 json.dumps(sort_keys=True) 相同：落盘时显式排序，浮点逐位保留。
    assert_eq!(
        milvus_lite_core::pyjson::dumps(&serde_json::Value::Object(meta[0].clone())),
        r#"{"a": 493.21226683922947, "z": 0.9556595384052861}"#
    );
}

#[test]
fn json_nesting_depth_is_limited() {
    // 0004「决策」：3.x 接受约 1000 层以内的嵌套；本实现上限为 127 层，更深明确报错。
    let schema = CollectionSchema {
        fields: vec![
            pk_field("id", false),
            vec_field("v", 2),
            FieldSchema::new("j", DataType::Json),
        ],
        enable_dynamic_field: false,
        properties: Vec::new(),
    };
    let mut c = Collection::new("j", schema).unwrap();
    let nested = |depth: usize| format!("{}{}", "[".repeat(depth), "]".repeat(depth)).into_bytes();
    let row = |id: i64, text: Vec<u8>| {
        batch(vec![
            ("id", InputValues::Int(vec![id])),
            ("v", vectors(&[[1.0, 0.0]])),
            ("j", InputValues::Json(vec![text])),
        ])
    };
    c.insert(row(1, nested(127))).unwrap();
    let e = c.insert(row(2, nested(128))).unwrap_err();
    assert!(matches!(e, EngineError::SchemaValidation(_)), "{e}");
    assert!(e.to_string().contains("recursion limit exceeded"), "{e}");
    assert_eq!(c.count(), 1);
}

#[test]
fn more_scalar_type_checks_match_3x() {
    let schema = CollectionSchema {
        fields: vec![
            pk_field("id", false),
            vec_field("v", 2),
            FieldSchema::new("i16", DataType::Int16),
            FieldSchema::new("i32", DataType::Int32),
            FieldSchema::new("d", DataType::Double),
        ],
        enable_dynamic_field: false,
        properties: Vec::new(),
    };
    let mut c = Collection::new("s", schema).unwrap();
    let row = |i16: i64, i32: i64, d: InputValues| {
        batch(vec![
            ("id", InputValues::Int(vec![1])),
            ("v", vectors(&[[1.0, 0.0]])),
            ("i16", InputValues::Int(vec![i16])),
            ("i32", InputValues::Int(vec![i32])),
            ("d", d),
        ])
    };
    assert_eq!(
        insert_err(&mut c, row(32_768, 0, InputValues::Float(vec![1.0]))),
        "field 'i16' value 32768 does not match dtype DataType.INT16"
    );
    assert_eq!(
        insert_err(
            &mut c,
            row(0, -2_147_483_649, InputValues::Float(vec![1.0]))
        ),
        "field 'i32' value -2147483649 does not match dtype DataType.INT32"
    );
    assert_eq!(
        insert_err(&mut c, row(0, 0, InputValues::Bool(vec![true]))),
        "field 'd' value True does not match dtype DataType.DOUBLE"
    );
    assert_eq!(
        insert_err(&mut c, row(0, 0, strs(&["x"]))),
        "field 'd' value 'x' does not match dtype DataType.DOUBLE"
    );
    c.insert(row(-32_768, 2_147_483_647, InputValues::Int(vec![3])))
        .unwrap();
    let got = c.get(&[Pk::Int64(1)], &["d".into()]).unwrap();
    assert_eq!(got.rows.fields[0].1, ColumnData::Double(vec![3.0]));
}

#[test]
fn empty_meta_in_static_schema_is_accepted() {
    // 3.x：$meta 为 {} 时没有多余字段，写入成功。
    let mut c = basic(false, false);
    let mut b = rows(&[1], &[[1.0, 0.0]], &["a"]);
    b.columns
        .push(("$meta".into(), InputValues::Json(vec![b"{}".to_vec()])));
    c.insert(b).unwrap();
    let got = c.get(&[Pk::Int64(1)], &[]).unwrap();
    assert!(got.rows.meta.is_none());
}

#[test]
fn search_on_empty_collection_returns_empty_hits() {
    let c = basic(false, false);
    let r = c
        .search(
            SearchRequest {
                anns_field: None,
                vectors: QueryVectors::Dense(vec![vec![1.0, 0.0], vec![0.0, 1.0]]),
                metric_type: None,
                limit: 3,
                offset: 0,
                output_fields: vec![],
            },
            &compute(),
        )
        .unwrap();
    assert_eq!(r.hits.len(), 2);
    assert!(
        r.hits
            .iter()
            .all(|h| h.rows.is_empty() && h.scores.is_empty())
    );
}

fn search_with(c: &Collection, anns: Option<&str>, vectors: QueryVectors) -> EngineError {
    c.search(
        SearchRequest {
            anns_field: anns.map(str::to_owned),
            vectors,
            metric_type: None,
            limit: 1,
            offset: 0,
            output_fields: vec![],
        },
        &compute(),
    )
    .unwrap_err()
}

#[test]
fn sparse_and_text_queries_fail_where_3x_fails() {
    let mut c = basic(false, false);
    let text = search_with(&c, None, QueryVectors::Text);
    assert!(matches!(text, EngineError::SchemaValidation(_)));
    assert_eq!(
        text.to_string(),
        "Text query on field 'v' requires a TEXT_EMBEDDING function targeting that field"
    );
    let sparse = search_with(&c, None, QueryVectors::Sparse);
    assert!(matches!(sparse, EngineError::InvalidArgument(_)));
    assert_eq!(
        sparse.to_string(),
        "float() argument must be a string or a real number, not 'dict'"
    );
    // 3.x 先解析 anns_field，再看查询向量的种类；更早的是 load 状态。
    assert_eq!(
        search_with(&c, Some("nope"), QueryVectors::Text).to_string(),
        "anns_field 'nope' not found in schema"
    );
    flat_index(&mut c, "L2");
    c.release();
    assert!(matches!(
        search_with(&c, Some("nope"), QueryVectors::Sparse),
        EngineError::CollectionNotLoaded(_)
    ));
}

#[test]
fn describe_indexes_matches_3x() {
    let mut c = basic(false, false);
    let e = c.describe_indexes(Some("v")).unwrap_err();
    assert!(matches!(e, EngineError::IndexNotFound(_)));
    assert_eq!(e.to_string(), "no index on collection 'c'");
    assert!(c.describe_indexes(None).is_err());
    flat_index(&mut c, "IP");
    assert_eq!(c.describe_indexes(None).unwrap().len(), 1);
    assert_eq!(c.describe_indexes(Some("v")).unwrap()[0].metric_type, "IP");
    assert_eq!(
        c.describe_indexes(Some("t")).unwrap_err().to_string(),
        "no index on field 't' of collection 'c'"
    );
}

#[test]
fn get_literals_skips_literals_of_the_wrong_type() {
    let mut c = basic(false, false);
    c.insert(rows(&[1, 2], &[[1.0, 0.0], [0.0, 1.0]], &["a", "b"]))
        .unwrap();
    let lits = [
        PkLiteral::Str("1".into()),
        PkLiteral::Int(2),
        PkLiteral::IntOutOfRange,
        PkLiteral::Int(2),
    ];
    assert_eq!(ints(&c.get_literals(&lits, &[]).unwrap().rows.pks), [2, 2]);

    let schema = CollectionSchema {
        fields: vec![
            FieldSchema {
                is_primary: true,
                ..FieldSchema::new("pk", DataType::VarChar)
            },
            vec_field("v", 2),
        ],
        ..CollectionSchema::default()
    };
    let mut s = Collection::new("s", schema).unwrap();
    s.insert(batch(vec![
        ("pk", strs(&["1", "a"])),
        ("v", vectors(&[[1.0, 0.0], [0.0, 1.0]])),
    ]))
    .unwrap();
    let got = s
        .get_literals(&[PkLiteral::Int(1), PkLiteral::Str("a".into())], &[])
        .unwrap();
    assert_eq!(got.rows.pks, [Pk::VarChar("a".into())]);
}

#[test]
fn json_fields_accept_top_level_scalars_and_strings() {
    // 0004「决策」：3.x 对顶层数字与布尔报 pyarrow 的错，字符串值读回时丢了引号；本实现都按 JSON 原样保存。
    let schema = CollectionSchema {
        fields: vec![
            pk_field("id", false),
            vec_field("v", 2),
            FieldSchema::new("j", DataType::Json),
        ],
        ..CollectionSchema::default()
    };
    let mut c = Collection::new("j", schema).unwrap();
    let values: [&[u8]; 4] = [b"1", b"1.5", b"true", b"\"abc\""];
    c.insert(batch(vec![
        ("id", InputValues::Int(vec![1, 2, 3, 4])),
        ("v", vectors(&[[1.0, 0.0]; 4])),
        (
            "j",
            InputValues::Json(values.iter().map(|v| v.to_vec()).collect()),
        ),
    ]))
    .unwrap();
    let got = c
        .get(&[Pk::Int64(4), Pk::Int64(1)], &["j".to_owned()])
        .unwrap();
    assert_eq!(
        got.rows.fields[0].1,
        ColumnData::Json(vec![b"\"abc\"".to_vec(), b"1".to_vec()])
    );
}

#[test]
fn wide_integers_in_dynamic_fields_round_trip() {
    // M2 使用 arbitrary_precision，与 3.x 一样保留超出 64 位的整数字面量。
    let mut c = basic(true, false);
    let meta = |text: &str| {
        batch(vec![
            ("id", InputValues::Int(vec![1])),
            ("v", vectors(&[[1.0, 0.0]])),
            ("t", strs(&["a"])),
            ("$meta", InputValues::Json(vec![text.as_bytes().to_vec()])),
        ])
    };
    for text in [
        r#"{"q": 123456789012345678901234567890}"#,
        r#"{"q": [1, -9223372036854775809]}"#,
        r#"{"q": {"r": 18446744073709551616}}"#,
    ] {
        c.insert(meta(text)).unwrap();
        let got = c.get(&[Pk::Int64(1)], &[]).unwrap();
        let actual = milvus_lite_core::pyjson::dumps(&serde_json::Value::Object(
            got.rows.meta.unwrap()[0].clone(),
        ));
        let expected = milvus_lite_core::pyjson::dumps(&serde_json::from_str(text).unwrap());
        assert_eq!(actual, expected, "{text}");
    }
    // 64 位以内的整数、浮点数、字符串中的长数字都照常保存。
    c.insert(meta(
        r#"{"a": 18446744073709551615, "b": -9223372036854775808, "c": 1.5e300, "d": "123456789012345678901234567890", "e": "x\"1e999"}"#,
    ))
    .unwrap();
    let wide = batch(vec![
        ("id", InputValues::Int(vec![2])),
        ("v", vectors(&[[1.0, 0.0]])),
        ("t", strs(&["a"])),
        (
            "extra",
            InputValues::Json(vec![b"99999999999999999999".to_vec()]),
        ),
    ]);
    c.insert(wide).unwrap();
    let got = c.get(&[Pk::Int64(2)], &[]).unwrap();
    assert_eq!(
        milvus_lite_core::pyjson::dumps(&serde_json::Value::Object(
            got.rows.meta.unwrap()[0].clone(),
        )),
        r#"{"extra": 99999999999999999999}"#
    );
}

#[test]
fn missing_key_columns_are_reported_before_allocating_per_row_state() {
    let mut c = basic(true, false);
    // num_rows 由请求给出；没有任何列时不应先按它分配内存。
    let empty = InsertBatch {
        num_rows: usize::MAX,
        columns: vec![],
    };
    assert_eq!(
        insert_err(&mut c, empty),
        "primary key 'id' missing or None"
    );
    let mut auto = basic(true, true);
    let empty = InsertBatch {
        num_rows: usize::MAX,
        columns: vec![],
    };
    assert_eq!(
        insert_err(&mut auto, empty),
        "vector field 'v' missing or None"
    );
}

#[test]
fn scalar_index_with_empty_metric_is_none() {
    // 3.x：标量索引的 metric_type 为空串时按 `or "NONE"` 处理。
    let mut c = basic(false, false);
    c.create_index(
        "t",
        IndexRequest {
            index_type: "INVERTED".to_owned(),
            metric_type: Some(String::new()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(c.index("t").unwrap().metric_type, "NONE");
}
