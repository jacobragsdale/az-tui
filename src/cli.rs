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

    /// terminal · terminal-light · mono · custom
    #[arg(long, value_name = "NAME", global = true)]
    pub theme: Option<String>,

    /// Read this file instead of ~/.config/az-tui/config.toml.
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    /// Keep the cache here instead of in the data directory.
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

    /// Fetch credentials for every AKS cluster the login can see and print a
    /// [[clusters]] block for each.
    Setup {
        /// Write the blocks to config.toml when there is no file yet.
        #[arg(long)]
        write: bool,
    },

    /// One line per secret: vault, name, enabled, expires, updated.
    ///
    /// Never prints a value; `secret get` is the one command that does.
    Secrets {
        /// The same grammar as `/` in the TUI.
        query: Option<String>,
        /// Only this vault. Repeat for more.
        #[arg(long = "vault", value_name = "NAME")]
        vaults: Vec<String>,
        /// One JSON array: vault, name, enabled, content_type, expires,
        /// created, updated, managed, tags. Never a value.
        #[arg(long)]
        json: bool,
        /// Read Azure rather than the cache.
        #[arg(long)]
        refresh: bool,
    },

    /// One secret's value. The only command that prints one.
    Secret {
        #[command(subcommand)]
        command: SecretCommand,
    },

    /// One line per repository: registry, repository, tags, updated.
    Repos {
        /// The same grammar as `/` in the TUI.
        query: Option<String>,
        /// Only this registry. Repeat for more.
        #[arg(long = "registry", value_name = "NAME")]
        registries: Vec<String>,
        /// One JSON array: registry, repository, tag_count, manifest_count,
        /// created, updated.
        #[arg(long)]
        json: bool,
        /// Read Azure rather than the cache.
        #[arg(long)]
        refresh: bool,
    },

    /// One line per tag of one repository, newest first.
    Tags {
        /// The repository, as the catalog names it.
        repo: String,
        /// Which registry, when more than one holds a repository by this
        /// name.
        #[arg(long, value_name = "NAME")]
        registry: Option<String>,
        /// One JSON array: registry, repository, tag, digest, pull, created,
        /// updated.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum SecretCommand {
    /// Print one secret's value.
    ///
    /// This is the one command in az-tui that prints a value. It goes to
    /// stdout with no trailing newline of its own, so `$(az-tui secret get
    /// NAME)` is the value byte for byte.
    Get {
        /// The secret's name, as the vault lists it.
        name: String,
        /// Which vault, when more than one holds a secret by this name.
        #[arg(long, value_name = "NAME")]
        vault: Option<String>,
        /// A version other than the current one.
        #[arg(long, value_name = "ID")]
        version: Option<String>,
        /// `{"vault","name","version","content_type","value"}`.
        #[arg(long)]
        json: bool,
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
        let cli = Cli::parse_from(["az-tui", "secrets", "--no-cache", "--config", "x.toml"]);
        assert!(cli.no_cache);
        assert_eq!(cli.config.as_deref(), Some(std::path::Path::new("x.toml")));
        assert!(matches!(cli.command, Some(Command::Secrets { .. })));
    }
}
