//! Tokens, borrowed from the Azure CLI's login.
//!
//! There is no credential of our own: `az login` has already done the hard
//! part, and `az account get-access-token` hands out a token for whichever
//! audience is asked for. A registry is the exception — it mints its own, from
//! an ARM token — and that trade lives in [`super::acr`], behind the same
//! [`Audience`] so the client keeps owning the retry policy.

use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// The audience a token is minted for. The trailing slash on ARM's is part of
/// it: a token minted for `https://management.azure.com` without it is
/// rejected by ARM.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Audience {
    Arm,
    Vault,
    /// The audience a registry's *own* token is minted for by the CLI, which
    /// is then traded for the registry's refresh and access tokens. Not the
    /// ARM audience: a registry accepts an ARM-scoped token only while its
    /// `azureADAuthenticationAsArmPolicy` is enabled, which the documentation
    /// recommends turning off, so a hardened registry would answer 401. This
    /// is what `az acr` itself asks for.
    ContainerRegistry,
    /// One registry's data plane, signed for the one thing about to be read.
    /// The token is not minted by the CLI at all — see [`super::acr`].
    Acr {
        login_server: String,
        scope: String,
    },
}

impl Audience {
    /// What `--resource` is given for this audience.
    #[must_use]
    pub const fn resource(&self) -> &'static str {
        match self {
            Self::Arm => "https://management.azure.com/",
            Self::Vault => "https://vault.azure.net",
            Self::ContainerRegistry | Self::Acr { .. } => "https://containerregistry.azure.net",
        }
    }

    /// What the client caches this audience's token under.
    #[must_use]
    pub fn cache_key(&self) -> String {
        match self {
            // One access token per registry per scope: a catalog listing and
            // a repository's tags are signed for different things.
            Self::Acr {
                login_server,
                scope,
            } => format!("acr:{login_server}:{scope}"),
            other => other.resource().to_owned(),
        }
    }

    /// What an error calls it.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Arm => "arm",
            Self::Vault => "vault",
            Self::ContainerRegistry => "registry",
            Self::Acr { .. } => "acr",
        }
    }
}

/// Where a token comes from. One seam, so every test hands the client fixed
/// strings instead of shelling out to `az`.
pub trait TokenSource: Send {
    fn token(&self, audience: &Audience) -> Result<String>;

    /// The tenant the login is in, for the one form field that wants it.
    /// `None` is legal — the registry exchange declares `tenant` optional —
    /// and is what a test hands back.
    fn tenant(&self) -> Option<String> {
        None
    }
}

/// The real thing: the Azure CLI's login.
pub struct AzCli;

impl TokenSource for AzCli {
    fn tenant(&self) -> Option<String> {
        az(&["account", "show", "--query", "tenantId", "-o", "tsv"]).ok()
    }

    fn token(&self, audience: &Audience) -> Result<String> {
        az(&[
            "account",
            "get-access-token",
            "--resource",
            audience.resource(),
            "--query",
            "accessToken",
            "-o",
            "tsv",
        ])
    }
}

/// One `az` call, run without a terminal to talk to, and whatever single
/// value it printed. A failure ends in "run `az login`", because that is what
/// it nearly always is.
pub fn az(arguments: &[&str]) -> Result<String> {
    let output = Command::new("az")
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .context("could not run `az`; install the Azure CLI and run `az login`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`az {}` failed: {} — run `az login`",
            arguments.join(" "),
            first_line(&stderr)
        );
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if value.is_empty() {
        bail!(
            "`az {}` answered with nothing — run `az login`",
            arguments.join(" ")
        );
    }
    Ok(value)
}

/// Every enabled subscription the login can see, as GUIDs.
pub fn subscriptions() -> Result<Vec<String>> {
    let listed = az(&[
        "account",
        "list",
        "--query",
        "[?state=='Enabled'].id",
        "-o",
        "tsv",
    ])?;
    Ok(lines(&listed))
}

/// `az --version`'s first line, which is the version and nothing else.
pub fn az_version() -> Result<String> {
    Ok(first_line(&az(&["version", "--query", "\"azure-cli\"", "-o", "tsv"])?).to_owned())
}

/// Who is signed in, and to which tenant.
pub fn account() -> Result<(String, String)> {
    let shown = az(&[
        "account",
        "show",
        "--query",
        "[user.name, tenantId]",
        "-o",
        "tsv",
    ])?;
    // `-o tsv` on a two-element array is one tab-separated line.
    let mut fields = shown.split(['\t', '\n']).map(str::trim);
    let user = fields.next().unwrap_or_default().to_owned();
    let tenant = fields.next().unwrap_or_default().to_owned();
    if user.is_empty() && tenant.is_empty() {
        bail!("`az account show` answered with nothing — run `az login`");
    }
    Ok((user, tenant))
}

/// The non-blank lines of some `-o tsv` output.
fn lines(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `az` writes several lines of complaint; the first one is the reason.
fn first_line(raw: &str) -> &str {
    raw.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

/// A token source over fixed strings, counting what it minted. Every test in
/// this crate signs with one.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct FixedTokens {
    pub mints: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// What the next mint answers with; the default is the audience's label
    /// with a counter, so a re-mint is visibly a different token.
    pub answers: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<Result<String>>>>,
}

#[cfg(test)]
impl FixedTokens {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many tokens were minted, whatever the audience.
    pub fn count(&self) -> usize {
        self.mints.lock().unwrap().len()
    }
}

#[cfg(test)]
impl TokenSource for FixedTokens {
    fn token(&self, audience: &Audience) -> Result<String> {
        let mut minted = self.mints.lock().unwrap();
        minted.push(audience.label().to_owned());
        if let Some(answer) = self.answers.lock().unwrap().pop_front() {
            return answer;
        }
        Ok(format!("{}-token-{}", audience.label(), minted.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arm_audience_keeps_its_trailing_slash_and_the_vault_one_has_none() {
        assert_eq!(Audience::Arm.resource(), "https://management.azure.com/");
        assert_eq!(Audience::Vault.resource(), "https://vault.azure.net");
        assert_eq!(
            Audience::ContainerRegistry.resource(),
            "https://containerregistry.azure.net",
            "not the ARM audience: a hardened registry refuses that one"
        );
        assert_ne!(Audience::Arm.cache_key(), Audience::Vault.cache_key());
    }

    #[test]
    fn a_registry_token_is_cached_per_registry_and_per_scope() {
        let catalog = Audience::Acr {
            login_server: "acr.azurecr.io".into(),
            scope: "registry:catalog:*".into(),
        };
        let repository = Audience::Acr {
            login_server: "acr.azurecr.io".into(),
            scope: "repository:api:metadata_read".into(),
        };
        let elsewhere = Audience::Acr {
            login_server: "other.azurecr.io".into(),
            scope: "registry:catalog:*".into(),
        };
        assert_ne!(catalog.cache_key(), repository.cache_key());
        assert_ne!(catalog.cache_key(), elsewhere.cache_key());
        assert_ne!(catalog.cache_key(), Audience::Arm.cache_key());
    }

    #[test]
    fn tsv_output_is_read_a_line_at_a_time_with_blanks_dropped() {
        assert_eq!(lines("a\n\n b \n"), ["a", "b"]);
        assert_eq!(lines(""), Vec::<String>::new());
        assert_eq!(first_line("\nERROR: nope\nmore\n"), "ERROR: nope");
    }

    #[test]
    fn a_missing_az_says_to_install_it_rather_than_panicking() {
        // The command name cannot exist, so this exercises the spawn failure
        // without depending on whether `az` is installed here.
        let error = Command::new("az-tui-no-such-binary")
            .output()
            .err()
            .map(|error| error.to_string());
        assert!(error.is_some(), "a missing binary is an Err, not a panic");
    }

    #[test]
    fn a_fixed_token_source_counts_and_re_mints() {
        let tokens = FixedTokens::new();
        assert_eq!(tokens.token(&Audience::Arm).unwrap(), "arm-token-1");
        assert_eq!(tokens.token(&Audience::Arm).unwrap(), "arm-token-2");
        assert_eq!(tokens.count(), 2);
    }
}
