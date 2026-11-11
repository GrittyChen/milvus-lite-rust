//! 分组检索（0024）：候选窗口、分组、offset 切片与空值组，期望值来自真实 3.x 的实测输出。
#![allow(clippy::unwrap_used)]
use milvus_lite_core::data::Validity;
use milvus_lite_core::{
    CollectionSchema, ColumnData, Compute, DataDir, DataType, Db, EngineError, FieldSchema,
    GroupBy, InputValues, InsertBatch, Pk, QueryVectors, ReadSnapshot, SearchRange, SearchRequest,
    SearchResult,
};

fn open(root: &std::path::Path) -> std::sync::Arc<Db> {
    let pool = Compute::new(std::num::NonZeroUsize::new(2).unwrap()).unwrap();
    Db::new(DataDir::open(root).unwrap(), pool).unwrap()
}

fn schema(pk: &str) -> CollectionSchema {
    let mut id = FieldSchema::new(pk, DataType::Int64);
    id.is_primary = true;
    let mut v = FieldSchema::new("v", DataType::FloatVector);
    v.dim = Some(2);
    let mut n = FieldSchema::new("n", DataType::Int64);
    n.nullable = true;
    CollectionSchema {
        fields: vec![
            id,
            v,
            FieldSchema::new("grp", DataType::VarChar),
            n,
            FieldSchema::new("flag", DataType::Bool),
            FieldSchema::new("f", DataType::Float),
        ],
        enable_dynamic_field: false,
        properties: vec![],
    }
}

/// 按距离排名依次排列的组：第 i 行到查询点 [0, 0] 的 L2 距离随 i 严格递增。
fn insert_layout(db: &Db, pk: &str, layout: &[(&str, usize)]) {
    let groups: Vec<&str> = layout
        .iter()
        .flat_map(|(g, n)| std::iter::repeat_n(*g, *n))
        .collect();
    let rows = groups.len();
    db.insert(
        "",
        "c",
        InsertBatch {
            num_rows: rows,
            columns: vec![
                (pk.into(), InputValues::Int((0..rows as i64).collect())),
                (
                    "v".into(),
                    InputValues::FloatVector {
                        dim: 2,
                        values: (0..rows).flat_map(|i| [i as f32 * 0.01, 0.]).collect(),
                    },
                ),
                (
                    "grp".into(),
                    InputValues::Str(groups.iter().map(|g| (*g).to_owned()).collect()),
                ),
                (
                    "n".into(),
                    InputValues::Nullable {
                        values: Box::new(InputValues::Int(
                            (0..rows as i64).map(|i| i % 3).collect(),
                        )),
                        validity: Validity::from_bools(
                            &(0..rows).map(|i| i % 4 != 0).collect::<Vec<_>>(),
                        ),
                    },
                ),
                (
                    "flag".into(),
                    InputValues::Bool((0..rows).map(|i| i % 2 == 0).collect()),
                ),
                (
                    "f".into(),
                    InputValues::Float((0..rows).map(|i| i as f64).collect()),
                ),
            ],
        },
    )
    .unwrap();
}

fn setup(root: &std::path::Path, pk: &str, layout: &[(&str, usize)]) -> std::sync::Arc<Db> {
    let db = open(root);
    db.create_collection("", "c", schema(pk)).unwrap();
    insert_layout(&db, pk, layout);
    db
}

fn snapshot(db: &Db) -> ReadSnapshot {
    db.collection("", "c").unwrap().read().read_snapshot()
}

fn request(limit: usize, offset: usize) -> SearchRequest {
    SearchRequest {
        anns_field: None,
        vectors: QueryVectors::Dense(vec![vec![0., 0.]]),
        metric_type: Some("L2".into()),
        limit,
        offset,
        output_fields: vec!["grp".into()],
        params: Default::default(),
        negative_top_k: false,
    }
}

fn group(field: &str, size: i64, strict: bool) -> Option<GroupBy> {
    Some(GroupBy {
        field: field.into(),
        size,
        strict,
    })
}

fn run(
    c: &ReadSnapshot,
    pool: &Compute,
    req: SearchRequest,
    range: Option<SearchRange>,
    group: Option<GroupBy>,
) -> Result<SearchResult, EngineError> {
    c.search_grouped(req, pool, "", range, group)
}

/// 命中所属组的首字母按展平顺序拼接。
fn letters(result: &SearchResult, query: usize) -> String {
    let hits = &result.hits[query];
    let Some(ColumnData::VarChar(values)) = hits.group_values.as_ref().map(ColumnData::values)
    else {
        panic!("group values missing");
    };
    values.concat()
}

// 3.x 的布局：X 占满前 29 名，Y 第 30 名，Z、W、V、U、T 依次在后。
const LAYOUT: [(&str, usize); 7] = [
    ("X", 29),
    ("Y", 1),
    ("Z", 5),
    ("W", 40),
    ("V", 3),
    ("U", 30),
    ("T", 60),
];

#[test]
fn candidate_window_decides_which_groups_are_reachable() {
    let root = tempfile::tempdir().unwrap();
    let db = setup(root.path(), "id", &LAYOUT);
    let pool = db.compute();
    let c = snapshot(&db);
    let x = |n: usize| "X".repeat(n);
    // 以下全部是 3.x 在同一数据上的实测：窗口是 max(end × size × 3, end × 10)。
    let cases: [(usize, i64, String); 16] = [
        (1, 1, x(1)),
        (1, 2, x(2)),
        (1, 3, x(3)),
        (1, 10, x(10)),
        (2, 1, x(1)),
        (2, 2, x(2)),
        (2, 3, x(3)),
        (2, 10, x(10) + "Y"),
        (3, 1, "XY".into()),
        (3, 2, "XXY".into()),
        (3, 3, "XXXY".into()),
        (3, 10, x(10) + "YZZZZZ"),
        (4, 1, "XYZW".into()),
        (4, 2, "XXYZZWW".into()),
        (4, 3, "XXXYZZZWWW".into()),
        (4, 10, x(10) + "YZZZZZ" + &"W".repeat(10)),
    ];
    for (limit, size, expected) in cases {
        let result = run(&c, pool, request(limit, 0), None, group("grp", size, false)).unwrap();
        assert_eq!(letters(&result, 0), expected, "limit {limit} size {size}");
        assert_eq!(
            result.hits[0].scores.len(),
            expected.len(),
            "limit {limit} size {size}"
        );
    }
    // strict：组内条数必须正好等于 size；Y 只有一条，被丢弃后取前 3 个满额的组。
    let strict = run(&c, pool, request(3, 0), None, group("grp", 4, true)).unwrap();
    assert_eq!(letters(&strict, 0), "XXXXZZZZ");
    // offset 为正时在展平后的命中上切片，不是按组跳过。
    for (offset, size, expected) in [(1, 1, "Y"), (2, 1, "ZW"), (1, 2, "XY")] {
        let result = run(
            &c,
            pool,
            request(2, offset),
            None,
            group("grp", size, false),
        )
        .unwrap();
        assert_eq!(letters(&result, 0), expected, "offset {offset} size {size}");
    }
    // 没有分组时不产生分组值，行为与原来相同。
    let plain = run(&c, pool, request(3, 0), None, None).unwrap();
    assert!(plain.hits[0].group_values.is_none());
    db.close();
}

#[test]
fn non_positive_group_size_keeps_no_hits_and_huge_values_do_not_preallocate() {
    let root = tempfile::tempdir().unwrap();
    let db = setup(root.path(), "id", &LAYOUT);
    let pool = db.compute();
    let c = snapshot(&db);
    for size in [-1, i64::MIN] {
        let result = run(&c, pool, request(3, 0), None, group("grp", size, false)).unwrap();
        assert!(result.hits[0].scores.is_empty());
        assert_eq!(result.hits[0].group_values.as_ref().unwrap().len(), 0);
    }
    // 3.x 在 1e10 量级长时间无响应；这里候选集惰性增长，结果就是整个数据集按组展平。
    let result = run(&c, pool, request(2, 0), None, group("grp", i64::MAX, false)).unwrap();
    assert_eq!(result.hits[0].scores.len(), 29 + 1);
    assert_eq!(letters(&result, 0), "X".repeat(29) + "Y");
    db.close();
}

#[test]
fn nulls_form_one_group_and_the_value_column_keeps_validity() {
    let root = tempfile::tempdir().unwrap();
    // n 的有效位：第 0、4、8… 行为空；值为 i % 3。
    let db = setup(root.path(), "id", &[("A", 12)]);
    let pool = db.compute();
    let c = snapshot(&db);
    let result = run(&c, pool, request(10, 0), None, group("n", 1, false)).unwrap();
    let hits = &result.hits[0];
    // 第 0 行为空，排在最前；其余按首次出现：1、2、0。
    assert_eq!(hits.rows.pks[0], Pk::Int64(0));
    let column = hits.group_values.as_ref().unwrap();
    assert_eq!(column.len(), 4);
    assert!(column.is_null(0));
    assert!((1..4).all(|i| !column.is_null(i)));
    let ColumnData::Int64(values) = column.values() else {
        panic!("INT64 group values expected");
    };
    assert_eq!(values[1..], [1, 2, 0]);
    // strict 同样适用于空值组：size 2 时空值组有 3 行 → 仍只保留前 2 条，满额。
    let strict = run(&c, pool, request(10, 0), None, group("n", 2, true)).unwrap();
    assert_eq!(strict.hits[0].scores.len(), 8);
    db.close();
}

#[test]
fn group_field_types_and_validation_order() {
    let root = tempfile::tempdir().unwrap();
    let db = setup(root.path(), "id", &[("A", 4), ("B", 4)]);
    let pool = db.compute();
    let c = snapshot(&db);
    for field in ["id", "grp", "n", "flag"] {
        run(&c, pool, request(2, 0), None, group(field, 1, false)).unwrap();
    }
    let message = |field: &str| {
        run(&c, pool, request(2, 0), None, group(field, 1, false))
            .unwrap_err()
            .to_string()
    };
    assert!(message("f").contains("group_by_field 'f' has type FLOAT which is not supported"));
    assert!(message("v").contains("group_by_field 'v' has type FLOAT_VECTOR"));
    assert!(message("nosuch").contains("group_by_field 'nosuch' not found in schema"));
    // 分组字段先于 anns_field 校验。
    let mut bad_anns = request(2, 0);
    bad_anns.anns_field = Some("nosuchvec".into());
    let error = run(&c, pool, bad_anns.clone(), None, group("nosuch", 1, false)).unwrap_err();
    assert!(error.to_string().contains("group_by_field"));
    let error = run(&c, pool, bad_anns, None, group("grp", 1, false)).unwrap_err();
    assert!(error.to_string().contains("anns_field"));
    db.close();
}

#[test]
fn primary_key_of_any_name_is_a_normal_group_field() {
    // 3.x 只有主键恰好叫 id 时才分得出组；其余名字全部落进同一个空值组（0024 登记为不复制）。
    for pk in ["id", "pk"] {
        let root = tempfile::tempdir().unwrap();
        let db = setup(root.path(), pk, &[("A", 6)]);
        let pool = db.compute();
        let c = snapshot(&db);
        let result = run(&c, pool, request(3, 0), None, group(pk, 1, false)).unwrap();
        assert_eq!(
            result.hits[0].rows.pks,
            vec![Pk::Int64(0), Pk::Int64(1), Pk::Int64(2)]
        );
        let ColumnData::Int64(values) = result.hits[0].group_values.as_ref().unwrap().values()
        else {
            panic!("INT64 group values expected");
        };
        assert_eq!(values, &[0, 1, 2]);
        db.close();
    }
}

#[test]
fn range_filter_applies_before_grouping_and_queries_group_independently() {
    let root = tempfile::tempdir().unwrap();
    let db = setup(root.path(), "id", &[("A", 3), ("B", 3), ("C", 3)]);
    let pool = db.compute();
    let c = snapshot(&db);
    // L2 距离 0.0001 * i² 之内：只有前几行落在范围内，范围先截断到 end 再分组。
    let range = Some(SearchRange {
        radius: Some(0.0009),
        range_filter: Some(0.),
    });
    let result = run(&c, pool, request(5, 0), range, group("grp", 2, false)).unwrap();
    assert_eq!(letters(&result, 0), "AAB");
    // 多个查询向量各自分组。
    let mut req = request(2, 0);
    req.vectors = QueryVectors::Dense(vec![vec![0., 0.], vec![0.08, 0.]]);
    let result = run(&c, pool, req, None, group("grp", 2, false)).unwrap();
    assert_eq!(letters(&result, 0), "AABB");
    assert_eq!(letters(&result, 1), "CCBB");
    db.close();
}

#[test]
fn groups_see_deletes_and_flushed_segments() {
    let root = tempfile::tempdir().unwrap();
    let db = setup(root.path(), "id", &[("A", 2), ("B", 2), ("C", 2)]);
    db.flush_collection("", "c").unwrap();
    db.delete("", "c", &[Pk::Int64(0), Pk::Int64(1)]).unwrap();
    let pool = db.compute();
    let c = snapshot(&db);
    let result = run(&c, pool, request(3, 0), None, group("grp", 1, false)).unwrap();
    assert_eq!(letters(&result, 0), "BC");
    db.close();
}
