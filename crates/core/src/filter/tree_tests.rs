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
    let fixture: Value = crate::json_text::value_str(include_str!(
        "../../tests/fixtures/nullable_filter_v321.json"
    ))
    .unwrap();
    assert_eq!(
        fixture["reference"],
        "1320d26ec3d88b23b525e774df6e3c8e7c208f81"
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
            functions: Vec::new(),
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
            DataType::FloatVector
            | DataType::SparseFloatVector
            | DataType::Array
            | DataType::Timestamptz => {
                panic!("type outside scalar fixture")
            }
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
                .map(|v| {
                    crate::json_text::value_str(v)
                        .unwrap()
                        .as_object()
                        .unwrap()
                        .clone()
                })
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
        crate::json_text::value_str(include_str!("../../tests/fixtures/filter_tree.json")).unwrap();
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
            functions: Vec::new(),
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
                    DataType::Array => panic!("ARRAY is covered by its own truth fixture"),
                    DataType::Timestamptz => panic!("TIMESTAMPTZ has its own fixtures"),
                    DataType::SparseFloatVector => panic!("sparse vectors are not in this fixture"),
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

#[test]
fn array_columns_match_reference_routes_and_masks() {
    let fixture: Value =
        crate::json_text::value_str(include_str!("../../tests/fixtures/array_v321.json")).unwrap();
    let mut failures = Vec::new();
    for (i, case) in fixture["filters"].as_array().unwrap().iter().enumerate() {
        let dtype = match case["element_type"].as_str().unwrap() {
            "bool" => DataType::Bool,
            "int8" => DataType::Int8,
            "int16" => DataType::Int16,
            "int32" => DataType::Int32,
            "int64" => DataType::Int64,
            "float" => DataType::Float,
            "double" => DataType::Double,
            "varchar" => DataType::VarChar,
            _ => panic!("type"),
        };
        let f = FieldSchema {
            nullable: true,
            element_type: Some(dtype),
            ..FieldSchema::new("x", DataType::Array)
        };
        let input = crate::data::InputValues::Array(
            case["values"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| {
                    v.as_array()
                        .map(|a| crate::data::InputValues::JsonValues(a.clone()))
                })
                .collect(),
        );
        let column = crate::data::array::convert(&f, &input).unwrap();
        let schema = CollectionSchema {
            functions: Vec::new(),
            fields: vec![
                FieldSchema {
                    is_primary: true,
                    ..FieldSchema::new("id", DataType::Int64)
                },
                FieldSchema {
                    dim: Some(2),
                    ..FieldSchema::new("vector", DataType::FloatVector)
                },
                f,
                FieldSchema::new("j", DataType::Json),
            ],
            enable_dynamic_field: true,
            properties: vec![],
        };
        let columns = vec![
            ColumnData::Int64(vec![1, 2, 3, 4]),
            ColumnData::FloatVector {
                dim: 2,
                values: vec![1., 0., 1., 0., 1., 0., 1., 0.],
            },
            column,
            ColumnData::Json(
                [r#"{"k":1}"#, r#"{"k":null}"#, "{}", "null"]
                    .iter()
                    .map(|s| s.as_bytes().to_vec())
                    .collect(),
            ),
        ];
        let meta: Vec<serde_json::Map<String, Value>> =
            [r#"{"d":1}"#, r#"{"d":null}"#, "{}", r#"{"d":false}"#]
                .iter()
                .map(|s| {
                    crate::json_text::value_str(s)
                        .unwrap()
                        .as_object()
                        .unwrap()
                        .clone()
                })
                .collect();
        let rows: Vec<_> = (0..4)
            .map(|row| DynamicRow {
                columns: &columns,
                meta: Some(&meta[row]),
                row,
                sequence: None,
            })
            .collect();
        let expr = case["expression"].as_str().unwrap();
        let mut phase = "parse";
        let mut route = None;
        let result = ParsedFilter::parse(expr).and_then(|p| {
            phase = "compile";
            let bound = Predicate::from_parsed(&p, &schema)?;
            route = Some(if let Some(t) = &bound.tree {
                t.backend
            } else if bound.field_names.is_some() {
                Backend::Hybrid
            } else {
                Backend::Arrow
            });
            phase = "evaluate";
            if bound.requires_source_rows() {
                bound.evaluate_source_rows(&rows)
            } else {
                bound.evaluate(&columns, &SourceBits::from_live(&[true; 4]))
            }
        });
        let actual = match result {
            Ok(m) => serde_json::json!({"mask":(0..4).map(|i|m.get(i)).collect::<Vec<_>>()}),
            Err(e) => serde_json::json!({"phase":phase,"error":e.to_string()}),
        };
        if actual.get("mask") != case.get("mask")
            || actual.get("phase") != case.get("phase")
            || actual.get("error") != case.get("error")
        {
            failures.push(format!("{i} {expr} expected {case} actual {actual}"));
        }
        if let Some(expected) = case["backend"].as_str() {
            let actual = match route {
                Some(Backend::Arrow) => "arrow",
                Some(Backend::Hybrid) => "hybrid",
                Some(Backend::Python) => "python",
                None => "none",
            };
            if actual != expected {
                failures.push(format!("{i} route {actual} != {expected}"));
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

/// 0032：算术的组件真值（[generate_filter_arith_fixtures.py](../../../tests/differential/generate_filter_arith_fixtures.py)）。
/// 第 1 步只放开 Arrow 路线；hybrid 与 Python 路线的用例在 Rust 一侧仍应是 UNIMPLEMENTED。
#[test]
fn arithmetic_matches_reference_corpus() {
    let fixture: Value =
        crate::json_text::value_str(include_str!("../../tests/fixtures/filter_arith.json"))
            .unwrap();
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
        "ARRAY" => DataType::Array,
        "FLOAT_VECTOR" => DataType::FloatVector,
        _ => panic!("fixture data type"),
    };
    let mut fields = Vec::new();
    let mut columns = Vec::new();
    for field in fixture["fields"].as_array().unwrap() {
        let name = field[0].as_str().unwrap();
        let mut f = FieldSchema::new(name, types(field[1].as_str().unwrap()));
        f.is_primary = name == "id";
        f.nullable = field[2].as_bool().unwrap();
        f.element_type = field[3].as_str().map(types);
        if f.data_type == DataType::FloatVector {
            f.dim = Some(2);
        }
        if name == "s" {
            f.enable_analyzer = true;
            f.enable_match = true;
        }
        let values = fixture["columns"][name].as_array().unwrap();
        let valid: Vec<bool> = values.iter().map(|v| !v.is_null()).collect();
        let data = match f.data_type {
            DataType::Int64 => {
                ColumnData::Int64(values.iter().map(|v| v.as_i64().unwrap_or(0)).collect())
            }
            DataType::Int8 => ColumnData::Int8(
                values
                    .iter()
                    .map(|v| v.as_i64().unwrap_or(0) as i8)
                    .collect(),
            ),
            DataType::Int16 => ColumnData::Int16(
                values
                    .iter()
                    .map(|v| v.as_i64().unwrap_or(0) as i16)
                    .collect(),
            ),
            DataType::Int32 => ColumnData::Int32(
                values
                    .iter()
                    .map(|v| v.as_i64().unwrap_or(0) as i32)
                    .collect(),
            ),
            DataType::Float => ColumnData::Float(
                values
                    .iter()
                    .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                    .collect(),
            ),
            DataType::Double => {
                ColumnData::Double(values.iter().map(|v| v.as_f64().unwrap_or(0.0)).collect())
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
            DataType::Array => {
                let input = crate::data::InputValues::Array(
                    values
                        .iter()
                        .map(|v| {
                            v.as_array()
                                .map(|a| crate::data::InputValues::JsonValues(a.clone()))
                        })
                        .collect(),
                );
                crate::data::array::convert(&f, &input).unwrap()
            }
            _ => panic!("fixture data type"),
        };
        columns.push(
            if f.nullable && f.data_type != DataType::Array && valid.contains(&false) {
                ColumnData::Nullable {
                    values: Box::new(data),
                    validity: crate::data::Validity::from_bools(&valid),
                }
            } else {
                data
            },
        );
        fields.push(f);
    }
    let schema = CollectionSchema {
        functions: Vec::new(),
        fields,
        enable_dynamic_field: true,
        properties: vec![],
    };
    let metas: Vec<Vec<serde_json::Map<String, Value>>> = fixture["meta"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| {
            group
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row.as_object().unwrap().clone())
                .collect()
        })
        .collect();
    let mut failures = Vec::new();
    let mut evaluated = 0;
    for (index, case) in fixture["cases"].as_array().unwrap().iter().enumerate() {
        let meta = &metas[case["meta"].as_u64().unwrap() as usize];
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
        let result = ParsedFilter::parse(expr).and_then(|parsed| {
            phase = "compile";
            let predicate = Predicate::from_parsed(&parsed, &schema)?;
            phase = "evaluate";
            if predicate.requires_source_rows() {
                predicate.evaluate_source_rows(&rows)
            } else {
                predicate.evaluate(&columns, &SourceBits::from_live(&[true; 4]))
            }
        });
        let backend = case["backend"].as_str();
        let unquoted: String = expr.split('\'').step_by(2).collect();
        if matches!(backend, Some("hybrid" | "python")) && unquoted.contains(['+', '-', '*', '/']) {
            if !matches!(result, Err(crate::error::EngineError::Unsupported { .. })) {
                failures.push(format!("case {index}: {expr}; non-Arrow route not gated"));
            }
            continue;
        }
        evaluated += 1;
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
    }
    assert!(evaluated > 1000, "corpus lost its Arrow cases: {evaluated}");
    assert!(
        failures.is_empty(),
        "{} divergences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// null 槽里可以是任意值（3.x 或其他写入方写出的数据目录）。Arrow 的隐式整数转浮点只检查有效槽，
/// JSON 语料构造不出这种数据，这里直接构造。
#[test]
fn arithmetic_ignores_out_of_range_values_in_null_slots() {
    let mut id = FieldSchema::new("id", DataType::Int64);
    id.is_primary = true;
    let mut v = FieldSchema::new("v", DataType::FloatVector);
    v.dim = Some(2);
    let mut n = FieldSchema::new("n", DataType::Int64);
    n.nullable = true;
    let mut f = FieldSchema::new("f", DataType::Float);
    f.nullable = true;
    let schema = CollectionSchema {
        functions: Vec::new(),
        fields: vec![id, v, n, f],
        enable_dynamic_field: false,
        properties: vec![],
    };
    let columns = vec![
        ColumnData::Int64(vec![1, 2]),
        ColumnData::FloatVector {
            dim: 2,
            values: vec![1.0, 0.0, 1.0, 0.0],
        },
        ColumnData::Nullable {
            values: Box::new(ColumnData::Int64(vec![1 << 62, 5])),
            validity: crate::data::Validity::from_bools(&[false, true]),
        },
        ColumnData::Float(vec![1.0, 2.0]),
    ];
    // 布尔树按数据源的活行求值（`evaluate_source_rows`），与真实读取路径相同。
    let evaluate = |expr: &str, columns: &[ColumnData]| {
        let rows: Vec<_> = (0..2)
            .map(|row| DynamicRow {
                columns,
                meta: None,
                row,
                sequence: Some((row + 1) as u64),
            })
            .collect();
        let predicate = Predicate::from_parsed(&ParsedFilter::parse(expr)?, &schema)?;
        assert!(predicate.requires_source_rows(), "{expr}");
        predicate.evaluate_source_rows(&rows)
    };
    for (expr, expected) in [
        ("n + f > 0", [false, true]),
        ("f + n > 0", [false, true]),
        ("n == f", [false, false]),
        ("n < f", [false, false]),
        ("n / f > 2", [false, true]),
        ("(n + 1) / 2 == 3", [false, true]),
        ("n * 1.5 == 7.5", [false, true]),
    ] {
        let mask = evaluate(expr, &columns).unwrap_or_else(|error| panic!("{expr}: {error}"));
        assert_eq!([mask.get(0), mask.get(1)], expected, "{expr}");
    }
    // 有效槽里的同一个值照常报错。
    let columns = [
        &columns[..2],
        &[
            ColumnData::Int64(vec![1 << 62, 5]),
            ColumnData::Float(vec![1.0, 2.0]),
        ],
    ]
    .concat();
    let error = evaluate("n + f > 0", &columns).unwrap_err();
    assert_eq!(
        error.to_string(),
        "Integer value 4611686018427387904 not in range: -16777216 to 16777216"
    );
}
