use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) fn now_ms() -> i64 {
    // Pre-epoch clocks (or an UNIX_EPOCH underflow) report as 0.
    system_time_to_ms(SystemTime::now()).unwrap_or(0)
}

pub(crate) fn system_time_to_ms(time: SystemTime) -> Option<i64> {
    let millis = time.duration_since(UNIX_EPOCH).ok()?.as_millis();
    Some(millis.min(i64::MAX as u128) as i64)
}

pub(crate) fn system_time_from_ms(ms: i64) -> Option<SystemTime> {
    if ms < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_millis(ms as u64))
}

/// Whole milliseconds in a `Duration`, saturating instead of wrapping.
pub(crate) fn duration_to_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

/// Format a millisecond epoch timestamp as a UTC `YYYY-MM-DD` date string.
/// Hand-rolled civil-from-days math (Howard Hinnant's algorithm) so one date
/// format doesn't pull in a calendar dependency.
pub(crate) fn utc_date_string(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Whole days between a `YYYY-MM-DD` UTC date string and `now_ms`, saturating
/// at zero for future dates. `None` when the string isn't a plain civil date.
pub(crate) fn days_since_utc_date(date: &str, now_ms: i64) -> Option<f64> {
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let date_days = days_from_civil(year, month, day);
    let now_days = now_ms.div_euclid(86_400_000);
    Some((now_days - date_days).max(0) as f64)
}

/// Proleptic Gregorian `(year, month, day)` to days since 1970-01-01
/// (Hinnant's days_from_civil).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Days since 1970-01-01 to a proleptic Gregorian `(year, month, day)`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097); // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153; // March-based month [0, 11]
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_known_timestamp() {
        let ms = 1_700_000_000_123;
        let time = system_time_from_ms(ms).expect("non-negative ms is representable");
        assert_eq!(system_time_to_ms(time), Some(ms));
    }

    #[test]
    fn rejects_negative_milliseconds() {
        assert_eq!(system_time_from_ms(-1), None);
    }

    #[test]
    fn formats_known_utc_dates() {
        assert_eq!(utc_date_string(0), "1970-01-01");
        // 2023-11-14T22:13:20Z
        assert_eq!(utc_date_string(1_700_000_000_123), "2023-11-14");
        // Leap day, midnight UTC.
        assert_eq!(utc_date_string(1_709_164_800_000), "2024-02-29");
        // Last millisecond of a year stays in that year.
        assert_eq!(utc_date_string(1_735_689_600_000 - 1), "2024-12-31");
        assert_eq!(utc_date_string(1_735_689_600_000), "2025-01-01");
    }

    #[test]
    fn pre_epoch_timestamps_floor_to_the_earlier_day() {
        assert_eq!(utc_date_string(-1), "1969-12-31");
    }

    #[test]
    fn days_since_round_trips_with_utc_date_string() {
        let now = 1_700_000_000_123; // 2023-11-14
        assert_eq!(days_since_utc_date("2023-11-14", now), Some(0.0));
        assert_eq!(days_since_utc_date("2023-11-04", now), Some(10.0));
        // Future dates clamp to zero rather than going negative.
        assert_eq!(days_since_utc_date("2024-01-01", now), Some(0.0));
        assert_eq!(days_since_utc_date("", now), None);
        assert_eq!(days_since_utc_date("not-a-date", now), None);
        assert_eq!(days_since_utc_date("2023-13-01", now), None);
    }
}
