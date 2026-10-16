//! 有序分页沿用已发布有效位图；快照后发生的删除、写入和 flush 不改变本次结果。
#![allow(clippy::unwrap_used)]
use milvus_lite_core::filter::ParsedFilter;
use milvus_lite_core::{
    CollectionSchema, Compute, DataDir, DataType, Db, FieldSchema, InputValues, InsertBatch, Pk,
};

#[test]
fn ordered_pages_use_typed_primary_keys_and_an_immutable_snapshot() {
    for varchar in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let db = Db::new(
            DataDir::open(root.path()).unwrap(),
            Compute::for_host().unwrap(),
        )
        .unwrap();
        let mut pk = FieldSchema::new(
            "id",
            if varchar {
                DataType::VarChar
            } else {
                DataType::Int64
            },
        );
        pk.is_primary = true;
        pk.max_length = varchar.then_some(32);
        let mut vector = FieldSchema::new("v", DataType::FloatVector);
        vector.dim = Some(2);
        db.create_collection(
            "",
            "c",
            CollectionSchema {
                fields: vec![pk, vector],
                enable_dynamic_field: false,
                properties: vec![],
            },
        )
        .unwrap();
        let ints = [9, -3, 4, 0, 7, 2];
        let strings = ["f", "a", "é", "d", "b", "中"];
        for (start, end) in [(0, 3), (3, 6)] {
            db.insert(
                "",
                "c",
                InsertBatch {
                    num_rows: end - start,
                    columns: vec![
                        (
                            "id".into(),
                            if varchar {
                                InputValues::Str(
                                    strings[start..end].iter().map(|s| (*s).into()).collect(),
                                )
                            } else {
                                InputValues::Int(ints[start..end].to_vec())
                            },
                        ),
                        (
                            "v".into(),
                            InputValues::FloatVector {
                                dim: 2,
                                values: [1., 0.].repeat(end - start),
                            },
                        ),
                    ],
                },
            )
            .unwrap();
            if start == 0 {
                db.flush_collection("", "c").unwrap();
            }
        }
        let handle = db.collection("", "c").unwrap();
        let snapshot = handle.read().read_snapshot();
        let all = ParsedFilter::parse("").unwrap();
        let mut expected: Vec<_> = if varchar {
            strings.iter().map(|s| Pk::VarChar((*s).into())).collect()
        } else {
            ints.iter().map(|i| Pk::Int64(*i)).collect()
        };
        assert_eq!(
            snapshot.query_parsed(&all, &[], None, 0).unwrap().rows.pks,
            expected
        );
        if varchar {
            expected.sort_by_key(|pk| match pk {
                Pk::VarChar(s) => s.clone(),
                _ => unreachable!(),
            });
        } else {
            expected.sort_by_key(|pk| match pk {
                Pk::Int64(i) => *i,
                _ => unreachable!(),
            });
        }
        db.delete("", "c", &expected[..2]).unwrap();
        db.flush_collection("", "c").unwrap();
        for (limit, offset, range) in [
            (Some(2), 1, 1..3),
            (Some(0), 0, 0..1),
            (Some(-1), 0, 0..1),
            (Some(2), -1, 0..1),
            (None, -2, 4..6),
        ] {
            assert_eq!(
                snapshot
                    .query_ordered_parsed(&all, &[], limit, offset)
                    .unwrap()
                    .rows
                    .pks,
                expected[range]
            );
        }
        assert_eq!(
            handle
                .read()
                .read_snapshot()
                .query_ordered_parsed(&all, &[], None, 0)
                .unwrap()
                .rows
                .pks,
            expected[2..]
        );
        db.close();
    }
}
