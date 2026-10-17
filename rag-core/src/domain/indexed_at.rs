//! IndexedAt: when a source was indexed, as seconds since the Unix epoch (UTC).
//! The domain never reads the clock; the `Clock` port supplies "now".

/// Seconds since 1970-01-01T00:00:00Z.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexedAt(i64);

impl IndexedAt {
    pub fn from_epoch_seconds(seconds: i64) -> Self {
        Self(seconds)
    }

    pub fn epoch_seconds(&self) -> i64 {
        self.0
    }

    /// Seconds from `self` to `now`; zero when `now` is not later.
    pub fn age_seconds(&self, now: IndexedAt) -> u64 {
        u64::try_from(now.0.saturating_sub(self.0)).unwrap_or(0)
    }

    /// `YYYY-MM-DDTHH:MM:SSZ` in UTC.
    pub fn to_rfc3339(&self) -> String {
        let days = self.0.div_euclid(86_400);
        let seconds = self.0.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
            seconds / 3600,
            seconds % 3600 / 60,
            seconds % 60
        )
    }
}

/// Proleptic Gregorian date for a day count since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::IndexedAt;

    fn at(seconds: i64) -> IndexedAt {
        IndexedAt::from_epoch_seconds(seconds)
    }

    #[test]
    fn keeps_the_epoch_seconds() {
        assert_eq!(at(42).epoch_seconds(), 42);
    }

    #[test]
    fn age_is_the_difference_to_now() {
        assert_eq!(at(100).age_seconds(at(160)), 60);
    }

    #[test]
    fn age_is_zero_when_now_is_earlier() {
        assert_eq!(at(100).age_seconds(at(50)), 0);
    }

    #[test]
    fn formats_the_epoch() {
        assert_eq!(at(0).to_rfc3339(), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn formats_a_known_instant() {
        assert_eq!(at(1_700_000_000).to_rfc3339(), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn formats_a_leap_day() {
        assert_eq!(at(951_782_400).to_rfc3339(), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn formats_instants_before_the_epoch() {
        assert_eq!(at(-1).to_rfc3339(), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn orders_by_time() {
        assert!(at(1) < at(2));
    }
}
