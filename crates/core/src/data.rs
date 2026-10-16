//! 列式数据：写入输入、存储列与读取结果。

use serde_json::{Map, Value};

use crate::error::{EngineError, Result, py_repr};
use crate::schema::{DataType, FieldSchema};
use crate::storage::visibility::SourceBits;

pub(crate) mod array;
pub use array::ArrayData;

/// 列空值独立于行的删除有效位；全部有效的列不持有此对象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Validity(SourceBits);

impl Validity {
    pub fn from_bools(values: &[bool]) -> Self {
        Self(SourceBits::from_live(values))
    }

    pub fn len(&self) -> usize {
        self.0.len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, row: usize) -> bool {
        self.0.get(row)
    }

    pub(crate) fn bits(&self) -> &SourceBits {
        &self.0
    }
}

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
    /// 每个父行是一个标量元素列；None 是父 null，子列自己的有效位表示元素 null。
    Array(Vec<Option<InputValues>>),
    Null(usize),
    JsonValues(Vec<Value>),
    Nullable {
        values: Box<InputValues>,
        validity: Validity,
    },
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
    pub fn values(&self) -> &Self {
        match self {
            Self::Nullable { values, .. } => values,
            _ => self,
        }
    }

    pub fn is_null(&self, row: usize) -> bool {
        match self {
            Self::Array(rows) => rows[row].is_none(),
            Self::Null(_) => true,
            Self::Nullable { validity, .. } => !validity.get(row),
            _ => false,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Array(rows) => rows.len(),
            Self::Null(n) => *n,
            Self::JsonValues(values) => values.len(),
            Self::Nullable { values, .. } => values.len(),
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

/// 按主键类型保存键，避免 Int64 条目承担 String 变体的空间；不改变两类键的区分。
#[derive(Debug, Clone)]
pub(crate) struct PkMap<V> {
    ints: std::collections::HashMap<i64, V>,
    strings: std::collections::HashMap<String, V>,
}

impl<V> Default for PkMap<V> {
    fn default() -> Self {
        Self {
            ints: std::collections::HashMap::new(),
            strings: std::collections::HashMap::new(),
        }
    }
}

impl<V> PkMap<V> {
    pub(crate) fn get(&self, key: &Pk) -> Option<&V> {
        match key {
            Pk::Int64(k) => self.ints.get(k),
            Pk::VarChar(k) => self.strings.get(k.as_str()),
        }
    }
    pub(crate) fn get_mut(&mut self, key: &Pk) -> Option<&mut V> {
        match key {
            Pk::Int64(k) => self.ints.get_mut(k),
            Pk::VarChar(k) => self.strings.get_mut(k.as_str()),
        }
    }
    pub(crate) fn insert(&mut self, key: Pk, value: V) -> Option<V> {
        match key {
            Pk::Int64(k) => self.ints.insert(k, value),
            Pk::VarChar(k) => self.strings.insert(k, value),
        }
    }
    pub(crate) fn remove(&mut self, key: &Pk) -> Option<V> {
        match key {
            Pk::Int64(k) => self.ints.remove(k),
            Pk::VarChar(k) => self.strings.remove(k.as_str()),
        }
    }
    pub(crate) fn len(&self) -> usize {
        self.ints.len() + self.strings.len()
    }
    pub(crate) fn reserve(&mut self, kind: DataType, additional: usize) {
        match kind {
            DataType::Int64 => self.ints.reserve(additional),
            DataType::VarChar => self.strings.reserve(additional),
            // 仅在 CollectionSchema 校验主键类型之后调用。
            _ => unreachable!("validated primary key is Int64 or VarChar"),
        }
    }
}

/// 按字段类型存储的一列。
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnData {
    Array(ArrayData),
    Nullable {
        values: Box<ColumnData>,
        validity: Validity,
    },
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
    pub fn values(&self) -> &Self {
        match self {
            Self::Nullable { values, .. } => values,
            _ => self,
        }
    }

    pub fn validity(&self) -> Option<&Validity> {
        match self {
            Self::Nullable { validity, .. } => Some(validity),
            _ => None,
        }
    }

    pub(crate) fn values_mut(&mut self) -> &mut Self {
        match self {
            Self::Nullable { values, .. } => values,
            _ => self,
        }
    }

    pub(crate) fn nulls(field: &FieldSchema, n: usize) -> Self {
        let values = match field.data_type {
            DataType::Bool => Self::Bool(vec![false; n]),
            DataType::Int8 => Self::Int8(vec![0; n]),
            DataType::Int16 => Self::Int16(vec![0; n]),
            DataType::Int32 => Self::Int32(vec![0; n]),
            DataType::Int64 => Self::Int64(vec![0; n]),
            DataType::Float => Self::Float(vec![0.0; n]),
            DataType::Double => Self::Double(vec![0.0; n]),
            DataType::VarChar => Self::VarChar(vec![String::new(); n]),
            DataType::Json => Self::Json(vec![b"null".to_vec(); n]),
            DataType::Array => Self::Array(ArrayData::empty_rows(field, n)),
            DataType::FloatVector => unreachable!("nullable vector schema is rejected"),
        };
        values.with_validity(&vec![false; n])
    }

    pub fn is_null(&self, row: usize) -> bool {
        self.validity().is_some_and(|valid| !valid.get(row))
    }

    pub(crate) fn with_validity(self, valid: &[bool]) -> Self {
        if valid.iter().all(|&value| value) {
            self
        } else {
            Self::Nullable {
                values: Box::new(self),
                validity: Validity::from_bools(valid),
            }
        }
    }

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
            DataType::Array => Self::Array(ArrayData::empty_rows(field, 0)),
            DataType::FloatVector => Self::FloatVector {
                dim: field_dim(field),
                values: Vec::new(),
            },
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Array(values) => values.len(),
            Self::Nullable { values, .. } => values.len(),
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
        if let Self::Nullable { values, validity } = self {
            values.push_row(src.values(), row);
            validity.0.push(!src.is_null(row));
            return;
        }
        if src.is_null(row) {
            let mut valid: Vec<bool> = (0..self.len()).map(|i| !self.is_null(i)).collect();
            valid.push(!src.is_null(row));
            let empty = match self.values() {
                Self::Array(values) => Self::Array(values.empty_like()),
                Self::Bool(_) => Self::Bool(Vec::new()),
                Self::Int8(_) => Self::Int8(Vec::new()),
                Self::Int16(_) => Self::Int16(Vec::new()),
                Self::Int32(_) => Self::Int32(Vec::new()),
                Self::Int64(_) => Self::Int64(Vec::new()),
                Self::Float(_) => Self::Float(Vec::new()),
                Self::Double(_) => Self::Double(Vec::new()),
                Self::VarChar(_) => Self::VarChar(Vec::new()),
                Self::Json(_) => Self::Json(Vec::new()),
                _ => unreachable!("nullable storage contains a scalar column"),
            };
            let previous = std::mem::replace(self, empty);
            let mut values = match previous {
                Self::Nullable { values, .. } => *values,
                values => values,
            };
            values.push_row(src.values(), row);
            *self = values.with_validity(&valid);
            return;
        }
        let src = src.values();
        match (self, src) {
            (Self::Array(dst), Self::Array(source)) => dst.push_row(source, row),
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
        if self.validity().is_some() || src.validity().is_some() {
            for row in 0..src.len() {
                self.push_row(src, row);
            }
            return;
        }
        macro_rules! append {
            ($($variant:ident),*) => {
                match (self, src) {
                    (Self::Array(dst), Self::Array(source)) => {
                        for row in 0..source.len() { dst.push_row(source, row); }
                    }
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
    if let InputValues::Nullable {
        values: inner,
        validity,
    } = values
    {
        if validity.len() != n
            || matches!(
                inner.as_ref(),
                InputValues::Nullable { .. } | InputValues::Null(_)
            )
        {
            return Err(EngineError::schema(format!(
                "invalid nullable input for field {}",
                py_repr(name)
            )));
        }
        return check_column_len(name, inner, n);
    }
    if let InputValues::Array(rows) = values {
        // Rust 引擎入口可直接提供元素有效位；不能只校验 gRPC 能携带的父列掩码。
        for items in rows.iter().flatten() {
            check_column_len(name, items, items.len())?;
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_child_validity_is_checked_before_conversion() {
        let items = InputValues::Nullable {
            values: Box::new(InputValues::Int(vec![1, 2])),
            validity: Validity::from_bools(&[true]),
        };
        let input = InputValues::Array(vec![None, Some(items)]);
        assert_eq!(
            check_column_len("x", &input, 2).unwrap_err().to_string(),
            "invalid nullable input for field 'x'"
        );
        let items = InputValues::Nullable {
            values: Box::new(InputValues::Int(vec![1, 2])),
            validity: Validity::from_bools(&[false, true]),
        };
        assert!(check_column_len("x", &InputValues::Array(vec![None, Some(items)]), 2).is_ok());
    }

    #[test]
    fn nullable_gather_append_and_old_clone_keep_word_boundaries() {
        let field = FieldSchema::new("x", DataType::Int64);
        let valid: Vec<_> = (0..130).map(|row| !matches!(row, 63 | 64 | 129)).collect();
        let source = ColumnData::Int64((0..130).collect()).with_validity(&valid);
        let mut gathered = ColumnData::empty(&field);
        for row in 0..130 {
            gathered.push_row(&source, row);
        }
        assert_eq!(gathered, source);
        let old = gathered.clone();
        gathered.append_column(&ColumnData::Int64(vec![999]));
        assert_eq!(old.len(), 130);
        assert_eq!(gathered.len(), 131);
        for (row, valid) in valid.iter().enumerate() {
            assert_eq!(gathered.is_null(row), !valid);
            assert_eq!(old.is_null(row), !valid);
        }
        assert!(!gathered.is_null(130));
        let mut all_valid = ColumnData::empty(&field);
        for row in [0, 1, 128, 130] {
            all_valid.push_row(&gathered, row);
        }
        assert_eq!(all_valid, ColumnData::Int64(vec![0, 1, 128, 999]));
        assert!(all_valid.validity().is_none());
    }
    use std::collections::HashMap;

    #[test]
    fn typed_primary_map_matches_enum_keyed_map_for_mixed_keys_and_mutations() {
        let keys = [
            Pk::Int64(i64::MIN),
            Pk::Int64(-1),
            Pk::Int64(0),
            Pk::Int64(7),
            Pk::Int64(i64::MAX),
            Pk::VarChar(String::new()),
            Pk::VarChar("0".into()),
            Pk::VarChar("7".into()),
            Pk::VarChar("é/東京".into()),
            Pk::VarChar("a".repeat(1000)),
        ];
        let mut actual = PkMap::default();
        let mut expected = HashMap::new();
        let mut seed = 0x91f0_u64;
        for step in 0..2000_i64 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let key = &keys[((seed >> 16) as usize) % keys.len()];
            match (seed >> 32) % 4 {
                0 => assert_eq!(
                    actual.insert(key.clone(), step),
                    expected.insert(key.clone(), step)
                ),
                1 => assert_eq!(actual.remove(key), expected.remove(key)),
                2 => assert_eq!(
                    actual.get_mut(key).map(|v| {
                        *v += 1;
                        *v
                    }),
                    expected.get_mut(key).map(|v| {
                        *v += 1;
                        *v
                    }),
                ),
                _ => {
                    let mut clone = actual.clone();
                    assert_eq!(clone.remove(key), expected.get(key).copied());
                    assert_eq!(clone.get(key), None);
                }
            }
            assert_eq!(actual.len(), expected.len());
            for key in &keys {
                assert_eq!(actual.get(key), expected.get(key));
            }
        }
    }

    #[test]
    fn primary_reservation_selects_one_key_table_and_strings_move_without_copy() {
        let mut ints = PkMap::<usize>::default();
        ints.reserve(DataType::Int64, 100);
        assert!(ints.ints.capacity() >= 100);
        assert_eq!(ints.strings.capacity(), 0);
        let mut strings = PkMap::<usize>::default();
        strings.reserve(DataType::VarChar, 100);
        assert!(strings.strings.capacity() >= 100);
        assert_eq!(strings.ints.capacity(), 0);
        let key = "é/東京".to_owned();
        let original = key.as_ptr();
        strings.insert(Pk::VarChar(key), 3);
        assert_eq!(strings.strings.keys().next().unwrap().as_ptr(), original);
        assert_eq!(strings.get(&Pk::VarChar("é/東京".into())), Some(&3));
        assert!(std::mem::size_of::<(i64, usize)>() < std::mem::size_of::<(Pk, usize)>());
    }
}
