//! Numbers and times as people read them.

use std::time::{SystemTime, UNIX_EPOCH};

/// Sizes step the way the system's file manager counts them, so that a number
/// here agrees with the one in Explorer or Finder: by 1024 on Windows, which
/// calls the result MB and GB all the same, and by 1000 on macOS.
const STEP: f64 = if cfg!(target_os = "macos") {
    1000.0
} else {
    1024.0
};

fn scaled(bytes: u64, step: f64) -> (String, &'static str) {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= step && unit < UNITS.len() - 1 {
        value /= step;
        unit += 1;
    }
    let number = if unit == 0 || value >= 100.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    };
    (number, UNITS[unit])
}

/// A size as its number and its unit, for setting them in different weights.
pub fn size(bytes: u64) -> (String, &'static str) {
    scaled(bytes, STEP)
}

pub fn bytes(bytes: u64) -> String {
    let (number, unit) = size(bytes);
    format!("{number} {unit}")
}

/// A share of a whole, never rounding something down to nothing.
pub fn percent(part: u64, whole: u64) -> String {
    if part == 0 || whole == 0 {
        return "0%".to_owned();
    }
    let share = part as f64 * 100.0 / whole as f64;
    if share < 1.0 {
        "<1%".to_owned()
    } else {
        format!("{share:.0}%")
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
    format!("{:02}:{:02}", day / 3_600, day % 3_600 / 60)
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

    fn show(bytes: u64, step: f64) -> String {
        let (number, unit) = scaled(bytes, step);
        format!("{number} {unit}")
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(show(0, 1024.0), "0 B");
        assert_eq!(show(1023, 1024.0), "1023 B");
        assert_eq!(show(1536, 1024.0), "1.5 KB");
        assert_eq!(show(250 << 20, 1024.0), "250 MB");
        assert_eq!(show(3 << 30, 1024.0), "3.0 GB");
        assert_eq!(show(1_500_000, 1000.0), "1.5 MB");
        assert_eq!(show(999, 1000.0), "999 B");
    }

    #[test]
    fn shares_are_never_rounded_to_nothing() {
        assert_eq!(percent(0, 100), "0%");
        assert_eq!(percent(1, 1000), "<1%");
        assert_eq!(percent(31, 100), "31%");
        assert_eq!(percent(5, 0), "0%");
    }

    #[test]
    fn counts_have_separators() {
        assert_eq!(count(7), "7");
        assert_eq!(count(1234), "1,234");
        assert_eq!(count(104_330), "104,330");
    }
}
