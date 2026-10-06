//! 列式数据：写入输入、存储列与读取结果。

use serde_json::{Map, Value};

use crate::error::{EngineError, Result, py_repr};
use crate::schema::{DataType, FieldSchema};

/// 主键值。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Pk {
    Int64(i64),
    VarChar(String),
}

/// 写入时一列的原始值。按值的种类区分（对应 3.x 解码后的 Python 值类型），
/// 字段类型的检查与转换在引擎里做，与 3.x 的 `validate_record` 一致。
#[derive(Debug, Clone, PartialEq)]
pub enum InputValues {
    Bool(Vec<bool>),
    /// proto 的 int_data / long_data。
    Int(Vec<i64>),
    /// proto 的 float_data（已扩展为 f64）/ double_data。
    Float(Vec<f64>),
    Str(Vec<String>),
    /// proto 的 json_data：每行一段 JSON 文本。
    Json(Vec<Vec<u8>>),
    FloatVector {
        dim: usize,
        values: Vec<f32>,
    },
}

impl InputValues {
    pub fn len(&self) -> usize {
        match self {
            Self::Bool(v) => v.len(),
            Self::Int(v) => v.len(),
            Self::Float(v) => v.len(),
            Self::Str(v) => v.len(),
            Self::Json(v) => v.len(),
            Self::FloatVector { dim, values } => values.len().checked_div(*dim).unwrap_or(0),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 名为 `$meta` 的列承载动态字段（每行一个 JSON 对象）；动态 collection 中不在 schema 里的
/// 其他列也作为动态字段。列按请求中的顺序排列：同名动态 key 以后出现者为准，与 3.x 一致。
pub const META_FIELD: &str = "$meta";

#[derive(Debug, Clone, PartialEq)]
pub struct InsertBatch {
    pub num_rows: usize,
    pub columns: Vec<(String, InputValues)>,
}

/// 按字段类型存储的一列。
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnData {
    Bool(Vec<bool>),
    Int8(Vec<i8>),
    Int16(Vec<i16>),
    Int32(Vec<i32>),
    Int64(Vec<i64>),
    Float(Vec<f32>),
    Double(Vec<f64>),
    VarChar(Vec<String>),
    /// 校验过的 JSON 文本；持久化读取视图与 Arrow 批使用同一文本（0007）。
    Json(Vec<Vec<u8>>),
    FloatVector {
        dim: usize,
        values: Vec<f32>,
    },
}

impl ColumnData {
    pub(crate) fn empty(field: &FieldSchema) -> Self {
        match field.data_type {
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::Int8 => Self::Int8(Vec::new()),
            DataType::Int16 => Self::Int16(Vec::new()),
            DataType::Int32 => Self::Int32(Vec::new()),
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float => Self::Float(Vec::new()),
            DataType::Double => Self::Double(Vec::new()),
            DataType::VarChar => Self::VarChar(Vec::new()),
            DataType::Json => Self::Json(Vec::new()),
            DataType::FloatVector => Self::FloatVector {
                dim: field_dim(field),
                values: Vec::new(),
            },
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Bool(v) => v.len(),
            Self::Int8(v) => v.len(),
            Self::Int16(v) => v.len(),
            Self::Int32(v) => v.len(),
            Self::Int64(v) => v.len(),
            Self::Float(v) => v.len(),
            Self::Double(v) => v.len(),
            Self::VarChar(v) => v.len(),
            Self::Json(v) => v.len(),
            Self::FloatVector { dim, values } => values.len().checked_div(*dim).unwrap_or(0),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 把 `src` 的第 `row` 行追加到本列。两列来自同一 schema 字段，变体必然相同。
    pub(crate) fn push_row(&mut self, src: &ColumnData, row: usize) {
        match (self, src) {
            (Self::Bool(d), Self::Bool(s)) => d.push(s[row]),
            (Self::Int8(d), Self::Int8(s)) => d.push(s[row]),
            (Self::Int16(d), Self::Int16(s)) => d.push(s[row]),
            (Self::Int32(d), Self::Int32(s)) => d.push(s[row]),
            (Self::Int64(d), Self::Int64(s)) => d.push(s[row]),
            (Self::Float(d), Self::Float(s)) => d.push(s[row]),
            (Self::Double(d), Self::Double(s)) => d.push(s[row]),
            (Self::VarChar(d), Self::VarChar(s)) => d.push(s[row].clone()),
            (Self::Json(d), Self::Json(s)) => d.push(s[row].clone()),
            (Self::FloatVector { dim, values: d }, Self::FloatVector { values: s, .. }) => {
                d.extend_from_slice(&s[row * *dim..(row + 1) * *dim])
            }
            // 同一字段的存储列与输出列都由 ColumnData::empty(field) 创建，变体一致。
            _ => unreachable!("column type mismatch within one schema field"),
        }
    }

    /// 同一 schema 的相邻块只拼接，保留失效行与物理行号。
    pub(crate) fn append_column(&mut self, src: &ColumnData) {
        macro_rules! append {
            ($($variant:ident),*) => {
                match (self, src) {
                    $((Self::$variant(dst), Self::$variant(source)) => dst.extend_from_slice(source),)*
                    (Self::FloatVector { dim, values: dst }, Self::FloatVector { dim: other, values: source }) => {
                        assert_eq!(dim, other, "columns of one field have the same dimension");
                        dst.extend_from_slice(source);
                    }
                    _ => unreachable!("columns of one field have the same type"),
                }
            };
        }
        append!(
            Bool, Int8, Int16, Int32, Int64, Float, Double, VarChar, Json
        );
    }
}

/// 校验通过的 schema 中 FLOAT_VECTOR 的 dim 必为正数。
pub(crate) fn field_dim(field: &FieldSchema) -> usize {
    field.dim.and_then(|d| usize::try_from(d).ok()).unwrap_or(0)
}

/// 读取结果：主键列、按 schema 顺序的字段列，以及开启动态字段时每行的动态字段。
#[derive(Debug, Clone, PartialEq)]
pub struct Rows {
    pub pks: Vec<Pk>,
    pub fields: Vec<(String, ColumnData)>,
    /// 开启动态字段时每行一个对象（按投影过滤后，可能为空对象）；未开启时为 None。
    pub meta: Option<Vec<Map<String, Value>>>,
}

impl Rows {
    pub fn len(&self) -> usize {
        self.pks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pks.is_empty()
    }
}

/// 3.x 在解码 FieldData 时的行数检查（`translators/records.py`）。
pub fn check_column_len(name: &str, values: &InputValues, n: usize) -> Result<()> {
    if let InputValues::FloatVector { dim, values } = values {
        let expected = n.saturating_mul(*dim);
        if *dim == 0 || values.len() != expected {
            return Err(EngineError::schema(format!(
                "FieldData {} float_vector data has {} elements, expected num_rows({n}) * dim({dim}) = {expected}",
                py_repr(name),
                values.len()
            )));
        }
        return Ok(());
    }
    if values.len() != n {
        return Err(EngineError::schema(format!(
            "FieldData {} has {} rows, expected {n}",
            py_repr(name),
            values.len()
        )));
    }
    Ok(())
}
