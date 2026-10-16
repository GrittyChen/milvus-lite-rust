//! 范围过滤先取得有限候选窗口，再比较 float64 公共距离，最后分页和物化。
#![allow(clippy::unwrap_used)]
use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, FieldSchema, InputValues, InsertBatch, Pk,
    QueryVectors, SearchRange, SearchRequest,
};

#[test]
fn range_window_float64_thresholds_parallel_queries_and_old_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let pool = Compute::new(std::num::NonZeroUsize::new(3).unwrap()).unwrap();
    let db = Db::new(DataDir::open(root.path()).unwrap(), pool).unwrap();
    let pool = db.compute();
    let mut id = FieldSchema::new("id", DataType::Int64);
    id.is_primary = true;
    let mut v = FieldSchema::new("v", DataType::FloatVector);
    v.dim = Some(2);
    db.create_collection(
        "",
        "c",
        CollectionSchema {
            fields: vec![id, v],
            enable_dynamic_field: false,
            properties: vec![],
        },
    )
    .unwrap();
    for start in [1, 9, 17, 25] {
        db.insert(
            "",
            "c",
            InsertBatch {
                num_rows: 8,
                columns: vec![
                    ("id".into(), InputValues::Int((start..start + 8).collect())),
                    (
                        "v".into(),
                        InputValues::FloatVector {
                            dim: 2,
                            values: (start..start + 8)
                                .flat_map(|i| if i == 2 { [0.5, 1.] } else { [i as f32, 0.] })
                                .collect(),
                        },
                    ),
                ],
            },
        )
        .unwrap();
        db.flush_collection("", "c").unwrap();
    }
    let snapshot = db.collection("", "c").unwrap().read().read_snapshot();
    let request = |limit, offset| SearchRequest {
        anns_field: None,
        vectors: QueryVectors::Dense(vec![vec![0., 0.]; 129]),
        metric_type: Some("L2".into()),
        limit,
        offset,
        output_fields: vec![],
    };
    let range = Some(SearchRange {
        radius: Some(10000.),
        range_filter: Some(36.),
    });
    for (limit, offset, ids) in [
        (1, 0, vec![]),
        (2, 0, vec![Pk::Int64(6), Pk::Int64(7)]),
        (1, 1, vec![Pk::Int64(7)]),
    ] {
        let result = snapshot
            .search_filtered_with_range(request(limit, offset), pool, "", range)
            .unwrap();
        assert_eq!(result.hits.len(), 129);
        for hits in result.hits {
            assert_eq!(hits.rows.pks, ids);
        }
    }
    for (radius, ids) in [
        (1.25000002, vec![Pk::Int64(1)]),
        (1.25000005, vec![Pk::Int64(1), Pk::Int64(2)]),
    ] {
        let range = Some(SearchRange {
            radius: Some(radius),
            range_filter: None,
        });
        let result = snapshot
            .search_filtered_with_range(request(10, 0), pool, "", range)
            .unwrap();
        assert_eq!(result.hits[0].rows.pks, ids);
    }
    let ordinary = snapshot.search(request(2, 0), pool).unwrap();
    assert_eq!(
        snapshot
            .search_filtered_with_range(request(2, 0), pool, "", Some(SearchRange::default()))
            .unwrap(),
        ordinary
    );
    db.delete("", "c", &[Pk::Int64(6)]).unwrap();
    db.flush_collection("", "c").unwrap();
    assert_eq!(
        snapshot
            .search_filtered_with_range(request(2, 0), pool, "", range)
            .unwrap()
            .hits[0]
            .rows
            .pks,
        vec![Pk::Int64(6), Pk::Int64(7)]
    );
    assert_eq!(
        db.collection("", "c")
            .unwrap()
            .read()
            .read_snapshot()
            .search_filtered_with_range(request(2, 0), pool, "", range)
            .unwrap()
            .hits[0]
            .rows
            .pks,
        vec![Pk::Int64(7), Pk::Int64(8)]
    );
    db.close();
}
