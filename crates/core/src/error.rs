//! 引擎错误。各变体与 3.x 的异常类一一对应，错误信息沿用 3.x 原文；错误码由 gRPC 层映射。

use crate::data_dir::DataDirError;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// 3.x `CollectionNotFoundError`。
    #[error("collection {} does not exist", py_repr(.0))]
    CollectionNotFound(String),
    /// 3.x `CollectionAlreadyExistsError`。
    #[error("collection {} already exists", py_repr(.0))]
    CollectionAlreadyExists(String),
    /// 3.x `CollectionNotLoadedError`。
    #[error(
        "Collection {} is in state 'released'; call load() before search/get/query",
        py_repr(.0)
    )]
    CollectionNotLoaded(String),
    /// 3.x `IndexAlreadyExistsError`。
    #[error("index already exists for field {}; call drop_index first", py_repr(.0))]
    IndexAlreadyExists(String),
    /// 3.x `SchemaValidationError`：schema、写入数据与请求参数的校验失败。
    #[error("{0}")]
    SchemaValidation(String),
    /// 3.x 中以 `ValueError` 等非 MilvusLite 异常抛出的参数错误。
    #[error("{0}")]
    InvalidArgument(String),
    /// 本实现当前阶段尚不支持的功能（见 0004「不支持」）。
    #[error("{feature}: {reason}")]
    Unsupported {
        feature: &'static str,
        reason: String,
    },
    #[error(transparent)]
    DataDir(#[from] DataDirError),
}

impl EngineError {
    pub(crate) fn schema(msg: impl Into<String>) -> Self {
        Self::SchemaValidation(msg.into())
    }

    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        Self::InvalidArgument(msg.into())
    }

    pub(crate) fn unsupported(feature: &'static str, reason: impl Into<String>) -> Self {
        Self::Unsupported {
            feature,
            reason: reason.into(),
        }
    }
}

pub type Result<T, E = EngineError> = std::result::Result<T, E>;

/// 按 Python `repr(str)` 的规则给字符串加引号，使错误信息与 3.x 的 `{name!r}` 一致。
pub fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if !py_printable(c) => {
                let n = u32::from(c);
                if n < 0x100 {
                    out.push_str(&format!("\\x{n:02x}"));
                } else if n < 0x10000 {
                    out.push_str(&format!("\\u{n:04x}"));
                } else {
                    out.push_str(&format!("\\U{n:08x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// 近似 Python 的 `str.isprintable`：控制字符、除空格外的空白、常见格式字符、私用区与非字符都不可打印。
/// 未分配码位（Cn）没有逐一识别，这类字符在错误信息中原样输出。
fn py_printable(c: char) -> bool {
    let n = u32::from(c);
    let format = matches!(
        n,
        0xAD | 0x600..=0x605 | 0x61C | 0x6DD | 0x70F | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E
            | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB | 0xE0001 | 0xE0020..=0xE007F
    );
    let private_use = matches!(n, 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD);
    let noncharacter = (0xFDD0..=0xFDEF).contains(&n) || n & 0xFFFE == 0xFFFE;
    !(c.is_control() || (c.is_whitespace() && c != ' ') || format || private_use || noncharacter)
}

/// Python `repr(list[str])`，用于 3.x 错误信息中的字段名列表。
pub(crate) fn py_repr_list<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    let parts: Vec<String> = items.into_iter().map(py_repr).collect();
    format!("[{}]", parts.join(", "))
}

/// Python `repr(float)`：最短往返表示，整数值带 `.0`，指数形式为 `1e-05`。
pub(crate) fn py_float_repr(v: f64) -> String {
    if v.is_nan() {
        return "nan".to_owned();
    }
    if v.is_infinite() {
        return if v > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    let abs = v.abs();
    if abs != 0.0 && !(1e-4..1e16).contains(&abs) {
        // Rust 的 `{:e}` 是最短往返的尾数；Python 的指数至少两位并带符号。
        let s = format!("{v:e}");
        let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
        let exp: i32 = exp.parse().unwrap_or(0);
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exp.abs());
    }
    let s = format!("{v:?}");
    if s.contains('.') || s.contains('e') {
        s
    } else {
        format!("{s}.0")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repr_matches_python() {
        assert_eq!(py_repr("abc"), "'abc'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(py_repr("a\\b\n"), "'a\\\\b\\n'");
        assert_eq!(py_repr("中文"), "'中文'");
        assert_eq!(py_repr("\u{1}"), "'\\x01'");
        assert_eq!(py_repr("a\u{a0}b"), "'a\\xa0b'");
        assert_eq!(py_repr("\u{200b}"), "'\\u200b'");
        assert_eq!(py_repr("\u{10ffff}"), "'\\U0010ffff'");
        assert_eq!(py_repr("\u{2028}"), "'\\u2028'");
        assert_eq!(py_repr(""), "''");
    }

    #[test]
    fn list_repr_matches_python() {
        assert_eq!(py_repr_list(["zz", "a"]), "['zz', 'a']");
        assert_eq!(py_repr_list([]), "[]");
    }

    #[test]
    fn float_repr_matches_python() {
        assert_eq!(py_float_repr(1.0), "1.0");
        assert_eq!(py_float_repr(-2.5), "-2.5");
        assert_eq!(py_float_repr(f64::from(0.1f32)), "0.10000000149011612");
        assert_eq!(py_float_repr(1e-5), "1e-05");
        assert_eq!(py_float_repr(1.5e16), "1.5e+16");
        assert_eq!(py_float_repr(1e16), "1e+16");
        assert_eq!(py_float_repr(123456789.0), "123456789.0");
        assert_eq!(py_float_repr(0.0001), "0.0001");
        assert_eq!(py_float_repr(0.0), "0.0");
    }
}
