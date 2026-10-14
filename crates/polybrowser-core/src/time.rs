use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// A point in time, stored as microseconds since the Unix epoch (UTC).
///
/// Browsers disagree about epochs and units; everything is normalised to this
/// on the way in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(i64);

/// Microseconds between 1601-01-01 (the epoch Chromium uses) and 1970-01-01.
const WEBKIT_EPOCH_OFFSET_MICROS: i64 = 11_644_473_600_000_000;
/// Seconds between 1970-01-01 and 2001-01-01 (the epoch Safari uses).
const COCOA_EPOCH_OFFSET_SECS: f64 = 978_307_200.0;

impl Timestamp {
    pub const UNIX_EPOCH: Timestamp = Timestamp(0);

    pub fn now() -> Self {
        SystemTime::now().into()
    }

    pub const fn from_unix_micros(micros: i64) -> Self {
        Timestamp(micros)
    }

    pub const fn from_unix_secs(secs: i64) -> Self {
        Timestamp(secs.saturating_mul(1_000_000))
    }

    pub const fn as_unix_micros(self) -> i64 {
        self.0
    }

    pub const fn as_unix_secs(self) -> i64 {
        self.0.div_euclid(1_000_000)
    }

    /// From microseconds since 1601-01-01, as used by Chromium-family browsers.
    pub const fn from_webkit_micros(micros: i64) -> Self {
        Timestamp(micros.saturating_sub(WEBKIT_EPOCH_OFFSET_MICROS))
    }

    /// To microseconds since 1601-01-01, as used by Chromium-family browsers.
    pub const fn as_webkit_micros(self) -> i64 {
        self.0.saturating_add(WEBKIT_EPOCH_OFFSET_MICROS)
    }

    /// From seconds since 2001-01-01, as used by Safari.
    pub fn from_cocoa_secs(secs: f64) -> Self {
        Timestamp(((secs + COCOA_EPOCH_OFFSET_SECS) * 1_000_000.0) as i64)
    }

    /// To seconds since 2001-01-01, as used by Safari.
    pub fn as_cocoa_secs(self) -> f64 {
        self.0 as f64 / 1_000_000.0 - COCOA_EPOCH_OFFSET_SECS
    }

    /// Formats as an RFC 3339 / ISO 8601 UTC time with second precision,
    /// e.g. `2026-10-13T08:15:00Z`.
    pub fn to_rfc3339(self) -> String {
        let secs = self.as_unix_secs();
        let days = secs.div_euclid(86_400);
        let second_of_day = secs.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
            second_of_day / 3600,
            (second_of_day % 3600) / 60,
            second_of_day % 60
        )
    }
}

/// Days since the Unix epoch to a proleptic Gregorian (year, month, day).
/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

impl From<SystemTime> for Timestamp {
    fn from(time: SystemTime) -> Self {
        match time.duration_since(UNIX_EPOCH) {
            Ok(after) => Timestamp(i64::try_from(after.as_micros()).unwrap_or(i64::MAX)),
            Err(before) => {
                Timestamp(-i64::try_from(before.duration().as_micros()).unwrap_or(i64::MAX))
            }
        }
    }
}

impl From<Timestamp> for SystemTime {
    fn from(timestamp: Timestamp) -> Self {
        let magnitude = Duration::from_micros(timestamp.0.unsigned_abs());
        if timestamp.0 >= 0 {
            UNIX_EPOCH + magnitude
        } else {
            UNIX_EPOCH - magnitude
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_rfc3339() {
        assert_eq!(Timestamp::UNIX_EPOCH.to_rfc3339(), "1970-01-01T00:00:00Z");
        assert_eq!(
            Timestamp::from_unix_secs(951_782_400).to_rfc3339(),
            "2000-02-29T00:00:00Z"
        );
        assert_eq!(
            Timestamp::from_unix_secs(1_791_879_300).to_rfc3339(),
            "2026-10-13T08:15:00Z"
        );
        assert_eq!(
            Timestamp::from_unix_secs(-1).to_rfc3339(),
            "1969-12-31T23:59:59Z"
        );
    }

    #[test]
    fn converts_foreign_epochs() {
        // 2021-01-01T00:00:00Z in each browser's own representation.
        let expected = Timestamp::from_unix_secs(1_609_459_200);
        assert_eq!(
            Timestamp::from_webkit_micros(13_253_932_800_000_000),
            expected
        );
        assert_eq!(Timestamp::from_cocoa_secs(631_152_000.0), expected);
    }

    #[test]
    fn round_trips_system_time() {
        let timestamp = Timestamp::from_unix_micros(1_700_000_000_123_456);
        assert_eq!(Timestamp::from(SystemTime::from(timestamp)), timestamp);
    }
}
