//! UTC time formatting without a date-time dependency.
//!
//! Only two formats are needed and both are fixed by upstream output, so the
//! civil-calendar conversion is implemented directly rather than pulling in a
//! general date library (dependency budget, §8 of PLAN.md).

use std::time::{SystemTime, UNIX_EPOCH};

const DAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Seconds since the Unix epoch, as cloud-init records in `status.json`.
pub fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

/// RFC 2822-ish stamp used by `cloud-init status --long`:
/// `Mon, 01 Jan 2024 00:00:00 +0000`.
pub fn format_last_update(epoch: f64) -> String {
    #[allow(clippy::cast_possible_truncation)]
    let secs = epoch.floor() as i64;
    let (year, month, day, hour, minute, second, weekday) = civil_from_epoch(secs);
    let day_name = DAY_NAMES.get(weekday).copied().unwrap_or("Mon");
    let month_name = MONTH_NAMES
        .get(month.saturating_sub(1))
        .copied()
        .unwrap_or("Jan");
    format!(
        "{day_name}, {day:02} {month_name} {year:04} {hour:02}:{minute:02}:{second:02} +0000"
    )
}

/// `YYYY-MM-DD` in UTC — `datetime.now(timezone.utc).date().strftime("%Y-%m-%d")`.
pub fn format_iso_date(epoch: f64) -> String {
    #[allow(clippy::cast_possible_truncation)]
    let secs = epoch.floor() as i64;
    let (year, month, day, ..) = civil_from_epoch(secs);
    format!("{year:04}-{month:02}-{day:02}")
}

/// `YYYY-MM-DD HH:MM:SS,mmm` — the cloud-init log timestamp format.
pub fn format_log_stamp(epoch: f64) -> String {
    #[allow(clippy::cast_possible_truncation)]
    let secs = epoch.floor() as i64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let millis = ((epoch - epoch.floor()) * 1000.0) as u32;
    let (year, month, day, hour, minute, second, _) = civil_from_epoch(secs);
    format!(
        "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02},{millis:03}"
    )
}

/// `str(datetime.fromtimestamp(epoch, timezone.utc))`, which `cloud-init analyze
/// boot` interpolates verbatim: `YYYY-MM-DD HH:MM:SS[.ffffff]+00:00`.
///
/// Python omits the fractional part when it is zero, so this does too.
pub fn format_python_datetime_utc(epoch: f64) -> String {
    let micros = round_half_even_micros(epoch);
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    let (year, month, day, hour, minute, second, _) = civil_from_epoch(secs);
    let stamp =
        format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}");
    if frac == 0 {
        format!("{stamp}+00:00")
    } else {
        format!("{stamp}.{frac:06}+00:00")
    }
}

/// `datetime.fromtimestamp()` snaps an epoch float to the nearest microsecond,
/// ties to even. Every duration upstream reports is a difference of two such
/// values, so deltas must be derived from the rounded form to stay bit-identical.
#[allow(clippy::cast_possible_truncation, clippy::float_cmp)]
pub fn round_half_even_micros(epoch: f64) -> i64 {
    let scaled = epoch * 1e6;
    let floor = scaled.floor();
    let frac = scaled - floor;
    let base = floor as i64;
    if frac > 0.5 || (frac == 0.5 && base.rem_euclid(2) != 0) {
        base.saturating_add(1)
    } else {
        base
    }
}

/// Inverse of [`civil_from_epoch`] — Howard Hinnant's `days_from_civil`.
#[allow(clippy::integer_division)]
pub fn epoch_from_civil(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + hour * 3600 + minute * 60 + second
}

/// Current UTC year, which `dump.parse_timestamp()` grafts onto syslog stamps.
pub fn current_year() -> i64 {
    #[allow(clippy::cast_possible_truncation)]
    let secs = now_epoch().floor() as i64;
    civil_from_epoch(secs).0
}

type Civil = (i64, usize, i64, i64, i64, i64, usize);

/// Howard Hinnant's `civil_from_days`, plus time-of-day and weekday.
///
/// The truncating divisions are the algorithm, not an oversight: every operand is
/// non-negative here because `div_euclid`/`rem_euclid` normalise first.
#[allow(clippy::integer_division)]
fn civil_from_epoch(secs: i64) -> Civil {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;
    // 1970-01-01 was a Thursday, index 3 in a Monday-first table.
    let weekday = usize::try_from((days + 3).rem_euclid(7)).unwrap_or(0);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    (
        year,
        usize::try_from(month).unwrap_or(1),
        day,
        hour,
        minute,
        second,
        weekday,
    )
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn formats_the_epoch() {
        assert_eq!(format_last_update(0.0), "Thu, 01 Jan 1970 00:00:00 +0000");
        assert_eq!(format_log_stamp(0.0), "1970-01-01 00:00:00,000");
    }

    #[test]
    fn formats_a_known_instant() {
        // 2024-02-29T13:45:07.250Z, a leap day.
        let epoch = 1_709_214_307.25;
        assert_eq!(format_last_update(epoch), "Thu, 29 Feb 2024 13:45:07 +0000");
        assert_eq!(format_log_stamp(epoch), "2024-02-29 13:45:07,250");
    }

    #[test]
    fn handles_year_boundaries() {
        // 2023-12-31T23:59:59Z
        assert_eq!(
            format_last_update(1_704_067_199.0),
            "Sun, 31 Dec 2023 23:59:59 +0000"
        );
    }

    #[test]
    fn round_trips_civil_dates() {
        for secs in [0_i64, 1_709_214_307, 1_704_067_199, -86_400] {
            let (y, mo, d, h, mi, s, _) = civil_from_epoch(secs);
            assert_eq!(
                epoch_from_civil(y, i64::try_from(mo).unwrap(), d, h, mi, s),
                secs
            );
        }
    }

    #[test]
    fn renders_python_datetime_repr() {
        assert_eq!(
            format_python_datetime_utc(1_788_278_593.481_295),
            "2026-09-01 16:03:13.481295+00:00"
        );
        // Python drops `.000000`, so an exact second has no fractional part.
        assert_eq!(format_python_datetime_utc(0.0), "1970-01-01 00:00:00+00:00");
        assert_eq!(
            format_python_datetime_utc(-1.0),
            "1969-12-31 23:59:59+00:00"
        );
    }

    #[test]
    fn rounds_microseconds_to_even_on_ties() {
        assert_eq!(round_half_even_micros(0.000_000_5), 0);
        assert_eq!(round_half_even_micros(0.000_001_5), 2);
        assert_eq!(round_half_even_micros(0.000_001_4), 1);
    }
}
