//! Numbers and times as people read them.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes} B"),
        _ if value >= 100.0 => format!("{value:.0} {}", UNITS[unit]),
        _ => format!("{value:.1} {}", UNITS[unit]),
    }
}

pub fn count(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// "3 分钟前", "2 天前".
pub fn ago(unix: i64) -> String {
    let seconds = (now_unix() - unix).max(0);
    match seconds {
        0..60 => "刚刚".to_owned(),
        60..3_600 => format!("{} 分钟前", seconds / 60),
        3_600..86_400 => format!("{} 小时前", seconds / 3_600),
        86_400..2_592_000 => format!("{} 天前", seconds / 86_400),
        2_592_000..31_536_000 => format!("{} 个月前", seconds / 2_592_000),
        _ => format!("{} 年前", seconds / 31_536_000),
    }
}

pub fn clock(time: SystemTime) -> String {
    let unix = time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let local = unix as i64 + local_offset_seconds();
    let day = local.rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", day / 3_600, day % 3_600 / 60, day % 60)
}

#[cfg(windows)]
fn local_offset_seconds() -> i64 {
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    let mut info: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
    let state = unsafe { GetTimeZoneInformation(&mut info) };
    // TIME_ZONE_ID_DAYLIGHT
    let daylight = if state == 2 { info.DaylightBias } else { 0 };
    -i64::from(info.Bias + daylight) * 60
}

#[cfg(unix)]
fn local_offset_seconds() -> i64 {
    let now = now_unix() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(250 << 20), "250 MiB");
        assert_eq!(bytes(3 << 30), "3.0 GiB");
    }

    #[test]
    fn counts_have_separators() {
        assert_eq!(count(7), "7");
        assert_eq!(count(1234), "1,234");
        assert_eq!(count(104_330), "104,330");
    }
}
