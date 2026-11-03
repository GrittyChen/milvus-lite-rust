//! Query 分派保留主键快路径，其余 S1 在不可变快照上求值与编码。

use milvus_lite_core::filter::ParsedFilter;
use milvus_lite_core::timezone::TimezoneSource;
use milvus_lite_core::{CollectionHandle, EngineError};
use serde_json::Value;

use crate::output::{self, TimeFields};
use crate::pb::common::KeyValuePair;
use crate::pb::schema::{self as pb, field_data, scalar_field};
use crate::pb::service as svc;
use crate::status;

const COUNT: &str = "count(*)";

#[cfg(test)]
thread_local! {
    static BEFORE_PARSE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static AFTER_PREPARE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

/// 3.x `_extract_timezone` / `_extract_time_fields`：取第一个同名键，值按 JSON 解码（解不了就是原文），
/// 非空字符串才生效。
pub(crate) fn string_param(params: &[KeyValuePair], key: &str) -> Option<String> {
    let kv = params.iter().find(|kv| kv.key == key)?;
    match serde_json::from_str(&kv.value).unwrap_or_else(|_| Value::String(kv.value.clone())) {
        Value::String(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

/// `database_timezone` 读取所在数据库的 `timezone` 属性，只在需要时才会被调用；调用者不得持有集合锁。
pub(crate) fn query(
    handle: &CollectionHandle,
    database_timezone: &dyn Fn() -> Result<Option<String>, EngineError>,
    req: svc::QueryRequest,
) -> Result<svc::QueryResults, EngineError> {
    #[cfg(test)]
    BEFORE_PARSE.with(|hook| {
        let pause = hook.borrow_mut().take();
        if let Some(pause) = pause {
            pause();
        }
    });
    // 集合的时区属性只读一瞬间的锁；解析本身仍在锁外，load 校验后才传播错误，保持 3.x 的优先级。
    let collection_timezone = handle.read().schema().timezone().map(str::to_owned);
    let timezone = TimezoneSource::new(
        string_param(&req.query_params, "timezone"),
        collection_timezone,
        database_timezone,
    );
    // 表达式非空时先校验请求时区再解析（3.x 的过滤编译处）；空表达式的 count 不经过这里。
    let parsed = ParsedFilter::parse_with(&req.expr, &timezone);
    let c = handle.read();
    if req.output_fields.iter().any(|f| f == COUNT) {
        // 3.x：只要请求了 count(*) 就只返回计数；expr 为空时取总行数，不查 load 状态、不看分区。
        // expr 非空时 3.x 走通用查询，先检查 load 状态。
        if !req.expr.is_empty() {
            c.require_loaded()?;
            let snapshot = c.read_named_snapshot(&req.partition_names)?;
            drop(c);
            return Ok(count_result(
                snapshot.name(),
                snapshot.count_filtered_parsed(&parsed?)?,
            ));
        }
        return Ok(count_result(c.name(), c.count()));
    }
    // query_params 中只要出现 iterator 键，3.x 就改走按主键排序的通用查询。
    let iterator = req.query_params.iter().any(|kv| kv.key == "iterator");
    let pk = c
        .schema()
        .fields
        .iter()
        .find(|f| f.is_primary)
        .map(|f| f.name.clone())
        .ok_or_else(|| {
            EngineError::SchemaValidation("schema has no primary key field".to_owned())
        })?;
    // 按主键读取与通用查询都先检查 load 状态，M1 不支持的形式在此之后报告（0004「决策」）。
    c.require_loaded()?;
    let parsed = parsed?;
    // Get 保留请求中的主键次序且不分页；iterator 必须走过滤、排序后分页的通用路线。
    let literals = (!iterator).then(|| parsed.primary_keys(&pk)).flatten();
    let prepared = literals
        .as_ref()
        .map(|literals| {
            c.prepare_partition_get_literals(literals, &req.output_fields, &req.partition_names)
        })
        .transpose()?;
    let snapshot = prepared
        .is_none()
        .then(|| c.read_named_snapshot(&req.partition_names))
        .transpose()?;
    drop(c);
    #[cfg(test)]
    AFTER_PREPARE.with(|hook| {
        let pause = hook.borrow_mut().take();
        if let Some(pause) = pause {
            pause();
        }
    });
    let (name, schema, result) = if let Some(prepared) = &prepared {
        (prepared.name(), prepared.schema(), prepared.read())
    } else {
        let snapshot = snapshot
            .as_ref()
            .ok_or_else(|| EngineError::Storage("query snapshot missing".into()))?;
        let (mut limit, mut offset) = (None, 0);
        for kv in &req.query_params {
            if let Some(value) = crate::pyint::int_str(&kv.value) {
                match kv.key.as_str() {
                    "limit" => limit = Some(value),
                    "offset" => offset = value,
                    _ => {}
                }
            }
        }
        (
            snapshot.name(),
            snapshot.schema(),
            if iterator {
                snapshot.query_ordered_parsed(&parsed, &req.output_fields, limit, offset)?
            } else {
                snapshot.query_parsed(&parsed, &req.output_fields, limit, offset)?
            },
        )
    };
    // 响应编码处再校验一次请求时区（3.x 的第二个校验点）：空表达式也会在这里报错。
    timezone.next_phase();
    timezone.validate_request()?;
    let time_fields = TimeFields::new(
        string_param(&req.query_params, "time_fields").as_deref(),
        &timezone,
    );
    Ok(svc::QueryResults {
        status: Some(status::success()),
        fields_data: output::fields_data(
            schema,
            &result.projection,
            &[&result.rows],
            time_fields.as_ref(),
        )?,
        collection_name: name.to_owned(),
        output_fields: if req.output_fields.is_empty() {
            Vec::new()
        } else {
            result.projection.output_fields(schema, true)
        },
        primary_field_name: pk,
        ..Default::default()
    })
}

fn count_result(name: &str, count: usize) -> svc::QueryResults {
    let count = i64::try_from(count).unwrap_or(i64::MAX);
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
        collection_name: name.to_owned(),
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
    fn query_parses_before_acquiring_the_state_lock() {
        let root = tempfile::tempdir().unwrap();
        let db = Db::new(
            DataDir::open(root.path()).unwrap(),
            Compute::for_host().unwrap(),
        )
        .unwrap();
        let mut id = FieldSchema::new("id", DataType::Int64);
        id.is_primary = true;
        let mut vector = FieldSchema::new("v", DataType::FloatVector);
        vector.dim = Some(2);
        db.create_collection(
            "",
            "c",
            CollectionSchema {
                fields: vec![id, vector],
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
        let (ready_tx, ready_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            BEFORE_PARSE.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    ready_tx.send(()).unwrap();
                    resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                }))
            });
            query(
                &handle,
                &|| Ok(None),
                svc::QueryRequest {
                    collection_name: "c".into(),
                    expr: "id>=0".into(),
                    output_fields: vec!["id".into()],
                    ..Default::default()
                },
            )
            .unwrap()
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
        let result = reader.join().unwrap();
        writer.join().unwrap();
        completed.unwrap();
        assert_eq!(result.fields_data[0].field_name, "id");
        assert!(
            matches!(&result.fields_data[0].field,Some(field_data::Field::Scalars(s))
            if matches!(&s.data,Some(scalar_field::Data::LongData(values)) if values.data.is_empty()))
        );
        db.close();
    }

    #[test]
    fn query_reads_the_database_zone_once_per_phase() {
        use std::cell::Cell;
        let root = tempfile::tempdir().unwrap();
        let db = Db::new(
            DataDir::open(root.path()).unwrap(),
            Compute::for_host().unwrap(),
        )
        .unwrap();
        let mut id = FieldSchema::new("id", DataType::Int64);
        id.is_primary = true;
        let mut v = FieldSchema::new("v", DataType::FloatVector);
        v.dim = Some(2);
        let ts = FieldSchema::new("ts", DataType::Timestamptz);
        db.create_collection(
            "",
            "c",
            CollectionSchema {
                fields: vec![id, v, ts],
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
                    ("ts".into(), InputValues::Int(vec![1_735_689_600_000_000])),
                ],
            },
        )
        .unwrap();
        let handle = db.collection("", "c").unwrap();
        // 过滤编译读到 UTC（命中 2025-01-01T00:00Z），其间数据库时区变成东京；响应编码重新读取，
        // `time_fields` 按东京输出小时（9），而不是复用过滤阶段的值。
        let reads = Cell::new(0);
        let database = || {
            reads.set(reads.get() + 1);
            Ok(Some(
                if reads.get() == 1 {
                    "UTC"
                } else {
                    "Asia/Tokyo"
                }
                .to_owned(),
            ))
        };
        let reply = query(
            &handle,
            &database,
            svc::QueryRequest {
                collection_name: "c".into(),
                expr: "ts == ISO '2025-01-01T00:00:00'".into(),
                output_fields: vec!["ts".into()],
                query_params: vec![crate::pb::common::KeyValuePair {
                    key: "time_fields".into(),
                    value: "hour".into(),
                }],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(reads.get(), 2);
        let column = reply
            .fields_data
            .iter()
            .find(|f| f.field_name == "ts")
            .unwrap();
        let Some(field_data::Field::Scalars(s)) = &column.field else {
            panic!("{column:?}")
        };
        let Some(scalar_field::Data::ArrayData(a)) = &s.data else {
            panic!("{column:?}")
        };
        let Some(scalar_field::Data::LongData(hours)) = &a.data[0].data else {
            panic!("{column:?}")
        };
        assert_eq!(hours.data, [9]);
        db.close();
    }

    #[test]
    fn query_gathers_and_encodes_after_releasing_state_lock() {
        for iterator in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let db = Db::new(
                DataDir::open(root.path()).unwrap(),
                Compute::for_host().unwrap(),
            )
            .unwrap();
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
                query_params: if iterator {
                    vec![crate::pb::common::KeyValuePair {
                        key: "iterator".into(),
                        value: "false".into(),
                    }]
                } else {
                    vec![]
                },
                ..Default::default()
            };
            let expected = query(&handle, &|| Ok(None), req.clone()).unwrap();
            let (ready_tx, ready_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            let reader = std::thread::spawn(move || {
                AFTER_PREPARE.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move || {
                        ready_tx.send(()).unwrap();
                        resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    }))
                });
                assert_eq!(query(&handle, &|| Ok(None), req).unwrap(), expected);
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
}
