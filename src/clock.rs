//! Wall-clock times as recordings name and players ask for them: UTC,
//! `YYYY-MM-DD` and `HH-MM-SS` for a recording's directories, RFC 3339 for
//! the API — what the few lines here need, rather than a calendar crate.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A time in UTC, to the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Utc {
    pub(crate) year: i64,
    pub(crate) month: u32,
    pub(crate) day: u32,
    pub(crate) hour: u32,
    pub(crate) minute: u32,
    pub(crate) second: u32,
}

impl Utc {
    /// `time`, to the second below it.
    pub(crate) fn of(time: SystemTime) -> Self {
        let seconds = time
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs() as i64);
        let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
        let of_day = seconds.rem_euclid(86_400);
        Self {
            year,
            month,
            day,
            hour: (of_day / 3600) as u32,
            minute: (of_day / 60 % 60) as u32,
            second: (of_day % 60) as u32,
        }
    }

    pub(crate) fn time(&self) -> SystemTime {
        let days = days_from_civil(self.year, self.month, self.day);
        let seconds = days * 86_400
            + i64::from(self.hour) * 3600
            + i64::from(self.minute) * 60
            + i64::from(self.second);
        UNIX_EPOCH + Duration::from_secs(seconds.max(0) as u64)
    }

    /// `YYYY-MM-DD`.
    pub(crate) fn date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// `HH-MM-SS`.
    pub(crate) fn hms(&self) -> String {
        format!("{:02}-{:02}-{:02}", self.hour, self.minute, self.second)
    }

    /// From a recording's `YYYY-MM-DD` and `HH-MM-SS` directories.
    pub(crate) fn from_dirs(date: &str, hms: &str) -> Option<Self> {
        let date: Vec<&str> = date.split('-').collect();
        let hms: Vec<&str> = hms.split('-').collect();
        let [year, month, day] = date[..] else {
            return None;
        };
        let [hour, minute, second] = hms[..] else {
            return None;
        };
        let utc = Self {
            year: year.parse().ok()?,
            month: month.parse().ok()?,
            day: day.parse().ok()?,
            hour: hour.parse().ok()?,
            minute: minute.parse().ok()?,
            second: second.parse().ok()?,
        };
        utc.valid().then_some(utc)
    }

    fn valid(&self) -> bool {
        (1..=12).contains(&self.month)
            && (1..=31).contains(&self.day)
            && self.hour < 24
            && self.minute < 60
            && self.second < 60
            && Self::of(self.time()) == *self
    }
}

/// `time` as RFC 3339 in UTC, to the millisecond: `2026-10-23T14:00:00.000Z`.
pub(crate) fn rfc3339(time: SystemTime) -> String {
    let utc = Utc::of(time);
    let millis = time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.subsec_millis());
    format!(
        "{}T{:02}:{:02}:{:02}.{millis:03}Z",
        utc.date(),
        utc.hour,
        utc.minute,
        utc.second
    )
}

/// An RFC 3339 time in UTC — `2026-10-23T14:00:00Z`, with a fraction of a
/// second or without. Another offset than `Z` is not taken.
pub(crate) fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let text = text.strip_suffix('Z').or_else(|| text.strip_suffix('z'))?;
    let (date, time) = text.split_once(['T', 't'])?;
    let (time, fraction) = match time.split_once('.') {
        Some((time, fraction)) => (time, Some(fraction)),
        None => (time, None),
    };
    let utc = Utc::from_dirs(date, &time.replace(':', "-"))?;
    if time.matches(':').count() != 2 {
        return None;
    }
    let nanos = match fraction {
        Some(digits) if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) => {
            let digits = &digits[..digits.len().min(9)];
            digits.parse::<u32>().ok()? * 10u32.pow(9 - digits.len() as u32)
        }
        Some(_) => return None,
        None => 0,
    };
    Some(utc.time() + Duration::from_nanos(u64::from(nanos)))
}

/// The day `days` after 1970-01-01, in the proleptic Gregorian calendar
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// How many days 1970-01-01 is before `year-month-day`.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_go_to_names_and_back() {
        let time = UNIX_EPOCH + Duration::from_secs(1_792_764_000); // 2026-10-23T14:00:00Z
        let utc = Utc::of(time);
        assert_eq!(
            (utc.date(), utc.hms()),
            ("2026-10-23".into(), "14-00-00".into())
        );
        assert_eq!(Utc::from_dirs("2026-10-23", "14-00-00"), Some(utc));
        assert_eq!(utc.time(), time);
        assert_eq!(
            rfc3339(time + Duration::from_millis(250)),
            "2026-10-23T14:00:00.250Z"
        );
        // A leap day, and the turn of a century that is not a leap year.
        for (date, ok) in [
            ("2024-02-29", true),
            ("2100-02-29", false),
            ("2026-13-01", false),
        ] {
            assert_eq!(Utc::from_dirs(date, "00-00-00").is_some(), ok, "{date}");
        }
        assert!(Utc::from_dirs("2026-10-23", "24-00-00").is_none());
    }

    #[test]
    fn rfc3339_is_read_in_utc_alone() {
        let time = UNIX_EPOCH + Duration::from_secs(1_792_764_000);
        assert_eq!(parse_rfc3339("2026-10-23T14:00:00Z"), Some(time));
        assert_eq!(
            parse_rfc3339("2026-10-23T14:00:00.5Z"),
            Some(time + Duration::from_millis(500))
        );
        for text in [
            "2026-10-23T14:00:00+09:00",
            "2026-10-23 14:00Z",
            "yesterday",
            "",
        ] {
            assert_eq!(parse_rfc3339(text), None, "{text}");
        }
    }
}
