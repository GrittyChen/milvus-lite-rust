//! Collection schema 与校验。校验顺序与错误信息与 3.x 的 `validate_schema` 一致。

use std::collections::HashSet;

use crate::error::{EngineError, Result, py_repr, py_repr_list};

/// M1 支持的字段类型（见 0004「范围」）。其余 Milvus 类型在 gRPC 层翻译时拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    Bool,
    Int8,
    Int16,
    Int32,
    Int64,
    Float,
    Double,
    VarChar,
    Json,
    Array,
    FloatVector,
}

impl DataType {
    /// 3.x 枚举成员名，即 `DataType.X.name`。
    pub fn name(self) -> &'static str {
        match self {
            Self::Bool => "BOOL",
            Self::Int8 => "INT8",
            Self::Int16 => "INT16",
            Self::Int32 => "INT32",
            Self::Int64 => "INT64",
            Self::Float => "FLOAT",
            Self::Double => "DOUBLE",
            Self::VarChar => "VARCHAR",
            Self::Json => "JSON",
            Self::Array => "ARRAY",
            Self::FloatVector => "FLOAT_VECTOR",
        }
    }

    /// 3.x 在 f-string 中直接插入枚举时的文本，即 `str(DataType.X)`。
    pub fn qualified(self) -> String {
        format!("DataType.{}", self.name())
    }

    pub fn is_vector(self) -> bool {
        self == Self::FloatVector
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldSchema {
    pub name: String,
    pub data_type: DataType,
    pub is_primary: bool,
    pub auto_id: bool,
    pub is_partition_key: bool,
    pub nullable: bool,
    pub default_value: Option<serde_json::Value>,
    /// FLOAT_VECTOR 的维度。保持有符号，以便原样报告 `dim > 0` 的校验错误。
    pub dim: Option<i64>,
    /// VARCHAR 的最大字符数；3.x 允许不设，此时不限长度。
    pub max_length: Option<i64>,
    pub element_type: Option<DataType>,
    pub max_capacity: Option<i64>,
}

impl FieldSchema {
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
            is_primary: false,
            auto_id: false,
            is_partition_key: false,
            nullable: false,
            default_value: None,
            dim: None,
            max_length: None,
            element_type: None,
            max_capacity: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CollectionSchema {
    pub fields: Vec<FieldSchema>,
    pub enable_dynamic_field: bool,
    /// collection properties，按 Python dict 的语义：key 唯一、保持首次出现的位置。
    /// 3.x 原样保存与回显，只有 `timezone` 有语义（见 [`CollectionSchema::validate`]）。
    pub properties: Vec<(String, String)>,
}

/// 3.x 的保留字段名；`iso` 与 `interval` 不区分大小写。
const RESERVED_FIELD_NAMES: [&str; 7] = [
    "$meta",
    "INTERVAL",
    "ISO",
    "_partition",
    "_seq",
    "interval",
    "iso",
];

impl CollectionSchema {
    pub fn validate(&self) -> Result<()> {
        if self.fields.is_empty() {
            return Err(EngineError::schema("schema has no fields"));
        }
        // 3.x 在这里校验并规范化 timezone；它只作用于 TIMESTAMPTZ，M1 不支持（0004「决策」）。
        if self.properties.iter().any(|(k, _)| k == "timezone") {
            return Err(EngineError::unsupported(
                "timezone property",
                "collection timezone is not supported in this build of milvus-lite-rust yet",
            ));
        }
        let mut seen = HashSet::new();
        for f in &self.fields {
            if let Some(feature) = f.default_value.as_ref().and_then(default_limit) {
                return Err(EngineError::unsupported(
                    feature,
                    format!("{feature} are not supported in this build of milvus-lite-rust yet"),
                ));
            }
            if f.name.is_empty() {
                return Err(EngineError::schema("field name must not be empty"));
            }
            // Python 的 casefold 还会把长 s（ſ）折叠为 s；对 iso / interval 而言只有这一处与 to_lowercase 不同。
            let folded = f.name.to_lowercase().replace('ſ', "s");
            if RESERVED_FIELD_NAMES.contains(&f.name.as_str())
                || folded == "iso"
                || folded == "interval"
            {
                return Err(EngineError::schema(format!(
                    "field name {} is reserved (one of {})",
                    py_repr(&f.name),
                    py_repr_list(RESERVED_FIELD_NAMES)
                )));
            }
            if !seen.insert(f.name.as_str()) {
                return Err(EngineError::schema(format!(
                    "duplicate field name: {}",
                    py_repr(&f.name)
                )));
            }
            if f.data_type == DataType::FloatVector && !f.dim.is_some_and(|d| d > 0) {
                let dim = f.dim.map_or_else(|| "None".to_owned(), |d| d.to_string());
                return Err(EngineError::schema(format!(
                    "vector field {} requires dim > 0, got {dim}",
                    py_repr(&f.name)
                )));
            }
            if f.data_type == DataType::Array {
                let element = f.element_type.ok_or_else(|| {
                    EngineError::schema(format!(
                        "ARRAY field {} requires element_type",
                        py_repr(&f.name)
                    ))
                })?;
                if matches!(
                    element,
                    DataType::Array | DataType::Json | DataType::FloatVector
                ) {
                    return Err(EngineError::unsupported(
                        "ARRAY element type",
                        format!(
                            "ARRAY element type {} is not supported in this build of milvus-lite-rust yet",
                            element.name()
                        ),
                    ));
                }
            }
        }

        let pks: Vec<&FieldSchema> = self.fields.iter().filter(|f| f.is_primary).collect();
        let pk = match pks.as_slice() {
            [] => return Err(EngineError::schema("schema has no primary key field")),
            [pk] => *pk,
            many => {
                return Err(EngineError::schema(format!(
                    "schema must have exactly one primary key field, got {}",
                    py_repr_list(many.iter().map(|f| f.name.as_str()))
                )));
            }
        };
        if !matches!(pk.data_type, DataType::VarChar | DataType::Int64) {
            return Err(EngineError::schema(format!(
                "primary key {} must be VARCHAR or INT64, got {}",
                py_repr(&pk.name),
                pk.data_type.qualified()
            )));
        }
        if pk.nullable {
            return Err(EngineError::schema(format!(
                "primary key {} must not be nullable",
                py_repr(&pk.name)
            )));
        }
        if pk.auto_id && pk.data_type != DataType::Int64 {
            return Err(EngineError::schema(format!(
                "auto_id primary key {} must be INT64, got {}",
                py_repr(&pk.name),
                pk.data_type.name()
            )));
        }

        let vectors = self
            .fields
            .iter()
            .filter(|f| f.data_type.is_vector())
            .count();
        if vectors == 0 {
            return Err(EngineError::schema(
                "schema has no vector field (FLOAT_VECTOR or SPARSE_FLOAT_VECTOR)",
            ));
        }
        let keys: Vec<_> = self.fields.iter().filter(|f| f.is_partition_key).collect();
        if keys.len() > 1 {
            return Err(EngineError::schema(format!(
                "at most one field can be partition key, got {}",
                py_repr_list(keys.iter().map(|f| f.name.as_str()))
            )));
        }
        if let Some(key) = keys.first() {
            if key.is_primary {
                return Err(EngineError::schema(format!(
                    "partition key {} must not be the primary key",
                    py_repr(&key.name)
                )));
            }
            if !matches!(key.data_type, DataType::Int64 | DataType::VarChar) {
                return Err(EngineError::schema(format!(
                    "partition key {} must be INT64 or VARCHAR, got {}",
                    py_repr(&key.name),
                    key.data_type.name()
                )));
            }
        }
        // 3.x 接受多个 FLOAT_VECTOR；M1 只支持一个（0004「不支持」）。
        if vectors > 1 {
            return Err(EngineError::unsupported(
                "multiple vector fields",
                "only one FLOAT_VECTOR field per collection is supported in this build of milvus-lite-rust yet",
            ));
        }
        if self
            .fields
            .iter()
            .any(|f| f.data_type.is_vector() && (f.nullable || f.default_value.is_some()))
        {
            return Err(EngineError::unsupported(
                "nullable vectors and vector defaults",
                "nullable vectors and vector defaults are not supported in this build of milvus-lite-rust yet",
            ));
        }
        Ok(())
    }

    pub fn partition_key(&self) -> Option<&FieldSchema> {
        self.fields.iter().find(|f| f.is_partition_key)
    }

    pub fn primary_position(&self) -> Option<usize> {
        self.fields.iter().position(|f| f.is_primary)
    }

    pub fn field(&self, name: &str) -> Option<&FieldSchema> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// 第一个 FLOAT_VECTOR 字段，与 3.x 的 `_vector_name` 相同。
    pub fn vector_field(&self) -> Option<&FieldSchema> {
        self.fields.iter().find(|f| f.data_type.is_vector())
    }
}

fn default_limit(value: &serde_json::Value) -> Option<&'static str> {
    let mut work = vec![value];
    while let Some(value) = work.pop() {
        match value {
            serde_json::Value::Number(number) if number.as_str().contains(['.', 'e', 'E']) => {
                if !number.as_str().parse::<f64>().is_ok_and(f64::is_finite) {
                    return Some("non-finite default values");
                }
            }
            serde_json::Value::Number(number)
                if number.as_str().bytes().filter(u8::is_ascii_digit).count() > 4300 =>
            {
                return Some("oversized integer default values");
            }
            serde_json::Value::Array(values) => work.extend(values),
            serde_json::Value::Object(values) => work.extend(values.values()),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_metadata_rejects_unreadable_python_integers_without_rejecting_strings() {
        let large: serde_json::Value = serde_json::from_str(&"9".repeat(4301)).unwrap();
        let mut field = FieldSchema::new("x", DataType::Int64);
        field.default_value = Some(large);
        let mut schema = CollectionSchema {
            fields: vec![pk("id"), vec("v", 2), field],
            enable_dynamic_field: false,
            properties: Vec::new(),
        };
        assert!(matches!(
            schema.validate(),
            Err(EngineError::Unsupported {
                feature: "oversized integer default values",
                ..
            })
        ));
        schema.fields[2].default_value = Some(serde_json::Value::String("0".repeat(4301)));
        assert!(schema.validate().is_ok());
    }

    fn pk(name: &str) -> FieldSchema {
        FieldSchema {
            is_primary: true,
            ..FieldSchema::new(name, DataType::Int64)
        }
    }

    fn vec(name: &str, dim: i64) -> FieldSchema {
        FieldSchema {
            dim: Some(dim),
            ..FieldSchema::new(name, DataType::FloatVector)
        }
    }

    fn schema(fields: Vec<FieldSchema>) -> CollectionSchema {
        CollectionSchema {
            fields,
            enable_dynamic_field: false,
            properties: Vec::new(),
        }
    }

    fn err(s: CollectionSchema) -> String {
        s.validate().unwrap_err().to_string()
    }

    #[test]
    fn valid_schema() {
        schema(vec![pk("id"), vec("v", 4)]).validate().unwrap();
    }

    #[test]
    fn messages_match_3x() {
        assert_eq!(err(schema(vec![])), "schema has no fields");
        assert_eq!(
            err(schema(vec![pk(""), vec("v", 2)])),
            "field name must not be empty"
        );
        assert_eq!(
            err(schema(vec![pk("id"), vec("$meta", 2)])),
            "field name '$meta' is reserved (one of ['$meta', 'INTERVAL', 'ISO', '_partition', '_seq', 'interval', 'iso'])"
        );
        assert!(
            err(schema(vec![pk("id"), vec("IsO", 2)])).starts_with("field name 'IsO' is reserved")
        );
        assert!(
            err(schema(vec![pk("id"), vec("iſo", 2)])).starts_with("field name 'iſo' is reserved")
        );
        assert_eq!(
            err(schema(vec![pk("id"), vec("v", 2), vec("v", 2)])),
            "duplicate field name: 'v'"
        );
        assert_eq!(
            err(schema(vec![pk("id"), vec("v", 0)])),
            "vector field 'v' requires dim > 0, got 0"
        );
        assert_eq!(
            err(schema(vec![
                FieldSchema::new("id", DataType::Int64),
                vec("v", 2)
            ])),
            "schema has no primary key field"
        );
        assert_eq!(
            err(schema(vec![pk("pk"), pk("pk2"), vec("v", 2)])),
            "schema must have exactly one primary key field, got ['pk', 'pk2']"
        );
        let float_pk = FieldSchema {
            is_primary: true,
            ..FieldSchema::new("pk", DataType::Float)
        };
        assert_eq!(
            err(schema(vec![float_pk, vec("v", 2)])),
            "primary key 'pk' must be VARCHAR or INT64, got DataType.FLOAT"
        );
        let str_auto = FieldSchema {
            is_primary: true,
            auto_id: true,
            ..FieldSchema::new("pk", DataType::VarChar)
        };
        assert_eq!(
            err(schema(vec![str_auto, vec("v", 2)])),
            "auto_id primary key 'pk' must be INT64, got VARCHAR"
        );
        assert_eq!(
            err(schema(vec![pk("id")])),
            "schema has no vector field (FLOAT_VECTOR or SPARSE_FLOAT_VECTOR)"
        );
    }

    #[test]
    fn multiple_vectors_are_unsupported_after_3x_checks() {
        let e = schema(vec![pk("id"), vec("a", 2), vec("b", 2)])
            .validate()
            .unwrap_err();
        assert!(matches!(e, EngineError::Unsupported { .. }), "{e}");
        // 3.x 的校验先于 M1 的限制：没有主键时报 3.x 的错误。
        let e = schema(vec![vec("a", 2), vec("b", 2)])
            .validate()
            .unwrap_err();
        assert_eq!(e.to_string(), "schema has no primary key field");
    }
}
