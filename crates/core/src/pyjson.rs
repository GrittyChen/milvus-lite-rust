//! 按 Python `json.dumps` 的默认参数输出 JSON 文本：分隔符 `", "` 与 `": "`、`ensure_ascii=True`、
//! 浮点数用 `repr(float)`。3.x 以这种文本回显 `$meta` 与索引参数，其中 DescribeIndex 的
//! `search_params` 会原样交给用户，因此要逐字符一致；M2 写数据文件时同样需要（0002「字段编码要点」）。

use serde_json::Value;

use crate::error::py_float_repr;

/// `json.dumps(value)`。serde_json 的对象按 key 排序，因此嵌套对象的输出与 `sort_keys=True` 相同。
pub fn dumps(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

/// `json.dumps(dict)`，顶层 key 按给定顺序输出（例如按请求顺序输出的动态字段）。
pub fn dumps_object<'a>(entries: impl IntoIterator<Item = (&'a str, &'a Value)>) -> String {
    let mut out = String::from("{");
    for (i, (key, value)) in entries.into_iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_str(&mut out, key);
        out.push_str(": ");
        write_value(&mut out, value);
    }
    out.push('}');
    out
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => out.push_str(&i.to_string()),
            (None, Some(u), _) => out.push_str(&u.to_string()),
            (None, None, Some(f)) => out.push_str(&py_float_repr(f)),
            (None, None, None) => out.push_str(&n.to_string()),
        },
        Value::String(s) => write_str(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(obj) => {
            out.push_str(&dumps_object(obj.iter().map(|(k, v)| (k.as_str(), v))));
        }
    }
}

/// Python `json` 的 `py_encode_basestring_ascii`。
fn write_str(out: &mut String, s: &str) {
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
        let (a, z) = (json!(1), json!("s"));
        assert_eq!(
            dumps_object([("z", &z), ("a", &a)]),
            r#"{"z": "s", "a": 1}"#
        );
    }
}
