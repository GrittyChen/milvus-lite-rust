//! TIMESTAMPTZ 的值：UTC Unix 微秒的 int64（0025）。
//!
//! 解析复用 `filter::temporal` 的 CPython `datetime.fromisoformat` 实现；无偏移字符串的时区换算见 [`crate::timezone`]。

use crate::error::{EngineError, Result};
use crate::filter::temporal::{self, IsoValue};
use crate::timezone::TimezoneSource;

/// 取值范围：`0001-01-01T00:00:00Z` 到 `9999-12-31T23:59:59.999999Z`（Python `datetime` 的范围）。
/// 3.x 对原始 gRPC 的整数微秒不校验，写入后任何读到该行的查询都会整体失败；本实现在写入处拒绝。
pub const MIN_MICROS: i64 = -62_135_596_800_000_000;
pub const MAX_MICROS: i64 = 253_402_300_799_999_999;

const OUT_OF_RANGE: &str = "date value out of range";

pub fn check_micros(micros: i64) -> Result<()> {
    if (MIN_MICROS..=MAX_MICROS).contains(&micros) {
        Ok(())
    } else {
        Err(EngineError::invalid(OUT_OF_RANGE))
    }
}

/// 把写入的时间字符串解析为 UTC 微秒。没有偏移的字符串需要时区，没有配置时与 3.x 一样报错。
pub fn parse(text: &str) -> Result<i64> {
    parse_with(text, &TimezoneSource::none())
}

/// 同 [`parse`]，没有偏移的字符串按 `timezone` 的有效时区解释（请求 → 集合 → 数据库）。
pub fn parse_with(text: &str, timezone: &TimezoneSource<'_>) -> Result<i64> {
    match temporal::parse_iso(text).map_err(error)? {
        value @ IsoValue::Aware { .. } => value
            .unix_micros()
            .ok_or_else(|| EngineError::invalid(OUT_OF_RANGE)),
        IsoValue::Naive { local_micros } => timezone.for_naive()?.local_to_utc_micros(local_micros),
    }
}

/// 字符串是否合法的 ISO 时间但没有偏移（需要时区才能换算）。
pub fn is_naive(text: &str) -> bool {
    matches!(temporal::parse_iso(text), Ok(IsoValue::Naive { .. }))
}

/// 3.x 的 `date value out of range` 是 OverflowError（1），其余是 SchemaValidationError（6）。
fn error(message: String) -> EngineError {
    if message == OUT_OF_RANGE {
        EngineError::invalid(message)
    } else {
        EngineError::schema(message)
    }
}

/// 创建集合时对 TIMESTAMPTZ 默认值的校验（3.x `parse_timestamptz`）：只接受带偏移的 ISO 字符串或整数微秒。
/// 整数微秒在创建时不查范围（3.x 同样接受，所以含越界默认值的 3.x 目录必须能打开）；
/// 使用该默认值的写入在转换处按 `check_micros` 拒绝。
pub fn check_default(value: &serde_json::Value, timezone: &TimezoneSource<'_>) -> Result<()> {
    use serde_json::Value;
    match value {
        Value::String(text) => parse_with(text, timezone).map(drop),
        Value::Number(number) if !number.as_str().contains(['.', 'e', 'E']) => number
            .as_i64()
            .map(drop)
            .ok_or_else(|| EngineError::invalid("Python int too large to convert to C long")),
        Value::Bool(_) => Err(EngineError::schema("TIMESTAMPTZ value must not be bool")),
        other => Err(EngineError::schema(format!(
            "TIMESTAMPTZ value must be ISO string, aware datetime, or int microseconds; got {}",
            match other {
                Value::Number(_) => "float",
                Value::Array(_) => "list",
                Value::Object(_) => "dict",
                _ => "NoneType",
            }
        ))),
    }
}

/// 响应里的 UTC ISO 字符串：以 `Z` 结尾，微秒为 0 时省略小数，否则固定 6 位（3.x `micros_to_iso_z`）。
/// 越界的值（只可能来自 3.x 写出的毒化数据）报 1 `date value out of range`。
pub fn format_iso_z(micros: i64) -> Result<String> {
    check_micros(micros)?;
    let days = micros.div_euclid(86_400_000_000);
    let rest = micros.rem_euclid(86_400_000_000);
    let (year, month, day) = civil_from_days(days);
    let seconds = rest / 1_000_000;
    let fraction = rest % 1_000_000;
    let mut out = format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    if fraction != 0 {
        out.push_str(&format!(".{fraction:06}"));
    }
    out.push('Z');
    Ok(out)
}

/// 自 1970-01-01 起的天数 → 公历年月日（Howard Hinnant 的 civil_from_days）。
pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn formats_like_3x_micros_to_iso_z() {
        assert_eq!(format_iso_z(0).unwrap(), "1970-01-01T00:00:00Z");
        assert_eq!(format_iso_z(-1).unwrap(), "1969-12-31T23:59:59.999999Z");
        assert_eq!(
            format_iso_z(500_000).unwrap(),
            "1970-01-01T00:00:00.500000Z"
        );
        assert_eq!(format_iso_z(MIN_MICROS).unwrap(), "0001-01-01T00:00:00Z");
        assert_eq!(
            format_iso_z(MAX_MICROS).unwrap(),
            "9999-12-31T23:59:59.999999Z"
        );
        assert_eq!(
            format_iso_z(parse("2024-02-29T12:34:56.789Z").unwrap()).unwrap(),
            "2024-02-29T12:34:56.789000Z"
        );
        assert!(format_iso_z(MAX_MICROS + 1).is_err());
        assert!(format_iso_z(i64::MIN).is_err());
    }

    #[test]
    fn parses_offsets_and_reports_3x_errors() {
        assert_eq!(
            parse("2025-01-01T00:00:00Z").unwrap(),
            1_735_689_600_000_000
        );
        assert_eq!(
            parse("2025-01-01T00:00:00+08:00").unwrap(),
            1_735_689_600_000_000 - 8 * 3_600_000_000
        );
        assert_eq!(
            parse("2025-01-01T00:00:00.1234567Z").unwrap() % 1_000_000,
            123_456
        );
        assert_eq!(
            parse("2025-01-01T00:00:00").unwrap_err().to_string(),
            temporal::NAIVE_MESSAGE
        );
        assert!(matches!(
            parse("0001-01-01T00:00:00+01:00"),
            Err(EngineError::InvalidArgument(m)) if m == "date value out of range"
        ));
        assert!(matches!(
            parse("abc"),
            Err(EngineError::SchemaValidation(_))
        ));
        assert_eq!(parse("0001-01-01T00:00:00Z").unwrap(), MIN_MICROS);
        assert_eq!(parse("9999-12-31T23:59:59.999999Z").unwrap(), MAX_MICROS);
    }
}
