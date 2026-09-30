//! The command line: flags that override `config.toml`, and the two
//! subcommands that set up and check what the screen reads. The reads
//! themselves, from a shell, are agent-cli's `kv` and `acr`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::ui::theme::ThemeChoice;

#[derive(Debug, Parser)]
#[command(
    name = "az-tui",
    version,
    about = "A fast terminal browser for AKS namespaces, Azure Key Vault secrets and Container Registry images"
)]
pub struct Cli {
    /// Only read this subscription. Repeat for more; left out, every
    /// subscription the login can see.
    #[arg(long = "subscription", value_name = "GUID", global = true)]
    pub subscriptions: Vec<String>,

    /// Only read this key vault, in the order given. Repeat for more.
    #[arg(long = "vault", value_name = "NAME")]
    pub vaults: Vec<String>,

    /// Only read this container registry, in the order given. Repeat for more.
    #[arg(long = "registry", value_name = "NAME")]
    pub registries: Vec<String>,

    /// Seconds between reads of the open AKS tab; 0 leaves `r` as the only
    /// read. The vault and registry cadence is `[azure].refresh` in the file.
    #[arg(long, value_name = "SECS")]
    pub refresh: Option<u64>,

    /// terminal · terminal-light · mono · custom [env: AZ_TUI_THEME]
    #[arg(long, value_name = "NAME", global = true, value_parser = theme_name)]
    pub theme: Option<String>,

    /// Read this file instead of $XDG_CONFIG_HOME/az-tui/config.toml
    /// (~/.config/az-tui/config.toml when that is unset) [env: AZ_TUI_CONFIG]
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    /// Keep the cache here instead of in the data directory [env: AZ_TUI_CACHE]
    #[arg(long, value_name = "PATH", global = true)]
    pub cache: Option<PathBuf>,

    /// Neither read nor write the cache.
    #[arg(long, global = true)]
    pub no_cache: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Check the az login and tokens, every vault and registry, then
    /// kubectl, kubelogin and every AKS scope in config.toml.
    Doctor,

    /// Fetch credentials for every AKS cluster in the current az
    /// subscription, or in each --subscription / [azure].subscriptions when
    /// given, and print a [[clusters]] block for each.
    Setup {
        /// Write the blocks to config.toml when there is no file yet.
        #[arg(long)]
        write: bool,
    },
}

impl Cli {
    /// The configuration this run works from: the file, then every flag that
    /// was given on top of it. A flag wins over the file; an empty list of
    /// flags leaves the file's list alone.
    pub fn merge(&self, mut config: Config) -> Config {
        if !self.subscriptions.is_empty() {
            config.azure.subscriptions = self.subscriptions.clone();
        }
        if !self.vaults.is_empty() {
            config.azure.vaults = self.vaults.clone();
        }
        if !self.registries.is_empty() {
            config.azure.registries = self.registries.clone();
        }
        if let Some(refresh) = self.refresh {
            config.refresh = Some(refresh);
        }
        config
    }

    /// Which theme this run paints with, once the file has been read.
    pub fn resolve_theme(&self, config: &Config) -> anyhow::Result<ThemeChoice> {
        let env = std::env::var("AZ_TUI_THEME").ok();
        let chosen = crate::ui::theme::chosen_theme(self.theme.as_deref(), env.as_deref())?;
        ThemeChoice::resolve(std::env::var_os("NO_COLOR").is_some(), chosen, config)
    }
}

/// A `--theme` name, checked while the arguments are read so a typo exits 2
/// and names the themes there are. `AZ_TUI_THEME` is checked later, by
/// `chosen_theme`.
fn theme_name(raw: &str) -> anyhow::Result<String> {
    if !raw.trim().is_empty() {
        ThemeChoice::parse(raw)?;
    }
    Ok(raw.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_are_all_optional_and_repeatable() {
        let cli = Cli::parse_from(["az-tui"]);
        assert!(cli.vaults.is_empty());
        assert!(cli.command.is_none());
        assert!(!cli.no_cache);

        let cli = Cli::parse_from(["az-tui", "--vault", "kv-a", "--vault", "kv-b", "--no-cache"]);
        assert_eq!(cli.vaults, ["kv-a", "kv-b"]);
        assert!(cli.no_cache);
    }

    #[test]
    fn a_flag_replaces_the_files_list_and_silence_leaves_it_alone() {
        let file = crate::config::parse("[azure]\nvaults = [\"kv-file\"]\nrefresh = 30\n").unwrap();

        let merged = Cli::parse_from(["az-tui"]).merge(file.clone());
        assert_eq!(merged.azure.vaults, ["kv-file"]);
        assert_eq!(merged.azure.refresh, Some(30));

        let merged =
            Cli::parse_from(["az-tui", "--vault", "kv-flag", "--refresh", "0"]).merge(file);
        assert_eq!(merged.azure.vaults, ["kv-flag"]);
        assert_eq!(merged.refresh, Some(0), "the flag is the AKS cadence");
        assert_eq!(
            merged.azure.refresh,
            Some(30),
            "the vaults' is the file's alone"
        );
    }

    #[test]
    fn the_subcommands_parse_and_the_global_flags_may_follow_them() {
        let cli = Cli::parse_from(["az-tui", "doctor"]);
        assert!(matches!(cli.command, Some(Command::Doctor)));
        let cli = Cli::parse_from(["az-tui", "doctor", "--config", "x.toml"]);
        assert!(matches!(cli.command, Some(Command::Doctor)));
        assert_eq!(cli.config.as_deref(), Some(std::path::Path::new("x.toml")));
        let cli = Cli::parse_from(["az-tui", "setup", "--write"]);
        assert!(matches!(cli.command, Some(Command::Setup { write: true })));

        // The natural order in a script: the command first, then how to run
        // it. Only the flags no subcommand names for itself are global.
        let cli = Cli::parse_from(["az-tui", "doctor", "--no-cache", "--cache", "c.json"]);
        assert!(cli.no_cache);
        assert_eq!(cli.cache.as_deref(), Some(std::path::Path::new("c.json")));
        assert!(matches!(cli.command, Some(Command::Doctor)));
    }

    #[test]
    fn a_bad_theme_is_an_argument_error_that_names_the_themes() {
        let error = Cli::try_parse_from(["az-tui", "--theme", "dracula"]).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        let said = error.to_string();
        assert!(
            said.contains("terminal, terminal-light, mono, custom"),
            "{said}"
        );

        let cli = Cli::parse_from(["az-tui", "doctor", "--theme", "light"]);
        assert_eq!(cli.theme.as_deref(), Some("light"), "an alias still reads");
    }
}
