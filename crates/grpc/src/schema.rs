//! CollectionSchema 在 Milvus proto 与引擎之间的转换，与 3.x `translators/schema.py` 一致。

use milvus_lite_core::error::py_repr;
use milvus_lite_core::{CollectionSchema, DataType, EngineError, FieldSchema};
use prost::Message;
use serde_json::Value;

use crate::pb::{common, schema as pb};
use crate::pyint;

/// 3.x 能建表的 Milvus 类型中，M1 支持的那些。
fn m1_type(t: i32) -> Option<DataType> {
    Some(match t {
        1 => DataType::Bool,
        2 => DataType::Int8,
        3 => DataType::Int16,
        4 => DataType::Int32,
        5 => DataType::Int64,
        10 => DataType::Float,
        11 => DataType::Double,
        21 => DataType::VarChar,
        23 => DataType::Json,
        101 => DataType::FloatVector,
        _ => return None,
    })
}

pub(crate) fn milvus_type(t: DataType) -> i32 {
    match t {
        DataType::Bool => 1,
        DataType::Int8 => 2,
        DataType::Int16 => 3,
        DataType::Int32 => 4,
        DataType::Int64 => 5,
        DataType::Float => 10,
        DataType::Double => 11,
        DataType::VarChar => 21,
        DataType::Json => 23,
        DataType::FloatVector => 101,
    }
}

/// 3.x 支持、M1 尚不支持的类型（ARRAY、GEOMETRY、TIMESTAMPTZ、稀疏向量）。
fn unsupported_in_m1(t: i32) -> Option<&'static str> {
    Some(match t {
        22 => "ARRAY fields",
        24 => "GEOMETRY fields",
        26 => "TIMESTAMPTZ fields",
        104 => "SPARSE_FLOAT_VECTOR fields",
        _ => return None,
    })
}

/// 3.x `translators/schema.py` 的 `_MILVUS_TYPE_NAMES`。
fn type_name(t: i32) -> String {
    match milvus_type_name(t) {
        Some(n) => n.to_owned(),
        None if t == 0 => "None".to_owned(),
        None => format!("unknown({t})"),
    }
}

/// Milvus DataType 的显示名，3.x 的两份映射表（schema 与 records）共用这些条目。
pub(crate) fn milvus_type_name(t: i32) -> Option<&'static str> {
    Some(match t {
        1 => "Bool",
        2 => "Int8",
        3 => "Int16",
        4 => "Int32",
        5 => "Int64",
        10 => "Float",
        11 => "Double",
        20 => "String",
        21 => "VarChar",
        22 => "Array",
        23 => "JSON",
        24 => "Geometry",
        25 => "Text",
        26 => "Timestamptz",
        100 => "BinaryVector",
        101 => "FloatVector",
        102 => "Float16Vector",
        103 => "BFloat16Vector",
        104 => "SparseFloatVector",
        105 => "Int8Vector",
        _ => return None,
    })
}

pub(crate) fn unsupported(feature: &'static str) -> EngineError {
    EngineError::Unsupported {
        feature,
        reason: "not supported in this build of milvus-lite-rust yet".to_owned(),
    }
}

/// Python 的真值判断，用于 `json.loads` 的结果。
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// CreateCollectionRequest.schema → 引擎 schema。
///
/// 3.x 翻译阶段的错误（不认识的类型、dim 与 max_length 不合法、function 类型不认识）优先报出；
/// M1 不支持的类型与属性记下第一个，翻译完成后才报告（0004「决策」）。
pub(crate) fn decode(bytes: &[u8]) -> Result<CollectionSchema, EngineError> {
    let proto = pb::CollectionSchema::decode(bytes).map_err(|_| {
        EngineError::InvalidArgument(
            "Error parsing message with type 'milvus.proto.schema.CollectionSchema': Wire format was corrupt"
                .to_owned(),
        )
    })?;
    let mut first_unsupported: Option<&'static str> = None;
    let mut note = |feature: &'static str| {
        first_unsupported.get_or_insert(feature);
    };
    let mut fields = Vec::with_capacity(proto.fields.len());
    for f in &proto.fields {
        let data_type = match (m1_type(f.data_type), unsupported_in_m1(f.data_type)) {
            (Some(t), _) => Some(t),
            (None, Some(feature)) => {
                note(feature);
                None
            }
            (None, None) => {
                return Err(EngineError::SchemaValidation(format!(
                    "field {} uses Milvus type {} which MilvusLite does not support. \
                     Phase 10.2 supports: BOOL, INT8/16/32/64, FLOAT, DOUBLE, VARCHAR, JSON, ARRAY, \
                     GEOMETRY, TIMESTAMPTZ, FLOAT_VECTOR, SPARSE_FLOAT_VECTOR.",
                    py_repr(&f.name),
                    type_name(f.data_type)
                )));
            }
        };
        // 同名 key 以后出现者为准（3.x 逐个写入 dict）。
        let param = |key: &str| {
            f.type_params
                .iter()
                .rev()
                .find(|kv| kv.key == key)
                .map(|kv| kv.value.as_str())
        };
        let mut dim = None;
        if data_type == Some(DataType::FloatVector) {
            let text = param("dim").filter(|s| !s.is_empty()).ok_or_else(|| {
                EngineError::SchemaValidation(format!(
                    "FLOAT_VECTOR field {} is missing required 'dim' type_param",
                    py_repr(&f.name)
                ))
            })?;
            dim = Some(pyint::int_str(text).ok_or_else(|| {
                EngineError::SchemaValidation(format!(
                    "FLOAT_VECTOR field {} has non-integer dim {}",
                    py_repr(&f.name),
                    py_repr(text)
                ))
            })?);
        }
        let mut max_length = None;
        if data_type == Some(DataType::VarChar)
            && let Some(text) = param("max_length").filter(|s| !s.is_empty())
        {
            max_length = Some(pyint::int_str(text).ok_or_else(|| {
                EngineError::SchemaValidation(format!(
                    "VARCHAR field {} has non-integer max_length {}",
                    py_repr(&f.name),
                    py_repr(text)
                ))
            })?);
        }
        // 以下属性在 3.x 中按这些条件生效（`_decode_field`），M1 都不支持。
        let flag = |key: &str| param(key).is_some_and(|v| v.to_lowercase() == "true");
        if flag("enable_analyzer") || flag("enable_match") {
            note("text analyzer and text match");
        }
        if let Some(text) = param("analyzer_params").filter(|s| !s.is_empty()) {
            // 3.x 只在能解析且为真值时使用；serde_json 解析不了但 Python 可能解析的也按使用处理。
            let used = match serde_json::from_str::<Value>(text) {
                Ok(v) => truthy(&v),
                Err(_) => pyint::maybe_python_json_container(text),
            };
            if used {
                note("text analyzer and text match");
            }
        }
        if f.nullable && data_type == Some(DataType::FloatVector) {
            note("nullable vectors");
        }
        let default_value = match f.default_value.as_ref().and_then(|v| v.data.as_ref()) {
            None => None,
            Some(pb::value_field::Data::BoolData(v)) => Some(Value::from(*v)),
            Some(pb::value_field::Data::IntData(v) | pb::value_field::Data::DateData(v)) => {
                Some(Value::from(*v))
            }
            Some(
                pb::value_field::Data::LongData(v)
                | pb::value_field::Data::TimestamptzData(v)
                | pb::value_field::Data::TimeData(v),
            ) => Some(Value::from(*v)),
            Some(pb::value_field::Data::FloatData(v)) => {
                serde_json::Number::from_f64(f64::from(*v))
                    .map(Value::Number)
                    .or_else(|| {
                        note("non-finite default values");
                        None
                    })
            }
            Some(pb::value_field::Data::DoubleData(v)) => serde_json::Number::from_f64(*v)
                .map(Value::Number)
                .or_else(|| {
                    note("non-finite default values");
                    None
                }),
            Some(pb::value_field::Data::StringData(v)) => Some(Value::String(v.clone())),
            Some(pb::value_field::Data::BytesData(_)) => {
                note("binary default values");
                None
            }
        };
        if default_value.is_some() && data_type == Some(DataType::FloatVector) {
            note("vector default values");
        }
        if f.is_partition_key {
            note("partition key");
        }
        if f.is_function_output {
            note("functions");
        }
        if let Some(data_type) = data_type {
            fields.push(FieldSchema {
                is_primary: f.is_primary_key,
                auto_id: f.auto_id,
                dim,
                max_length,
                nullable: f.nullable,
                default_value,
                ..FieldSchema::new(f.name.clone(), data_type)
            });
        }
    }
    for function in &proto.functions {
        // 3.x 只认识 BM25(1) 与 TEXT_EMBEDDING(2)。
        if !matches!(function.r#type, 1 | 2) {
            return Err(EngineError::SchemaValidation(format!(
                "function {} uses unsupported type {}",
                py_repr(&function.name),
                function.r#type
            )));
        }
        note("functions");
    }
    if let Some(feature) = first_unsupported {
        return Err(unsupported(feature));
    }
    #[allow(deprecated)]
    if proto.auto_id {
        for f in fields.iter_mut().filter(|f| f.is_primary) {
            f.auto_id = true;
        }
    }
    Ok(CollectionSchema {
        fields,
        enable_dynamic_field: proto.enable_dynamic_field,
        properties: Vec::new(),
    })
}

/// CreateCollectionRequest.properties → 按 Python dict 的语义：丢弃空 key，同名 key 保留首次出现的位置、取最后的值。
pub(crate) fn properties(kvs: &[common::KeyValuePair]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for kv in kvs.iter().filter(|kv| !kv.key.is_empty()) {
        match out.iter_mut().find(|(k, _)| *k == kv.key) {
            Some(entry) => entry.1.clone_from(&kv.value),
            None => out.push((kv.key.clone(), kv.value.clone())),
        }
    }
    out
}

pub(crate) fn kv(key: &str, value: impl Into<String>) -> common::KeyValuePair {
    common::KeyValuePair {
        key: key.to_owned(),
        value: value.into(),
    }
}

/// DescribeCollection 中的 schema，只填 3.x 填的字段（`milvus_lite_to_milvus_schema`）。
pub(crate) fn encode(
    name: &str,
    schema: &CollectionSchema,
) -> Result<pb::CollectionSchema, EngineError> {
    let fields = schema
        .fields
        .iter()
        .map(|f| {
            let mut type_params = Vec::new();
            if let Some(dim) = f.dim.filter(|_| f.data_type == DataType::FloatVector) {
                type_params.push(kv("dim", dim.to_string()));
            }
            if let Some(max) = f.max_length.filter(|_| f.data_type == DataType::VarChar) {
                type_params.push(kv("max_length", max.to_string()));
            }
            Ok(pb::FieldSchema {
                name: f.name.clone(),
                data_type: milvus_type(f.data_type),
                is_primary_key: f.is_primary,
                auto_id: f.auto_id,
                type_params,
                nullable: f.nullable,
                default_value: f
                    .default_value
                    .as_ref()
                    .map(|v| encode_default(f.data_type, v))
                    .transpose()?
                    .flatten(),
                ..Default::default()
            })
        })
        .collect::<Result<Vec<_>, EngineError>>()?;
    #[allow(deprecated)]
    Ok(pb::CollectionSchema {
        name: name.to_owned(),
        auto_id: schema.fields.iter().any(|f| f.is_primary && f.auto_id),
        fields,
        enable_dynamic_field: schema.enable_dynamic_field,
        ..Default::default()
    })
}

fn encode_default(dtype: DataType, value: &Value) -> Result<Option<pb::ValueField>, EngineError> {
    use pb::value_field::Data;
    let int = || {
        pyint::proto_integer(value)
            .map_err(|error| EngineError::InvalidArgument(error.message().to_owned()))
    };
    let float = || {
        pyint::proto_float(value)
            .map_err(|error| EngineError::InvalidArgument(error.message().to_owned()))
    };
    let data = match dtype {
        DataType::Bool => Data::BoolData(
            if let Value::Number(number) = value
                && !number.as_str().contains(['.', 'e', 'E'])
            {
                number
                    .as_str()
                    .bytes()
                    .any(|b| b.is_ascii_digit() && b != b'0')
            } else {
                truthy(value)
            },
        ),
        DataType::Int8 | DataType::Int16 | DataType::Int32 => {
            let integer = int()?;
            Data::IntData(i32::try_from(integer).map_err(|_| {
                EngineError::InvalidArgument(format!("Value out of range: {integer}"))
            })?)
        }
        DataType::Int64 => Data::LongData(int()?),
        DataType::Float => Data::FloatData(float()? as f32),
        DataType::Double => Data::DoubleData(float()?),
        DataType::VarChar => Data::StringData(match value {
            Value::String(v) => v.clone(),
            other => milvus_lite_core::error::py_json_repr(other),
        }),
        DataType::Json | DataType::FloatVector => return Ok(None),
    };
    Ok(Some(pb::ValueField { data: Some(data) }))
}
