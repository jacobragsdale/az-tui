//! `key:value` in the search box, per tab.
//!
//! A token is a field filter when its key is one this tab knows; anything
//! else is a word and goes to [`crate::search`] to be matched literally.
//! That is deliberate: `https://kv` should search for `https://kv`, not
//! fail because `https` is not a field.
//!
//! Every filter and every word is ANDed. There is no `or`, no negation and
//! no grouping.
//!
// ponytail: values do not take quotes, so `tag:owner=platform team` is two
// tokens. A quoted value wants one pass of a small tokeniser here; nobody has
// needed one in a search box whose whole job is finding a name.

use crate::timestamp::Timestamp;

/// A parsed query: the words to match literally, and the fields to test.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Query {
    pub words: Vec<String>,
    pub fields: Vec<(String, String)>,
}

impl Query {
    /// Splits a raw query into words and `key:value` pairs. `known` says
    /// which keys this tab understands; a token with any other key is a word.
    #[must_use]
    pub fn parse(raw: &str, known: &[&str]) -> Self {
        let mut query = Self::default();
        for token in raw.split_whitespace() {
            match token.split_once(':') {
                Some((key, value)) if known.iter().any(|held| held.eq_ignore_ascii_case(key)) => {
                    // A key this tab knows with nothing after the colon is
                    // nothing yet: half-typing a filter must not empty the
                    // table on the way to typing it.
                    if !value.is_empty() {
                        query
                            .fields
                            .push((key.to_ascii_lowercase(), value.to_owned()));
                    }
                }
                _ => query.words.push(token.to_owned()),
            }
        }
        query
    }
}

/// `yes`, `no`, `true`, `false`, `1`, `0`. Anything else is not an opinion
/// and the filter is ignored rather than matching nothing.
#[must_use]
pub fn boolean(raw: &str) -> Option<bool> {
    match raw.to_ascii_lowercase().as_str() {
        "yes" | "true" | "y" | "1" => Some(true),
        "no" | "false" | "n" | "0" => Some(false),
        _ => None,
    }
}

/// What a `<Nd` / `>Nd` / `none` / `expired` filter asks about a stamp.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum When {
    /// `<30d`: set, and within that many days from now — an expiry coming up,
    /// or something changed recently.
    Within(i64),
    /// `>30d`: set, and further away than that.
    Beyond(i64),
    /// `none`: not set at all.
    Never,
    /// `expired`: set, and already in the past.
    Past,
}

impl When {
    /// Reads `<30d`, `>7d`, `30d`, `none` or `expired`. A bare number of days
    /// means `<`, because that is what anybody means by `expires:30d`.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        match raw.to_ascii_lowercase().as_str() {
            "none" | "never" | "-" => return Some(Self::Never),
            "expired" | "past" => return Some(Self::Past),
            _ => {}
        }
        let (make, rest) = match raw.strip_prefix('<') {
            Some(rest) => (Self::Within as fn(i64) -> Self, rest),
            None => match raw.strip_prefix('>') {
                Some(rest) => (Self::Beyond as fn(i64) -> Self, rest),
                None => (Self::Within as fn(i64) -> Self, raw),
            },
        };
        let days: i64 = rest.trim_end_matches(['d', 'D']).trim().parse().ok()?;
        Some(make(days))
    }

    /// Whether one stamp answers this filter, measured from `now`.
    #[must_use]
    pub fn holds(self, stamp: Option<Timestamp>, now: Timestamp) -> bool {
        let Some(stamp) = stamp else {
            return self == Self::Never;
        };
        // Positive when the stamp is still ahead of now.
        let seconds = now.seconds_until(stamp);
        let days = seconds / 86_400;
        match self {
            Self::Never => false,
            Self::Past => seconds < 0,
            // Something already past is inside every window: an expiry three
            // days gone is certainly "expiring within 30 days".
            Self::Within(limit) => days <= limit,
            Self::Beyond(limit) => days > limit,
        }
    }
}

/// A `tag:key` or `tag:key=value` test against a row's sorted tags.
#[must_use]
pub fn tag_matches(tags: &[(String, String)], filter: &str) -> bool {
    match filter.split_once('=') {
        Some((key, value)) => tags.iter().any(|(held_key, held_value)| {
            held_key.eq_ignore_ascii_case(key)
                && held_value
                    .to_ascii_lowercase()
                    .contains(&value.to_ascii_lowercase())
        }),
        None => tags.iter().any(|(held_key, _)| {
            held_key
                .to_ascii_lowercase()
                .contains(&filter.to_ascii_lowercase())
        }),
    }
}

/// Whether a cell contains what was asked for, ignoring case. The shape
/// every plain `key:` filter takes.
#[must_use]
pub fn contains(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timestamp::ts;

    const SECRETS: &[&str] = &[
        "vault", "name", "type", "enabled", "managed", "expires", "tag",
    ];

    #[test]
    fn a_token_is_a_field_only_when_the_tab_knows_the_key() {
        let query = Query::parse("db-pass vault:kv-prod enabled:no", SECRETS);
        assert_eq!(query.words, ["db-pass"]);
        assert_eq!(
            query.fields,
            [
                ("vault".to_owned(), "kv-prod".to_owned()),
                ("enabled".to_owned(), "no".to_owned())
            ]
        );
    }

    #[test]
    fn a_colon_in_something_that_is_not_a_filter_stays_a_word() {
        let query = Query::parse("https://kv-prod.vault.azure.net foo:bar", SECRETS);
        assert_eq!(
            query.words,
            ["https://kv-prod.vault.azure.net", "foo:bar"],
            "an unknown key is not a mistake, it is what was typed"
        );
        assert!(query.fields.is_empty());

        let query = Query::parse("vault:", SECRETS);
        assert!(
            query.words.is_empty() && query.fields.is_empty(),
            "a key with nothing after it is nothing yet, not a word that empties the table"
        );
    }

    #[test]
    fn a_boolean_reads_the_ways_people_write_one() {
        assert_eq!(boolean("yes"), Some(true));
        assert_eq!(boolean("TRUE"), Some(true));
        assert_eq!(boolean("no"), Some(false));
        assert_eq!(boolean("0"), Some(false));
        assert_eq!(boolean("maybe"), None);
    }

    #[test]
    fn a_when_reads_both_directions_and_both_absences() {
        assert_eq!(When::parse("<30d"), Some(When::Within(30)));
        assert_eq!(When::parse(">7d"), Some(When::Beyond(7)));
        assert_eq!(When::parse("30d"), Some(When::Within(30)), "bare means <");
        assert_eq!(When::parse("30"), Some(When::Within(30)));
        assert_eq!(When::parse("none"), Some(When::Never));
        assert_eq!(When::parse("expired"), Some(When::Past));
        assert_eq!(When::parse("soon"), None);
    }

    #[test]
    fn a_when_answers_about_a_stamp_ahead_of_now_and_one_behind_it() {
        let now = ts("2026-09-11T20:00:00Z");
        let in_12_days = Some(ts("2026-09-23T20:00:00Z"));
        let in_90_days = Some(ts("2026-12-10T20:00:00Z"));
        let three_days_gone = Some(ts("2026-09-08T20:00:00Z"));

        assert!(When::Within(30).holds(in_12_days, now));
        assert!(!When::Within(30).holds(in_90_days, now));
        assert!(When::Beyond(30).holds(in_90_days, now));
        assert!(!When::Beyond(30).holds(in_12_days, now));
        assert!(
            When::Within(30).holds(three_days_gone, now),
            "already expired is inside every window"
        );
        assert!(When::Past.holds(three_days_gone, now));
        assert!(!When::Past.holds(in_12_days, now));
        assert!(When::Never.holds(None, now));
        assert!(!When::Never.holds(in_12_days, now));
        assert!(
            !When::Within(30).holds(None, now),
            "a secret with no expiry is not expiring"
        );
    }

    #[test]
    fn a_tag_filter_matches_a_key_or_a_key_and_a_value() {
        let tags = [
            ("env".to_owned(), "prod".to_owned()),
            ("owner".to_owned(), "platform".to_owned()),
        ];
        assert!(tag_matches(&tags, "env"));
        assert!(tag_matches(&tags, "ENV"));
        assert!(tag_matches(&tags, "env=prod"));
        assert!(tag_matches(&tags, "env=PROD"));
        assert!(!tag_matches(&tags, "env=qa"));
        assert!(!tag_matches(&tags, "team"));
    }

    #[test]
    fn a_plain_field_is_a_case_insensitive_substring() {
        assert!(contains("kv-prod", "PROD"));
        assert!(!contains("kv-prod", "qa"));
    }
}
