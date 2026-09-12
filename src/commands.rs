//! The subcommands: the same reads from a shell, for scripts and agents.
//!
//! Each runs on the calling thread with the same client the TUI uses, reads
//! the cache when it is fresh enough, and prints a table or `--json`.
//!
//! `secret get` is the one command that prints a value. Nothing else does —
//! not `secrets`, not its `--json`, not an error message — and a test greps
//! the JSON for a `value` key to keep it that way.

use std::io::Write;

use anyhow::Result;
use serde_json::json;

use crate::azure::transport::Client;
use crate::azure::{Registry, Repository, SecretRow, Vault, acr, graph, vault};
use crate::cache;
use crate::config::Azure;
use crate::filter::Query;
use crate::timestamp::Timestamp;

/// What a command exits with. 0 ok, 1 a read failed, 2 the arguments were
/// wrong — the shape `grep` and friends use, so a script can tell "nothing
/// matched" from "you asked wrong".
pub const OK: i32 = 0;
pub const FAILED: i32 = 1;
pub const BAD_ARGUMENTS: i32 = 2;

/// A command that could not be answered as asked, and what to exit with.
#[derive(Debug)]
pub struct Failure {
    pub message: String,
    pub code: i32,
}

impl Failure {
    fn arguments(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: BAD_ARGUMENTS,
        }
    }
}

impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        // A missing login is the one failure worth rewording: the CLI
        // answers it with a paragraph of its own stack, and what a person
        // needs is the two words that fix it. Every command's errors funnel
        // through here, as the worker's do through its own.
        let message = if crate::azure::transport::is_signed_out(&error) {
            "not signed in — run `az login`".to_owned()
        } else {
            format!("{error:#}")
        };
        Self {
            message,
            code: FAILED,
        }
    }
}

/// Everything a command needs: the configuration, a client, and where the
/// cache is.
pub struct Context<'a> {
    pub azure: &'a Azure,
    pub client: &'a Client,
    pub cache: Option<&'a std::path::Path>,
}

impl Context<'_> {
    /// The inventory, from the cache when it is young enough and from Azure
    /// otherwise. A command that has to be current passes `refresh`.
    fn inventory(&self, refresh: bool) -> Result<(Vec<Vault>, Vec<Registry>)> {
        if !refresh && let Some(snapshot) = self.snapshot() {
            return Ok((snapshot.inventory.vaults, snapshot.inventory.registries));
        }
        let inventory = graph::inventory(self.client, self.azure)?;
        Ok((inventory.vaults, inventory.registries))
    }

    /// The cache, if there is one and it is younger than the refresh
    /// interval. Older than that and a shell command should go and look.
    fn snapshot(&self) -> Option<cache::Snapshot> {
        let snapshot = cache::load(self.cache?)?;
        let stale_after = self.azure.refresh.unwrap_or(300);
        // `refresh = 0` turns the timer off in the TUI; from a shell it means
        // the cache never goes off by itself.
        if stale_after == 0 {
            return Some(snapshot);
        }
        let age = snapshot.read_at.seconds_until(Timestamp::now());
        (age >= 0 && age.unsigned_abs() < stale_after).then_some(snapshot)
    }
}

/// `az-tui secrets [QUERY] [--vault NAME]…`
pub fn secrets(
    out: &mut impl Write,
    context: &Context<'_>,
    query: Option<&str>,
    only: &[String],
    json: bool,
    refresh: bool,
) -> Result<(), Failure> {
    let rows = read_secrets(context, only, refresh)?;
    let now = Timestamp::now();
    let parsed = Query::parse(query.unwrap_or_default(), crate::app::secrets::SCHEMA);
    let mut words = crate::search::Query::new(&parsed.words);
    let shown: Vec<&SecretRow> = rows
        .iter()
        .filter(|row| {
            crate::app::secrets::passes(row, &parsed, now)
                && words.matches(&crate::app::secrets::haystack(row))
        })
        .collect();

    if json {
        // No `value` key here, and none possible: `SecretRow` has no field
        // for one.
        let document: Vec<_> = shown
            .iter()
            .map(|row| {
                json!({
                    "vault": row.vault,
                    "name": row.name,
                    "enabled": row.enabled,
                    "content_type": row.content_type,
                    "expires": row.expires.map(Timestamp::to_rfc3339),
                    "created": row.created.map(Timestamp::to_rfc3339),
                    "updated": row.updated.map(Timestamp::to_rfc3339),
                    "managed": row.managed,
                    "tags": row.tags.iter().cloned().collect::<std::collections::BTreeMap<_, _>>(),
                })
            })
            .collect();
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&document).map_err(anyhow::Error::from)?
        )
        .map_err(anyhow::Error::from)?;
        return Ok(());
    }

    for row in shown {
        writeln!(
            out,
            "{:<16} {:<40} {:<8} {:<10} {}",
            row.vault,
            row.name,
            if row.enabled { "enabled" } else { "disabled" },
            row.expires
                .map_or_else(|| "—".to_owned(), |at| at.relative_age(now)),
            row.updated
                .map_or_else(|| "—".to_owned(), |at| at.relative_age(now))
        )
        .map_err(anyhow::Error::from)?;
    }
    Ok(())
}

/// `az-tui secret get NAME [--vault NAME] [--version ID]`
///
/// The one command that prints a value.
pub fn secret_get(
    out: &mut impl Write,
    context: &Context<'_>,
    name: &str,
    only: Option<&str>,
    version: Option<&str>,
    json: bool,
) -> Result<(), Failure> {
    let (vaults, _) = context.inventory(false)?;
    let vaults = allowed_vaults(vaults, context.azure, only);
    if vaults.is_empty() {
        return Err(Failure::arguments(match only {
            Some(named) => format!("no vault called {named}"),
            None => "the login can reach no vaults".to_owned(),
        }));
    }

    // Which vaults actually hold it. A name in more than one and no --vault
    // is ambiguous, and guessing would be the worst possible answer.
    let mut holding = Vec::new();
    for vault in &vaults {
        match vault::secrets(context.client, vault) {
            Ok(rows) => {
                if let Some(row) = rows.into_iter().find(|row| row.name == name) {
                    holding.push((vault.clone(), row));
                }
            }
            // A vault that would not answer cannot be ruled in or out, and
            // saying so is better than a silent "not found".
            Err(error) if vaults.len() == 1 => return Err(error.into()),
            Err(_) => {}
        }
    }
    let [(held, row)] = holding.as_slice() else {
        return Err(if holding.is_empty() {
            Failure {
                message: format!("no secret called {name} in {}", named(&vaults)),
                code: FAILED,
            }
        } else {
            Failure::arguments(format!(
                "{name} is in more than one vault ({}); name one with --vault",
                holding
                    .iter()
                    .map(|(vault, _)| vault.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        });
    };

    // The one place outside the TUI that reads a value out.
    let (secret, read) = vault::value(context.client, held, name, version)?;
    if json {
        let document = json!({
            "vault": held.name,
            "name": name,
            "version": read,
            "content_type": row.content_type,
            "value": secret.expose(),
        });
        writeln!(out, "{document}").map_err(anyhow::Error::from)?;
    } else {
        // No newline of its own: a value that ends in one keeps it, and one
        // that does not is not given one.
        out.write_all(secret.expose().as_bytes())
            .map_err(anyhow::Error::from)?;
    }
    Ok(())
}

/// `az-tui repos [QUERY] [--registry NAME]…`
pub fn repos(
    out: &mut impl Write,
    context: &Context<'_>,
    query: Option<&str>,
    only: &[String],
    json: bool,
    refresh: bool,
) -> Result<(), Failure> {
    let rows = read_repositories(context, only, refresh)?;
    let now = Timestamp::now();
    let parsed = Query::parse(
        query.unwrap_or_default(),
        crate::app::registries::REPOSITORY_SCHEMA,
    );
    let mut words = crate::search::Query::new(&parsed.words);
    let shown: Vec<&Repository> = rows
        .iter()
        .filter(|row| {
            crate::app::registries::repository_passes(row, &parsed, now)
                && words.matches(&crate::app::registries::haystack(row))
        })
        .collect();

    if json {
        let document: Vec<_> = shown
            .iter()
            .map(|row| {
                json!({
                    "registry": row.registry,
                    "repository": row.name,
                    "tag_count": row.tag_count,
                    "manifest_count": row.manifest_count,
                    "created": row.created.map(Timestamp::to_rfc3339),
                    "updated": row.updated.map(Timestamp::to_rfc3339),
                })
            })
            .collect();
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&document).map_err(anyhow::Error::from)?
        )
        .map_err(anyhow::Error::from)?;
        return Ok(());
    }
    for row in shown {
        writeln!(
            out,
            "{:<16} {:<40} {:>6} {}",
            row.registry,
            row.name,
            row.tag_count
                .map_or_else(|| "—".to_owned(), |count| count.to_string()),
            row.updated
                .map_or_else(|| "—".to_owned(), |at| at.relative_age(now))
        )
        .map_err(anyhow::Error::from)?;
    }
    Ok(())
}

/// `az-tui tags REPO [--registry NAME]`
pub fn tags(
    out: &mut impl Write,
    context: &Context<'_>,
    repo: &str,
    only: Option<&str>,
    json: bool,
) -> Result<(), Failure> {
    let (_, registries) = context.inventory(false)?;
    let registries = allowed_registries(registries, context.azure, only);
    if registries.is_empty() {
        return Err(Failure::arguments(match only {
            Some(named) => format!("no registry called {named}"),
            None => "the login can reach no registries".to_owned(),
        }));
    }

    let mut holding = Vec::new();
    for registry in &registries {
        match acr::repositories(context.client, registry) {
            Ok(names) if names.iter().any(|held| held == repo) => holding.push(registry.clone()),
            Ok(_) => {}
            Err(error) if registries.len() == 1 => return Err(error.into()),
            Err(_) => {}
        }
    }
    let [held] = holding.as_slice() else {
        return Err(if holding.is_empty() {
            Failure {
                message: format!(
                    "no repository called {repo} in {}",
                    named_registries(&registries)
                ),
                code: FAILED,
            }
        } else {
            Failure::arguments(format!(
                "{repo} is in more than one registry ({}); name one with --registry",
                named_registries(&holding)
            ))
        });
    };

    let tags = acr::tags(context.client, held, repo)?;
    let now = Timestamp::now();
    if json {
        let document: Vec<_> = tags
            .iter()
            .map(|tag| {
                json!({
                    "registry": held.name,
                    "repository": repo,
                    "tag": tag.name,
                    "digest": tag.digest,
                    "pull": acr::pull_reference(&held.login_server, repo, &tag.name),
                    "created": tag.created.map(Timestamp::to_rfc3339),
                    "updated": tag.updated.map(Timestamp::to_rfc3339),
                })
            })
            .collect();
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&document).map_err(anyhow::Error::from)?
        )
        .map_err(anyhow::Error::from)?;
        return Ok(());
    }
    for tag in &tags {
        writeln!(
            out,
            "{:<30} {:<20} {}",
            tag.name,
            acr::short_digest(&tag.digest),
            tag.updated
                .map_or_else(|| "—".to_owned(), |at| at.relative_age(now))
        )
        .map_err(anyhow::Error::from)?;
    }
    Ok(())
}

/// Every secret in every allowed vault, from the cache or from Azure.
fn read_secrets(
    context: &Context<'_>,
    only: &[String],
    refresh: bool,
) -> Result<Vec<SecretRow>, Failure> {
    if !refresh && let Some(snapshot) = context.snapshot() {
        let wanted = wanted(only, &context.azure.vaults);
        return Ok(snapshot
            .secrets
            .into_iter()
            .filter(|row| {
                wanted
                    .as_ref()
                    .is_none_or(|names| names.contains(&row.vault))
            })
            .collect());
    }
    let (vaults, _) = context.inventory(refresh)?;
    let vaults = allowed_vaults(vaults, context.azure, None);
    let vaults = keep_named(vaults, only, |vault| vault.name.as_str());
    let mut rows = Vec::new();
    let mut failed = Vec::new();
    for vault in &vaults {
        match vault::secrets(context.client, vault) {
            Ok(found) => rows.extend(found),
            Err(error) => failed.push(format!("{error:#}")),
        }
    }
    if rows.is_empty() && !failed.is_empty() {
        return Err(Failure {
            message: failed.join("; "),
            code: FAILED,
        });
    }
    for message in failed {
        eprintln!("warning: {message}");
    }
    Ok(rows)
}

fn read_repositories(
    context: &Context<'_>,
    only: &[String],
    refresh: bool,
) -> Result<Vec<Repository>, Failure> {
    if !refresh && let Some(snapshot) = context.snapshot() {
        let wanted = wanted(only, &context.azure.registries);
        return Ok(snapshot
            .repositories
            .into_iter()
            .filter(|row| {
                wanted
                    .as_ref()
                    .is_none_or(|names| names.contains(&row.registry))
            })
            .collect());
    }
    let (_, registries) = context.inventory(refresh)?;
    let registries = allowed_registries(registries, context.azure, None);
    let registries = keep_named(registries, only, |registry| registry.name.as_str());
    let mut rows = Vec::new();
    let mut failed = Vec::new();
    for registry in &registries {
        match acr::repositories(context.client, registry) {
            Ok(names) => rows.extend(names.into_iter().map(|name| Repository {
                registry: registry.name.clone(),
                name,
                tag_count: None,
                manifest_count: None,
                created: None,
                updated: None,
            })),
            Err(error) => failed.push(format!("{error:#}")),
        }
    }
    if rows.is_empty() && !failed.is_empty() {
        return Err(Failure {
            message: failed.join("; "),
            code: FAILED,
        });
    }
    for message in failed {
        eprintln!("warning: {message}");
    }
    Ok(rows)
}

/// The names a command was told to keep, lowercased, or `None` for all of
/// them. A `--vault` flag narrows the file's list rather than replacing it.
fn wanted(flags: &[String], configured: &[String]) -> Option<Vec<String>> {
    let names: Vec<String> = if flags.is_empty() {
        configured.to_vec()
    } else {
        flags.to_vec()
    };
    (!names.is_empty()).then(|| names.iter().map(|name| name.trim().to_owned()).collect())
}

fn keep_named<T: Clone>(
    found: Vec<T>,
    only: &[String],
    name_of: impl Fn(&T) -> &str + Copy,
) -> Vec<T> {
    if only.is_empty() {
        return found;
    }
    crate::azure::allowed(found, only, name_of)
}

fn allowed_vaults(found: Vec<Vault>, azure: &Azure, only: Option<&str>) -> Vec<Vault> {
    let found = crate::azure::allowed(found, &azure.vaults, |vault| vault.name.as_str());
    match only {
        Some(name) => found
            .into_iter()
            .filter(|vault| vault.name.eq_ignore_ascii_case(name.trim()))
            .collect(),
        None => found,
    }
}

fn allowed_registries(found: Vec<Registry>, azure: &Azure, only: Option<&str>) -> Vec<Registry> {
    let found = crate::azure::allowed(found, &azure.registries, |registry| registry.name.as_str());
    match only {
        Some(name) => found
            .into_iter()
            .filter(|registry| registry.name.eq_ignore_ascii_case(name.trim()))
            .collect(),
        None => found,
    }
}

fn named(vaults: &[Vault]) -> String {
    vaults
        .iter()
        .map(|vault| vault.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn named_registries(registries: &[Registry]) -> String {
    registries
        .iter()
        .map(|registry| registry.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::transport::fake::{Answer, client as fake_client};
    use serde_json::Value;

    fn inventory_answer() -> Answer {
        Answer::json(json!({
            "data": [
                {
                    "id": "/vaults/kv-dev", "name": "kv-dev",
                    "type": "microsoft.keyvault/vaults", "subscriptionId": "s",
                    "resourceGroup": "rg", "location": "eastus", "sku": "standard",
                    "vaultUri": "https://kv-dev.vault.azure.net/", "loginServer": "",
                },
                {
                    "id": "/vaults/kv-prod", "name": "kv-prod",
                    "type": "microsoft.keyvault/vaults", "subscriptionId": "s",
                    "resourceGroup": "rg", "location": "eastus", "sku": "standard",
                    "vaultUri": "https://kv-prod.vault.azure.net/", "loginServer": "",
                },
                {
                    "id": "/registries/acrprod", "name": "acrprod",
                    "type": "microsoft.containerregistry/registries", "subscriptionId": "s",
                    "resourceGroup": "rg", "location": "eastus", "sku": "Premium",
                    "loginServer": "acrprod.azurecr.io", "vaultUri": "",
                },
            ],
        }))
    }

    fn listing(vault: &str, names: &[&str]) -> Answer {
        Answer::json(json!({
            "value": names
                .iter()
                .map(|name| json!({
                    "id": format!("https://{vault}.vault.azure.net/secrets/{name}"),
                    "contentType": "text/plain",
                    "attributes": { "enabled": true, "updated": 1_789_156_800_i64 },
                }))
                .collect::<Vec<_>>(),
        }))
    }

    fn context<'a>(client: &'a Client, azure: &'a Azure) -> Context<'a> {
        Context {
            azure,
            client,
            // No cache: every test here is about what the commands read and
            // print, not about when they skip reading.
            cache: None,
        }
    }

    fn text(out: Vec<u8>) -> String {
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn secrets_prints_one_line_per_secret_and_the_query_narrows_it() {
        let azure = Azure::default();
        let (client, _, _) = fake_client([
            inventory_answer(),
            listing("kv-dev", &["api-key", "db-password"]),
            listing("kv-prod", &["db-password"]),
        ]);
        let mut out = Vec::new();
        secrets(
            &mut out,
            &context(&client, &azure),
            Some("db"),
            &[],
            false,
            true,
        )
        .unwrap();
        let printed = text(out);
        assert_eq!(printed.lines().count(), 2, "{printed}");
        assert!(
            printed.contains("kv-dev           db-password"),
            "{printed}"
        );
        assert!(
            printed.contains("kv-prod          db-password"),
            "{printed}"
        );
        assert!(!printed.contains("api-key"), "{printed}");
    }

    #[test]
    fn the_json_of_a_listing_can_never_carry_a_value() {
        let azure = Azure::default();
        let (client, _, _) = fake_client([
            inventory_answer(),
            listing("kv-dev", &["api-key"]),
            listing("kv-prod", &[]),
        ]);
        let mut out = Vec::new();
        secrets(&mut out, &context(&client, &azure), None, &[], true, true).unwrap();
        let printed = text(out);
        let document: Value = serde_json::from_str(&printed).unwrap();
        assert_eq!(document.as_array().unwrap().len(), 1);
        assert_eq!(document[0]["name"], json!("api-key"));
        assert!(
            !printed.contains("\"value\""),
            "`secrets` has no field for one and must never grow one: {printed}"
        );
    }

    #[test]
    fn a_vault_flag_narrows_what_is_read_at_all() {
        let azure = Azure::default();
        let (client, transport, _) =
            fake_client([inventory_answer(), listing("kv-prod", &["db-password"])]);
        let mut out = Vec::new();
        secrets(
            &mut out,
            &context(&client, &azure),
            None,
            &["kv-prod".to_owned()],
            false,
            true,
        )
        .unwrap();
        assert!(text(out).contains("kv-prod"));
        assert_eq!(
            transport.urls().len(),
            2,
            "the inventory and one vault, not both vaults"
        );
    }

    #[test]
    fn secret_get_prints_the_value_and_nothing_else() {
        let azure = Azure::default();
        let (client, _, _) = fake_client([
            inventory_answer(),
            listing("kv-dev", &["api-key"]),
            listing("kv-prod", &[]),
            Answer::json(json!({
                "value": "s3cr3t",
                "id": "https://kv-dev.vault.azure.net/secrets/api-key/8f3a2c1d",
            })),
        ]);
        let mut out = Vec::new();
        secret_get(
            &mut out,
            &context(&client, &azure),
            "api-key",
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            text(out),
            "s3cr3t",
            "no trailing newline of its own: $(az-tui secret get …) is the value"
        );
    }

    #[test]
    fn secret_get_keeps_a_value_that_ends_in_a_newline() {
        let azure = Azure::default();
        let (client, _, _) = fake_client([
            inventory_answer(),
            listing("kv-dev", &["pem"]),
            listing("kv-prod", &[]),
            Answer::json(json!({
                "value": "-----BEGIN-----\nabc\n",
                "id": "https://kv-dev.vault.azure.net/secrets/pem/v1",
            })),
        ]);
        let mut out = Vec::new();
        secret_get(
            &mut out,
            &context(&client, &azure),
            "pem",
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(text(out), "-----BEGIN-----\nabc\n");
    }

    #[test]
    fn secret_get_refuses_to_guess_when_two_vaults_hold_the_name() {
        let azure = Azure::default();
        let (client, _, _) = fake_client([
            inventory_answer(),
            listing("kv-dev", &["db-password"]),
            listing("kv-prod", &["db-password"]),
        ]);
        let mut out = Vec::new();
        let failure = secret_get(
            &mut out,
            &context(&client, &azure),
            "db-password",
            None,
            None,
            false,
        )
        .unwrap_err();
        assert_eq!(failure.code, BAD_ARGUMENTS);
        assert!(
            failure.message.contains("kv-dev, kv-prod"),
            "{}",
            failure.message
        );
        assert!(failure.message.contains("--vault"), "{}", failure.message);
        assert!(out.is_empty(), "and nothing was printed");
    }

    #[test]
    fn a_missing_login_says_the_two_words_that_fix_it() {
        use crate::azure::auth::FixedTokens;
        use crate::azure::transport::NoLogin;

        let tokens = FixedTokens::new();
        for _ in 0..4 {
            tokens
                .answers
                .lock()
                .unwrap()
                .push_back(Err(anyhow::Error::new(NoLogin("stack".to_owned()))));
        }
        let client = Client::new(
            Box::new(tokens),
            Box::new(crate::azure::transport::fake::FakeTransport::answering([])),
        );
        let azure = Azure::default();
        let mut out = Vec::new();
        let failure =
            secrets(&mut out, &context(&client, &azure), None, &[], false, true).unwrap_err();
        assert_eq!(failure.message, "not signed in — run `az login`");
        assert_eq!(failure.code, FAILED);
    }

    #[test]
    fn a_name_nothing_holds_is_a_read_failure_rather_than_an_argument_one() {
        let azure = Azure::default();
        let (client, _, _) = fake_client([
            inventory_answer(),
            listing("kv-dev", &["api-key"]),
            listing("kv-prod", &["api-key"]),
        ]);
        let mut out = Vec::new();
        let failure = secret_get(
            &mut out,
            &context(&client, &azure),
            "nope",
            None,
            None,
            false,
        )
        .unwrap_err();
        assert_eq!(failure.code, FAILED);
        assert!(
            failure.message.contains("no secret called nope"),
            "{}",
            failure.message
        );
    }

    #[test]
    fn secret_get_json_names_the_vault_and_the_version_it_read() {
        let azure = Azure::default();
        let (client, _, _) = fake_client([
            inventory_answer(),
            listing("kv-dev", &["api-key"]),
            listing("kv-prod", &[]),
            Answer::json(json!({
                "value": "s3cr3t",
                "id": "https://kv-dev.vault.azure.net/secrets/api-key/8f3a2c1d",
            })),
        ]);
        let mut out = Vec::new();
        secret_get(
            &mut out,
            &context(&client, &azure),
            "api-key",
            None,
            None,
            true,
        )
        .unwrap();
        let document: Value = serde_json::from_str(&text(out)).unwrap();
        assert_eq!(document["vault"], json!("kv-dev"));
        assert_eq!(document["version"], json!("8f3a2c1d"));
        assert_eq!(document["content_type"], json!("text/plain"));
        assert_eq!(document["value"], json!("s3cr3t"));
    }

    #[test]
    fn repos_and_tags_print_what_a_script_would_pull() {
        let azure = Azure::default();
        let (client, _, _) = fake_client([
            inventory_answer(),
            Answer::json(json!({ "refresh_token": "r" })),
            Answer::json(json!({ "access_token": "a" })),
            Answer::json(json!({ "repositories": ["payments-api", "web"] })),
        ]);
        let mut out = Vec::new();
        repos(
            &mut out,
            &context(&client, &azure),
            Some("pay"),
            &[],
            false,
            true,
        )
        .unwrap();
        let printed = text(out);
        assert_eq!(printed.lines().count(), 1, "{printed}");
        assert!(printed.contains("acrprod"), "{printed}");
        assert!(printed.contains("payments-api"), "{printed}");

        let (client, _, _) = fake_client([
            inventory_answer(),
            Answer::json(json!({ "refresh_token": "r" })),
            Answer::json(json!({ "access_token": "a" })),
            Answer::json(json!({ "repositories": ["payments-api"] })),
            Answer::json(json!({ "access_token": "repo" })),
            Answer::json(json!({
                "tags": [
                    { "name": "1.42.0", "digest": "sha256:ab12ef0199", "lastUpdateTime": "2026-09-11T18:00:00Z" },
                ],
            })),
        ]);
        let mut out = Vec::new();
        tags(
            &mut out,
            &context(&client, &azure),
            "payments-api",
            None,
            true,
        )
        .unwrap();
        let document: Value = serde_json::from_str(&text(out)).unwrap();
        assert_eq!(document[0]["tag"], json!("1.42.0"));
        assert_eq!(
            document[0]["digest"],
            json!("sha256:ab12ef0199"),
            "the whole digest, not the short one"
        );
        assert_eq!(
            document[0]["pull"],
            json!("acrprod.azurecr.io/payments-api:1.42.0"),
            "the reference a script feeds to docker pull"
        );
    }

    #[test]
    fn a_cache_young_enough_is_read_instead_of_azure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        cache::save(
            &path,
            &cache::Snapshot::new(
                Timestamp::now(),
                crate::azure::Inventory::default(),
                vec![SecretRow {
                    vault: "kv-cached".into(),
                    name: "from-the-cache".into(),
                    enabled: true,
                    created: None,
                    updated: None,
                    expires: None,
                    not_before: None,
                    content_type: None,
                    tags: Vec::new(),
                    managed: false,
                }],
                Vec::new(),
            ),
        )
        .unwrap();

        let azure = Azure::default();
        // No answers at all: reading one would panic the fake transport,
        // which is exactly the assertion.
        let (client, transport, _) = fake_client([]);
        let context = Context {
            azure: &azure,
            client: &client,
            cache: Some(&path),
        };
        let mut out = Vec::new();
        secrets(&mut out, &context, None, &[], false, false).unwrap();
        assert!(text(out).contains("from-the-cache"));
        assert!(transport.sent().is_empty(), "nothing went out");

        // `--refresh` goes and looks whatever the cache holds.
        let (client, transport, _) = fake_client([
            inventory_answer(),
            listing("kv-dev", &[]),
            listing("kv-prod", &[]),
        ]);
        let context = Context {
            azure: &azure,
            client: &client,
            cache: Some(&path),
        };
        let mut out = Vec::new();
        secrets(&mut out, &context, None, &[], false, true).unwrap();
        assert!(!transport.sent().is_empty());
    }

    #[test]
    fn a_cache_older_than_the_refresh_interval_is_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let long_ago = Timestamp::parse("2020-01-01T00:00:00Z").unwrap();
        cache::save(
            &path,
            &cache::Snapshot::new(
                long_ago,
                crate::azure::Inventory::default(),
                Vec::new(),
                Vec::new(),
            ),
        )
        .unwrap();

        let azure = Azure::default();
        let (client, transport, _) = fake_client([
            inventory_answer(),
            listing("kv-dev", &["a"]),
            listing("kv-prod", &[]),
        ]);
        let context = Context {
            azure: &azure,
            client: &client,
            cache: Some(&path),
        };
        let mut out = Vec::new();
        secrets(&mut out, &context, None, &[], false, false).unwrap();
        assert!(
            !transport.sent().is_empty(),
            "a stale cache is not an answer"
        );
    }
}
