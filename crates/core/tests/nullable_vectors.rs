//! 0038：nullable 稠密向量的写入、存储、检索排除与 upsert 继承。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use milvus_lite_core::data::Validity;
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

fn vector(name: &str, nullable: bool) -> FieldSchema {
    FieldSchema {
        dim: Some(2),
        nullable,
        ..FieldSchema::new(name, DataType::FloatVector)
    }
}

/// `vectors` 中每一项是一个向量字段及其是否 nullable；`key` 时加 INT64 partition key。
fn schema(vectors: &[(&str, bool)], key: bool) -> CollectionSchema {
    let mut fields = vec![FieldSchema {
        is_primary: true,
        ..FieldSchema::new("id", DataType::Int64)
    }];
    fields.extend(
        vectors
            .iter()
            .map(|(name, nullable)| vector(name, *nullable)),
    );
    if key {
        fields.push(FieldSchema {
            is_partition_key: true,
            ..FieldSchema::new("k", DataType::Int64)
        });
    }
    fields.push(FieldSchema {
        nullable: true,
        ..FieldSchema::new("f", DataType::Int64)
    });
    CollectionSchema {
        functions: Vec::new(),
        fields,
        enable_dynamic_field: false,
        properties: Vec::new(),
    }
}

/// id 为偶数的行是空向量；有效行是 `[1, id]`，空行的值槽故意填非零，写入时应被清零。
fn vectors(ids: &[i64]) -> InputValues {
    let valid: Vec<bool> = ids.iter().map(|id| id % 2 != 0).collect();
    InputValues::Nullable {
        values: Box::new(InputValues::FloatVector {
            dim: 2,
            values: ids.iter().flat_map(|id| [1.0, *id as f32]).collect(),
        }),
        validity: Validity::from_bools(&valid),
    }
}

fn batch(ids: &[i64], columns: Vec<(&str, InputValues)>) -> InsertBatch {
    let mut all = vec![("id".to_owned(), InputValues::Int(ids.to_vec()))];
    all.extend(
        columns
            .into_iter()
            .map(|(name, values)| (name.to_owned(), values)),
    );
    InsertBatch {
        num_rows: ids.len(),
        columns: all,
    }
}

/// 每个主键的向量：None 是空值。
fn stored(db: &Db, field: &str) -> Vec<(i64, Option<Vec<f32>>)> {
    let handle = db.collection("", "c").unwrap();
    let rows = handle
        .read()
        .read_snapshot()
        .query("", &[field.into()], None, 0)
        .unwrap()
        .rows;
    let column = &rows
        .fields
        .iter()
        .find(|(name, _)| name == field)
        .unwrap()
        .1;
    let ColumnData::FloatVector { dim, values } = column.values() else {
        panic!("vector column lost its type: {column:?}");
    };
    let mut out: Vec<_> = rows
        .pks
        .iter()
        .enumerate()
        .map(|(row, pk)| {
            let Pk::Int64(id) = pk else { panic!("{pk:?}") };
            let value = values[row * dim..(row + 1) * dim].to_vec();
            if column.is_null(row) {
                assert_eq!(value, [0.0, 0.0], "null slot of {id} is not zero");
            }
            (*id, (!column.is_null(row)).then_some(value))
        })
        .collect();
    out.sort_by_key(|(id, _)| *id);
    out
}

fn expected(ids: &[i64]) -> Vec<(i64, Option<Vec<f32>>)> {
    ids.iter()
        .map(|id| (*id, (id % 2 != 0).then(|| vec![1.0, *id as f32])))
        .collect()
}

/// `queries` 条查询各自的命中主键；单条、一组 BATCH（4 条）与多组分片走扫描的三条路径。
fn search(db: &Db, field: &str, metric: &str, queries: usize) -> Vec<Vec<i64>> {
    let handle = db.collection("", "c").unwrap();
    let result = handle
        .read()
        .read_snapshot()
        .search(
            SearchRequest {
                anns_field: Some(field.into()),
                vectors: QueryVectors::Dense(vec![vec![1.0, 0.5]; queries]),
                metric_type: Some(metric.into()),
                limit: 1000,
                offset: 0,
                output_fields: Vec::new(),
                params: Default::default(),
                negative_top_k: false,
            },
            db.compute(),
        )
        .unwrap();
    result
        .hits
        .into_iter()
        .map(|hits| {
            let mut ids: Vec<i64> = hits
                .rows
                .pks
                .iter()
                .map(|pk| match pk {
                    Pk::Int64(id) => *id,
                    other => panic!("{other:?}"),
                })
                .collect();
            ids.sort_unstable();
            ids
        })
        .collect()
}

/// 检索只返回有效行：三种度量、三条扫描路径都一样，任何空行出现都说明 validity 没有生效。
fn assert_search_skips_nulls(db: &Db, field: &str, ids: &[i64]) {
    let valid: Vec<i64> = ids.iter().copied().filter(|id| id % 2 != 0).collect();
    for metric in ["L2", "IP", "COSINE"] {
        for queries in [1, 4, 9] {
            let got = search(db, field, metric, queries);
            assert_eq!(got.len(), queries);
            for hits in got {
                assert_eq!(hits, valid, "{metric} with {queries} queries");
            }
        }
    }
}

#[test]
fn nulls_survive_every_storage_stage_and_are_skipped_by_search() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(&[("v", true)], false))
        .unwrap();
    // 64 行以上才会让分片扫描（TILE_ROWS）切成多段。
    let first: Vec<i64> = (1..=80).collect();
    db.insert("", "c", batch(&first, vec![("v", vectors(&first))]))
        .unwrap();
    assert_eq!(stored(&db, "v"), expected(&first));
    assert_search_skips_nulls(&db, "v", &first);

    db.flush_collection("", "c").unwrap();
    assert_eq!(stored(&db, "v"), expected(&first));
    assert_search_skips_nulls(&db, "v", &first);

    // 多个文件，正常关闭时合并；合并逐行搬运 validity。
    let mut all = first.clone();
    for start in [81, 91] {
        let ids: Vec<i64> = (start..start + 10).collect();
        db.insert("", "c", batch(&ids, vec![("v", vectors(&ids))]))
            .unwrap();
        db.flush_collection("", "c").unwrap();
        all.extend(ids);
    }
    // 只在 WAL 里的一批：重开时回放。
    let tail: Vec<i64> = (101..=104).collect();
    db.insert("", "c", batch(&tail, vec![("v", vectors(&tail))]))
        .unwrap();
    all.extend(&tail);
    assert_search_skips_nulls(&db, "v", &all);
    db.close();
    let manifest = milvus_lite_core::storage::manifest::Manifest::read(
        &root.path().join("collections/c/manifest.json"),
    )
    .unwrap();
    assert_eq!(manifest.partitions["_default"].data_files.len(), 1);

    let reopened = open(root.path());
    assert_eq!(stored(&reopened, "v"), expected(&all));
    assert_search_skips_nulls(&reopened, "v", &all);
}

/// 缺整列视为全为空；全部为空、dim 与 schema 不符的列按 schema 的 dim 写空值（3.x 不看空行的 dim）。
#[test]
fn missing_or_all_null_columns_store_nulls_with_the_schema_dim() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(&[("v", true)], false))
        .unwrap();
    db.insert("", "c", batch(&[1, 2], Vec::new())).unwrap();
    let wrong_dim = InputValues::Nullable {
        values: Box::new(InputValues::FloatVector {
            dim: 5,
            values: vec![7.0; 10],
        }),
        validity: Validity::from_bools(&[false, false]),
    };
    db.insert("", "c", batch(&[3, 4], vec![("v", wrong_dim)]))
        .unwrap();
    assert_eq!(
        stored(&db, "v"),
        vec![(1, None), (2, None), (3, None), (4, None)]
    );
    assert!(search(&db, "v", "L2", 1)[0].is_empty());
    db.flush_collection("", "c").unwrap();
    assert_eq!(stored(&db, "v").len(), 4);

    // 含有效行时 dim 照旧检查。
    let mixed = InputValues::Nullable {
        values: Box::new(InputValues::FloatVector {
            dim: 3,
            values: vec![1.0; 6],
        }),
        validity: Validity::from_bools(&[true, false]),
    };
    assert_eq!(
        db.insert("", "c", batch(&[5, 6], vec![("v", mixed)]))
            .unwrap_err()
            .to_string(),
        "vector field 'v' expected dim 2, got 3"
    );
}

/// 非 nullable 的向量字段照旧拒绝空值与缺列。
#[test]
fn non_nullable_vector_fields_still_reject_nulls() {
    let root = tempfile::tempdir().unwrap();
    let db = open(root.path());
    db.create_collection("", "c", schema(&[("v", false), ("w", false)], false))
        .unwrap();
    let full = || InputValues::FloatVector {
        dim: 2,
        values: vec![1.0; 4],
    };
    for (columns, message) in [
        (vec![("w", full())], "vector field 'v' missing or None"),
        (
            vec![("v", vectors(&[1, 2])), ("w", full())],
            "vector field 'v' missing or None",
        ),
        (vec![("v", full())], "vector field 'w' missing or None"),
        (
            vec![("v", full()), ("w", vectors(&[1, 2]))],
            "vector field 'w' missing or None",
        ),
    ] {
        assert_eq!(
            db.insert("", "c", batch(&[1, 2], columns))
                .unwrap_err()
                .to_string(),
            message
        );
    }
}

/// 多向量集合的三种布局：只有 nullable 的字段接受空值，检索只在被检索的字段上排除空行。
#[test]
fn multi_vector_layouts_null_only_the_nullable_fields() {
    for layout in [[true, false], [false, true], [true, true]] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        db.create_collection(
            "",
            "c",
            schema(&[("a", layout[0]), ("b", layout[1])], false),
        )
        .unwrap();
        let ids: Vec<i64> = (1..=6).collect();
        let column = |nullable: bool| {
            if nullable {
                vectors(&ids)
            } else {
                InputValues::FloatVector {
                    dim: 2,
                    values: ids.iter().flat_map(|id| [1.0, *id as f32]).collect(),
                }
            }
        };
        db.insert(
            "",
            "c",
            batch(
                &ids,
                vec![("a", column(layout[0])), ("b", column(layout[1]))],
            ),
        )
        .unwrap();
        db.flush_collection("", "c").unwrap();
        for (field, nullable) in [("a", layout[0]), ("b", layout[1])] {
            let all: Vec<_> = ids
                .iter()
                .map(|id| (*id, Some(vec![1.0, *id as f32])))
                .collect();
            if nullable {
                assert_eq!(stored(&db, field), expected(&ids), "{layout:?} {field}");
                assert_search_skips_nulls(&db, field, &ids);
            } else {
                assert_eq!(stored(&db, field), all, "{layout:?} {field}");
                assert_eq!(search(&db, field, "L2", 1)[0], ids, "{layout:?} {field}");
            }
        }
    }
}

/// upsert 省略向量时继承旧值（含空值），显式给空值时置空；partition key 集合走另一条合并路径，结果相同。
#[test]
fn upsert_inherits_or_clears_nulls_with_and_without_partition_key() {
    for key in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let db = open(root.path());
        db.create_collection("", "c", schema(&[("v", true)], key))
            .unwrap();
        let ids = [1, 2, 3];
        let mut columns = vec![("v", vectors(&ids))];
        if key {
            columns.push(("k", InputValues::Int(vec![7; 3])));
        }
        db.insert("", "c", batch(&ids, columns)).unwrap();
        db.flush_collection("", "c").unwrap();

        // 只改 f：1 与 3 保留向量，2 保留空值。
        let mut columns = vec![("f", InputValues::Int(vec![9; 3]))];
        if key {
            columns.push(("k", InputValues::Int(vec![7; 3])));
        }
        db.upsert_with_pk_type("", "c", batch(&ids, columns))
            .unwrap();
        assert_eq!(stored(&db, "v"), expected(&ids), "key={key}");

        // 3 显式置空，2 写入向量。
        let mut columns = vec![(
            "v",
            InputValues::Nullable {
                values: Box::new(InputValues::FloatVector {
                    dim: 2,
                    values: vec![5.0, 5.0, 0.0, 0.0],
                }),
                validity: Validity::from_bools(&[true, false]),
            },
        )];
        if key {
            columns.push(("k", InputValues::Int(vec![7; 2])));
        }
        db.upsert_with_pk_type("", "c", batch(&[2, 3], columns))
            .unwrap();
        let want = vec![
            (1, Some(vec![1.0, 1.0])),
            (2, Some(vec![5.0, 5.0])),
            (3, None),
        ];
        assert_eq!(stored(&db, "v"), want, "key={key}");
        assert_eq!(search(&db, "v", "L2", 1)[0], [1, 2], "key={key}");
        db.close();
        assert_eq!(stored(&open(root.path()), "v"), want, "key={key}");
    }
}
