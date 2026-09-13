//! The Key Vault data plane: list a vault's secrets, list one secret's
//! versions, and read one value when — and only when — someone asks for it in
//! as many words.
//!
//! Every call here carries the Vault audience's token and `api-version=7.4`,
//! and the base is the vault's `uri` from the inventory, which already ends
//! in a slash.

use anyhow::{Context, Result};
use serde_json::Value;

use super::auth::Audience;
use super::graph::text;
use super::transport::{Client, Request, api_error};
use super::{Secret, SecretRow, SecretVersion, Vault};
use crate::timestamp::Timestamp;

/// The data-plane version every call here is asked at.
///
// ponytail: 7.4. Key Vault has since moved to date-based versioning and
// `2025-07-01` is what the REST reference documents; 7.4 is no longer in the
// spec repository but no retirement has been announced, it is the version
// deployed everywhere including the sovereign clouds, and none of the four
// shapes read below changed between them. Bump this one constant if a vault
// ever refuses it — there is nothing else to change.
const API_VERSION: &str = "7.4";
/// The most items a listing may ask for. The service refuses more: this is
/// its cap, not ours, and it is why a vault with 500 secrets is 20 round
/// trips and why the cache earns its keep.
const PAGE: usize = 25;

/// Every secret in one vault, with the attributes the table shows. The value
/// is never among them.
pub fn secrets(client: &Client, vault: &Vault) -> Result<Vec<SecretRow>> {
    let mut rows = Vec::new();
    let mut url = Some(format!(
        "{}secrets?api-version={API_VERSION}&maxresults={PAGE}",
        base(vault)
    ));
    // The listing hands back the next page's whole address — api-version and
    // skip token included — so it is followed rather than rebuilt.
    while let Some(next) = url {
        let page = client
            .call(&Audience::Vault, Request::get(&next))
            .map_err(|error| explain(vault, error))?;
        rows.extend(
            page["value"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|entry| row(vault, entry)),
        );
        url = text(&page["nextLink"]);
    }
    Ok(rows)
}

/// One secret's versions, newest first.
pub fn versions(client: &Client, vault: &Vault, name: &str) -> Result<Vec<SecretVersion>> {
    let mut versions: Vec<SecretVersion> = Vec::new();
    let mut url = Some(format!(
        "{}secrets/{}/versions?api-version={API_VERSION}&maxresults={PAGE}",
        base(vault),
        segment(name)
    ));
    while let Some(next) = url {
        let page = client
            .call(&Audience::Vault, Request::get(&next))
            .map_err(|error| explain(vault, error))?;
        versions.extend(
            page["value"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(version),
        );
        url = text(&page["nextLink"]);
    }
    // The service does not promise an order, and the newest is the one
    // anybody is looking for.
    versions.sort_by_key(|version| std::cmp::Reverse(version.created));
    Ok(versions)
}

/// One secret's value, and the version it came from.
///
/// **This is the only function in the crate that produces a [`Secret`].** It
/// is reached from exactly two keys — `v` and `y` — and from `secret get`; a
/// listing never calls it and neither does a refresh.
pub fn value(
    client: &Client,
    vault: &Vault,
    name: &str,
    version: Option<&str>,
) -> Result<(Secret, String)> {
    let url = match version {
        Some(version) => format!(
            "{}secrets/{}/{}?api-version={API_VERSION}",
            base(vault),
            segment(name),
            segment(version)
        ),
        None => format!(
            "{}secrets/{}?api-version={API_VERSION}",
            base(vault),
            segment(name)
        ),
    };
    let answer = client
        .call(&Audience::Vault, Request::get(&url))
        .map_err(|error| explain(vault, error))?;
    let held = answer["value"]
        .as_str()
        .with_context(|| format!("{} did not answer with a value for {name}", vault.name))?;
    // The id's last segment is the version the vault actually handed over,
    // which is what "current" means when none was asked for.
    let version = text(&answer["id"])
        .and_then(|id| id.rsplit('/').next().map(str::to_owned))
        .unwrap_or_default();
    Ok((Secret::new(held), version))
}

/// The two refusals common enough to be worth saying in fewer words than the
/// service does. Everything else keeps the message Key Vault wrote, which is
/// usually a good one.
fn explain(vault: &Vault, error: anyhow::Error) -> anyhow::Error {
    let Some(refusal) = api_error(&error).filter(|refusal| refusal.status == 403) else {
        return error;
    };
    if refusal.has_code("ForbiddenByFirewall") {
        return anyhow::anyhow!(
            "{}: blocked by the vault firewall (your IP is not allowed)",
            vault.name
        );
    }
    anyhow::anyhow!(
        "{}: no permission to read secrets (needs the Key Vault Secrets User role or a `list` access policy)",
        vault.name
    )
}

/// The vault's data-plane base, with exactly one trailing slash however the
/// inventory wrote it.
fn base(vault: &Vault) -> String {
    format!("{}/", vault.uri.trim_end_matches('/'))
}

/// A secret name in a path. Names are limited to `[0-9a-zA-Z-]`, so this
/// escapes nothing in practice — it is here so a name the service one day
/// allows cannot build a URL that means something else.
fn segment(raw: &str) -> String {
    let mut encoded = String::new();
    super::transport::percent_encode(raw, &mut encoded);
    encoded
}

fn row(vault: &Vault, entry: &Value) -> Option<SecretRow> {
    let id = text(&entry["id"])?;
    let name = id.rsplit('/').next().filter(|name| !name.is_empty())?;
    let attributes = &entry["attributes"];
    let mut tags: Vec<(String, String)> = entry["tags"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, value)| (key.clone(), value.as_str().unwrap_or_default().to_owned()))
        .collect();
    tags.sort();
    Some(SecretRow {
        vault: vault.name.clone(),
        name: name.to_owned(),
        // A vault says so only when a secret is disabled, and a secret with
        // no attributes at all is a usable one.
        enabled: attributes["enabled"].as_bool().unwrap_or(true),
        created: unix(&attributes["created"]),
        updated: unix(&attributes["updated"]),
        expires: unix(&attributes["exp"]),
        not_before: unix(&attributes["nbf"]),
        content_type: text(&entry["contentType"]),
        tags,
        managed: entry["managed"].as_bool().unwrap_or(false),
    })
}

fn version(entry: &Value) -> Option<SecretVersion> {
    let id = text(&entry["id"])?;
    let version = id.rsplit('/').next().filter(|held| !held.is_empty())?;
    let attributes = &entry["attributes"];
    Some(SecretVersion {
        version: version.to_owned(),
        enabled: attributes["enabled"].as_bool().unwrap_or(true),
        created: unix(&attributes["created"]),
        updated: unix(&attributes["updated"]),
        expires: unix(&attributes["exp"]),
    })
}

/// A unix second, as a vault writes its attributes. `null` is not 1970.
fn unix(value: &Value) -> Option<Timestamp> {
    Timestamp::from_unix(value.as_i64()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::transport::fake::{Answer, client as fake_client};
    use crate::timestamp::ts;
    use serde_json::json;

    fn vault() -> Vault {
        Vault {
            id: "/subscriptions/s/resourceGroups/rg/providers/Microsoft.KeyVault/vaults/kv-prod"
                .into(),
            name: "kv-prod".into(),
            resource_group: "rg".into(),
            location: "eastus".into(),
            uri: "https://kv-prod.vault.azure.net/".into(),
        }
    }

    #[test]
    fn a_listing_follows_next_link_verbatim() {
        let (client, transport, _) = fake_client([
            Answer::json(json!({
                "value": [{ "id": "https://kv-prod.vault.azure.net/secrets/a", "attributes": { "enabled": true } }],
                "nextLink": "https://kv-prod.vault.azure.net/secrets?api-version=7.4&$skiptoken=XYZ",
            })),
            Answer::json(json!({
                "value": [{ "id": "https://kv-prod.vault.azure.net/secrets/b", "attributes": { "enabled": true } }],
                "nextLink": Value::Null,
            })),
        ]);
        let rows = secrets(&client, &vault()).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        let urls = transport.urls();
        assert_eq!(
            urls[0], "https://kv-prod.vault.azure.net/secrets?api-version=7.4&maxresults=25",
            "the cap is the service's 25, not ours"
        );
        assert_eq!(
            urls[1], "https://kv-prod.vault.azure.net/secrets?api-version=7.4&$skiptoken=XYZ",
            "the next page's address is followed as it was written"
        );
        assert!(
            transport.bearers()[0]
                .as_deref()
                .unwrap()
                .starts_with("vault-"),
            "signed with the vault audience, not ARM's"
        );
    }

    #[test]
    fn every_attribute_is_read_and_a_bare_entry_still_makes_a_row() {
        let (client, _, _) = fake_client([Answer::json(json!({
            "value": [
                {
                    "id": "https://kv-prod.vault.azure.net/secrets/db-password",
                    "contentType": "text/plain",
                    "tags": { "owner": "platform", "env": "prod" },
                    "managed": Value::Null,
                    "attributes": {
                        "enabled": true,
                        "created": 1_709_251_200,
                        "updated": 1_725_753_600,
                        "exp": 1_767_225_600,
                        "nbf": Value::Null,
                        "recoveryLevel": "Recoverable+Purgeable",
                    },
                },
                { "id": "https://kv-prod.vault.azure.net/secrets/bare", "attributes": { "enabled": false } },
            ],
        }))]);
        let rows = secrets(&client, &vault()).unwrap();

        let full = &rows[0];
        assert_eq!(full.vault, "kv-prod");
        assert_eq!(full.name, "db-password");
        assert_eq!(full.content_type.as_deref(), Some("text/plain"));
        assert_eq!(
            full.tags,
            [
                ("env".to_owned(), "prod".to_owned()),
                ("owner".to_owned(), "platform".to_owned())
            ],
            "tags read in key order however the vault wrote them"
        );
        assert!(!full.managed, "a null `managed` is not a managed secret");
        assert_eq!(full.created, Some(ts("2024-03-01T00:00:00Z")));
        assert_eq!(full.expires, Some(ts("2026-01-01T00:00:00Z")));
        assert_eq!(full.not_before, None);

        let bare = &rows[1];
        assert!(!bare.enabled);
        assert_eq!(bare.created, None);
        assert!(bare.tags.is_empty());
        assert_eq!(bare.content_type, None);
    }

    #[test]
    fn a_missing_enabled_attribute_reads_as_usable() {
        let (client, _, _) = fake_client([Answer::json(json!({
            "value": [{ "id": "https://kv-prod.vault.azure.net/secrets/x", "attributes": {} }],
        }))]);
        assert!(secrets(&client, &vault()).unwrap()[0].enabled);
    }

    #[test]
    fn versions_come_back_newest_first() {
        let (client, transport, _) = fake_client([Answer::json(json!({
            "value": [
                { "id": "https://kv-prod.vault.azure.net/secrets/db/1c2b90aa", "attributes": { "enabled": true, "created": 1_709_251_200 } },
                { "id": "https://kv-prod.vault.azure.net/secrets/db/8f3a2c1d", "attributes": { "enabled": true, "created": 1_725_753_600 } },
            ],
        }))]);
        let versions = versions(&client, &vault(), "db").unwrap();
        assert_eq!(
            versions
                .iter()
                .map(|v| v.version.as_str())
                .collect::<Vec<_>>(),
            ["8f3a2c1d", "1c2b90aa"]
        );
        assert!(transport.urls()[0].contains("/secrets/db/versions?api-version=7.4"));
    }

    #[test]
    fn a_value_comes_back_with_the_version_it_came_from_and_never_prints_itself() {
        let (client, transport, _) = fake_client([Answer::json(json!({
            "value": "s3cr3t",
            "id": "https://kv-prod.vault.azure.net/secrets/db/8f3a2c1d",
            "contentType": "text/plain",
            "attributes": { "enabled": true },
        }))]);
        let (secret, version) = value(&client, &vault(), "db", None).unwrap();
        assert_eq!(secret.expose(), "s3cr3t");
        assert_eq!(version, "8f3a2c1d");
        assert!(!format!("{secret:?}").contains("s3cr3t"));
        assert_eq!(
            transport.urls()[0],
            "https://kv-prod.vault.azure.net/secrets/db?api-version=7.4"
        );
        assert!(
            transport.bearers()[0]
                .as_deref()
                .unwrap()
                .starts_with("vault-")
        );
    }

    #[test]
    fn one_version_is_asked_for_by_its_own_path() {
        let (client, transport, _) = fake_client([Answer::json(json!({
            "value": "old",
            "id": "https://kv-prod.vault.azure.net/secrets/db/1c2b90aa",
        }))]);
        let (_, version) = value(&client, &vault(), "db", Some("1c2b90aa")).unwrap();
        assert_eq!(version, "1c2b90aa");
        assert_eq!(
            transport.urls()[0],
            "https://kv-prod.vault.azure.net/secrets/db/1c2b90aa?api-version=7.4"
        );
    }

    #[test]
    fn a_firewall_and_a_permission_are_told_apart() {
        let (client, _, _) = fake_client([Answer::status(
            403,
            r#"{"error":{"code":"Forbidden","message":"Client address is not authorized","innererror":{"code":"ForbiddenByFirewall"}}}"#,
        )]);
        let error = format!("{:#}", secrets(&client, &vault()).unwrap_err());
        assert!(
            error.contains("kv-prod: blocked by the vault firewall"),
            "{error}"
        );

        let (client, _, _) = fake_client([Answer::status(
            403,
            r#"{"error":{"code":"Forbidden","message":"Caller is not authorized to perform action on resource"}}"#,
        )]);
        let error = format!("{:#}", secrets(&client, &vault()).unwrap_err());
        assert!(
            error.contains("kv-prod: no permission to read secrets"),
            "{error}"
        );
        assert!(error.contains("Key Vault Secrets User"), "{error}");
    }

    #[test]
    fn a_vault_uri_written_without_a_slash_still_builds_one_url() {
        let mut vault = vault();
        vault.uri = "https://kv-prod.vault.azure.net".into();
        let (client, transport, _) = fake_client([Answer::json(json!({ "value": [] }))]);
        secrets(&client, &vault).unwrap();
        assert!(
            transport.urls()[0].starts_with("https://kv-prod.vault.azure.net/secrets?"),
            "{:?}",
            transport.urls()
        );
    }
}
