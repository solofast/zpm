//! RFC3339 UTC 时间工具（秒级精度，无外部依赖）。

use std::time::{SystemTime, UNIX_EPOCH};

/// Unix 秒 → RFC3339 UTC 字符串（如 `1970-01-01T00:00:00Z`）。
/// 采用 civil-from-days 算法（Howard Hinnant），不引入时间库依赖。
pub fn rfc3339_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (rem / 3_600, rem % 3_600 / 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// 当前 UTC 时间的 RFC3339 字符串（秒级精度）。
pub fn rfc3339_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    rfc3339_from_unix(secs)
}

/// 天数（自 1970-01-01，可为负）→ (年, 月, 日)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468; // 移历元至 0000-03-01
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_1970() {
        assert_eq!(rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
    }

    /// 负数（纪元前）路径：-1 → 1969-12-31T23:59:59Z
    #[test]
    fn negative_epoch_is_1969() {
        assert_eq!(rfc3339_from_unix(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn known_date() {
        assert_eq!(rfc3339_from_unix(951868800), "2000-03-01T00:00:00Z");
    }

    #[test]
    fn now_shape() {
        let s = rfc3339_now();
        assert_eq!(s.len(), 20);
        assert!(s.ends_with('Z'));
    }
}
