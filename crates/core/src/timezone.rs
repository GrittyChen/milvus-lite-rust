//! 时区名解析、本地时间与 UTC 微秒的换算、有效时区的优先级（0025）。
//!
//! 3.x 把时区名交给 Python `zoneinfo`，数据来自运行平台；本实现用内置 tzdata（jiff），换算规则取
//! `datetime.replace(tzinfo=ZoneInfo(...))` 的 `fold=0`：重复的本地时间取第一次出现，不存在的本地时间沿用
//! 变换前的偏移。

use std::cell::RefCell;

use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;

use crate::error::{EngineError, Result, py_repr};
use crate::timestamptz::{MAX_MICROS, MIN_MICROS, civil_from_days};

const OUT_OF_RANGE: &str = "date value out of range";
const MICROS_PER_SECOND: i128 = 1_000_000;
const MICROS_PER_DAY: i128 = 86_400_000_000;
/// 自 `0001-01-01` 到 `1970-01-01` 的天数。
const UNIX_EPOCH_ORDINAL: i128 = 719_162;
/// jiff 的 `Timestamp::MAX` 是 `9999-12-30T22:00:00.999999999Z`，比 Python 的上界少约 26 小时。
/// 更晚的瞬间与本地时间用这一点的偏移补算；各时区在此之前的一段时间里没有偏移变换。
const ANCHOR_SECOND: i64 = 253_402_207_200;
/// 本地时间从 `9999-12-29T00:00:00` 起走补算路径（Unix 微秒）。
const HIGH_LOCAL_MICROS: i128 = 253_402_041_600_000_000;

/// 一个已解析的时区。
#[derive(Debug, Clone)]
pub struct Zone {
    tz: TimeZone,
}

/// 某个瞬间在时区内的各分量，供 `time_fields` 使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Components {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub minute: i64,
    pub second: i64,
    pub microsecond: i64,
}

/// 按 3.x 的次序检查并解析时区名：路径格式错误（1，文案同 `zoneinfo`）→ 找不到（6）。
///
/// 名字大小写不敏感是内置 tzdata 的当前行为（与 macOS 的 3.x 一致，Linux 上的 3.x 区分大小写）；
/// 单元测试固定这一点，升级 jiff 时由测试暴露变化。
pub fn resolve(name: &str) -> Result<Zone> {
    validate_key(name)?;
    TimeZone::get(name)
        .map(|tz| Zone { tz })
        .map_err(|_| EngineError::schema(format!("unknown timezone {}", py_repr(name))))
}

/// 集合与数据库的 `timezone` 属性值：非空且能解析（3.x `validate_timezone_name`）；校验后原样保存。
pub fn validate_property(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(EngineError::schema(
            "timezone property must be a non-empty string",
        ));
    }
    resolve(value).map(drop)
}

/// `zoneinfo._tzpath._validate_tzfile_path`：绝对路径、会改变长度的不规范路径、越出 TZPATH 的路径。
fn validate_key(key: &str) -> Result<()> {
    if key.starts_with('/') {
        return Err(EngineError::invalid(format!(
            "ZoneInfo keys may not be absolute paths, got: {key}"
        )));
    }
    let normalized = normpath(key);
    if normalized.chars().count() != key.chars().count() {
        return Err(EngineError::invalid(format!(
            "ZoneInfo keys must be normalized relative paths, got: {key}"
        )));
    }
    // 对应 `normpath(join("_/", key)).startswith("_/")`：相对路径回退出基准目录即越界。
    let mut depth = 0usize;
    for part in normalized.split('/') {
        match part {
            "." | "" => {}
            ".." if depth == 0 => {
                return Err(EngineError::invalid(format!(
                    "ZoneInfo keys must refer to subdirectories of TZPATH, got: {key}"
                )));
            }
            ".." => depth -= 1,
            _ => depth += 1,
        }
    }
    // 整个路径正好归一到基准目录本身（如 `.`）也不是子目录。
    if depth == 0 {
        return Err(EngineError::invalid(format!(
            "ZoneInfo keys must refer to subdirectories of TZPATH, got: {key}"
        )));
    }
    Ok(())
}

/// POSIX `os.path.normpath`（相对路径）：去掉空段与 `.`，折叠 `a/..`，保留无法折叠的前导 `..`；结果为空时是 `.`。
fn normpath(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|last| *last != "..") => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    }
}

impl Zone {
    pub fn utc() -> Self {
        Self { tz: TimeZone::UTC }
    }

    /// 没有偏移的本地时间（自 `0001-01-01T00:00:00` 起的微秒）换算成 UTC Unix 微秒。
    /// 结果超出 `[0001-01-01, 9999-12-31T23:59:59.999999]` 报 1 `date value out of range`。
    pub(crate) fn local_to_utc_micros(&self, local_since_year_1: i128) -> Result<i64> {
        let local = local_since_year_1 - UNIX_EPOCH_ORDINAL * MICROS_PER_DAY;
        let utc = if local >= HIGH_LOCAL_MICROS {
            let offset = i128::from(self.anchor_offset()) * MICROS_PER_SECOND;
            local - offset
        } else {
            let days = local.div_euclid(MICROS_PER_DAY);
            let rest = local.rem_euclid(MICROS_PER_DAY);
            let (year, month, day) = civil_from_days(i64::try_from(days).map_err(|_| range())?);
            let seconds = rest / MICROS_PER_SECOND;
            let datetime = DateTime::new(
                i16::try_from(year).map_err(|_| range())?,
                month as i8,
                day as i8,
                (seconds / 3600) as i8,
                (seconds / 60 % 60) as i8,
                (seconds % 60) as i8,
                // 微秒部分单独加回：jiff 的负时间戳带负的纳秒分量，`as_second` 向零截断。
                0,
            )
            .map_err(|_| range())?;
            let timestamp = self
                .tz
                .to_ambiguous_timestamp(datetime)
                .compatible()
                .map_err(|_| range())?;
            i128::from(timestamp.as_second()) * MICROS_PER_SECOND + rest % MICROS_PER_SECOND
        };
        i64::try_from(utc)
            .ok()
            .filter(|utc| (MIN_MICROS..=MAX_MICROS).contains(utc))
            .ok_or_else(range)
    }

    /// UTC Unix 微秒在本时区的各分量；本地时间落在 `datetime` 的范围之外报 1 `date value out of range`
    /// （`astimezone` 的 OverflowError）。
    pub fn components(&self, utc_micros: i64) -> Result<Components> {
        crate::timestamptz::check_micros(utc_micros)?;
        let seconds = utc_micros.div_euclid(1_000_000);
        let offset = if seconds >= ANCHOR_SECOND {
            self.anchor_offset()
        } else {
            let timestamp = Timestamp::from_microsecond(utc_micros).map_err(|_| range())?;
            self.tz.to_offset(timestamp).seconds()
        };
        let local = i128::from(utc_micros) + i128::from(offset) * MICROS_PER_SECOND;
        let local = i64::try_from(local)
            .ok()
            .filter(|local| (MIN_MICROS..=MAX_MICROS).contains(local))
            .ok_or_else(range)?;
        let days = local.div_euclid(86_400_000_000);
        let rest = local.rem_euclid(86_400_000_000);
        let (year, month, day) = civil_from_days(days);
        let seconds = rest / 1_000_000;
        Ok(Components {
            year,
            month,
            day,
            hour: seconds / 3600,
            minute: seconds / 60 % 60,
            second: seconds % 60,
            microsecond: rest % 1_000_000,
        })
    }

    fn anchor_offset(&self) -> i32 {
        // ANCHOR_SECOND 在 jiff 的范围内。
        match Timestamp::from_second(ANCHOR_SECOND) {
            Ok(timestamp) => self.tz.to_offset(timestamp).seconds(),
            Err(_) => 0,
        }
    }
}

fn range() -> EngineError {
    EngineError::invalid(OUT_OF_RANGE)
}

fn no_database() -> Result<Option<String>> {
    Ok(None)
}

/// 一次操作里可能用到的三处时区：请求参数、集合属性与（按需读取的）数据库属性。
///
/// `validate_request` 只看请求值；`effective` 才按「请求 → 集合 → 数据库」解析，只有走到数据库一层才会调用
/// 数据库闭包，因此不需要时区的操作不受数据库属性文件状态影响。调用者不得在持有集合锁时构造或调用闭包。
pub struct TimezoneSource<'a> {
    request: Option<String>,
    collection: Option<String>,
    database: &'a dyn Fn() -> Result<Option<String>>,
    resolved: RefCell<Option<Option<Zone>>>,
}

impl<'a> TimezoneSource<'a> {
    /// 没有任何时区配置：无偏移的时间字符串报 3.x 的「需要时区」错误。
    pub fn none() -> TimezoneSource<'static> {
        TimezoneSource {
            request: None,
            collection: None,
            database: &no_database,
            resolved: RefCell::new(None),
        }
    }

    /// `request` 是请求里的 `timezone`（空串视为没有），`collection` 是集合属性值。
    pub fn new(
        request: Option<String>,
        collection: Option<String>,
        database: &'a dyn Fn() -> Result<Option<String>>,
    ) -> Self {
        Self {
            request: request.filter(|name| !name.is_empty()),
            collection: collection.filter(|name| !name.is_empty()),
            database,
            resolved: RefCell::new(None),
        }
    }

    /// 校验请求里的时区名（3.x 在过滤编译处与响应编码处各做一次）；没有请求时区时什么也不做。
    pub fn validate_request(&self) -> Result<()> {
        match &self.request {
            Some(name) => resolve(name).map(drop),
            None => Ok(()),
        }
    }

    /// 开始请求的下一个阶段（响应编码）：丢弃上一阶段解析出的有效时区。3.x 在过滤编译与响应编码处各读一次
    /// 当前的数据库属性，期间数据库时区变化会被看到，所以不按请求记忆（0025「时区解析的归属与锁」）。
    pub fn next_phase(&self) {
        *self.resolved.borrow_mut() = None;
    }

    /// 有效时区：请求 → 集合 → 数据库；都没有配置为 `None`。同一阶段内只解析一次。集合与数据库的取值在 3.x 里不重新校验，
    /// 解析失败时与使用处一致地报错。
    pub fn effective(&self) -> Result<Option<Zone>> {
        if let Some(zone) = &*self.resolved.borrow() {
            return Ok(zone.clone());
        }
        let name = match &self.request {
            Some(name) => Some(name.clone()),
            None => match &self.collection {
                Some(name) => Some(name.clone()),
                None => (self.database)()?.filter(|name| !name.is_empty()),
            },
        };
        let zone = name.as_deref().map(resolve).transpose()?;
        *self.resolved.borrow_mut() = Some(zone.clone());
        Ok(zone)
    }

    /// 无偏移的时间字符串需要的时区；没有配置时报 3.x 的 6。
    pub(crate) fn for_naive(&self) -> Result<Zone> {
        self.effective()?
            .ok_or_else(|| EngineError::schema(crate::filter::temporal::NAIVE_MESSAGE))
    }

    /// `time_fields` 的时区；没有配置时用 `UTC`。
    pub fn for_time_fields(&self) -> Result<Zone> {
        Ok(self.effective()?.unwrap_or_else(Zone::utc))
    }

    /// 无偏移的时间字符串 → UTC 微秒；带偏移的字符串不需要时区，由 [`crate::timestamptz`] 直接解析。
    pub fn parse_timestamp(&self, text: &str) -> Result<i64> {
        crate::timestamptz::parse_with(text, self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn utc_micros(zone: &str, local: &str) -> Result<i64> {
        let source = TimezoneSource::new(Some(zone.to_owned()), None, &no_database);
        source.parse_timestamp(local)
    }

    #[test]
    fn names_are_case_insensitive_and_unknown_names_report_6() {
        for name in [
            "UTC",
            "utc",
            "asia/shanghai",
            "ASIA/SHANGHAI",
            "Asia/Calcutta",
        ] {
            assert!(resolve(name).is_ok(), "{name}");
        }
        for name in [
            "Nope/Zone",
            " UTC",
            "UTC ",
            "America",
            "zone.tab",
            "tzdata.zi",
            "posixrules",
            "localtime",
            "Z",
            "+08:00",
            "UTC\0",
        ] {
            assert!(
                matches!(resolve(name), Err(EngineError::SchemaValidation(m)) if m.starts_with("unknown timezone ")),
                "{name:?}"
            );
        }
        assert_eq!(
            resolve("UTC\0").unwrap_err().to_string(),
            "unknown timezone 'UTC\\x00'"
        );
    }

    #[test]
    fn path_shaped_names_report_the_zoneinfo_errors_with_code_1() {
        for (name, message) in [
            (
                "/etc/localtime",
                "ZoneInfo keys may not be absolute paths, got: /etc/localtime",
            ),
            (
                "Asia/Shanghai/",
                "ZoneInfo keys must be normalized relative paths, got: Asia/Shanghai/",
            ),
            (
                "America//New_York",
                "ZoneInfo keys must be normalized relative paths, got: America//New_York",
            ),
            (
                "../etc/passwd",
                "ZoneInfo keys must refer to subdirectories of TZPATH, got: ../etc/passwd",
            ),
            (
                "..",
                "ZoneInfo keys must refer to subdirectories of TZPATH, got: ..",
            ),
            (
                ".",
                "ZoneInfo keys must refer to subdirectories of TZPATH, got: .",
            ),
            (
                "a/../b",
                "ZoneInfo keys must be normalized relative paths, got: a/../b",
            ),
        ] {
            assert!(
                matches!(resolve(name), Err(EngineError::InvalidArgument(m)) if m == message),
                "{name:?}"
            );
        }
    }

    // 期望值取自 3.x（CPython 3.12 + 系统 tzdata）对同一输入的 `replace(tzinfo=ZoneInfo)` 换算。
    #[test]
    fn local_times_follow_fold_zero() {
        let cases = [
            // 夏令时开始：不存在的本地时间沿用变换前的偏移。
            (
                "America/New_York",
                "2025-03-09T02:30:00",
                "2025-03-09T07:30:00Z",
            ),
            (
                "America/New_York",
                "2025-03-09T01:59:59.999999",
                "2025-03-09T06:59:59.999999Z",
            ),
            (
                "America/New_York",
                "2025-03-09T03:00:00",
                "2025-03-09T07:00:00Z",
            ),
            // 夏令时结束：重复的本地时间取第一次出现。
            (
                "America/New_York",
                "2025-11-02T01:30:00",
                "2025-11-02T05:30:00Z",
            ),
            (
                "America/New_York",
                "2025-11-02T00:59:59",
                "2025-11-02T04:59:59Z",
            ),
            (
                "America/New_York",
                "2025-11-02T02:00:00",
                "2025-11-02T07:00:00Z",
            ),
            (
                "Europe/London",
                "2025-03-30T01:30:00",
                "2025-03-30T01:30:00Z",
            ),
            (
                "Europe/London",
                "2025-10-26T01:30:00",
                "2025-10-26T00:30:00Z",
            ),
            (
                "Australia/Lord_Howe",
                "2025-10-05T02:15:00",
                "2025-10-04T15:45:00Z",
            ),
            (
                "Australia/Lord_Howe",
                "2025-04-06T01:45:00",
                "2025-04-05T14:45:00Z",
            ),
            // 历史偏移（含秒）。
            (
                "America/New_York",
                "1800-01-01T00:00:00",
                "1800-01-01T04:56:02Z",
            ),
            (
                "Pacific/Honolulu",
                "1896-01-01T00:00:00",
                "1896-01-01T10:31:26Z",
            ),
            (
                "Asia/Shanghai",
                "2025-01-01T08:00:00",
                "2025-01-01T00:00:00Z",
            ),
            (
                "Asia/Kathmandu",
                "2025-01-01T00:00:00",
                "2024-12-31T18:15:00Z",
            ),
            // 遥远的未来按 POSIX 规则。
            (
                "America/New_York",
                "2100-07-01T12:00:00",
                "2100-07-01T16:00:00Z",
            ),
            (
                "America/New_York",
                "2400-11-04T01:30:00",
                "2400-11-04T05:30:00Z",
            ),
            ("Etc/GMT+12", "0001-01-01T00:00:00", "0001-01-01T12:00:00Z"),
            // 高端：补算路径。
            ("Asia/Tokyo", "9999-12-31T23:59:59", "9999-12-31T14:59:59Z"),
            ("Asia/Tokyo", "9999-12-31T12:00:00", "9999-12-31T03:00:00Z"),
            (
                "Pacific/Kiritimati",
                "9999-12-31T23:59:59",
                "9999-12-31T09:59:59Z",
            ),
            ("Etc/GMT-14", "9999-12-31T23:59:59", "9999-12-31T09:59:59Z"),
            (
                "UTC",
                "9999-12-31T23:59:59.999999",
                "9999-12-31T23:59:59.999999Z",
            ),
            ("Asia/Tokyo", "0001-01-01T09:18:59", "0001-01-01T00:00:00Z"),
            (
                "America/New_York",
                "9999-12-31T00:00:00",
                "9999-12-31T05:00:00Z",
            ),
            (
                "America/New_York",
                "0001-01-01T00:00:00",
                "0001-01-01T04:56:02Z",
            ),
        ];
        for (zone, local, expected) in cases {
            let expected = crate::timestamptz::parse(expected).unwrap();
            assert_eq!(utc_micros(zone, local).unwrap(), expected, "{zone} {local}");
        }
    }

    #[test]
    fn conversions_outside_the_datetime_range_report_1() {
        for (zone, local) in [
            ("America/New_York", "9999-12-31T23:59:59"),
            ("Asia/Tokyo", "0001-01-01T00:00:00"),
            ("Asia/Tokyo", "0001-01-01T09:00:00"),
        ] {
            let result = utc_micros(zone, local);
            assert!(
                matches!(&result, Err(EngineError::InvalidArgument(m)) if m == OUT_OF_RANGE),
                "{zone} {local}: {result:?}"
            );
        }
    }

    #[test]
    fn components_use_the_zone_and_overflow_at_the_edges() {
        let zone = resolve("America/New_York").unwrap();
        let micros = crate::timestamptz::parse("2025-03-09T07:30:15.25Z").unwrap();
        assert_eq!(
            zone.components(micros).unwrap(),
            Components {
                year: 2025,
                month: 3,
                day: 9,
                hour: 3,
                minute: 30,
                second: 15,
                microsecond: 250_000
            }
        );
        let tokyo = resolve("Asia/Tokyo").unwrap();
        let top = crate::timestamptz::parse("9999-12-31T14:59:59Z").unwrap();
        assert_eq!(tokyo.components(top).unwrap().day, 31);
        assert_eq!(tokyo.components(top).unwrap().hour, 23);
        assert!(matches!(
            tokyo.components(MAX_MICROS),
            Err(EngineError::InvalidArgument(m)) if m == OUT_OF_RANGE
        ));
        assert!(matches!(
            zone.components(MIN_MICROS),
            Err(EngineError::InvalidArgument(m)) if m == OUT_OF_RANGE
        ));
        assert!(tokyo.components(MIN_MICROS).is_ok());
        assert_eq!(Zone::utc().components(MAX_MICROS).unwrap().year, 9999);
    }

    #[test]
    fn priority_is_request_then_collection_then_database_and_the_database_is_read_lazily() {
        use std::cell::Cell;
        let calls = Cell::new(0);
        let database = || {
            calls.set(calls.get() + 1);
            Ok(Some("Asia/Tokyo".to_owned()))
        };
        let micros =
            |source: &TimezoneSource| source.parse_timestamp("2025-01-01T09:00:00").unwrap();
        let epoch = crate::timestamptz::parse("2025-01-01T00:00:00Z").unwrap();
        // 请求优先于集合与数据库，数据库没有被读取。
        let source = TimezoneSource::new(
            Some("Asia/Shanghai".into()),
            Some("Asia/Tokyo".into()),
            &database,
        );
        assert_eq!(micros(&source), epoch + 3_600_000_000);
        assert_eq!(calls.get(), 0);
        // 集合优先于数据库。
        let source = TimezoneSource::new(None, Some("Asia/Shanghai".into()), &database);
        assert_eq!(micros(&source), epoch + 3_600_000_000);
        assert_eq!(calls.get(), 0);
        // 请求为空串视为没有；只剩数据库时才读取，且同一操作只读一次。
        let source = TimezoneSource::new(Some(String::new()), None, &database);
        assert_eq!(micros(&source), epoch);
        assert_eq!(micros(&source), epoch);
        assert_eq!(calls.get(), 1);
        // validate_request 与 effective 的区别：只校验请求值，不碰数据库。
        let source = TimezoneSource::new(None, None, &database);
        source.validate_request().unwrap();
        assert_eq!(calls.get(), 1);
        assert!(
            TimezoneSource::new(Some("bad".into()), None, &database)
                .validate_request()
                .is_err()
        );
        // 没有任何配置：无偏移字符串报需要时区，time_fields 用 UTC。
        let none = TimezoneSource::none();
        assert_eq!(
            none.parse_timestamp("2025-01-01T09:00:00")
                .unwrap_err()
                .to_string(),
            crate::filter::temporal::NAIVE_MESSAGE
        );
        assert_eq!(
            none.for_time_fields()
                .unwrap()
                .components(epoch)
                .unwrap()
                .hour,
            0
        );
    }

    #[test]
    fn each_phase_reads_the_current_database_zone() {
        use std::cell::Cell;
        let zone = Cell::new("UTC");
        let database = || Ok(Some(zone.get().to_owned()));
        let source = TimezoneSource::new(None, None, &database);
        let epoch = crate::timestamptz::parse("2025-01-01T00:00:00Z").unwrap();
        // 过滤编译阶段：解析一次并在本阶段内复用。
        assert_eq!(
            source.parse_timestamp("2025-01-01T09:00:00").unwrap(),
            epoch + 9 * 3_600_000_000
        );
        zone.set("Asia/Tokyo");
        assert_eq!(
            source.parse_timestamp("2025-01-01T09:00:00").unwrap(),
            epoch + 9 * 3_600_000_000
        );
        // 响应编码阶段重新读取，看到期间的变化。
        source.next_phase();
        assert_eq!(
            source
                .for_time_fields()
                .unwrap()
                .components(epoch)
                .unwrap()
                .hour,
            9
        );
        assert_eq!(
            source.parse_timestamp("2025-01-01T09:00:00").unwrap(),
            epoch
        );
    }

    #[test]
    fn a_failing_database_read_only_matters_when_the_database_zone_is_needed() {
        let database = || Err(EngineError::Storage("unreadable".into()));
        let source = TimezoneSource::new(None, None, &database);
        source.validate_request().unwrap();
        assert!(source.parse_timestamp("2025-01-01T00:00:00Z").is_ok());
        assert!(matches!(
            source.parse_timestamp("2025-01-01T00:00:00"),
            Err(EngineError::Storage(_))
        ));
        let with_collection = TimezoneSource::new(None, Some("UTC".into()), &database);
        assert!(
            with_collection
                .parse_timestamp("2025-01-01T00:00:00")
                .is_ok()
        );
    }
}
