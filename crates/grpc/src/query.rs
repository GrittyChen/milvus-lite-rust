//! Query 的分派与编码，与 3.x `servicer.Query` 一致。M1 只支持两种形式（0004「范围」）：
//! 空 expr 的 `count(*)`，以及 `<主键> in [...]`。

use milvus_lite_core::filter::parse_pk_in;
use milvus_lite_core::{Collection, CollectionHandle, EngineError};
use serde_json::Value;

use crate::output;
use crate::pb::schema::{self as pb, field_data, scalar_field};
use crate::pb::service as svc;
use crate::schema::unsupported;
use crate::status;

const COUNT: &str = "count(*)";

#[cfg(test)]
thread_local! {
    static AFTER_PREPARE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn query(
    handle: &CollectionHandle,
    req: svc::QueryRequest,
) -> Result<svc::QueryResults, EngineError> {
    let c = handle.read();
    if req.output_fields.iter().any(|f| f == COUNT) {
        // 3.x：只要请求了 count(*) 就只返回计数；expr 为空时取总行数，不查 load 状态、不看分区。
        // expr 非空时 3.x 走通用查询，先检查 load 状态。
        if !req.expr.is_empty() {
            c.require_loaded()?;
            return Err(unsupported("count(*) with a filter expression"));
        }
        return Ok(count_result(&c));
    }
    // query_params 中只要出现 iterator 键，3.x 就改走按主键排序的通用查询。
    let iterator = req.query_params.iter().any(|kv| kv.key == "iterator");
    // 3.x `_extract_timezone` 取第一个 timezone 键，值经 JSON 解析后为非空字符串才生效。
    let timezone = req
        .query_params
        .iter()
        .find(|kv| kv.key == "timezone")
        .is_some_and(|kv| {
            let v =
                serde_json::from_str(&kv.value).unwrap_or_else(|_| Value::String(kv.value.clone()));
            matches!(v, Value::String(s) if !s.is_empty())
        });
    let pk = c
        .schema()
        .fields
        .iter()
        .find(|f| f.is_primary)
        .map(|f| f.name.clone())
        .ok_or_else(|| {
            EngineError::SchemaValidation("schema has no primary key field".to_owned())
        })?;
    let literals = (!req.expr.is_empty() && !iterator)
        .then(|| parse_pk_in(&req.expr, &pk))
        .flatten();
    // 按主键读取与通用查询都先检查 load 状态，M1 不支持的形式在此之后报告（0004「决策」）。
    c.require_loaded()?;
    let Some(literals) = literals else {
        return Err(unsupported(if iterator {
            "query iterators"
        } else if req.expr.is_empty() {
            "query without a filter expression"
        } else {
            "filter expressions other than '<primary key> in [...]'"
        }));
    };
    if req.partition_names.iter().any(|p| p != "_default") {
        return Err(unsupported("partitions other than '_default'"));
    }
    if timezone {
        return Err(unsupported("timezone"));
    }
    let prepared = c.prepare_get_literals(&literals, &req.output_fields)?;
    drop(c);
    #[cfg(test)]
    AFTER_PREPARE.with(|hook| {
        if let Some(pause) = hook.borrow_mut().take() {
            pause();
        }
    });
    let result = prepared.read();
    let schema = prepared.schema();
    Ok(svc::QueryResults {
        status: Some(status::success()),
        fields_data: output::fields_data(schema, &result.projection, &[&result.rows]),
        collection_name: prepared.name().to_owned(),
        output_fields: if req.output_fields.is_empty() {
            Vec::new()
        } else {
            result.projection.output_fields(schema, true)
        },
        primary_field_name: pk,
        ..Default::default()
    })
}

fn count_result(c: &Collection) -> svc::QueryResults {
    let count = i64::try_from(c.count()).unwrap_or(i64::MAX);
    svc::QueryResults {
        status: Some(status::success()),
        fields_data: vec![pb::FieldData {
            r#type: 5,
            field_name: COUNT.to_owned(),
            field: Some(field_data::Field::Scalars(pb::ScalarField {
                data: Some(scalar_field::Data::LongData(pb::LongArray {
                    data: vec![count],
                })),
            })),
            ..Default::default()
        }],
        collection_name: c.name().to_owned(),
        output_fields: vec![COUNT.to_owned()],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use milvus_lite_core::{
        CollectionSchema, Compute, DataDir, DataType, Db, FieldSchema, InputValues, InsertBatch,
    };
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    #[test]
    fn query_gathers_and_encodes_after_releasing_state_lock() {
        let root = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::new(
            DataDir::open(root.path()).unwrap(),
            Compute::for_host().unwrap(),
        ));
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
        db.insert(
            "",
            "c",
            InsertBatch {
                num_rows: 1,
                columns: vec![
                    ("id".into(), InputValues::Int(vec![1])),
                    (
                        "v".into(),
                        InputValues::FloatVector {
                            dim: 2,
                            values: vec![1.0, 0.0],
                        },
                    ),
                ],
            },
        )
        .unwrap();
        let handle = db.collection("", "c").unwrap();
        let req = svc::QueryRequest {
            collection_name: "c".into(),
            expr: "id in [1, 1, 999]".into(),
            output_fields: vec!["v".into()],
            ..Default::default()
        };
        let expected = query(&handle, req.clone()).unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            AFTER_PREPARE.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    ready_tx.send(()).unwrap();
                    resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                }))
            });
            assert_eq!(query(&handle, req).unwrap(), expected);
        });
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let writer_db = Arc::clone(&db);
        let (done_tx, done_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            writer_db
                .delete("", "c", &[milvus_lite_core::Pk::Int64(1)])
                .unwrap();
            writer_db.flush_collection("", "c").unwrap();
            done_tx.send(()).unwrap();
        });
        let completed = done_rx.recv_timeout(Duration::from_secs(10));
        resume_tx.send(()).unwrap();
        reader.join().unwrap();
        writer.join().unwrap();
        completed.unwrap();
        db.close();
    }
}
