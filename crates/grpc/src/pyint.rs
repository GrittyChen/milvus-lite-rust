//! 3.x 用 Python 的 `int()` / `float()` 转换请求参数（search_params 的值先经 `json.loads`，
//! type_params 是原始字符串）。合法输入的结果与 Python 完全一致；失败信息覆盖常见情形
//! （非数字字符串、null、列表、对象），NaN、Infinity、非 ASCII 数字与超出 int64 的值见 0004「决策」。

use milvus_lite_core::error::{py_json_repr, py_repr};
use serde_json::Value;

/// Python 异常的种类与信息。3.x 按种类决定错误码，例如 topk 的 TypeError / ValueError 报 code 6，
/// OverflowError 报 code 1。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PyError {
    Type(String),
    Value(String),
    Overflow(String),
}

impl PyError {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Type(m) | Self::Value(m) | Self::Overflow(m) => m,
        }
    }
}

/// Python `type(v).__name__`，只用于 `int()` / `float()` 不接受的类型。
fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "str",
    }
}

/// Python `int(v)`，v 为 `json.loads` 的结果。超出 int64 时饱和。
pub(crate) fn int(v: &Value) -> Result<i64, PyError> {
    match v {
        Value::Bool(b) => Ok(i64::from(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i)
            } else if n.as_u64().is_some() {
                Ok(i64::MAX)
            } else {
                float_to_int(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(s) => int_str(s).ok_or_else(|| {
            PyError::Value(format!(
                "invalid literal for int() with base 10: {}",
                py_repr(s)
            ))
        }),
        other => Err(PyError::Type(format!(
            "int() argument must be a string, a bytes-like object or a real number, not {}",
            py_repr(type_name(other))
        ))),
    }
}

/// protobuf 的整数槽位必须拒绝越界；请求参数的历史饱和规则不能用于 schema 默认值。
pub(crate) fn proto_integer(value: &Value) -> Result<i64, PyError> {
    let bounded = |text: String| {
        text.parse::<i64>()
            .map_err(|_| PyError::Value(format!("Value out of range: {text}")))
    };
    match value {
        Value::Number(number) if !number.as_str().contains(['.', 'e', 'E']) => {
            bounded(number.as_str().to_owned())
        }
        Value::String(text) => {
            let normalized: String = text
                .chars()
                .map(|c| {
                    milvus_lite_core::filter::python_decimal_digit(c)
                        .and_then(|v| char::from_digit(v, 10))
                        .unwrap_or(c)
                })
                .collect();
            let Some(digits) = strip_number(normalized.trim()) else {
                return int(value);
            };
            let (negative, digits) = if let Some(d) = digits.strip_prefix('-') {
                (true, d)
            } else {
                (false, digits.strip_prefix('+').unwrap_or(&digits))
            };
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return int(value);
            }
            if digits.len() > 4300 {
                return Err(PyError::Value(format!(
                    "Exceeds the limit (4300 digits) for integer string conversion: value has {} digits; use sys.set_int_max_str_digits() to increase the limit",
                    digits.len()
                )));
            }
            let digits = digits.trim_start_matches('0');
            let normalized = if digits.is_empty() {
                "0".into()
            } else if negative {
                format!("-{digits}")
            } else {
                digits.to_owned()
            };
            bounded(normalized)
        }
        Value::Number(number) => {
            let float = number
                .as_str()
                .parse::<f64>()
                .map_err(|_| PyError::Value("invalid floating default".into()))?;
            if !float.is_finite() {
                return float_to_int(float);
            }
            if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&float.trunc())
            {
                return Err(PyError::Value(format!(
                    "Value out of range: {:.0}",
                    float.trunc()
                )));
            }
            Ok(float.trunc() as i64)
        }
        _ => int(value),
    }
}

fn float_to_int(f: f64) -> Result<i64, PyError> {
    if f.is_nan() {
        return Err(PyError::Value(
            "cannot convert float NaN to integer".to_owned(),
        ));
    }
    if f.is_infinite() {
        return Err(PyError::Overflow(
            "cannot convert float infinity to integer".to_owned(),
        ));
    }
    // `as` 向零截断并在超出范围时饱和，与 Python 的截断一致（超出 int64 的部分见模块说明）。
    Ok(f.trunc() as i64)
}

pub(crate) fn proto_float(value: &Value) -> Result<f64, PyError> {
    match value {
        Value::Number(number) => {
            let parsed = number
                .as_str()
                .parse::<f64>()
                .map_err(|_| PyError::Value("invalid floating default".into()))?;
            if !number.as_str().contains(['.', 'e', 'E']) && !parsed.is_finite() {
                return Err(PyError::Overflow(
                    "int too large to convert to float".into(),
                ));
            }
            Ok(parsed)
        }
        Value::String(text) => {
            let normalized: String = text
                .chars()
                .map(|c| {
                    milvus_lite_core::filter::python_decimal_digit(c)
                        .and_then(|v| char::from_digit(v, 10))
                        .unwrap_or(c)
                })
                .collect();
            float_str(&normalized).map_or_else(|| float(value), Ok)
        }
        _ => float(value),
    }
}

/// Python `int(s)`（十进制）：去掉首尾空白，可带正负号，数字之间可有单个下划线。
/// 不是合法字面量时返回 None；超出 int64 时饱和。
pub(crate) fn int_str(s: &str) -> Option<i64> {
    let digits = strip_number(s.trim())?;
    let (negative, digits) = match digits.as_bytes().first() {
        Some(b'-') => (true, &digits[1..]),
        Some(b'+') => (false, &digits[1..]),
        _ => (false, digits.as_str()),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let text = if negative {
        format!("-{digits}")
    } else {
        digits.to_owned()
    };
    Some(
        text.parse::<i64>()
            .unwrap_or(if negative { i64::MIN } else { i64::MAX }),
    )
}

/// 去掉数字之间的单个下划线；下划线出现在开头、结尾、连续出现或不在两个数字之间时不合法。
fn strip_number(s: &str) -> Option<String> {
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

/// Python `float(v)`，v 为 `json.loads` 的结果。
pub(crate) fn float(v: &Value) -> Result<f64, PyError> {
    match v {
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        // arbitrary_precision 保留了溢出数字文本；JSON 指数溢出是 infinity，整数转浮点溢出则报错。
        Value::Number(_) => proto_float(v),
        Value::String(s) => float_str(s).ok_or_else(|| {
            PyError::Value(format!("could not convert string to float: {}", py_repr(s)))
        }),
        other => Err(PyError::Type(format!(
            "float() argument must be a string or a real number, not {}",
            py_repr(type_name(other))
        ))),
    }
}

fn float_str(s: &str) -> Option<f64> {
    let t = strip_number(s.trim())?;
    let body = t.trim_start_matches(['+', '-']);
    // Rust 的 f64 解析还接受 "infinity" 等 Python 同样接受的写法；Python 不接受十六进制与空串。
    if body.is_empty() || body.starts_with("0x") || body.starts_with("0X") {
        return None;
    }
    t.parse::<f64>().ok()
}

/// serde_json 解析失败的文本是否可能被 Python 的 `json.loads` 解析为对象或数组：Python 还接受
/// NaN、Infinity、超出 float64 的数与孤立的代理转义。这种情况下无法知道其中的内容，调用方不能
/// 像 3.x 解析失败时那样当作原字符串或空对象处理，否则会静默忽略其中的参数（0004「决策」）。
pub(crate) fn maybe_python_json_container(text: &str) -> bool {
    matches!(text.trim_start().as_bytes().first(), Some(b'{' | b'['))
}

/// 把 `json.loads` 的结果转为 Python `str(v)`：字符串原样，其余同 `repr`。
pub(crate) fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => py_json_repr(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schema_proto_integers_never_saturate_wide_defaults() {
        assert_eq!(proto_integer(&json!(i64::MAX)), Ok(i64::MAX));
        assert_eq!(proto_integer(&json!(i64::MIN)), Ok(i64::MIN));
        assert_eq!(proto_integer(&json!("+0007")), Ok(7));
        for (value, expected) in [
            (json!(u64::MAX), "Value out of range: 18446744073709551615"),
            (
                json!("9223372036854775808"),
                "Value out of range: 9223372036854775808",
            ),
            (
                json!(9_223_372_036_854_775_808.0),
                "Value out of range: 9223372036854775808",
            ),
            (json!(1e20), "Value out of range: 100000000000000000000"),
        ] {
            assert_eq!(proto_integer(&value).unwrap_err().message(), expected);
        }
        assert_eq!(int(&json!(u64::MAX)), Ok(i64::MAX));
    }

    #[test]
    fn schema_default_coercion_matches_unicode_and_python_digit_limits() {
        assert_eq!(proto_integer(&json!("١_٢")), Ok(12));
        assert_eq!(proto_integer(&json!("１２")), Ok(12));
        assert_eq!(proto_float(&json!("١.٥")), Ok(1.5));
        let text = "0".repeat(4301);
        assert_eq!(
            proto_integer(&json!(text)).unwrap_err().message(),
            "Exceeds the limit (4300 digits) for integer string conversion: value has 4301 digits; use sys.set_int_max_str_digits() to increase the limit"
        );
        let big: Value = serde_json::from_str(&format!("1{}", "0".repeat(400))).unwrap();
        assert_eq!(
            proto_float(&big).unwrap_err().message(),
            "int too large to convert to float"
        );
        assert!(int(&json!("١")).is_err());
    }

    // 期望值取自 Python 3.12。
    #[test]
    fn int_matches_python() {
        assert_eq!(int(&json!(3)), Ok(3));
        assert_eq!(int(&json!(2.9)), Ok(2));
        assert_eq!(int(&json!(-2.9)), Ok(-2));
        assert_eq!(int(&json!(true)), Ok(1));
        assert_eq!(int(&json!(" 7 ")), Ok(7));
        assert_eq!(int(&json!("+1_000")), Ok(1000));
        assert_eq!(int(&json!("-0")), Ok(0));
        assert_eq!(int(&json!(u64::MAX)), Ok(i64::MAX));
        for bad in ["x", "1.5", "1__0", "_1", "1_", "", " ", "- 1", "0x10"] {
            assert_eq!(
                int(&json!(bad)),
                Err(PyError::Value(format!(
                    "invalid literal for int() with base 10: {}",
                    py_repr(bad)
                ))),
                "{bad:?}"
            );
        }
        assert_eq!(
            int(&Value::Null).unwrap_err().message(),
            "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"
        );
        assert!(matches!(int(&json!([1])), Err(PyError::Type(_))));
        assert!(matches!(int(&json!({})), Err(PyError::Type(_))));
    }

    #[test]
    fn float_matches_python() {
        assert_eq!(float(&json!("1_0.5")), Ok(10.5));
        assert_eq!(float(&json!(" -inf ")), Ok(f64::NEG_INFINITY));
        assert_eq!(float(&json!("1e3")), Ok(1000.0));
        assert_eq!(float(&json!(".5")), Ok(0.5));
        assert!(float(&json!("nan")).is_ok_and(f64::is_nan));
        assert_eq!(
            float(&json!("x")).unwrap_err().message(),
            "could not convert string to float: 'x'"
        );
        assert_eq!(
            float(&Value::Null).unwrap_err().message(),
            "float() argument must be a string or a real number, not 'NoneType'"
        );
        assert!(float(&json!("0x1")).is_err());
    }

    #[test]
    fn str_matches_python() {
        assert_eq!(py_str(&json!("L2")), "L2");
        assert_eq!(py_str(&json!(123)), "123");
        assert_eq!(py_str(&json!(true)), "True");
        assert_eq!(py_str(&json!(["a"])), "['a']");
    }
}
