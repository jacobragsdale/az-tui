//! One instant, in UTC, however Azure wrote it.
//!
//! Key Vault writes its attribute stamps as unix seconds; a container
//! registry writes RFC 3339 strings. Both land here so that sorting, the
//! cache and every relative age in the UI read one type.
//!
//! Lifted from ticket-tui and trimmed to what two tables need: parse, format
//! a calendar date, and say how long ago something was in one or two
//! characters.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{Date, OffsetDateTime, UtcOffset};

const DATE_ONLY: &[time::format_description::FormatItem<'static>] =
    format_description!("[year]-[month]-[day]");

/// A UTC instant. Normalised on the way in, so ordering and display never
/// depend on which offset the service happened to write.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Timestamp {
    instant: OffsetDateTime,
}

impl Timestamp {
    #[must_use]
    pub fn now() -> Self {
        Self::from_offset_date_time(OffsetDateTime::now_utc())
    }

    #[must_use]
    pub fn from_offset_date_time(instant: OffsetDateTime) -> Self {
        Self {
            instant: instant.to_offset(UtcOffset::UTC),
        }
    }

    /// An RFC 3339 stamp, as a registry writes them.
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        if let Ok(parsed) = OffsetDateTime::parse(trimmed, &Rfc3339) {
            return Some(Self::from_offset_date_time(parsed));
        }
        Date::parse(trimmed, DATE_ONLY)
            .ok()
            .map(|date| Self::from_offset_date_time(date.midnight().assume_utc()))
    }

    /// A unix second, as a vault writes its attributes. A vault writes `null`
    /// for an attribute that is not set, which is `None` rather than 1970.
    #[must_use]
    pub fn from_unix(seconds: i64) -> Option<Self> {
        OffsetDateTime::from_unix_timestamp(seconds)
            .ok()
            .map(Self::from_offset_date_time)
    }

    #[must_use]
    pub fn to_rfc3339(self) -> String {
        self.instant
            .format(&Rfc3339)
            .unwrap_or_else(|_| self.calendar_date())
    }

    /// `2026-09-11`, which is what the details pane prints beside an age.
    #[must_use]
    pub fn calendar_date(self) -> String {
        self.instant
            .format(DATE_ONLY)
            .unwrap_or_else(|_| self.instant.to_string())
    }

    /// Whole seconds from this instant to `later`, negative when `later` is
    /// before it. How an expiry says whether it has already passed.
    #[must_use]
    pub fn seconds_until(self, later: Self) -> i64 {
        (later.instant - self.instant).whole_seconds()
    }

    /// How long ago this was, in the fewest characters that still say it:
    /// `now`, `40m`, `6h`, `3d`, `6mo`, `2y`. A stamp in the future reads the
    /// same way — the caller says "in 12d" or "expired 3d ago" around it.
    #[must_use]
    pub fn relative_age(self, now: Self) -> String {
        let seconds = self.seconds_until(now).abs();
        let minutes = seconds / 60;
        let hours = minutes / 60;
        let days = hours / 24;
        if minutes < 1 {
            return "now".to_owned();
        }
        if hours < 1 {
            return format!("{minutes}m");
        }
        if days < 1 {
            return format!("{hours}h");
        }
        // A month is taken as 30 days and a year as 365: this is a column
        // eight characters wide, not an invoice.
        if days < 60 {
            return format!("{days}d");
        }
        if days < 365 {
            return format!("{}mo", days / 30);
        }
        format!("{}y", days / 365)
    }
}

/// A stamp's age for a cell, or a dash where there is no stamp.
#[must_use]
pub fn age(stamp: Option<Timestamp>, now: Timestamp) -> String {
    stamp.map_or_else(|| "—".to_owned(), |stamp| stamp.relative_age(now))
}

impl fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_rfc3339())
    }
}

/// The cache holds stamps; RFC 3339 is what it holds them as, so the file
/// can be read by a person and by the next version of this program.
impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_rfc3339())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).ok_or_else(|| serde::de::Error::custom(format!("bad timestamp {raw:?}")))
    }
}

#[cfg(test)]
pub(crate) fn ts(raw: &str) -> Timestamp {
    Timestamp::parse(raw).unwrap_or_else(|| panic!("not a timestamp: {raw:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_shapes_azure_writes_land_on_the_same_instant() {
        // 2026-09-11T20:00:00Z is 1789156800 seconds after the epoch.
        assert_eq!(
            Timestamp::from_unix(1_789_156_800).unwrap(),
            ts("2026-09-11T20:00:00Z")
        );
        assert_eq!(ts("2026-09-11T15:00:00-05:00"), ts("2026-09-11T20:00:00Z"));
        assert_eq!(ts("2026-09-11").calendar_date(), "2026-09-11");
        assert!(Timestamp::parse("").is_none());
        assert!(Timestamp::parse("yesterday").is_none());
    }

    #[test]
    fn an_age_is_one_or_two_characters_and_a_unit() {
        let now = ts("2026-09-11T20:00:00Z");
        assert_eq!(ts("2026-09-11T19:59:30Z").relative_age(now), "now");
        assert_eq!(ts("2026-09-11T19:20:00Z").relative_age(now), "40m");
        assert_eq!(ts("2026-09-11T14:00:00Z").relative_age(now), "6h");
        assert_eq!(ts("2026-09-08T20:00:00Z").relative_age(now), "3d");
        assert_eq!(
            ts("2026-07-14T20:00:00Z").relative_age(now),
            "59d",
            "days up to 60"
        );
        assert_eq!(
            ts("2026-07-13T20:00:00Z").relative_age(now),
            "2mo",
            "then months"
        );
        assert_eq!(ts("2026-03-15T20:00:00Z").relative_age(now), "6mo");
        assert_eq!(ts("2024-09-11T20:00:00Z").relative_age(now), "2y");
        assert_eq!(
            ts("2026-09-14T20:00:00Z").relative_age(now),
            "3d",
            "a stamp ahead of now reads the same; the caller says which way"
        );
    }

    #[test]
    fn seconds_until_is_signed_so_an_expiry_can_say_it_has_passed() {
        let now = ts("2026-09-11T20:00:00Z");
        assert_eq!(ts("2026-09-11T19:00:00Z").seconds_until(now), 3600);
        assert_eq!(ts("2026-09-11T21:00:00Z").seconds_until(now), -3600);
    }

    #[test]
    fn a_stamp_round_trips_through_the_cache_as_rfc_3339() {
        let stamp = ts("2026-09-11T20:00:00Z");
        let json = serde_json::to_string(&stamp).unwrap();
        assert_eq!(json, "\"2026-09-11T20:00:00Z\"");
        assert_eq!(serde_json::from_str::<Timestamp>(&json).unwrap(), stamp);
        assert!(serde_json::from_str::<Timestamp>("\"nope\"").is_err());
    }
}
