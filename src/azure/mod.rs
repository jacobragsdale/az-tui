//! Azure, as this program reads it: the resources a login can reach, the
//! tokens each plane wants, and the one client that carries them.
//!
//! Everything here reads. [`transport::Method`] spells two verbs, `Get` and
//! `Post`, and neither writes; there is no way to name a write verb anywhere
//! in the crate, so the read-only promise is enforced by the type rather than
//! by discipline. A grep for the three write verbs over `src/` is the audit,
//! and it comes back empty.

pub mod auth;
pub mod graph;
pub mod transport;

use std::fmt;

use serde::{Deserialize, Serialize};

/// One key vault, as Resource Graph lists it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Vault {
    /// The full ARM resource id, which is also what the portal link is built
    /// from.
    pub id: String,
    pub name: String,
    pub subscription_id: String,
    pub resource_group: String,
    pub location: String,
    pub sku: String,
    /// The data-plane base, `https://kv-prod.vault.azure.net/`, trailing
    /// slash included.
    pub uri: String,
}

/// One container registry, as Resource Graph lists it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Registry {
    pub id: String,
    pub name: String,
    pub subscription_id: String,
    pub resource_group: String,
    pub location: String,
    pub sku: String,
    /// The data-plane host, `acrprod.azurecr.io`.
    pub login_server: String,
}

/// Everything the login can reach that this program knows about.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Inventory {
    pub vaults: Vec<Vault>,
    pub registries: Vec<Registry>,
}

/// A secret's value.
///
/// Neither `Debug` nor `Display` will print it, so it cannot reach a log
/// line, an error, a panic message or the cache by accident. [`Secret::expose`]
/// is the one way to read it and is meant to be conspicuous at the call site;
/// `grep -rn 'expose()' src/` is the audit.
///
/// It deliberately derives no `Serialize`: a value that cannot be serialised
/// cannot be written to the cache or the session however hard someone tries.
#[derive(Clone, Eq, PartialEq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a value read out of a vault. Called from exactly one place —
    /// [`vault::value`](crate::azure::vault::value) — so that every secret in
    /// the crate has one provenance.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value itself, for the one place that is about to show or copy it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// How many lines the value has, for the details pane's `(+14 lines)`
    /// without reading the value out.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.0.lines().count().max(1)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

/// Where the portal shows one resource, for the key that opens it in a
/// browser. The `@` is the tenant, which the portal fills in from the session.
#[must_use]
pub fn portal_url(id: &str) -> String {
    format!("https://portal.azure.com/#@/resource{id}")
}

/// The resources an allowlist names, in the order it names them, matched
/// without regard to case. An empty allowlist keeps everything; a name the
/// login cannot reach is simply absent, and `doctor` is what reports it.
pub fn allowed<T: Clone>(found: Vec<T>, names: &[String], name_of: impl Fn(&T) -> &str) -> Vec<T> {
    if names.is_empty() {
        return found;
    }
    names
        .iter()
        .filter_map(|wanted| {
            found
                .iter()
                .find(|held| name_of(held).eq_ignore_ascii_case(wanted.trim()))
                .cloned()
        })
        .collect()
}

/// The allowlist names nothing found, for `doctor` to say so.
pub fn missing<T>(found: &[T], names: &[String], name_of: impl Fn(&T) -> &str) -> Vec<String> {
    names
        .iter()
        .map(|name| name.trim())
        .filter(|wanted| {
            !wanted.is_empty()
                && !found
                    .iter()
                    .any(|held| name_of(held).eq_ignore_ascii_case(wanted))
        })
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_never_prints_itself() {
        let secret = Secret::new("hunter2");
        assert_eq!(format!("{secret:?}"), "[redacted]");
        assert_eq!(format!("{secret}"), "[redacted]");
        assert!(!format!("{secret:?} {secret}").contains("hunter2"));
        assert_eq!(secret.expose(), "hunter2");
    }

    #[test]
    fn a_secret_counts_its_lines_without_showing_them() {
        assert_eq!(Secret::new("").line_count(), 1);
        assert_eq!(Secret::new("one").line_count(), 1);
        assert_eq!(Secret::new("one\ntwo\nthree").line_count(), 3);
    }

    #[test]
    fn a_portal_url_carries_the_resource_id() {
        assert_eq!(
            portal_url("/subscriptions/s/resourceGroups/rg/providers/Microsoft.KeyVault/vaults/kv"),
            "https://portal.azure.com/#@/resource/subscriptions/s/resourceGroups/rg/providers/Microsoft.KeyVault/vaults/kv"
        );
    }

    #[test]
    fn an_allowlist_fixes_the_order_and_ignores_case() {
        let found = vec![
            "kv-dev".to_owned(),
            "kv-prod".to_owned(),
            "kv-qa".to_owned(),
        ];
        let names = vec![
            "KV-PROD".to_owned(),
            " kv-dev ".to_owned(),
            "kv-gone".to_owned(),
        ];
        assert_eq!(
            allowed(found.clone(), &names, String::as_str),
            ["kv-prod", "kv-dev"],
            "the allowlist's order wins, and a name that is not there is absent"
        );
        assert_eq!(missing(&found, &names, String::as_str), ["kv-gone"]);
        assert_eq!(
            allowed(found.clone(), &[], String::as_str),
            found,
            "an empty allowlist keeps everything"
        );
    }
}
