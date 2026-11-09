//! Collection schema 与校验。校验顺序与错误信息与 3.x 的 `validate_schema` 一致。

use std::collections::HashSet;

use crate::error::{EngineError, Result, py_repr, py_repr_list};
use crate::timezone::TimezoneSource;

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
    /// UTC Unix 微秒的 int64（0025）；列沿用 `ColumnData::Int64`，类型差异只在 schema。
    Timestamptz,
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
            Self::Timestamptz => "TIMESTAMPTZ",
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
    /// 不带数据库时区的校验：字符串形式的 TIMESTAMPTZ 默认值没有偏移时只能依靠集合自己的 `timezone` 属性。
    pub fn validate(&self) -> Result<()> {
        self.validate_with(&|| Ok(None))
    }

    /// 集合属性里的 `timezone`（原样取值；空串视为没有）。
    pub fn timezone(&self) -> Option<&str> {
        self.properties
            .iter()
            .find(|(k, _)| k == "timezone")
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    }

    /// 集合属性与数据库属性一起构成默认值的有效时区（0025）。
    fn timezone_source<'a>(
        &self,
        database: &'a dyn Fn() -> Result<Option<String>>,
    ) -> TimezoneSource<'a> {
        TimezoneSource::new(None, self.timezone().map(str::to_owned), database)
    }

    /// `database` 取数据库的 `timezone` 属性，只在没有偏移的字符串默认值需要时才调用。
    pub fn validate_with(&self, database: &dyn Fn() -> Result<Option<String>>) -> Result<()> {
        if self.fields.is_empty() {
            return Err(EngineError::schema("schema has no fields"));
        }
        // 3.x 在这里校验 timezone（值原样保存，不规范化大小写）。
        if let Some((_, value)) = self.properties.iter().find(|(k, _)| k == "timezone") {
            crate::timezone::validate_property(value)?;
        }
        let timezone = self.timezone_source(database);
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
                    DataType::Array
                        | DataType::Json
                        | DataType::FloatVector
                        | DataType::Timestamptz
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
            if f.data_type == DataType::Timestamptz
                && let Some(default) = f.default_value.as_ref().filter(|v| !v.is_null())
            {
                crate::timestamptz::check_default(default, &timezone)?;
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

    /// 校验通过后的规范化：TIMESTAMPTZ 的字符串默认值换成 UTC 微秒（3.x 在 `validate_schema` 里就地替换）。
    /// 没有偏移的默认值按集合、数据库的时区换算；之后改时区不会改变已保存的 schema。
    pub(crate) fn normalized(
        mut self,
        database: &dyn Fn() -> Result<Option<String>>,
    ) -> Result<Self> {
        let timezone = self.timezone_source(database);
        let mut converted = Vec::new();
        for (index, field) in self.fields.iter().enumerate() {
            if field.data_type == DataType::Timestamptz
                && let Some(serde_json::Value::String(text)) = &field.default_value
            {
                converted.push((
                    index,
                    serde_json::Value::from(crate::timestamptz::parse_with(text, &timezone)?),
                ));
            }
        }
        for (index, value) in converted {
            self.fields[index].default_value = Some(value);
        }
        Ok(self)
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

    /// FLOAT_VECTOR 字段数，也是内存范数每行的个数（[0028](../../../docs/decisions/0028-multi-vector-fields.md)）。
    pub(crate) fn vector_count(&self) -> usize {
        self.fields
            .iter()
            .filter(|f| f.data_type == DataType::FloatVector)
            .count()
    }

    /// `pos` 处的向量字段在所有 FLOAT_VECTOR 字段中的序号，即它在每行范数中的位置。
    pub(crate) fn vector_ordinal(&self, pos: usize) -> usize {
        self.fields[..pos]
            .iter()
            .filter(|f| f.data_type == DataType::FloatVector)
            .count()
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
    fn multiple_vectors_are_accepted_and_numbered_in_schema_order() {
        let s = schema(vec![vec("a", 2), pk("id"), vec("b", 8), vec("c", 4)]);
        s.validate().unwrap();
        assert_eq!(s.vector_count(), 3);
        assert_eq!(s.vector_ordinal(0), 0);
        assert_eq!(s.vector_ordinal(2), 1);
        assert_eq!(s.vector_ordinal(3), 2);
        assert_eq!(crate::data::max_vector_dim(&s), 8);
        let e = schema(vec![vec("a", 2), vec("b", 2)])
            .validate()
            .unwrap_err();
        assert_eq!(e.to_string(), "schema has no primary key field");
    }
}
