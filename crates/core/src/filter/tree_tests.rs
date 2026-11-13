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
/// 三条路线逐条对照；3.x 的 MemoryError 对应本实现的资源错误（登记的差异）。
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
        // null 槽里存放的值（3.x 一侧以 mask 构造），转换检查不能看到它们。
        let values = fixture["slots"]
            .get(name)
            .and_then(Value::as_array)
            .unwrap_or(values);
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
                    .map(|v| v.as_str().unwrap_or("null").as_bytes().to_vec())
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
        evaluated += 1;
        // 3.x 在分配失败时报 MemoryError（取决于机器内存，文案为空）；本实现超出资源上限时报
        // 固定文案的资源错误（0032 登记的差异）。
        if case["error_type"] == "MemoryError" {
            if !matches!(
                result,
                Err(crate::error::EngineError::FilterResourceExhausted(_))
            ) || phase != "evaluate"
            {
                failures.push(format!("case {index}: {expr}; expected resource error"));
            }
            continue;
        }
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
    assert!(evaluated > 1300, "corpus lost cases: {evaluated}");
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

/// 0032 的资源上限：3.x 没有，本实现单侧的边界。上限经测试参数注入，用小尺寸覆盖恰在上限、
/// 上限加一、每行累计与清零、位宽估算、输入预检、列表转换只计一次与分配失败。
#[test]
fn python_arithmetic_respects_resource_limits() {
    use super::python::evaluate_tree_with;
    use super::python_num::Limits;
    use super::tree::Tree;
    let mut id = FieldSchema::new("id", DataType::Int64);
    id.is_primary = true;
    let mut v = FieldSchema::new("v", DataType::FloatVector);
    v.dim = Some(2);
    let mut arr = FieldSchema::new("arr", DataType::Array);
    arr.element_type = Some(DataType::VarChar);
    arr.max_length = Some(1000);
    arr.max_capacity = Some(10);
    let schema = CollectionSchema {
        functions: Vec::new(),
        fields: vec![id, v, arr.clone()],
        enable_dynamic_field: true,
        properties: vec![],
    };
    let strings = serde_json::json!(["x".repeat(100), "y".repeat(50)]);
    let columns = vec![
        ColumnData::Int64(vec![1, 2]),
        ColumnData::FloatVector {
            dim: 2,
            values: vec![1.0, 0.0, 1.0, 0.0],
        },
        crate::data::array::convert(
            &arr,
            &crate::data::InputValues::Array(vec![
                Some(crate::data::InputValues::JsonValues(
                    strings.as_array().unwrap().clone(),
                )),
                Some(crate::data::InputValues::JsonValues(
                    strings.as_array().unwrap().clone(),
                )),
            ]),
        )
        .unwrap(),
    ];
    let run = |expr: &str, metas: &[serde_json::Value], value_bytes: u64, int_bits: u64| {
        let metas: Vec<_> = metas
            .iter()
            .map(|m| m.as_object().unwrap().clone())
            .collect();
        let rows: Vec<_> = metas
            .iter()
            .enumerate()
            .map(|(row, meta)| DynamicRow {
                columns: &columns,
                meta: Some(meta),
                row,
                sequence: None,
            })
            .collect();
        let tree = Tree::compile(&ParsedFilter::parse(expr).unwrap(), &schema).unwrap();
        evaluate_tree_with(
            &tree,
            &rows,
            Limits {
                value_bytes,
                int_bits,
            },
        )
        .map(|bits| (0..rows.len()).map(|i| bits.get(i)).collect::<Vec<_>>())
        .map_err(|error| error.to_string())
    };
    let value_error = |limit: u64| Err(format!("filter value exceeds the {limit}-byte limit"));
    let int_error = |limit: u64| Err(format!("filter integer exceeds the {limit}-bit limit"));
    let a = serde_json::json!({"x": "a", "n": 3});
    // 字符串每码位 4 字节：'a' * 10 恰为 40 字节。
    assert_eq!(
        run("x * 10 == x * 10", std::slice::from_ref(&a), 80, 64),
        Ok(vec![true])
    );
    assert_eq!(
        run("x * 10 == x * 10", std::slice::from_ref(&a), 79, 64),
        value_error(79)
    );
    // 同一行累计：两个 12 字节的重复加 24 字节的拼接共 48 字节；下一行重新计。
    let two = [a.clone(), a.clone()];
    assert_eq!(
        run("x * n + x * n == x * 6", &two, 72, 64),
        Ok(vec![true, true])
    );
    assert_eq!(run("x * n + x * n == x * 6", &two, 71, 64), value_error(71));
    // 3.x 的 OverflowError 先于本实现的上限。
    let ab = serde_json::json!({"x": "ab"});
    assert_eq!(
        run(
            "x * 4611686018427387904 == ''",
            std::slice::from_ref(&ab),
            1,
            64
        ),
        Err("repeated string is too long".into())
    );
    // 预算放行、分配器给不出的容量：返回同一个资源错误，不 abort。
    assert_eq!(
        run("x * 4611686018427387903 == ''", &[ab], u64::MAX, 64),
        value_error(u64::MAX)
    );
    // 大整数按估算位宽：乘法取两侧之和，加减取较大者加一。
    let number = |text: String| crate::json_text::value_str(&format!("{{\"x\": {text}}}")).unwrap();
    let power = |bits: u32| number((num_bigint::BigInt::from(1) << bits).to_string());
    assert_eq!(run("x * x > 0", &[power(99)], 1 << 20, 200), Ok(vec![true]));
    assert_eq!(
        run("x * x > 0", &[power(100)], 1 << 20, 200),
        int_error(200)
    );
    assert_eq!(
        run("x + x > 0", &[power(199)], 1 << 20, 200),
        int_error(200)
    );
    assert_eq!(
        run("x + 1 > 0", &[power(198)], 1 << 20, 200),
        Ok(vec![true])
    );
    // 解析出的大整数与大整数结果都按 bits / 8 计费：2^198 与 2^198 + 1 各占 25 字节。
    assert_eq!(run("x + 1 > 0", &[power(198)], 50, 200), Ok(vec![true]));
    assert_eq!(run("x + 1 > 0", &[power(198)], 49, 200), value_error(49));
    assert_eq!(run("x + 1 > 0", &[power(198)], 24, 200), value_error(24));
    // 输入的十进制文本在解析之前预检：60 位估算 200 位，61 位估算 203 位。
    let text = |n: usize| number("9".repeat(n));
    assert_eq!(run("x / 1 > 0", &[text(60)], 1 << 20, 200), Ok(vec![true]));
    assert_eq!(run("x / 1 > 0", &[text(61)], 1 << 20, 200), int_error(200));
    // 比较时为文本整数临时解析的大整数是 0032 所说的瞬时工作区，不计入预算：x = 2^400 + 1 与
    // y * y = 2^400 位数相近、必须解析，预算只够两侧各解析一次的 y（各 26 字节）与 y * y（51 字节）。
    let close =
        number((num_bigint::BigInt::from(1) << 400u32 | num_bigint::BigInt::from(1)).to_string());
    let mut meta = close.as_object().unwrap().clone();
    meta.insert("y".into(), power(200)["x"].clone());
    let meta = Value::Object(meta);
    assert_eq!(
        run("x > y * y", std::slice::from_ref(&meta), 103, 1 << 20),
        Ok(vec![true])
    );
    assert_eq!(
        run("x > y * y", std::slice::from_ref(&meta), 102, 1 << 20),
        value_error(102)
    );
    // 只比较、不参与算术的输入不解析，也不受位宽上限约束。
    assert_eq!(run("x > 1", &[text(5000)], 1 << 20, 200), Ok(vec![true]));
    // ARRAY<VARCHAR> 重复：元素转成共享形式只计一次（2 个元素 × 64 + 150 码位 × 4 = 728 字节），
    // 结果按元素数计（2 × 1000 × 64 = 128000 字节），字符串内容不随次数放大。
    let empty = [serde_json::json!({})];
    let need = 2 * 64 + 150 * 4 + 2 * 1000 * 64;
    assert_eq!(
        run(
            "array_length(arr) > 0 and arr * 1000 > arr",
            &empty,
            need,
            64
        ),
        Ok(vec![true])
    );
    assert_eq!(
        run(
            "array_length(arr) > 0 and arr * 1000 > arr",
            &empty,
            need - 1,
            64
        ),
        value_error(need - 1)
    );
}

/// Python 路线的值规则中，组件真值语料造不出的输入：超出 int64 的输入整数作重复次数、除以 -0.0、
/// ARRAY 中同一个 NaN 对象在重复与拼接后的身份，以及列表拼接的转换计费。
#[test]
fn python_arithmetic_edge_values() {
    use super::python::evaluate_tree_with;
    use super::python_num::{LIMITS, Limits};
    use super::tree::Tree;
    let mut id = FieldSchema::new("id", DataType::Int64);
    id.is_primary = true;
    let mut v = FieldSchema::new("v", DataType::FloatVector);
    v.dim = Some(2);
    let mut af = FieldSchema::new("af", DataType::Array);
    af.element_type = Some(DataType::Double);
    af.max_capacity = Some(4);
    let schema = CollectionSchema {
        functions: Vec::new(),
        fields: vec![id, v, af],
        enable_dynamic_field: true,
        properties: vec![],
    };
    let columns = vec![
        ColumnData::Int64(vec![1, 2]),
        ColumnData::FloatVector {
            dim: 2,
            values: vec![1.0, 0.0, 1.0, 0.0],
        },
        ColumnData::Array(crate::data::array::ArrayData {
            element_type: DataType::Double,
            element_name: "item".into(),
            offsets: vec![0, 1, 2],
            values: Box::new(ColumnData::Double(vec![f64::NAN, 1.0])),
        }),
    ];
    let run = |expr: &str, metas: &[&str], limits: Limits| {
        let metas: Vec<serde_json::Map<String, Value>> = metas
            .iter()
            .map(|m| {
                crate::json_text::value_str(m)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .clone()
            })
            .collect();
        let rows: Vec<_> = metas
            .iter()
            .enumerate()
            .map(|(row, meta)| DynamicRow {
                columns: &columns,
                meta: Some(meta),
                row,
                sequence: None,
            })
            .collect();
        let tree = Tree::compile(&ParsedFilter::parse(expr).unwrap(), &schema).unwrap();
        evaluate_tree_with(&tree, &rows, limits)
            .map(|bits| (0..rows.len()).map(|i| bits.get(i)).collect::<Vec<_>>())
            .map_err(|error| error.to_string())
    };
    let index = Err("cannot fit 'int' into an index-sized integer".to_owned());
    for count in [
        "9223372036854775808",
        "-9223372036854775809",
        "100000000000000000000000",
    ] {
        let meta = format!(r#"{{"x": "ab", "n": {count}}}"#);
        assert_eq!(run("x * n == ''", &[&meta], LIMITS), index, "{count}");
        assert_eq!(run("n * x == ''", &[&meta], LIMITS), index, "{count}");
    }
    assert_eq!(
        run(
            "x * n == ''",
            &[r#"{"x": "ab", "n": 4611686018427387904}"#],
            LIMITS
        ),
        Err("repeated string is too long".to_owned())
    );
    assert_eq!(
        run(
            "x * n == ''",
            &[r#"{"x": "ab", "n": -9223372036854775808}"#],
            LIMITS
        ),
        Ok(vec![true])
    );
    // float 除以 -0.0 与 0.0 一样是 ZeroDivisionError：未知，取反后仍不命中。
    let zeros = [r#"{"x": 1, "y": -0.0}"#, r#"{"x": 1, "y": 0.5}"#];
    assert_eq!(run("x / y > 0", &zeros, LIMITS), Ok(vec![false, true]));
    assert_eq!(
        run("not (x / y > 0)", &zeros, LIMITS),
        Ok(vec![false, false])
    );
    // 同一行的 ARRAY 元素是同一个 float 对象：重复与拼接后列表相等按身份判等，NaN 也相等。
    let empty = ["{}", "{}"];
    assert_eq!(
        run("array_length(af) > 0 and af * 2 == af * 2", &empty, LIMITS),
        Ok(vec![true, true])
    );
    assert_eq!(
        run("array_length(af) > 0 and af + af == af * 2", &empty, LIMITS),
        Ok(vec![true, true])
    );
    assert_eq!(
        run(
            "array_length(af) > 0 and af[0] + 0 == af[0] + 0",
            &empty,
            LIMITS
        ),
        Ok(vec![false, true])
    );
    // 拼接：两个操作数各转换 3 × 64 字节，结果 6 × 64；右侧的重复再转换 3 × 64、结果 6 × 64。
    let list = [r#"{"x": [1, 2, 3]}"#];
    let need = 3 * 64 * 2 + 6 * 64 + 3 * 64 + 6 * 64;
    let limits = |value_bytes| Limits {
        value_bytes,
        int_bits: LIMITS.int_bits,
    };
    assert_eq!(run("x + x == x * 2", &list, limits(need)), Ok(vec![true]));
    assert_eq!(
        run("x + x == x * 2", &list, limits(need - 1)),
        Err(format!("filter value exceeds the {}-byte limit", need - 1))
    );
}
