//! rerank 候选行里的 Python 值：转换、比较与算术都要给出 3.x 的结果或异常文案。

use std::cmp::Ordering;

use serde_json::Value;

use crate::data::ColumnData;
use crate::error::{EngineError, Result, py_repr};
use crate::filter::{int_cmp, int_float_cmp, python_decimal_digit};
use crate::schema::{DataType, FieldSchema};

/// 一个 Python 值。`None` 就是 Python 的 `None`。
#[derive(Debug, Clone, PartialEq)]
pub enum PyVal {
    None,
    Bool(bool),
    Int(i64),
    /// 超出 int64 的 Python 整数（动态字段可以写入），保留十进制文本。
    BigInt(String),
    Float(f64),
    Str(String),
    List(Vec<PyVal>),
    Dict(Vec<(String, PyVal)>),
    /// `datetime`（TIMESTAMPTZ 字段）：UTC 微秒。
    DateTime(i64),
}

const TOO_LARGE: &str = "int too large to convert to float";

impl PyVal {
    /// 3.x 候选行里 schema 字段的值：JSON 字段是存储文本，TIMESTAMPTZ 是带时区的 `datetime`。
    pub(crate) fn from_field(field: &FieldSchema, column: &ColumnData, row: usize) -> Result<Self> {
        if field.data_type == DataType::Timestamptz
            && !column.is_null(row)
            && let ColumnData::Int64(micros) = column.values()
        {
            return Ok(Self::DateTime(micros[row]));
        }
        Self::from_column(column, row)
    }

    fn from_column(column: &ColumnData, row: usize) -> Result<Self> {
        if column.is_null(row) {
            return Ok(Self::None);
        }
        Ok(match column.values() {
            ColumnData::Bool(v) => Self::Bool(v[row]),
            ColumnData::Int8(v) => Self::Int(i64::from(v[row])),
            ColumnData::Int16(v) => Self::Int(i64::from(v[row])),
            ColumnData::Int32(v) => Self::Int(i64::from(v[row])),
            ColumnData::Int64(v) => Self::Int(v[row]),
            ColumnData::Float(v) => Self::Float(f64::from(v[row])),
            ColumnData::Double(v) => Self::Float(v[row]),
            ColumnData::VarChar(v) => Self::Str(v[row].clone()),
            ColumnData::Json(v) => Self::Str(
                std::str::from_utf8(&v[row])
                    .map_err(|e| EngineError::Storage(e.to_string()))?
                    .to_owned(),
            ),
            ColumnData::FloatVector { dim, values } => Self::List(
                values[row * dim..(row + 1) * dim]
                    .iter()
                    .map(|v| Self::Float(f64::from(*v)))
                    .collect(),
            ),
            ColumnData::Array(array) => {
                let mut items = Vec::with_capacity(array.range(row).len());
                for index in array.range(row) {
                    items.push(Self::from_column(&array.values, index)?);
                }
                Self::List(items)
            }
            ColumnData::Nullable { .. } => {
                return Err(EngineError::Storage("nested nullable column".into()));
            }
        })
    }

    /// `json.loads` 的结果。
    pub(crate) fn from_json(value: &Value) -> Self {
        match value {
            Value::Null => Self::None,
            Value::Bool(b) => Self::Bool(*b),
            Value::Number(n) => {
                let text = n.as_str();
                if text.contains(['.', 'e', 'E']) {
                    Self::Float(text.parse().unwrap_or(f64::INFINITY))
                } else {
                    text.parse()
                        .map_or_else(|_| Self::BigInt(text.to_owned()), Self::Int)
                }
            }
            Value::String(s) => Self::Str(s.clone()),
            Value::Array(items) => Self::List(items.iter().map(Self::from_json).collect()),
            Value::Object(map) => Self::Dict(
                map.iter()
                    .map(|(k, v)| (k.clone(), Self::from_json(v)))
                    .collect(),
            ),
        }
    }

    /// Python `str(v)`；响应编码把盖掉 `$id` 的值写进 VARCHAR 主键时用。
    pub fn py_str(&self) -> String {
        match self {
            Self::Str(s) => s.clone(),
            Self::DateTime(micros) => crate::timestamptz::python_str(*micros),
            other => other.py_repr(),
        }
    }

    /// Python `repr(v)`。不经 JSON：JSON 写不出 NaN、无穷和宽整数，而它们都能出现在 ARRAY 或动态字段里。
    pub(crate) fn repr(&self) -> String {
        self.py_repr()
    }

    fn py_repr(&self) -> String {
        match self {
            Self::None => "None".to_owned(),
            Self::Bool(true) => "True".to_owned(),
            Self::Bool(false) => "False".to_owned(),
            Self::Int(i) => i.to_string(),
            Self::BigInt(text) => text.clone(),
            Self::Float(f) => crate::error::py_float_repr(*f),
            Self::Str(s) => py_repr(s),
            Self::List(items) => {
                let items: Vec<_> = items.iter().map(Self::py_repr).collect();
                format!("[{}]", items.join(", "))
            }
            Self::Dict(items) => {
                let items: Vec<_> = items
                    .iter()
                    .map(|(k, v)| format!("{}: {}", py_repr(k), v.py_repr()))
                    .collect();
                format!("{{{}}}", items.join(", "))
            }
            // TIMESTAMPTZ 不能作 ARRAY 元素，动态字段里也没有 datetime，所以它只出现在顶层（走 py_str）。
            Self::DateTime(micros) => crate::timestamptz::python_str(*micros),
        }
    }

    /// Python `type(v).__name__`。
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::None => "NoneType",
            Self::Bool(_) => "bool",
            Self::Int(_) | Self::BigInt(_) => "int",
            Self::Float(_) => "float",
            Self::Str(_) => "str",
            Self::List(_) => "list",
            Self::Dict(_) => "dict",
            Self::DateTime(_) => "datetime.datetime",
        }
    }

    /// `SortOp` 的缺失值：`None` 与 NaN 总排在最后。
    pub(crate) fn is_missing(&self) -> bool {
        match self {
            Self::None => true,
            Self::Float(f) => f.is_nan(),
            _ => false,
        }
    }

    /// Python `float(v)`。
    pub(crate) fn to_float(&self) -> Result<f64> {
        match self {
            Self::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
            Self::Int(i) => Ok(*i as f64),
            Self::BigInt(text) => big_to_float(text),
            Self::Float(f) => Ok(*f),
            Self::Str(s) => float_literal(s).ok_or_else(|| {
                EngineError::invalid(format!("could not convert string to float: {}", py_repr(s)))
            }),
            other => Err(EngineError::invalid(format!(
                "float() argument must be a string or a real number, not {}",
                py_repr(other.type_name())
            ))),
        }
    }

    /// Python `==`。
    pub(crate) fn py_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::None, Self::None) => true,
            (Self::Str(a), Self::Str(b)) => a == b,
            (Self::DateTime(a), Self::DateTime(b)) => a == b,
            (Self::List(a), Self::List(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.py_eq(b))
            }
            (Self::Dict(a), Self::Dict(b)) => {
                a.len() == b.len()
                    && a.iter().all(|(key, value)| {
                        b.iter()
                            .find(|(k, _)| k == key)
                            .is_some_and(|(_, other)| value.py_eq(other))
                    })
            }
            _ => number_order(self, other) == Some(Some(Ordering::Equal)),
        }
    }

    /// Python 的 `(a > b) - (a < b)`：`a > b` 先求值，所以类型错误的文案里是 `'>'`。
    pub(crate) fn py_order(&self, other: &Self) -> Result<Ordering> {
        if let Some(order) = number_order(self, other) {
            // 与 NaN 比较时两个方向都为假，差值为 0。
            return Ok(order.unwrap_or(Ordering::Equal));
        }
        match (self, other) {
            (Self::Str(a), Self::Str(b)) => Ok(a.cmp(b)),
            (Self::DateTime(a), Self::DateTime(b)) => Ok(a.cmp(b)),
            (Self::List(a), Self::List(b)) => {
                for (x, y) in a.iter().zip(b) {
                    if !x.py_eq(y) {
                        return x.py_order(y);
                    }
                }
                Ok(a.len().cmp(&b.len()))
            }
            _ => Err(EngineError::invalid(format!(
                "'>' not supported between instances of {} and {}",
                py_repr(self.type_name()),
                py_repr(other.type_name())
            ))),
        }
    }
}

/// 两边都是数字（含布尔）时给出比较结果，内层 None 表示有 NaN；不都是数字时外层为 None。
fn number_order(a: &PyVal, b: &PyVal) -> Option<Option<Ordering>> {
    let int = |value: &PyVal| match value {
        PyVal::Bool(b) => Some(i64::from(*b).to_string()),
        PyVal::Int(i) => Some(i.to_string()),
        PyVal::BigInt(text) => Some(text.clone()),
        _ => None,
    };
    match (a, b) {
        (PyVal::Int(a), PyVal::Int(b)) => Some(Some(a.cmp(b))),
        (PyVal::Float(a), PyVal::Float(b)) => Some(a.partial_cmp(b)),
        (PyVal::Float(a), other) => {
            let other = int(other)?;
            Some(int_float_cmp(&other, *a).map(Ordering::reverse))
        }
        (other, PyVal::Float(b)) => {
            let other = int(other)?;
            Some(int_float_cmp(&other, *b))
        }
        _ => Some(Some(int_cmp(&int(a)?, &int(b)?))),
    }
}

fn big_to_float(text: &str) -> Result<f64> {
    text.parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .ok_or_else(|| EngineError::invalid(TOO_LARGE))
}

/// Python `int(v)`，结果还要能写进 protobuf 的整数槽。
fn py_int(value: &PyVal) -> Result<i64> {
    let out_of_range = |text: String| EngineError::invalid(format!("Value out of range: {text}"));
    match value {
        PyVal::Bool(b) => Ok(i64::from(*b)),
        PyVal::Int(i) => Ok(*i),
        PyVal::BigInt(text) => Err(out_of_range(text.clone())),
        PyVal::Float(f) if f.is_nan() => {
            Err(EngineError::invalid("cannot convert float NaN to integer"))
        }
        PyVal::Float(f) if f.is_infinite() => Err(EngineError::invalid(
            "cannot convert float infinity to integer",
        )),
        PyVal::Float(f) => {
            let truncated = f.trunc();
            if truncated.abs() < 9.2e18 {
                Ok(truncated as i64)
            } else {
                Err(out_of_range(format!("{truncated:.0}")))
            }
        }
        PyVal::Str(s) => strip_number(s.trim())
            .and_then(|t| t.parse::<i64>().ok())
            .ok_or_else(|| {
                EngineError::invalid(format!(
                    "invalid literal for int() with base 10: {}",
                    py_repr(s)
                ))
            }),
        other => Err(EngineError::invalid(format!(
            "int() argument must be a string, a bytes-like object or a real number, not {}",
            py_repr(other.type_name())
        ))),
    }
}

fn unsupported_cell(field: &FieldSchema) -> EngineError {
    EngineError::Unsupported {
        feature: "rerank values written into this field type",
        reason: format!(
            "field {} of type {} cannot hold a computed rerank value in this build of milvus-lite-rust yet",
            py_repr(&field.name),
            field.data_type.name()
        ),
    }
}

/// 承载链算出的值（得分、组得分、因子）的空列。INT8 / INT16 用 int32：3.x 把 `int(v)` 直接写进
/// protobuf 的 int32 槽，不按字段类型收窄（组得分 25600 写进 INT8 字段仍是 25600）。
pub(crate) fn python_column(field: &FieldSchema) -> ColumnData {
    match field.data_type {
        DataType::Int8 | DataType::Int16 => ColumnData::Int32(Vec::new()),
        _ => ColumnData::empty(field),
    }
}

/// 3.x `_build_field_data` 的 `_coerce_for`：把候选行里的值写成该字段类型的一格（`None` 是空值占位），
/// 列的表示与 [`python_column`] 相同。只有虚拟列才需要它，普通字段直接复制存储列。
pub(crate) fn python_cell(field: &FieldSchema, value: &PyVal) -> Result<ColumnData> {
    if *value == PyVal::None {
        return Ok(match field.data_type {
            DataType::Int8 | DataType::Int16 => ColumnData::Int32(vec![0]).with_validity(&[false]),
            _ => ColumnData::nulls(field, 1),
        });
    }
    let narrow = |n: i64| -> Result<i32> {
        i32::try_from(n).map_err(|_| EngineError::invalid(format!("Value out of range: {n}")))
    };
    Ok(match field.data_type {
        DataType::Bool => ColumnData::Bool(vec![match value {
            PyVal::Bool(b) => *b,
            PyVal::Int(i) => *i != 0,
            PyVal::Float(f) => *f != 0.0,
            PyVal::Str(s) => !s.is_empty(),
            PyVal::List(items) => !items.is_empty(),
            PyVal::Dict(items) => !items.is_empty(),
            PyVal::BigInt(_) | PyVal::DateTime(_) => true,
            PyVal::None => false,
        }]),
        DataType::Int8 | DataType::Int16 | DataType::Int32 => {
            ColumnData::Int32(vec![narrow(py_int(value)?)?])
        }
        DataType::Int64 => ColumnData::Int64(vec![py_int(value)?]),
        DataType::Float => ColumnData::Float(vec![value.to_float()? as f32]),
        DataType::Double => ColumnData::Double(vec![value.to_float()?]),
        DataType::VarChar => ColumnData::VarChar(vec![value.py_str()]),
        DataType::Json => ColumnData::Json(vec![match value {
            PyVal::Str(s) => s.as_bytes().to_vec(),
            PyVal::Bool(true) => b"true".to_vec(),
            PyVal::Bool(false) => b"false".to_vec(),
            PyVal::Int(i) => i.to_string().into_bytes(),
            PyVal::BigInt(text) => text.clone().into_bytes(),
            // `json.dumps` 的非有限浮点是 NaN / Infinity / -Infinity，不是 `repr` 的 nan / inf。
            PyVal::Float(f) if f.is_nan() => b"NaN".to_vec(),
            PyVal::Float(f) if f.is_infinite() => {
                if *f > 0.0 {
                    b"Infinity".to_vec()
                } else {
                    b"-Infinity".to_vec()
                }
            }
            PyVal::Float(f) => crate::error::py_float_repr(*f).into_bytes(),
            _ => return Err(unsupported_cell(field)),
        }]),
        DataType::Timestamptz => match value {
            PyVal::DateTime(micros) => ColumnData::Int64(vec![*micros]),
            _ => return Err(unsupported_cell(field)),
        },
        DataType::Array | DataType::FloatVector => return Err(unsupported_cell(field)),
    })
}

/// 去掉数字之间的单个下划线；下划线出现在开头、结尾、连续出现或不在两个数字之间时不合法。
pub fn strip_number(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    for (i, c) in s.char_indices() {
        if c == '_' {
            let prev_digit = i > 0 && bytes[i - 1].is_ascii_digit();
            let next_digit = bytes.get(i + 1).is_some_and(u8::is_ascii_digit);
            if !(prev_digit && next_digit) {
                return None;
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// Python `float(s)` 接受的 ASCII 字面量。
pub fn float_str(s: &str) -> Option<f64> {
    let t = strip_number(s.trim())?;
    let body = t.trim_start_matches(['+', '-']);
    // Rust 的 f64 解析还接受 "infinity" 等 Python 同样接受的写法；Python 不接受十六进制与空串。
    if body.is_empty() || body.starts_with("0x") || body.starts_with("0X") {
        return None;
    }
    t.parse::<f64>().ok()
}

/// Python `float(s)`：另外接受任意 Unicode 十进制数字。
fn float_literal(s: &str) -> Option<f64> {
    let normalized: String = s
        .chars()
        .map(|c| {
            python_decimal_digit(c)
                .and_then(|v| char::from_digit(v, 10))
                .unwrap_or(c)
        })
        .collect();
    float_str(&normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message<T: std::fmt::Debug>(result: Result<T>) -> String {
        match result.unwrap_err() {
            EngineError::InvalidArgument(message) => message,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn computed_cells_follow_coerce_for() {
        // INT8 / INT16 不收窄：3.x 把 int(v) 直接写进 int32 槽。
        for data_type in [DataType::Int8, DataType::Int16] {
            let field = FieldSchema::new("$group_score", data_type);
            let mut column = python_column(&field);
            column.append_column(&python_cell(&field, &PyVal::Float(25600.7)).unwrap());
            column.append_column(&python_cell(&field, &PyVal::None).unwrap());
            assert_eq!(column.values(), &ColumnData::Int32(vec![25600, 0]));
            assert!(column.is_null(1) && !column.is_null(0));
        }
        assert_eq!(
            message(python_cell(
                &FieldSchema::new("x", DataType::Int8),
                &PyVal::Float(3e9)
            )),
            "Value out of range: 3000000000"
        );
        // JSON 用 json.dumps 的写法。
        let json_field = FieldSchema::new("x", DataType::Json);
        for (value, text) in [
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
            (0.5, "0.5"),
        ] {
            assert_eq!(
                python_cell(&json_field, &PyVal::Float(value)).unwrap(),
                ColumnData::Json(vec![text.as_bytes().to_vec()])
            );
        }
    }

    // 期望值取自 Python 3.12。
    #[test]
    fn float_conversion_matches_python() {
        assert_eq!(PyVal::Bool(true).to_float().unwrap(), 1.0);
        assert_eq!(PyVal::Int(3).to_float().unwrap(), 3.0);
        assert_eq!(
            PyVal::Int((1 << 53) + 1).to_float().unwrap(),
            9_007_199_254_740_992.0
        );
        assert_eq!(PyVal::Str(" 1_0.5 ".into()).to_float().unwrap(), 10.5);
        assert_eq!(PyVal::Str("١.٥".into()).to_float().unwrap(), 1.5);
        assert!(PyVal::Str("nan".into()).to_float().unwrap().is_nan());
        assert_eq!(
            message(PyVal::Str("x".into()).to_float()),
            "could not convert string to float: 'x'"
        );
        for (value, name) in [
            (PyVal::None, "NoneType"),
            (PyVal::List(vec![]), "list"),
            (PyVal::Dict(vec![]), "dict"),
        ] {
            assert_eq!(
                message(value.to_float()),
                format!("float() argument must be a string or a real number, not '{name}'")
            );
        }
        let big = format!("1{}", "0".repeat(400));
        assert_eq!(message(PyVal::BigInt(big).to_float()), TOO_LARGE);
    }

    #[test]
    fn comparison_follows_python_numeric_tower() {
        let two53 = 1_i64 << 53;
        assert!(PyVal::Bool(true).py_eq(&PyVal::Int(1)));
        assert!(PyVal::Bool(true).py_eq(&PyVal::Float(1.0)));
        assert!(PyVal::Int(two53).py_eq(&PyVal::Float(two53 as f64)));
        assert!(!PyVal::Int(two53 + 1).py_eq(&PyVal::Float(two53 as f64)));
        assert_eq!(
            PyVal::Int(two53 + 1)
                .py_order(&PyVal::Float(two53 as f64))
                .unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            PyVal::Float(two53 as f64)
                .py_order(&PyVal::Int(two53 + 1))
                .unwrap(),
            Ordering::Less
        );
        assert!(!PyVal::Float(f64::NAN).py_eq(&PyVal::Float(f64::NAN)));
        assert_eq!(
            PyVal::Float(f64::NAN).py_order(&PyVal::Int(1)).unwrap(),
            Ordering::Equal
        );
        assert_eq!(
            PyVal::BigInt("9223372036854775808".into())
                .py_order(&PyVal::Int(i64::MAX))
                .unwrap(),
            Ordering::Greater
        );
        assert!(PyVal::Float(0.0).py_eq(&PyVal::Float(-0.0)));
        assert!(!PyVal::Str("1".into()).py_eq(&PyVal::Int(1)));
    }

    #[test]
    fn strings_and_lists_compare_like_python() {
        // Python 按码点比较字符串，UTF-8 的字节序与之相同。
        assert_eq!(
            PyVal::Str("é".into())
                .py_order(&PyVal::Str("z".into()))
                .unwrap(),
            Ordering::Greater
        );
        let list = |items: Vec<PyVal>| PyVal::List(items);
        assert_eq!(
            list(vec![PyVal::Int(1), PyVal::Int(2)])
                .py_order(&list(vec![PyVal::Int(1), PyVal::Int(3)]))
                .unwrap(),
            Ordering::Less
        );
        assert_eq!(
            list(vec![PyVal::Int(1)])
                .py_order(&list(vec![PyVal::Int(1), PyVal::Int(3)]))
                .unwrap(),
            Ordering::Less
        );
        assert_eq!(
            message(list(vec![PyVal::Int(1)]).py_order(&list(vec![PyVal::Str("a".into())]))),
            "'>' not supported between instances of 'int' and 'str'"
        );
    }

    #[test]
    fn incomparable_types_raise_the_python_message() {
        assert_eq!(
            message(PyVal::Float(1.0).py_order(&PyVal::Str("a".into()))),
            "'>' not supported between instances of 'float' and 'str'"
        );
        let dict = PyVal::Dict(vec![("a".into(), PyVal::Int(1))]);
        assert_eq!(
            message(dict.py_order(&dict)),
            "'>' not supported between instances of 'dict' and 'dict'"
        );
        assert!(dict.py_eq(&PyVal::Dict(vec![("a".into(), PyVal::Float(1.0))])));
        assert!(!dict.py_eq(&PyVal::Dict(vec![("b".into(), PyVal::Int(1))])));
    }

    #[test]
    fn datetimes_only_compare_with_datetimes() {
        let (a, b) = (PyVal::DateTime(1), PyVal::DateTime(2));
        assert_eq!(a.py_order(&b).unwrap(), Ordering::Less);
        assert!(a.py_eq(&PyVal::DateTime(1)));
        assert!(!a.py_eq(&PyVal::Int(1)));
        assert_eq!(
            message(a.py_order(&PyVal::Float(1.0))),
            "'>' not supported between instances of 'datetime.datetime' and 'float'"
        );
        assert_eq!(
            message(a.to_float()),
            "float() argument must be a string or a real number, not 'datetime.datetime'"
        );
    }

    #[test]
    fn python_str_of_every_kind() {
        // 期望值取自 3.x 把这些值写进 VARCHAR 主键的结果（探针 varchar pk）。
        assert_eq!(PyVal::None.py_str(), "None");
        assert_eq!(PyVal::Bool(true).py_str(), "True");
        assert_eq!(PyVal::Int(3).py_str(), "3");
        assert_eq!(PyVal::Float(1.5).py_str(), "1.5");
        assert_eq!(PyVal::Float(f64::NAN).py_str(), "nan");
        assert_eq!(PyVal::Float(f64::NEG_INFINITY).py_str(), "-inf");
        assert_eq!(
            PyVal::BigInt("1180591620717411303424".into()).py_str(),
            "1180591620717411303424"
        );
        assert_eq!(
            PyVal::List(vec![
                PyVal::Float(f64::NAN),
                PyVal::Float(0.5),
                PyVal::None,
                PyVal::Bool(true)
            ])
            .py_str(),
            "[nan, 0.5, None, True]"
        );
        assert_eq!(
            PyVal::List(vec![PyVal::BigInt("1180591620717411303424".into())]).py_str(),
            "[1180591620717411303424]"
        );
        assert_eq!(PyVal::Str("x".into()).py_str(), "x");
        assert_eq!(
            PyVal::List(vec![PyVal::Int(1), PyVal::Str("a".into())]).py_str(),
            "[1, 'a']"
        );
        assert_eq!(
            PyVal::Dict(vec![("a".into(), PyVal::Int(1))]).py_str(),
            "{'a': 1}"
        );
        assert_eq!(
            PyVal::DateTime(1_735_689_600_000_000).py_str(),
            "2025-01-01 00:00:00+00:00"
        );
        assert_eq!(
            PyVal::DateTime(1_735_689_600_000_001).py_str(),
            "2025-01-01 00:00:00.000001+00:00"
        );
    }

    #[test]
    fn json_values_round_trip_with_python_types() {
        let value = PyVal::from_json(&json!({"a": [1, 1.5, "x", null, true]}));
        assert_eq!(value.type_name(), "dict");
        assert_eq!(value.py_str(), "{'a': [1, 1.5, 'x', None, True]}");
        let big: Value = crate::json_text::value_str("18446744073709551616").unwrap();
        assert_eq!(
            PyVal::from_json(&big),
            PyVal::BigInt("18446744073709551616".into())
        );
    }
}
