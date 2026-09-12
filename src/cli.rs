//! The command line: flags that override `config.toml`, and the subcommands
//! that read the same things from a shell instead of a screen.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::ui::theme::ThemeChoice;

#[derive(Debug, Parser)]
#[command(
    name = "az-tui",
    version,
    about = "A read-only terminal browser for Azure Key Vault secrets and Container Registry images"
)]
pub struct Cli {
    /// Only read this subscription. Repeat for more; left out, every
    /// subscription the login can see.
    #[arg(long = "subscription", value_name = "GUID")]
    pub subscriptions: Vec<String>,

    /// Only read this key vault, in the order given. Repeat for more.
    #[arg(long = "vault", value_name = "NAME")]
    pub vaults: Vec<String>,

    /// Only read this container registry, in the order given. Repeat for more.
    #[arg(long = "registry", value_name = "NAME")]
    pub registries: Vec<String>,

    /// Seconds between background refreshes; 0 leaves `r` as the only refresh.
    #[arg(long, value_name = "SECS")]
    pub refresh: Option<u64>,

    /// terminal · terminal-light · mono · custom
    #[arg(long, value_name = "NAME")]
    pub theme: Option<String>,

    /// Read this file instead of ~/.config/az-tui/config.toml.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Keep the cache here instead of in the data directory.
    #[arg(long, value_name = "PATH")]
    pub cache: Option<PathBuf>,

    /// Neither read nor write the cache.
    #[arg(long)]
    pub no_cache: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Check the login, the tokens and what the subscriptions hold.
    Doctor,
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
            config.azure.refresh = Some(refresh);
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
        assert_eq!(merged.azure.refresh, Some(0));
    }

    #[test]
    fn doctor_is_the_one_subcommand_so_far() {
        let cli = Cli::parse_from(["az-tui", "doctor"]);
        assert!(matches!(cli.command, Some(Command::Doctor)));
    }
}
