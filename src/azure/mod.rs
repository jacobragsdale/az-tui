//! Azure, as this program reads it: the resources a login can reach, the
//! tokens each plane wants, and the one client that carries them.
//!
//! Everything here reads. [`transport::Method`] spells two verbs, `Get` and
//! `Post`, and neither writes; there is no way to name a write verb anywhere
//! in the crate, so the read-only promise is enforced by the type rather than
//! by discipline. A grep for the three write verbs over `src/` is the audit,
//! and it comes back empty.

pub mod acr;
pub mod auth;
pub mod graph;
pub mod transport;
pub mod vault;

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::timestamp::Timestamp;

/// One key vault, as Resource Graph lists it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Vault {
    /// The full ARM resource id, which is also what the portal link is built
    /// from.
    pub id: String,
    pub name: String,
    pub resource_group: String,
    pub location: String,
    /// The data-plane base, `https://kv-prod.vault.azure.net/`, trailing
    /// slash included.
    pub uri: String,
}

/// One container registry, as Resource Graph lists it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Registry {
    pub id: String,
    pub name: String,
    pub resource_group: String,
    pub location: String,
    /// The data-plane host, `acrprod.azurecr.io`.
    pub login_server: String,
}

/// One secret in one vault, as a listing describes it. A listing never
/// carries the value, which is the point of listing one.
///
// ponytail: secrets only. Keys and certificates list the same way (`GET keys`,
// `GET certificates`, 25 to a page) and would come in as a `kind` column here
// and a third schema in `filter`; nobody asked for them in v1.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SecretRow {
    /// The vault's name, which is the row's first column.
    pub vault: String,
    /// The last path segment of the item's `id`.
    pub name: String,
    pub enabled: bool,
    pub created: Option<Timestamp>,
    pub updated: Option<Timestamp>,
    /// `attributes.exp`.
    pub expires: Option<Timestamp>,
    /// `attributes.nbf`.
    pub not_before: Option<Timestamp>,
    pub content_type: Option<String>,
    /// Sorted by key, so a row's tags read the same on every refresh.
    pub tags: Vec<(String, String)>,
    /// A certificate's backing secret, which the vault manages itself.
    pub managed: bool,
}

/// One version of one secret. Read on demand, never cached to disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretVersion {
    pub version: String,
    pub enabled: bool,
    pub created: Option<Timestamp>,
    pub updated: Option<Timestamp>,
    pub expires: Option<Timestamp>,
}

/// One repository in a registry. The counts and the stamps are `None` until
/// the attributes call fills them in: a catalog listing is names and nothing
/// else.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Repository {
    pub registry: String,
    pub name: String,
    pub tag_count: Option<u64>,
    pub manifest_count: Option<u64>,
    pub created: Option<Timestamp>,
    pub updated: Option<Timestamp>,
}

impl Repository {
    /// A row as the catalog lists it: a name, and everything else still to
    /// come.
    #[must_use]
    pub fn unfilled(registry: &str, name: &str) -> Self {
        Self {
            registry: registry.to_owned(),
            name: name.to_owned(),
            tag_count: None,
            manifest_count: None,
            created: None,
            updated: None,
        }
    }
}

/// One tag, and the manifest it points at.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tag {
    pub name: String,
    pub digest: String,
    pub created: Option<Timestamp>,
    pub updated: Option<Timestamp>,
}

/// One manifest, by what it weighs and what it runs on. A multi-arch index
/// names no architecture, and the UI prints `index` for it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Manifest {
    pub digest: String,
    /// Bytes, as the registry counts them.
    pub size: Option<u64>,
    pub architecture: Option<String>,
    pub os: Option<String>,
    pub created: Option<Timestamp>,
    /// Every tag pointing at this manifest.
    pub tags: Vec<String>,
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
/// a grep for that method name over `src/` is the audit.
///
/// It deliberately derives no `Serialize`: a value that cannot be serialised
/// cannot be written to the cache or the session however hard someone tries.
#[derive(Clone, Eq, PartialEq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a value read out of a vault or a cluster. Called from exactly
    /// two places — [`vault::value`](crate::azure::vault::value) and the
    /// `kubectl` secret read in [`crate::kube`] — so that every secret in the
    /// crate has one of two provenances.
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
