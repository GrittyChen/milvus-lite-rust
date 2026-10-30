//! 3.x `schema.json` 的完整文件形状；是否支持某个 schema 由引擎另行判定。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::schema::{CollectionSchema, DataType, FieldSchema};

const SCHEMA_FORMAT_VERSION: u32 = 2;

fn format_version() -> u32 {
    SCHEMA_FORMAT_VERSION
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedFunction {
    pub name: String,
    pub function_type: i64,
    #[serde(default)]
    pub input_field_names: Vec<String>,
    #[serde(default)]
    pub output_field_names: Vec<String>,
    #[serde(default)]
    pub params: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedField {
    pub name: String,
    pub dtype: String,
    #[serde(default)]
    pub is_primary: bool,
    #[serde(default)]
    pub auto_id: bool,
    #[serde(default)]
    pub dim: Option<i64>,
    #[serde(default)]
    pub max_length: Option<i64>,
    #[serde(default)]
    pub element_type: Option<String>,
    #[serde(default)]
    pub max_capacity: Option<i64>,
    #[serde(default)]
    pub nullable: bool,
    #[serde(default)]
    pub default_value: Option<Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub enable_analyzer: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analyzer_params: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub enable_match: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_function_output: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_partition_key: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaFile {
    pub collection_name: String,
    #[serde(default = "format_version")]
    pub schema_format_version: u32,
    pub version: u64,
    #[serde(default)]
    pub enable_dynamic_field: bool,
    #[serde(default)]
    pub properties: Map<String, Value>,
    pub fields: Vec<PersistedField>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub functions: Vec<PersistedFunction>,
}

#[derive(Debug, thiserror::Error)]
pub enum SchemaFileError {
    #[error("cannot read schema {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid schema file: {0}")]
    Invalid(String),
    #[error("schema is outside M2 scope: {0}")]
    Unsupported(String),
    #[error("cannot encode schema file: {0}")]
    Encode(#[from] serde_json::Error),
}

impl SchemaFile {
    pub fn from_core(collection_name: &str, schema: &CollectionSchema) -> Self {
        let fields = schema
            .fields
            .iter()
            .map(|field| PersistedField {
                name: field.name.clone(),
                dtype: dtype_name(field.data_type).to_owned(),
                is_primary: field.is_primary,
                auto_id: field.auto_id,
                dim: field.dim,
                max_length: field.max_length,
                element_type: field.element_type.map(|t| dtype_name(t).to_owned()),
                max_capacity: field.max_capacity,
                nullable: field.nullable,
                default_value: field.default_value.clone(),
                enable_analyzer: false,
                analyzer_params: None,
                enable_match: false,
                is_function_output: false,
                is_partition_key: field.is_partition_key,
            })
            .collect();
        let properties = schema
            .properties
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect();
        Self {
            collection_name: collection_name.to_owned(),
            schema_format_version: SCHEMA_FORMAT_VERSION,
            version: 1,
            enable_dynamic_field: schema.enable_dynamic_field,
            properties,
            fields,
            functions: Vec::new(),
        }
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, SchemaFileError> {
        let schema: Self = serde_json::from_slice(bytes)
            .map_err(|source| SchemaFileError::Invalid(source.to_string()))?;
        if schema.schema_format_version != SCHEMA_FORMAT_VERSION {
            return Err(SchemaFileError::Invalid(format!(
                "unsupported schema_format_version {}",
                schema.schema_format_version
            )));
        }
        for field in &schema.fields {
            if !known_dtype(&field.dtype)
                || field
                    .element_type
                    .as_deref()
                    .is_some_and(|s| !known_dtype(s))
            {
                return Err(SchemaFileError::Invalid(format!(
                    "unknown dtype or element_type for field {:?}",
                    field.name
                )));
            }
        }
        for function in &schema.functions {
            if !matches!(function.function_type, 1..=3) {
                return Err(SchemaFileError::Invalid(format!(
                    "unknown function_type {}",
                    function.function_type
                )));
            }
        }
        Ok(schema)
    }

    /// 完整解析与支持范围判定分开：范围外的 schema 仍可供 DescribeCollection 编码与 drop。
    pub fn to_core(&self) -> Result<CollectionSchema, SchemaFileError> {
        if !self.functions.is_empty() {
            return Err(SchemaFileError::Unsupported("schema functions".to_owned()));
        }
        if self.properties.contains_key("timezone") {
            return Err(SchemaFileError::Unsupported("timezone property".to_owned()));
        }
        let properties = self
            .properties
            .iter()
            .map(|(key, value)| {
                value
                    .as_str()
                    .map(|value| (key.clone(), value.to_owned()))
                    .ok_or_else(|| {
                        SchemaFileError::Unsupported(format!("non-string property {key:?}"))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let fields = self
            .fields
            .iter()
            .map(|field| {
                if (field.dtype == "float_vector"
                    && (field.nullable || field.default_value.is_some()))
                    || (field.dtype != "array"
                        && (field.element_type.is_some() || field.max_capacity.is_some()))
                    || field.enable_analyzer
                    || field.analyzer_params.is_some()
                    || field.enable_match
                    || field.is_function_output
                {
                    return Err(SchemaFileError::Unsupported(format!(
                        "field {:?} has unsupported attributes",
                        field.name
                    )));
                }
                let dtype = match field.dtype.as_str() {
                    "bool" => DataType::Bool,
                    "int8" => DataType::Int8,
                    "int16" => DataType::Int16,
                    "int32" => DataType::Int32,
                    "int64" => DataType::Int64,
                    "float" => DataType::Float,
                    "double" => DataType::Double,
                    "varchar" => DataType::VarChar,
                    "json" => DataType::Json,
                    "array" => DataType::Array,
                    "float_vector" => DataType::FloatVector,
                    _ => {
                        return Err(SchemaFileError::Unsupported(format!(
                            "field {:?} has dtype {:?}",
                            field.name, field.dtype
                        )));
                    }
                };
                Ok(FieldSchema {
                    nullable: field.nullable,
                    default_value: field.default_value.clone(),
                    name: field.name.clone(),
                    data_type: dtype,
                    is_primary: field.is_primary,
                    is_partition_key: field.is_partition_key,
                    auto_id: field.auto_id,
                    dim: field.dim,
                    max_length: field.max_length,
                    element_type: field.element_type.as_deref().and_then(core_dtype),
                    max_capacity: field.max_capacity,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let schema = CollectionSchema {
            fields,
            enable_dynamic_field: self.enable_dynamic_field,
            properties,
        };
        schema
            .validate()
            .map_err(|source| SchemaFileError::Unsupported(source.to_string()))?;
        Ok(schema)
    }

    pub fn read(path: &Path) -> Result<Self, SchemaFileError> {
        let bytes = std::fs::read(path).map_err(|source| SchemaFileError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&bytes)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, SchemaFileError> {
        Ok(crate::pyjson::dump_file(&serde_json::to_value(self)?).into_bytes())
    }
}

fn dtype_name(dtype: DataType) -> &'static str {
    match dtype {
        DataType::Bool => "bool",
        DataType::Int8 => "int8",
        DataType::Int16 => "int16",
        DataType::Int32 => "int32",
        DataType::Int64 => "int64",
        DataType::Float => "float",
        DataType::Double => "double",
        DataType::VarChar => "varchar",
        DataType::Json => "json",
        DataType::Array => "array",
        DataType::FloatVector => "float_vector",
    }
}

fn core_dtype(name: &str) -> Option<DataType> {
    Some(match name {
        "bool" => DataType::Bool,
        "int8" => DataType::Int8,
        "int16" => DataType::Int16,
        "int32" => DataType::Int32,
        "int64" => DataType::Int64,
        "float" => DataType::Float,
        "double" => DataType::Double,
        "varchar" => DataType::VarChar,
        "json" => DataType::Json,
        "array" => DataType::Array,
        "float_vector" => DataType::FloatVector,
        _ => return None,
    })
}

fn known_dtype(dtype: &str) -> bool {
    matches!(
        dtype,
        "bool"
            | "int8"
            | "int16"
            | "int32"
            | "int64"
            | "float"
            | "double"
            | "varchar"
            | "json"
            | "array"
            | "geometry"
            | "timestamptz"
            | "float_vector"
            | "sparse_float_vector"
    )
}

#[cfg(test)]
mod tests {
    use crate::schema::{CollectionSchema, FieldSchema};

    use super::*;

    #[test]
    fn writes_3x_field_shape() {
        let mut pk = FieldSchema::new("id", DataType::Int64);
        pk.is_primary = true;
        let mut vector = FieldSchema::new("vector", DataType::FloatVector);
        vector.dim = Some(2);
        let source = CollectionSchema {
            fields: vec![pk, vector],
            enable_dynamic_field: false,
            properties: Vec::new(),
        };
        let schema = SchemaFile::from_core("c", &source);
        let value: Value = serde_json::from_slice(&schema.to_bytes().unwrap()).unwrap();
        assert_eq!(value["schema_format_version"], 2);
        assert_eq!(value["version"], 1);
        assert_eq!(value["fields"][0]["dtype"], "int64");
        assert_eq!(value["fields"][0]["default_value"], Value::Null);
        assert_eq!(value["fields"][1]["dim"], 2);
        assert!(value.get("functions").is_none());
        assert!(SchemaFile::parse(&schema.to_bytes().unwrap()).is_ok());
    }

    #[test]
    fn preserves_known_but_unsupported_fields() {
        let bytes = br#"{"collection_name":"c","version":1,"fields":[{"name":"arr","dtype":"array","element_type":"int64","nullable":true}]}"#;
        let schema = SchemaFile::parse(bytes).unwrap();
        assert_eq!(schema.fields[0].dtype, "array");
        assert!(schema.fields[0].nullable);
        assert!(matches!(
            schema.to_core(),
            Err(SchemaFileError::Unsupported(_))
        ));
    }

    #[test]
    fn rejects_unknown_keys_and_dtype() {
        let bytes = br#"{"collection_name":"c","version":1,"fields":[],"new_key":1}"#;
        assert!(SchemaFile::parse(bytes).is_err());
        let bytes =
            br#"{"collection_name":"c","version":1,"fields":[{"name":"x","dtype":"future"}]}"#;
        assert!(SchemaFile::parse(bytes).is_err());
    }

    #[test]
    fn writes_the_same_bytes_as_3x_save_schema() {
        for fixture in [
            &include_bytes!("../../tests/fixtures/schema_3x.json")[..],
            &include_bytes!("../../tests/fixtures/schema_canonical_3x.json")[..],
        ] {
            let schema = SchemaFile::parse(fixture).unwrap();
            assert_eq!(schema.to_bytes().unwrap(), fixture);
            let rebuilt =
                SchemaFile::from_core(&schema.collection_name, &schema.to_core().unwrap());
            assert_eq!(rebuilt.to_bytes().unwrap(), fixture);
        }
    }

    #[test]
    fn supported_schema_converts_to_engine_type() {
        let schema =
            SchemaFile::parse(include_bytes!("../../tests/fixtures/schema_3x.json")).unwrap();
        let core = schema.to_core().unwrap();
        assert_eq!(core.fields[0].data_type, DataType::Int64);
        assert_eq!(core.fields[1].data_type, DataType::FloatVector);
    }
}
