"""固定参照环境（CPython 3.12 的 zoneinfo）的时区换算真值：无偏移本地时间 → UTC（`replace(tzinfo=...)` 的
`fold=0`）与 UTC → 本地各分量（`astimezone`）；升级参照环境或 jiff 时重生成并核对。

用法：`<参照 python> tests/differential/generate_timezone_fixtures.py [--verify]`。样本只取规范时区名与规则稳定的
区间（夏令时边界取 2025 年全部变换点，其余是历史、未来与 `datetime` 范围两端的固定点），不依赖平台 tzdata 的大小写行为。
"""
import json
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path
from zoneinfo import ZoneInfo

ZONES = [
    "UTC", "Etc/GMT+12", "Etc/GMT-14", "Asia/Shanghai", "Asia/Tokyo", "Asia/Kolkata", "Asia/Kathmandu", "Asia/Tehran",
    "Asia/Kabul", "Asia/Dubai", "Asia/Pyongyang", "Europe/London", "Europe/Amsterdam", "Europe/Moscow", "Europe/Dublin",
    "Europe/Lisbon", "America/New_York", "America/Chicago", "America/Los_Angeles", "America/Sao_Paulo",
    "America/St_Johns", "America/Santiago", "America/Havana", "America/Caracas", "Pacific/Honolulu", "Pacific/Apia",
    "Pacific/Kiritimati", "Pacific/Chatham", "Pacific/Auckland", "Pacific/Fiji", "Australia/Sydney",
    "Australia/Lord_Howe", "Australia/Adelaide", "Africa/Casablanca", "Africa/Cairo", "Africa/Lagos",
    "Atlantic/Azores", "Antarctica/Troll",
]
FIXED_LOCAL = [
    "0001-01-01T12:00:00", "1800-06-01T12:00:00", "1883-11-18T12:00:00", "1900-01-01T00:00:00", "1970-01-01T00:00:00",
    "2000-06-15T12:30:00", "2024-07-01T00:00:00", "2025-01-01T08:00:00", "2035-03-01T01:30:00", "2100-07-01T12:00:00",
    "2100-03-14T02:30:00", "2400-11-04T01:30:00", "9999-12-28T12:00:00", "9999-12-29T00:00:00", "9999-12-30T23:30:00",
    "9999-12-31T00:00:00", "9999-12-31T12:00:00", "9999-12-31T23:59:59.999999", "0001-01-01T00:00:00",
    "0001-01-01T09:00:00", "0001-01-01T23:59:59.999999",
]
UTC = timezone.utc
EPOCH = datetime(1970, 1, 1, tzinfo=UTC)


def iso_local(value):
    text = value.strftime("%Y-%m-%dT%H:%M:%S")
    return text + (f".{value.microsecond:06d}" if value.microsecond else "")


def iso_z(value):
    return iso_local(value) + "Z"


def transitions(zone):
    """2025 年内每个偏移变化点（UTC 瞬间与前后偏移），按小时扫描后二分到秒。"""
    tz = ZoneInfo(zone)
    out, step = [], timedelta(hours=1)
    start = datetime(2025, 1, 1, tzinfo=UTC)
    previous = start.astimezone(tz).utcoffset()
    for hour in range(1, 366 * 24):
        now = start + step * hour
        offset = now.astimezone(tz).utcoffset()
        if offset != previous:
            low, high = now - step, now
            while high - low > timedelta(seconds=1):
                mid = low + (high - low) / 2
                mid = mid.replace(microsecond=0)
                if mid.astimezone(tz).utcoffset() == previous:
                    low = mid
                else:
                    high = mid
            out.append((high, previous, offset))
        previous = offset
    return out


def local_to_utc(zone, text):
    try:
        local = datetime.fromisoformat(text).replace(tzinfo=ZoneInfo(zone))
        return iso_z(local.astimezone(UTC))
    except OverflowError:
        return "date value out of range"


def utc_components(zone, text):
    utc = datetime.fromisoformat(text.removesuffix("Z")).replace(tzinfo=UTC)
    try:
        local = utc.astimezone(ZoneInfo(zone))
    except OverflowError:
        return None
    return [local.year, local.month, local.day, local.hour, local.minute, local.second, local.microsecond]


def build():
    local, components = [], []
    for zone in ZONES:
        samples = list(FIXED_LOCAL)
        instants = ["0001-01-01T00:00:00Z", "1900-01-01T00:00:00Z", "1970-01-01T00:00:00Z", "2025-01-01T00:00:00.5Z",
                    "2100-07-01T12:00:00Z", "9999-12-30T21:59:59Z", "9999-12-30T22:00:00Z", "9999-12-31T14:59:59Z",
                    "9999-12-31T23:59:59.999999Z"]
        for at, before, after in transitions(zone):
            naive = at.replace(tzinfo=None)
            for shift in (before, after):
                for delta in (-timedelta(minutes=30), timedelta(0), timedelta(minutes=30)):
                    samples.append(iso_local((naive + shift + delta).replace(tzinfo=None)))
            instants += [iso_z(naive - timedelta(seconds=1)), iso_z(naive)]
        for text in dict.fromkeys(samples):
            local.append([zone, text, local_to_utc(zone, text)])
        for text in dict.fromkeys(instants):
            components.append([zone, text, utc_components(zone, text)])
    return {"reference": "CPython 3.12 zoneinfo", "local": local, "components": components}


if __name__ == "__main__":
    assert sys.version_info[:2] == (3, 12), sys.version
    target = Path(__file__).resolve().parents[2] / "crates/core/tests/fixtures/timezone_v321.json"
    text = json.dumps(build(), ensure_ascii=False, separators=(",", ":")) + "\n"
    if "--verify" in sys.argv:
        assert target.read_text() == text, "timezone fixture is out of date"
    else:
        target.write_text(text)
