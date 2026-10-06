//! 按 Python `json.dumps` 的分隔符、浮点与转义规则输出 JSON 文本。
//! `$meta` 用 `sort_keys=True` / `ensure_ascii=True`，JSON 字段保留键序并用 `ensure_ascii=False`。

use serde_json::Value;

use crate::error::py_float_repr;

/// `json.dumps(value, sort_keys=True)`；嵌套对象同样排序。
pub fn dumps(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, true, true);
    out
}

/// `json.dumps(dict)`，顶层 key 按给定顺序输出，嵌套对象排序（例如动态字段输出）。
pub fn dumps_object<'a>(entries: impl IntoIterator<Item = (&'a str, &'a Value)>) -> String {
    let mut out = String::from("{");
    for (i, (key, value)) in entries.into_iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_str(&mut out, key, true);
        out.push_str(": ");
        write_value(&mut out, value, true, true);
    }
    out.push('}');
    out
}

/// JSON 字段落盘：`json.dumps(value, ensure_ascii=False)`，保留输入对象的键序。
pub fn dumps_unicode(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, false, false);
    out
}

/// 数字文本含浮点形式标记时才进入 f64；宽整数不受浮点范围限制。
pub(crate) fn json_float_text(raw: &str) -> Option<f64> {
    raw.contains(['.', 'e', 'E'])
        .then(|| raw.parse::<f64>().ok())
        .flatten()
}

fn write_value(out: &mut String, value: &Value, ensure_ascii: bool, sort_keys: bool) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            let raw = n.to_string();
            if let Some(value) = json_float_text(&raw) {
                if value.is_infinite() {
                    out.push_str(if value.is_sign_positive() {
                        "Infinity"
                    } else {
                        "-Infinity"
                    });
                } else {
                    out.push_str(&py_float_repr(value));
                }
            } else if raw == "-0" {
                out.push('0');
            } else {
                out.push_str(&raw);
            }
        }
        Value::String(s) => write_str(out, s, ensure_ascii),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(out, item, ensure_ascii, sort_keys);
            }
            out.push(']');
        }
        Value::Object(obj) => {
            let mut entries: Vec<_> = obj.iter().collect();
            if sort_keys {
                entries.sort_by_key(|(key, _)| *key);
            }
            out.push('{');
            for (i, (key, value)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_str(out, key, ensure_ascii);
                out.push_str(": ");
                write_value(out, value, ensure_ascii, sort_keys);
            }
            out.push('}');
        }
    }
}

/// Python `json` 的 `py_encode_basestring_ascii`。
fn write_str(out: &mut String, s: &str, ensure_ascii: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ if !ensure_ascii && c >= ' ' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_python_json_dumps() {
        // 期望值取自 Python 3.12 的 json.dumps。
        assert_eq!(
            dumps(&json!({"b": 1, "a": [1.5, true, null, "x"]})),
            r#"{"a": [1.5, true, null, "x"], "b": 1}"#
        );
        assert_eq!(dumps(&json!(1e-5)), "1e-05");
        assert_eq!(dumps(&json!(1.0)), "1.0");
        assert_eq!(dumps(&json!(u64::MAX)), "18446744073709551615");
        assert_eq!(
            dumps(&json!("\u{4e2d}\u{1f600}\"\\\n\u{7}\u{7f}")),
            "\"\x5cu4e2d\x5cud83d\x5cude00\x5c\"\x5c\x5c\x5cn\x5cu0007\x5cu007f\""
        );
        assert_eq!(dumps(&json!({})), "{}");
        // arbitrary_precision 接受标准 JSON 的大指数，兼容输出分支仍然可达。
        let overflow: Value = serde_json::from_str("[1e999,-1e999]").unwrap();
        assert_eq!(dumps_unicode(&overflow), "[Infinity, -Infinity]");
        let (a, z) = (json!(1), json!("s"));
        assert_eq!(
            dumps_object([("z", &z), ("a", &a)]),
            r#"{"z": "s", "a": 1}"#
        );
        let wide: Value =
            serde_json::from_str(r#"{"z": 184467440737095516160, "a": {"b": 1E2, "a": -0}}"#)
                .unwrap();
        assert_eq!(
            dumps(&wide),
            r#"{"a": {"a": 0, "b": 100.0}, "z": 184467440737095516160}"#
        );
        assert_eq!(
            dumps_unicode(&wide),
            r#"{"z": 184467440737095516160, "a": {"b": 100.0, "a": 0}}"#
        );
        assert_eq!(
            dumps_unicode(&json!({"name":"中文😀"})),
            r#"{"name": "中文😀"}"#
        );
    }
}
