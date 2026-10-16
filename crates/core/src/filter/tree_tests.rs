//! 三条求值路线逐项对照真实 3.x 的同一批组件输入。

use super::dynamic::DynamicRow;
use super::tree::Backend;
use super::{ParsedFilter, Predicate};
use crate::data::ColumnData;
use crate::schema::{CollectionSchema, DataType, FieldSchema};
use crate::storage::visibility::SourceBits;
use serde_json::Value;

#[test]
fn nullable_typed_columns_match_reference_all_routes() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/nullable_filter_v321.json"
    ))
    .unwrap();
    assert_eq!(
        fixture["reference"],
        "43d1257774e629bc9f66873977ab7c320d5bf5a7"
    );
    let mut failures = Vec::new();
    for (index, case) in fixture["cases"].as_array().unwrap().iter().enumerate() {
        let dtype = match case["dtype"].as_str().unwrap() {
            "BOOL" => DataType::Bool,
            "INT8" => DataType::Int8,
            "INT16" => DataType::Int16,
            "INT32" => DataType::Int32,
            "INT64" => DataType::Int64,
            "FLOAT" => DataType::Float,
            "DOUBLE" => DataType::Double,
            "VARCHAR" => DataType::VarChar,
            "JSON" => DataType::Json,
            _ => panic!("type"),
        };
        let mut id = FieldSchema::new("id", DataType::Int64);
        id.is_primary = true;
        let mut vector = FieldSchema::new("vector", DataType::FloatVector);
        vector.dim = Some(2);
        let mut x = FieldSchema::new("x", dtype);
        x.nullable = true;
        let schema = CollectionSchema {
            fields: vec![id, vector, x, FieldSchema::new("j", DataType::Json)],
            enable_dynamic_field: true,
            properties: vec![],
        };
        let input = case["values"].as_array().unwrap();
        let valid: Vec<_> = input.iter().map(|v| !v.is_null()).collect();
        let column = match dtype {
            DataType::Bool => {
                ColumnData::Bool(input.iter().map(|v| v.as_bool().unwrap_or(false)).collect())
            }
            DataType::Int8 => ColumnData::Int8(
                input
                    .iter()
                    .map(|v| v.as_i64().unwrap_or(0) as i8)
                    .collect(),
            ),
            DataType::Int16 => ColumnData::Int16(
                input
                    .iter()
                    .map(|v| v.as_i64().unwrap_or(0) as i16)
                    .collect(),
            ),
            DataType::Int32 => ColumnData::Int32(
                input
                    .iter()
                    .map(|v| v.as_i64().unwrap_or(0) as i32)
                    .collect(),
            ),
            DataType::Int64 => {
                ColumnData::Int64(input.iter().map(|v| v.as_i64().unwrap_or(0)).collect())
            }
            DataType::Float => ColumnData::Float(
                input
                    .iter()
                    .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                    .collect(),
            ),
            DataType::Double => {
                ColumnData::Double(input.iter().map(|v| v.as_f64().unwrap_or(0.0)).collect())
            }
            DataType::VarChar => ColumnData::VarChar(
                input
                    .iter()
                    .map(|v| v.as_str().unwrap_or("").to_owned())
                    .collect(),
            ),
            DataType::Json => ColumnData::Json(
                input
                    .iter()
                    .map(|v| v.as_str().unwrap_or("null").as_bytes().to_vec())
                    .collect(),
            ),
            DataType::FloatVector => panic!("type"),
        }
        .with_validity(&valid);
        let columns = vec![
            ColumnData::Int64(vec![1, 2, 3, 4]),
            ColumnData::FloatVector {
                dim: 2,
                values: vec![1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0],
            },
            column,
            ColumnData::Json(
                [r#"{"k":1,"a":[1]}"#, r#"{"k":null,"a":[]}"#, "{}", "null"]
                    .iter()
                    .map(|v| v.as_bytes().to_vec())
                    .collect(),
            ),
        ];
        let meta: Vec<serde_json::Map<String, Value>> =
            [r#"{"d":1}"#, r#"{"d":null}"#, "{}", r#"{"d":{"k":1}}"#]
                .iter()
                .map(|v| serde_json::from_str(v).unwrap())
                .collect();
        let rows: Vec<_> = (0..4)
            .map(|row| DynamicRow {
                columns: &columns,
                meta: Some(&meta[row]),
                row,
                sequence: Some((row + 1) as u64),
            })
            .collect();
        let expr = case["expression"].as_str().unwrap();
        let mut phase = "parse";
        let mut backend = None;
        let result = ParsedFilter::parse(expr).and_then(|parsed| {
            phase = "compile";
            let predicate = Predicate::from_parsed(&parsed, &schema)?;
            backend = Some(if let Some(tree) = &predicate.tree {
                tree.backend
            } else if predicate.field_names.is_some() {
                Backend::Hybrid
            } else {
                Backend::Arrow
            });
            phase = "evaluate";
            if predicate.requires_source_rows() {
                predicate.evaluate_source_rows(&rows)
            } else {
                predicate.evaluate(&columns, &SourceBits::from_live(&[true; 4]))
            }
        });
        let actual = match result {
            Ok(mask) => {
                serde_json::json!({"mask":(0..4).map(|row|mask.get(row)).collect::<Vec<_>>()})
            }
            Err(error) => serde_json::json!({"phase":phase,"error":error.to_string()}),
        };
        if actual.get("mask") != case.get("mask")
            || actual.get("error") != case.get("error")
            || actual.get("phase") != case.get("phase")
        {
            failures.push(format!(
                "case {index} {dtype:?}: {expr}; expected={case}; actual={actual}"
            ));
        }
        if let Some(expected) = case["backend"].as_str() {
            let actual = match backend {
                Some(Backend::Arrow) => "arrow",
                Some(Backend::Hybrid) => "hybrid",
                Some(Backend::Python) => "python",
                None => "none",
            };
            if actual != expected {
                failures.push(format!("case {index}: backend {actual} != {expected}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} divergences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn all_routes_match_reference_tree_corpus() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../tests/fixtures/filter_tree.json")).unwrap();
    let types = |name: &str| match name {
        "INT64" => DataType::Int64,
        "INT8" => DataType::Int8,
        "INT16" => DataType::Int16,
        "INT32" => DataType::Int32,
        "FLOAT" => DataType::Float,
        "DOUBLE" => DataType::Double,
        "BOOL" => DataType::Bool,
        "VARCHAR" => DataType::VarChar,
        "JSON" => DataType::Json,
        "FLOAT_VECTOR" => DataType::FloatVector,
        _ => panic!("fixture data type"),
    };
    let fields: Vec<_> = fixture["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|field| {
            let mut f = FieldSchema::new(
                field[0].as_str().unwrap(),
                types(field[1].as_str().unwrap()),
            );
            f.is_primary = f.name == "id";
            if f.data_type == DataType::FloatVector {
                f.dim = Some(2);
            }
            f
        })
        .collect();
    let mut failures = Vec::new();
    for (index, case) in fixture["cases"].as_array().unwrap().iter().enumerate() {
        let mut schema = CollectionSchema {
            fields: fields.clone(),
            enable_dynamic_field: case["dynamic"].as_bool().unwrap(),
            properties: vec![],
        };
        let mut columns: Vec<_> = fields
            .iter()
            .map(|field| {
                let values = case["columns"][&field.name].as_array().unwrap();
                match field.data_type {
                    DataType::Int64 => {
                        ColumnData::Int64(values.iter().map(|v| v.as_i64().unwrap()).collect())
                    }
                    DataType::Int8 => {
                        ColumnData::Int8(values.iter().map(|v| v.as_i64().unwrap() as i8).collect())
                    }
                    DataType::Int16 => ColumnData::Int16(
                        values.iter().map(|v| v.as_i64().unwrap() as i16).collect(),
                    ),
                    DataType::Int32 => ColumnData::Int32(
                        values.iter().map(|v| v.as_i64().unwrap() as i32).collect(),
                    ),
                    DataType::Float => ColumnData::Float(
                        values.iter().map(|v| v.as_f64().unwrap() as f32).collect(),
                    ),
                    DataType::Double => {
                        ColumnData::Double(values.iter().map(|v| v.as_f64().unwrap()).collect())
                    }
                    DataType::Bool => {
                        ColumnData::Bool(values.iter().map(|v| v.as_bool().unwrap()).collect())
                    }
                    DataType::VarChar => ColumnData::VarChar(
                        values
                            .iter()
                            .map(|v| v.as_str().unwrap().to_owned())
                            .collect(),
                    ),
                    DataType::Json => ColumnData::Json(
                        values
                            .iter()
                            .map(|v| v.as_str().unwrap().as_bytes().to_vec())
                            .collect(),
                    ),
                    DataType::FloatVector => ColumnData::FloatVector {
                        dim: 2,
                        values: values
                            .iter()
                            .flat_map(|v| v.as_array().unwrap())
                            .map(|v| v.as_f64().unwrap() as f32)
                            .collect(),
                    },
                }
            })
            .collect();
        if case["collision"].as_bool().unwrap() {
            schema
                .fields
                .push(FieldSchema::new("__meta__x", DataType::VarChar));
            columns.push(ColumnData::VarChar(vec!["sentinel".into(); 4]));
        }
        let meta: Vec<_> = case["meta"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_object().unwrap().clone())
            .collect();
        let rows: Vec<_> = (0..4)
            .map(|row| DynamicRow {
                columns: &columns,
                meta: schema.enable_dynamic_field.then_some(&meta[row]),
                row,
                sequence: Some((row + 1) as u64),
            })
            .collect();
        let expr = case["expression"].as_str().unwrap();
        let mut phase = "parse";
        let mut backend = None;
        let result = ParsedFilter::parse(expr).and_then(|parsed| {
            phase = "compile";
            let predicate = Predicate::from_parsed(&parsed, &schema)?;
            backend = Some(if let Some(tree) = &predicate.tree {
                tree.backend
            } else if predicate.field_names.is_some() {
                Backend::Hybrid
            } else {
                Backend::Arrow
            });
            phase = "evaluate";
            if predicate.requires_source_rows() {
                predicate.evaluate_source_rows(&rows)
            } else {
                predicate.evaluate(&columns, &SourceBits::from_live(&[true; 4]))
            }
        });
        let actual = match result {
            Ok(mask) => {
                serde_json::json!({"mask":(0..4).map(|row|mask.get(row)).collect::<Vec<_>>()})
            }
            Err(error) => serde_json::json!({"error":error.to_string(),"phase":phase}),
        };
        if actual.get("mask") != case.get("mask")
            || actual.get("error") != case.get("error")
            || actual.get("phase") != case.get("phase")
        {
            failures.push(format!(
                "case {index}: {expr}; expected={}/{}/{}, actual={actual}",
                case["mask"], case["error"], case["phase"]
            ));
        }
        if let Some(expected) = case["backend"].as_str() {
            let actual = match backend {
                Some(Backend::Arrow) => "arrow",
                Some(Backend::Hybrid) => "hybrid",
                Some(Backend::Python) => "python",
                None => "none",
            };
            if actual != expected {
                failures.push(format!(
                    "case {index}: {expr}; backend {actual} expected {expected}"
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} divergences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
