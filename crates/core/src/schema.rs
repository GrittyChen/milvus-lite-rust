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
    /// 校验过的 WKT 原文（0034）；列沿用 `ColumnData::VarChar`，类型差异只在 schema。
    Geometry,
    FloatVector,
    /// 每行是按维度排序的 `<uint32, float32>` 小端对（0030）。
    SparseFloatVector,
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
            Self::Geometry => "GEOMETRY",
            Self::FloatVector => "FLOAT_VECTOR",
            Self::SparseFloatVector => "SPARSE_FLOAT_VECTOR",
        }
    }

    /// 3.x 在 f-string 中直接插入枚举时的文本，即 `str(DataType.X)`。
    pub fn qualified(self) -> String {
        format!("DataType.{}", self.name())
    }

    /// FLOAT_VECTOR：有 dim、范数，可作为默认检索字段（3.x 的 `_vector_name`）。
    pub fn is_dense_vector(self) -> bool {
        self == Self::FloatVector
    }

    pub fn is_sparse_vector(self) -> bool {
        self == Self::SparseFloatVector
    }

    /// 3.x 的 `all_vector_fields`：可作为检索字段、建索引时必须给度量。
    pub fn is_any_vector(self) -> bool {
        matches!(self, Self::FloatVector | Self::SparseFloatVector)
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
    pub enable_analyzer: bool,
    pub enable_match: bool,
    /// 3.x 保存任意 JSON 真值，不要求是对象；回显、持久化，BM25 输入字段用它构造分词器，`text_match` 不读它。
    pub analyzer_params: Option<serde_json::Value>,
    /// 解析成功、但为假值所以不保存也不回显的 `analyzer_params`（不含 null 与空对象，它们与没有设置等价）。
    /// 3.x 把原值交给 `create_analyzer`，BM25 输入字段上这些值报错；建表或打开成功时它必然为空。
    pub unsaved_analyzer_params: Option<serde_json::Value>,
    /// 3.x `FieldSchema.is_function_output`：函数的输出字段，或 proto 上带了这个标记的字段。
    pub is_function_output: bool,
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
            enable_analyzer: false,
            enable_match: false,
            analyzer_params: None,
            unsaved_analyzer_params: None,
            is_function_output: false,
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
    /// 3.x `CollectionSchema.functions`，按声明顺序。
    pub functions: Vec<Function>,
}

/// 3.x 的 BM25 function 类型号（proto `FunctionType.BM25`）。
pub const FUNCTION_BM25: i64 = 1;

/// 3.x `Function`。参数逐个按 `json.loads` 解析，失败时保留原字符串（`_decode_function`）。
#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub name: String,
    pub function_type: i64,
    pub input_field_names: Vec<String>,
    pub output_field_names: Vec<String>,
    pub params: serde_json::Map<String, serde_json::Value>,
}

/// 一个 BM25 function 的输入、输出位置与分词器。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Bm25Function {
    pub(crate) input: usize,
    pub(crate) output: usize,
    pub(crate) analyzer: crate::analyzer::Bm25Analyzer,
}

/// 3.x 用 `_validate_sparse_vector` 校验默认值。gRPC 的默认值只能是标量，得到 3.x 的类型错误；dict 只能来自
/// 3.x Python API 写的 schema，按登记的差异拒绝（0030）。
fn check_sparse_default(name: &str, value: &serde_json::Value) -> Result<()> {
    use serde_json::Value;
    let kind = match value {
        Value::Object(_) => {
            return Err(EngineError::unsupported(
                "sparse vector default values",
                format!(
                    "default value of sparse vector field {} is not supported in this build of milvus-lite-rust",
                    py_repr(name)
                ),
            ));
        }
        other => crate::pyjson::python_type_name(other),
    };
    Err(EngineError::schema(format!(
        "sparse vector field {}: sparse vector must be a dict, got {kind}",
        py_repr(name)
    )))
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
            if f.data_type == DataType::SparseFloatVector
                && let Some(default) = f.default_value.as_ref().filter(|v| !v.is_null())
            {
                check_sparse_default(&f.name, default)?;
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
                        | DataType::SparseFloatVector
                        | DataType::Timestamptz
                        | DataType::Geometry
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
            // 3.x 对默认值调用 `validate_geometry_wkt`：不是字符串或不是合法 WKT 都报同一条。
            if f.data_type == DataType::Geometry
                && let Some(default) = f.default_value.as_ref().filter(|v| !v.is_null())
                && !default.as_str().is_some_and(crate::geometry::is_valid)
            {
                return Err(EngineError::schema(format!(
                    "GEOMETRY field {} default_value is invalid",
                    py_repr(&f.name)
                )));
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
            .filter(|f| f.data_type.is_any_vector())
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
        self.validate_functions()?;
        self.bm25_functions()?;
        if self
            .fields
            .iter()
            .any(|f| f.data_type.is_dense_vector() && (f.nullable || f.default_value.is_some()))
        {
            return Err(EngineError::unsupported(
                "nullable vectors and vector defaults",
                "nullable vectors and vector defaults are not supported in this build of milvus-lite-rust yet",
            ));
        }
        Ok(())
    }

    /// 3.x `validate_schema` 末尾的函数校验，顺序与文案一致。
    fn validate_functions(&self) -> Result<()> {
        let mut names = HashSet::new();
        let mut outputs = HashSet::new();
        for function in &self.functions {
            if !names.insert(function.name.as_str()) {
                return Err(EngineError::schema(format!(
                    "duplicate function name {}",
                    py_repr(&function.name)
                )));
            }
            for output in &function.output_field_names {
                if !outputs.insert(output.as_str()) {
                    return Err(EngineError::schema(format!(
                        "function output field {} is used by multiple functions",
                        py_repr(output)
                    )));
                }
            }
            self.validate_function(function)?;
        }
        for f in &self.fields {
            if f.data_type == DataType::SparseFloatVector
                && f.is_function_output
                && !outputs.contains(f.name.as_str())
            {
                return Err(EngineError::schema(format!(
                    "sparse function output field {} has no producing Function",
                    py_repr(&f.name)
                )));
            }
        }
        // 3.x 接受标了输出却没有函数产出的非稀疏字段，写入时把它当作可以缺失；本实现不支持这种 schema。
        if self
            .fields
            .iter()
            .any(|f| f.is_function_output && !outputs.contains(f.name.as_str()))
        {
            return Err(EngineError::unsupported(
                "functions",
                "functions are not supported in this build of milvus-lite-rust yet",
            ));
        }
        Ok(())
    }

    /// 3.x `_validate_function` 的 BM25 分支；其他类型只能来自引擎接口，gRPC 与 `schema.json` 已先行拒绝。
    fn validate_function(&self, function: &Function) -> Result<()> {
        if function.function_type != FUNCTION_BM25 {
            return Err(EngineError::unsupported(
                "functions",
                "functions are not supported in this build of milvus-lite-rust yet",
            ));
        }
        let name = py_repr(&function.name);
        let [input] = function.input_field_names.as_slice() else {
            return Err(EngineError::schema(format!(
                "BM25 function {name} requires exactly one input field"
            )));
        };
        let Some(field) = self.field(input) else {
            return Err(EngineError::schema(format!(
                "BM25 function {name} input field {} not found in schema",
                py_repr(input)
            )));
        };
        if field.data_type != DataType::VarChar {
            return Err(EngineError::schema(format!(
                "BM25 function {name} input field {} must be VARCHAR, got {}",
                py_repr(input),
                field.data_type.name()
            )));
        }
        if !field.enable_analyzer {
            return Err(EngineError::schema(format!(
                "BM25 function {name} input field {} must have enable_analyzer=True",
                py_repr(input)
            )));
        }
        let [output] = function.output_field_names.as_slice() else {
            return Err(EngineError::schema(format!(
                "BM25 function {name} requires exactly one output field"
            )));
        };
        let Some(field) = self.field(output) else {
            return Err(EngineError::schema(format!(
                "BM25 function {name} output field {} not found in schema",
                py_repr(output)
            )));
        };
        if field.data_type != DataType::SparseFloatVector {
            return Err(EngineError::schema(format!(
                "BM25 function {name} output field {} must be SPARSE_FLOAT_VECTOR, got {}",
                py_repr(output),
                field.data_type.name()
            )));
        }
        Ok(())
    }

    /// 已校验 schema 的 BM25 function 与分词器（3.x 构造 Collection 时按函数顺序 `create_analyzer`）。
    pub(crate) fn bm25_functions(&self) -> Result<Vec<Bm25Function>> {
        let position = |name: &str| {
            self.fields
                .iter()
                .position(|f| f.name == name)
                .ok_or_else(|| EngineError::schema(format!("field {} not found", py_repr(name))))
        };
        self.functions
            .iter()
            .filter(|f| f.function_type == FUNCTION_BM25)
            .map(|function| {
                fn first<'a>(function: &Function, names: &'a [String]) -> Result<&'a str> {
                    names.first().map(String::as_str).ok_or_else(|| {
                        EngineError::schema(format!(
                            "BM25 function {} is not validated",
                            py_repr(&function.name)
                        ))
                    })
                }
                let input = position(first(function, &function.input_field_names)?)?;
                let output = position(first(function, &function.output_field_names)?)?;
                let field = &self.fields[input];
                let params = field
                    .analyzer_params
                    .as_ref()
                    .or(field.unsaved_analyzer_params.as_ref());
                Ok(Bm25Function {
                    input,
                    output,
                    analyzer: crate::analyzer::Bm25Analyzer::from_params(params)?,
                })
            })
            .collect()
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
        // 3.x 在翻译时给函数的输出字段打上标记；引擎接口建的 schema 在这里补上，两条入口保存的相同。
        for field in &mut self.fields {
            if self
                .functions
                .iter()
                .any(|f| f.output_field_names.contains(&field.name))
            {
                field.is_function_output = true;
            }
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
        self.fields.iter().find(|f| f.data_type.is_dense_vector())
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

    fn sparse(name: &str) -> FieldSchema {
        FieldSchema::new(name, DataType::SparseFloatVector)
    }

    /// 3.x `validate_schema`：稀疏字段算作向量字段，可以 nullable；默认值经 `_validate_sparse_vector` 校验，
    /// gRPC 只能给出标量默认值，dict 默认值按登记的差异拒绝（0030）。
    #[test]
    fn sparse_fields_count_as_vectors_and_check_defaults_like_3x() {
        let mut nullable = sparse("sp");
        nullable.nullable = true;
        let only_sparse = CollectionSchema {
            functions: Vec::new(),
            fields: vec![pk("id"), nullable],
            enable_dynamic_field: false,
            properties: Vec::new(),
        };
        only_sparse.validate().unwrap();
        assert!(only_sparse.vector_field().is_none());
        assert_eq!(only_sparse.vector_count(), 0);

        let with_default = |value: serde_json::Value| {
            let mut field = sparse("sp");
            field.default_value = Some(value);
            CollectionSchema {
                functions: Vec::new(),
                fields: vec![pk("id"), vec("v", 2), field],
                enable_dynamic_field: false,
                properties: Vec::new(),
            }
            .validate()
            .unwrap_err()
            .to_string()
        };
        for (value, kind) in [
            (serde_json::json!(5), "int"),
            (serde_json::json!(0.5), "float"),
            (serde_json::json!("x"), "str"),
            (serde_json::json!(true), "bool"),
        ] {
            assert_eq!(
                with_default(value),
                format!("sparse vector field 'sp': sparse vector must be a dict, got {kind}")
            );
        }
        assert!(
            with_default(serde_json::json!({"1": 0.5}))
                .contains("default value of sparse vector field 'sp' is not supported")
        );

        let mut dense = vec("v", 2);
        dense.nullable = true;
        assert!(matches!(
            CollectionSchema {
                functions: Vec::new(),
                fields: vec![pk("id"), dense],
                enable_dynamic_field: false,
                properties: Vec::new(),
            }
            .validate(),
            Err(EngineError::Unsupported { .. })
        ));
    }

    #[test]
    fn default_metadata_rejects_unreadable_python_integers_without_rejecting_strings() {
        let large: serde_json::Value = crate::json_text::value_str(&"9".repeat(4301)).unwrap();
        let mut field = FieldSchema::new("x", DataType::Int64);
        field.default_value = Some(large);
        let mut schema = CollectionSchema {
            functions: Vec::new(),
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
            functions: Vec::new(),
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
