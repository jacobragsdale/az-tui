//! `config.toml`: the one file a user — or the `theme` tool — writes to say
//! which subscriptions, vaults and registries to read and how the TUI should
//! look. It lives in `$XDG_CONFIG_HOME/az-tui/` (`~/.config` by default, on
//! macOS too), and it is optional: a missing file is the default
//! configuration.
//!
//! ```toml
//! [azure]
//! subscriptions = ["<guid>"]              # left out: every subscription the login can see
//! vaults = ["kv-dev", "kv-qa", "kv-prod"] # only these, in this order; left out: all
//! registries = ["acrdev", "acrprod"]
//! refresh = 300                           # seconds between background refreshes; 0 = only `r`
//! parallel = 8                            # vaults or repositories read at once during a refresh
//!
//! [theme]
//! preset = "custom"          # terminal · terminal-light · mono · custom
//!
//! [theme.custom]             # what `theme apply` writes, in its own words
//! name = "neon-void"
//! appearance = "dark"
//! bg = "#05060a"
//! # …
//! ```
//!
//! Every value here is a default: a flag or an `AZ_TUI_*` variable still wins
//! over the file. Keys this build does not know are ignored, so the file can
//! grow without an older binary refusing it. The `[theme.custom]` table is
//! byte-for-byte ticket-tui's, so the `theme` tool writes one file both read.

use std::path::Path;

use anyhow::{Context, Result};
use ratatui::style::Color;
use serde::{Deserialize, Deserializer};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub azure: Azure,
    #[serde(default)]
    pub theme: ThemeSection,
}

/// Which subscriptions, vaults and registries this run reads, and how often.
/// An empty list means "everything the login can reach"; a non-empty one is
/// an allowlist that also fixes the order the tables read in.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
pub struct Azure {
    #[serde(default, deserialize_with = "one_or_many")]
    pub subscriptions: Vec<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub vaults: Vec<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub registries: Vec<String>,
    /// Seconds between background refreshes. `Some(0)` turns the timer off
    /// and leaves `r` as the only way to read again.
    #[serde(default)]
    pub refresh: Option<u64>,
    /// How many vaults, registries or repositories a refresh reads at once.
    /// Left out is [`DEFAULT_PARALLEL`]; anything below one is one.
    #[serde(default)]
    pub parallel: Option<usize>,
}

/// How many reads a refresh runs side by side when the file does not say.
/// Eight is well inside every plane's per-resource quota; a Basic-tier
/// registry with hundreds of repositories is the one place to turn it down.
pub const DEFAULT_PARALLEL: usize = 8;

impl Azure {
    /// The threads a refresh fans out over.
    #[must_use]
    pub fn threads(&self) -> usize {
        self.parallel.unwrap_or(DEFAULT_PARALLEL).max(1)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
pub struct ThemeSection {
    /// Which theme to paint with, when the file says. Left out, the custom
    /// palette is used if there is one, and the terminal's own colours if not.
    #[serde(default)]
    pub preset: Option<String>,
    #[serde(default)]
    pub custom: Option<Palette>,
}

/// One palette in the `theme` tool's vocabulary: the grounds from the window
/// back, three weights of text, one accent, and seven hues. The tool also
/// writes a `name` and an `appearance`; nothing here reads them, and unknown
/// keys are ignored.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct Palette {
    pub bg: Rgb,
    pub bg_deep: Rgb,
    pub surface: Rgb,
    pub overlay: Rgb,
    pub fg: Rgb,
    pub subtle: Rgb,
    pub muted: Rgb,
    pub accent: Rgb,
    pub red: Rgb,
    pub green: Rgb,
    pub yellow: Rgb,
    pub blue: Rgb,
    pub cyan: Rgb,
    pub orange: Rgb,
    pub teal: Rgb,
}

/// A `#rrggbb` colour.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Parses `#rrggbb`, case-insensitively.
    pub fn parse(raw: &str) -> Result<Self> {
        let digits = raw
            .strip_prefix('#')
            .filter(|digits| digits.len() == 6)
            .with_context(|| format!("expected #rrggbb, got {raw:?}"))?;
        let value = u32::from_str_radix(digits, 16)
            .with_context(|| format!("expected #rrggbb, got {raw:?}"))?;
        // Six hex digits fit in three bytes, so every shift below truncates
        // nothing.
        #[allow(clippy::cast_possible_truncation)]
        Ok(Self(
            (value >> 16) as u8,
            (value >> 8 & 0xff) as u8,
            (value & 0xff) as u8,
        ))
    }

    /// The colour `t` of the way from this one to `other`, for the tints a
    /// palette does not name — a hover a shade lighter than the ground.
    #[must_use]
    pub fn mix(self, other: Self, t: f32) -> Self {
        let channel = |from: u8, to: u8| {
            let mixed = f32::from(from) + (f32::from(to) - f32::from(from)) * t;
            // Clamped to a byte before the cast, so nothing is truncated.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let byte = mixed.round().clamp(0.0, 255.0) as u8;
            byte
        };
        Self(
            channel(self.0, other.0),
            channel(self.1, other.1),
            channel(self.2, other.2),
        )
    }
}

impl From<Rgb> for Color {
    fn from(Rgb(r, g, b): Rgb) -> Self {
        Self::Rgb(r, g, b)
    }
}

impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// `vaults = "kv-prod"` and `vaults = ["kv-prod"]` both read: one is the
/// common case and a list is the same thing said twice.
fn one_or_many<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(one) => vec![one],
        OneOrMany::Many(many) => many,
    })
}

/// Reads the file, or the default configuration when there is none. A file
/// that will not parse is an error naming the path and the line.
pub fn load(path: &Path) -> Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(source) => parse(&source).with_context(|| format!("reading {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn parse(source: &str) -> Result<Config> {
    let config: Config = toml::from_str(source).map_err(|error| {
        // toml's own Display carries the line and a caret; keeping it whole
        // is the shortest way to point at the mistake.
        anyhow::anyhow!("{error}")
    })?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_and_a_partial_one_both_read() {
        assert_eq!(parse("").unwrap(), Config::default());
        let config = parse("[azure]\nvaults = [\"kv-prod\", \"kv-dev\"]\n").unwrap();
        assert_eq!(config.azure.vaults, ["kv-prod", "kv-dev"]);
        assert_eq!(config.azure.refresh, None);
        assert!(config.azure.subscriptions.is_empty());
    }

    #[test]
    fn one_name_reads_as_a_list_of_one() {
        let config = parse("[azure]\nvaults = \"kv-prod\"\nregistries = \"acrprod\"\n").unwrap();
        assert_eq!(config.azure.vaults, ["kv-prod"]);
        assert_eq!(config.azure.registries, ["acrprod"]);
    }

    #[test]
    fn unknown_keys_are_ignored_so_an_older_binary_still_starts() {
        let config =
            parse("[azure]\nrefresh = 60\nclusters = [\"aks-prod\"]\n\n[future]\nx = 1\n").unwrap();
        assert_eq!(config.azure.refresh, Some(60));
    }

    #[test]
    fn a_broken_file_names_the_line() {
        let error = parse("[azure]\nvaults = [\n").unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("line 2"), "{message}");
    }

    #[test]
    fn a_missing_file_is_the_default_configuration() {
        let config = load(Path::new("/nonexistent/az-tui/config.toml")).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn the_path_is_in_the_message_when_a_real_file_will_not_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[theme]\npreset = [1]\n").unwrap();
        let message = format!("{:#}", load(&path).unwrap_err());
        assert!(message.contains("config.toml"), "{message}");
    }

    #[test]
    fn mixing_moves_each_channel_part_of_the_way() {
        let black = Rgb(0, 0, 0);
        let white = Rgb(255, 255, 255);
        assert_eq!(black.mix(white, 0.5), Rgb(128, 128, 128));
        assert_eq!(black.mix(white, 0.0), black);
        assert_eq!(black.mix(white, 1.0), white);
    }

    #[test]
    fn a_colour_is_six_hex_digits_behind_a_hash() {
        assert_eq!(Rgb::parse("#bb9af7").unwrap(), Rgb(0xbb, 0x9a, 0xf7));
        assert!(Rgb::parse("bb9af7").is_err());
        assert!(Rgb::parse("#bb9").is_err());
        assert!(Rgb::parse("#gggggg").is_err());
    }
}
