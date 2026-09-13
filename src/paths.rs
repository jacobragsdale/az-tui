//! Where the three files live: `config.toml`, the cache and the session.
//!
//! Every lookup takes the environment as a closure rather than reading it,
//! so a test can say what `XDG_CONFIG_HOME` and `HOME` hold without touching
//! the process's own environment — which two tests running side by side would
//! otherwise fight over.

use std::ffi::OsString;
use std::path::PathBuf;

/// Reads one variable, the way the process does.
fn from_env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// `$XDG_CONFIG_HOME/az-tui`, else `~/.config/az-tui` — on macOS too, because
/// that is where every other terminal program keeps its own.
fn config_dir_with(env: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    absolute(env("XDG_CONFIG_HOME"))
        .or_else(|| absolute(env("HOME")).map(|home| home.join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("az-tui")
}

/// `$XDG_DATA_HOME/az-tui`, else `~/.local/share/az-tui`, and
/// `~/Library/Application Support/az-tui` on macOS.
fn data_dir_with(env: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    absolute(env("XDG_DATA_HOME"))
        .or_else(|| {
            absolute(env("HOME")).map(|home| {
                if cfg!(target_os = "macos") {
                    home.join("Library/Application Support")
                } else {
                    home.join(".local/share")
                }
            })
        })
        .unwrap_or_else(|| PathBuf::from(".local/share"))
        .join("az-tui")
}

/// The config file this run reads: `--config` first, then `AZ_TUI_CONFIG`,
/// then `config.toml` in the config directory.
#[must_use]
pub fn config_file(flag: Option<&std::path::Path>) -> PathBuf {
    config_file_with(flag, from_env)
}

pub fn config_file_with(
    flag: Option<&std::path::Path>,
    env: impl Fn(&str) -> Option<OsString> + Copy,
) -> PathBuf {
    if let Some(path) = flag {
        return path.to_path_buf();
    }
    if let Some(path) = env("AZ_TUI_CONFIG").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    config_dir_with(env).join("config.toml")
}

/// The cache file: `--cache` first, then `AZ_TUI_CACHE`, then the data
/// directory's `cache.json`.
#[must_use]
pub fn cache_file(flag: Option<&std::path::Path>) -> PathBuf {
    cache_file_with(flag, from_env)
}

pub fn cache_file_with(
    flag: Option<&std::path::Path>,
    env: impl Fn(&str) -> Option<OsString> + Copy,
) -> PathBuf {
    if let Some(path) = flag {
        return path.to_path_buf();
    }
    if let Some(path) = env("AZ_TUI_CACHE").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    data_dir_with(env).join("cache.json")
}

/// Where the session is kept. No flag names it; it is the tab, the sort and
/// the column widths, and nobody wants two of those.
#[must_use]
pub fn session_file() -> PathBuf {
    session_file_with(from_env)
}

pub fn session_file_with(env: impl Fn(&str) -> Option<OsString> + Copy) -> PathBuf {
    data_dir_with(env).join("session.json")
}

/// A variable only counts when it names an absolute path, which is what the
/// XDG spec says and what keeps a stray relative value from scattering files
/// wherever the shell happened to be.
fn absolute(value: Option<OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(value?);
    path.is_absolute().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(
        pairs: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Option<OsString> + Copy {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(*value))
        }
    }

    #[test]
    fn xdg_wins_over_home_and_a_relative_value_is_ignored() {
        let xdg = env_of(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/home/j")]);
        assert_eq!(config_dir_with(xdg), PathBuf::from("/xdg/az-tui"));

        let home_only = env_of(&[("HOME", "/home/j")]);
        assert_eq!(
            config_dir_with(home_only),
            PathBuf::from("/home/j/.config/az-tui")
        );

        let relative = env_of(&[("XDG_CONFIG_HOME", "xdg"), ("HOME", "/home/j")]);
        assert_eq!(
            config_dir_with(relative),
            PathBuf::from("/home/j/.config/az-tui"),
            "a relative XDG value is not a config home"
        );
    }

    #[test]
    fn the_data_directory_follows_the_platform() {
        let home = env_of(&[("HOME", "/home/j")]);
        let expected = if cfg!(target_os = "macos") {
            "/home/j/Library/Application Support/az-tui"
        } else {
            "/home/j/.local/share/az-tui"
        };
        assert_eq!(data_dir_with(home), PathBuf::from(expected));
        assert_eq!(
            data_dir_with(env_of(&[("XDG_DATA_HOME", "/data"), ("HOME", "/home/j")])),
            PathBuf::from("/data/az-tui")
        );
    }

    #[test]
    fn the_flag_beats_the_variable_which_beats_the_directory() {
        let env = env_of(&[
            ("HOME", "/home/j"),
            ("AZ_TUI_CONFIG", "/etc/az.toml"),
            ("AZ_TUI_CACHE", "/var/az.json"),
        ]);
        assert_eq!(
            config_file_with(Some(std::path::Path::new("/flag.toml")), env),
            PathBuf::from("/flag.toml")
        );
        assert_eq!(config_file_with(None, env), PathBuf::from("/etc/az.toml"));
        assert_eq!(cache_file_with(None, env), PathBuf::from("/var/az.json"));

        let bare = env_of(&[("HOME", "/home/j")]);
        assert_eq!(
            config_file_with(None, bare),
            PathBuf::from("/home/j/.config/az-tui/config.toml")
        );
        assert!(session_file_with(bare).ends_with("az-tui/session.json"));
    }
}
