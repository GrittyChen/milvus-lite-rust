//! ISO / INTERVAL 字面量的解析：语法错误在解析阶段报告，数值供过滤求值使用（0025）。
use crate::error::py_repr;

fn number(s: &str) -> Option<u32> {
    (!s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}
fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())
}
fn leap(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}
const LAST_ORDINAL: u64 = 3_652_059;
fn before_year(year: u32) -> u64 {
    let y = u64::from(year - 1);
    365 * y + y / 4 - y / 100 + y / 400
}
fn date(s: &str) -> Option<(usize, u64)> {
    let year = number(s.get(..4)?)?;
    if !(1..=9999).contains(&year) {
        return None;
    }
    let dashed = s.as_bytes().get(4) == Some(&b'-');
    let at = if dashed { 5 } else { 4 };
    if s.as_bytes().get(at) == Some(&b'W') {
        let week = number(s.get(at + 1..at + 3)?)?;
        // ISO 第 53 周只存在于周四起年或周三起的闰年。
        let previous = year - 1;
        let jan1 = (year + previous / 4 - previous / 100 + previous / 400) % 7;
        if week == 0
            || week
                > if jan1 == 4 || (jan1 == 3 && leap(year)) {
                    53
                } else {
                    52
                }
        {
            return None;
        }
        let end = at + 3;
        // CPython 对无星期几的 ISO 周日期采用奇偶长度消歧；数字也可以充当日期/时间分隔符。
        let finish = if dashed && s.as_bytes().get(end) == Some(&b'-') {
            if s.len() > end + 2 && s.as_bytes()[end + 2].is_ascii_digit() {
                end
            } else {
                end + 2
            }
        } else if !dashed {
            let mut index = end;
            while s.as_bytes().get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
            if index < end + 2 {
                index
            } else if index.is_multiple_of(2) {
                end
            } else {
                end + 1
            }
        } else {
            end
        };
        let day = if finish == end {
            1
        } else {
            number(s.get(finish - 1..finish)?)?
        };
        if !(1..=7).contains(&day) {
            return None;
        }
        let jan4weekday = (jan1 + 3) % 7;
        let jan4weekday = if jan4weekday == 0 { 7 } else { jan4weekday };
        let ordinal = before_year(year) as i64 + 4 - i64::from(jan4weekday - 1)
            + i64::from((week - 1) * 7 + day - 1);
        return (1..=LAST_ORDINAL as i64)
            .contains(&ordinal)
            .then_some((finish, ordinal as u64));
    }
    let month = number(s.get(at..at + 2)?)?;
    let day_at = at + 2 + usize::from(dashed);
    if dashed && s.as_bytes().get(at + 2) != Some(&b'-') {
        return None;
    }
    let day = number(s.get(day_at..day_at + 2)?)?;
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap(year) {
                29
            } else {
                28
            }
        }
        _ => return None,
    };
    let month_days = [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let ordinal = before_year(year)
        + month_days.iter().take((month - 1) as usize).sum::<u64>()
        + u64::from(day);
    (day > 0 && day <= days).then_some((day_at + 2, ordinal))
}
#[derive(Debug, Clone, Copy, Default)]
struct Clock {
    fields: [u32; 3],
    microsecond: u32,
    tail: bool,
}

impl Clock {
    fn micros(self) -> i128 {
        let [hour, minute, second] = self.fields;
        (i128::from(hour) * 3600 + i128::from(minute) * 60 + i128::from(second)) * 1_000_000
            + i128::from(self.microsecond)
    }
}

fn clock_digits(bytes: &[u8], at: &mut usize, count: usize) -> Option<u32> {
    let digits = bytes.get(*at..at.checked_add(count)?)?;
    let mut value = 0;
    for &digit in digits {
        if !digit.is_ascii_digit() {
            return None;
        }
        value = value * 10 + u32::from(digit - b'0');
    }
    *at += count;
    Some(value)
}

/// 按 CPython 3.12.14 的 parse_hh_mm_ss_ff 状态推进，而不是先按分隔符 split。
/// end 是时区起点，bytes 仍含时区；C 解析器用实际后继字节决定 tail，二者不能混为 EOF。
fn parse_clock(bytes: &[u8], start: usize, end: usize) -> Option<Clock> {
    let mut clock = Clock::default();
    let mut at = start;
    let mut separated = false;
    for field in 0..3 {
        clock.fields[field] = clock_digits(bytes, &mut at, 2)?;
        let next = bytes.get(at).copied().unwrap_or(0);
        at += 1;
        if field == 0 {
            separated = next == b':';
        }
        if at >= end {
            clock.tail = next != 0;
            return Some(clock);
        }
        if separated && next == b':' {
            continue;
        }
        if matches!(next, b'.' | b',') {
            break;
        }
        if separated {
            return None;
        }
        at -= 1;
    }
    // 紧凑 HHMMSS 后继续的数字也是小数；冒号格式第三段后的 ':' 同样推进到这里。
    let count = end.checked_sub(at)?.min(6);
    if count == 0 {
        return None;
    }
    clock.microsecond = clock_digits(bytes, &mut at, count)? * 10_u32.pow(6 - count as u32);
    while bytes.get(at).is_some_and(u8::is_ascii_digit) {
        at += 1;
    }
    clock.tail = bytes.get(at).is_some_and(|byte| *byte != 0);
    Some(clock)
}

fn parse_iso_time(time: &str) -> Option<(Clock, Option<i128>)> {
    let bytes = time.as_bytes();
    let zone = bytes
        .iter()
        .position(|byte| matches!(byte, b'Z' | b'+' | b'-'));
    let clock = parse_clock(bytes, 0, zone.unwrap_or(bytes.len()))?;
    let [hour, minute, second] = clock.fields;
    if hour >= 24 || minute >= 60 || second >= 60 {
        return None;
    }
    let Some(zone) = zone else {
        return (!clock.tail).then_some((clock, None));
    };
    if bytes[zone] == b'Z' {
        // C 解析器检查的是 Z 后的字节是否为 NUL，而非 Python 字符串的逻辑长度。
        return (bytes.get(zone + 1).copied().unwrap_or(0) == 0).then_some((clock, Some(0)));
    }
    let offset = parse_clock(bytes, zone + 1, bytes.len())?;
    if offset.tail {
        return None;
    }
    // CPython 的零秒偏移直接返回 UTC，忽略该偏移中的小数；非零偏移允许分钟/秒归一化。
    let seconds = offset.micros() / 1_000_000;
    let micros = if seconds == 0 { 0 } else { offset.micros() };
    if micros >= 86_400_000_000 {
        return None;
    }
    Some((
        clock,
        Some(if bytes[zone] == b'-' { -micros } else { micros }),
    ))
}

/// 一个 ISO 时间字符串的解析结果；微秒数相对 `0001-01-01T00:00:00`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IsoValue {
    /// 带偏移（或 `Z`），已换算成 UTC，且在 `datetime` 的范围内。
    Aware { utc_micros: i128 },
    /// 没有偏移的本地时间，需要时区才能换算。
    Naive { local_micros: i128 },
}

pub(crate) const NAIVE_MESSAGE: &str =
    "TIMESTAMPTZ value must include a timezone offset or use a configured timezone";
/// 自 `0001-01-01` 到 `1970-01-01` 的天数。
const UNIX_EPOCH_ORDINAL: i128 = 719_162;

impl IsoValue {
    /// UTC 的 Unix 微秒；仅 `Aware` 有值。
    pub(crate) fn unix_micros(self) -> Option<i64> {
        match self {
            Self::Aware { utc_micros } => {
                i64::try_from(utc_micros - UNIX_EPOCH_ORDINAL * 86_400_000_000).ok()
            }
            Self::Naive { .. } => None,
        }
    }
}

/// 按 CPython 3.12 `datetime.fromisoformat` 解析（前后空白先去掉）；错误文案与 3.x 相同。
pub(crate) fn parse_iso(s: &str) -> Result<IsoValue, String> {
    let text = s.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}'));
    let fail = || {
        format!(
            "TIMESTAMPTZ value {} is not a valid ISO 8601 timestamp",
            py_repr(s)
        )
    };
    let (end, ordinal) = date(text).ok_or_else(fail)?;
    let days = i128::from(ordinal - 1) * 86_400_000_000;
    if text.len() == end {
        return Ok(IsoValue::Naive { local_micros: days });
    }
    let rest = &text[end..];
    let separator = rest.chars().next().ok_or_else(fail)?;
    let rest = &rest[separator.len_utf8()..];
    let (clock, offset) = parse_iso_time(rest).ok_or_else(fail)?;
    let Some(offset) = offset else {
        return Ok(IsoValue::Naive {
            local_micros: days + clock.micros(),
        });
    };
    let utc = days + clock.micros() - offset;
    if !(0..i128::from(LAST_ORDINAL) * 86_400_000_000).contains(&utc) {
        return Err("date value out of range".into());
    }
    Ok(IsoValue::Aware { utc_micros: utc })
}

/// 3.x 在解析阶段就把字面量换算成数值；没有偏移的 ISO 需要时区（见 `NAIVE_MESSAGE`），超范围报 `date value out of range`。
pub(super) fn literal(s: &str, iso: bool) -> Result<super::parser::Literal, String> {
    use super::parser::Literal;
    if iso {
        return match parse_iso(s)? {
            value @ IsoValue::Aware { .. } => value
                .unix_micros()
                .map(Literal::Timestamp)
                .ok_or_else(|| "date value out of range".to_owned()),
            IsoValue::Naive { .. } => Err(NAIVE_MESSAGE.into()),
        };
    }
    let fail = || {
        format!(
            "unsupported INTERVAL {}; supported examples: P1D, PT3H, P2DT6H",
            py_repr(s)
        )
    };
    let upper = s.to_uppercase();
    let matched = upper.strip_suffix('\n').unwrap_or(&upper);
    let mut rest = matched.strip_prefix('P').ok_or_else(fail)?;
    let mut any = false;
    let mut seconds = 0.0;
    for unit in ['W', 'D', 'T', 'H', 'M', 'S'] {
        if unit == 'T' {
            if let Some(tail) = rest.strip_prefix('T') {
                rest = tail;
            } else {
                break;
            }
            continue;
        }
        let Some(end) = rest.find(unit) else {
            continue;
        };
        let amount: String = rest[..end]
            .chars()
            .map(|c| {
                super::unicode::decimal(c)
                    .and_then(|n| char::from_digit(n, 10))
                    .unwrap_or(c)
            })
            .collect();
        let mut split = amount.split('.');
        if !digits(split.next().unwrap_or(""))
            || split.next().is_some_and(|fraction| !digits(fraction))
            || split.next().is_some()
        {
            return Err(fail());
        }
        let value = amount.parse::<f64>().map_err(|_| fail())?;
        // 与 3.x 同序的浮点运算：周先乘 7 再乘 86400，舍入与一次乘 604800 可能不同。
        seconds += match unit {
            'W' => value * 7.0 * 86400.0,
            'D' => value * 86400.0,
            'H' => value * 3600.0,
            'M' => value * 60.0,
            _ => value,
        };
        any |= value > 0.0;
        rest = &rest[end + 1..];
    }
    if !rest.is_empty() || (!any && upper != "P0D") {
        return Err(fail());
    }
    let micros = seconds * 1_000_000.0;
    if !micros.is_finite() {
        return Err("cannot convert float infinity to integer".into());
    }
    Ok(Literal::Interval(micros.trunc()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn validate(s: &str, iso: bool) -> Result<(), String> {
        literal(s, iso).map(drop)
    }

    fn interval(s: &str) -> Result<f64, String> {
        match literal(s, false)? {
            super::super::parser::Literal::Interval(v) => Ok(v),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn interval_micros_follow_the_3x_float_arithmetic_and_truncate_toward_zero() {
        assert_eq!(interval("P1D").unwrap(), 86_400_000_000.0);
        assert_eq!(interval("p1w1d").unwrap(), 691_200_000_000.0);
        assert_eq!(interval("PT1.9999999S").unwrap(), 1_999_999.0);
        assert_eq!(interval("P1DT1H1M1.5S").unwrap(), 90_061_500_000.0);
        assert_eq!(interval("P0D").unwrap(), 0.0);
        // 周先乘 7 再乘 86400：一次乘 604800 会差 1 微秒（3.x 的结果是前者）。
        assert_eq!(interval("P6.52W").unwrap(), 3_943_296_000_000.0);
        assert_eq!(interval("P2.5507W").unwrap(), 1_542_663_360_000.0);
        assert_eq!(interval("P1DT").unwrap(), 86_400_000_000.0);
        // 超出 int64 的值保留给求值阶段报错；无穷大在解析阶段报错。
        assert!(interval("P106751992D").unwrap() >= 9_223_372_036_854_775_808.0);
        assert_eq!(
            interval(&format!("P{}D", "9".repeat(400))).unwrap_err(),
            "cannot convert float infinity to integer"
        );
        for bad in ["", "PT", "PT0S", "P0W", "P-1D", "P1Y", "P1M", "1D", "P1D1W"] {
            assert!(
                interval(bad)
                    .unwrap_err()
                    .starts_with("unsupported INTERVAL"),
                "{bad}"
            );
        }
    }

    #[test]
    fn clocks_and_offsets_share_one_scanner_with_cpython_tail_rules() {
        let (clock, offset) = parse_iso_time("00:00:08:30+08:30:08:123456789").unwrap();
        assert_eq!(clock.micros(), 8_300_000);
        assert_eq!(offset, Some(30_608_123_456));
        let (clock, offset) = parse_iso_time("12345678901234+08").unwrap();
        assert_eq!(clock.micros(), 45_296_789_012);
        assert_eq!(offset, Some(28_800_000_000));
        assert_eq!(parse_iso_time("00:00:00.3+00:00:00.4").unwrap().1, Some(0));
        assert!(validate("0001-01-01T00:00:00:3+00:00:00:4", true).is_ok());
        assert_eq!(
            validate("0001-01-01T00:00:00:3+00:00:01:4", true),
            Err("date value out of range".into())
        );
        for value in [
            "00:00:08::30",
            "00:00:08:3.0",
            "000008:30",
            "00:05:00Z08:30",
            "1234567",
        ] {
            assert!(parse_iso_time(value).is_none(), "{value}");
        }
        assert_eq!(
            parse_iso_time("1234567Z").unwrap().0.micros(),
            45_296_000_000
        );
        // 紧凑尾数从两位起按微秒缩放，超过六位截断；一个尾字节由 C 的早返回消耗。
        for (text, expected) in [
            ("00200400Z", 1_204_000_000),
            ("002004001Z", 1_204_001_000),
            ("12345678901234Z", 45_296_789_012),
        ] {
            assert_eq!(parse_iso_time(text).unwrap().0.micros(), expected, "{text}");
        }
        for text in ["00Z\0", "00Z\0+08", "00+08:00:00.123456\0ignored"] {
            assert!(parse_iso_time(text).is_some(), "{text:?}");
        }
        for text in ["00Z+08", "00Z08:30", "00+08Z\0", "00+08\0ignored"] {
            assert!(parse_iso_time(text).is_none(), "{text:?}");
        }
        for text in ["20250101T00400000", "20250101404000008"] {
            assert_eq!(
                validate(text, true),
                Err(
                    "TIMESTAMPTZ value must include a timezone offset or use a configured timezone"
                        .into()
                )
            );
        }
    }
}
