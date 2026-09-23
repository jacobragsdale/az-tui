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
use serde::Serialize;
use serde_json::json;

use crate::azure::transport::{Client, said};
use crate::azure::{
    Inventory, Registry, Repository, SecretRow, Vault, acr, allowed, graph, missing, vault,
};
use crate::cache::{self, CachedTab};
use crate::config::{Azure, REGISTRIES_TAB, SECRETS_TAB};
use crate::filter::Query;
use crate::parallel;
use crate::timestamp::Timestamp;

/// What a command exits with. 0 ok, 1 a read failed, 2 the arguments were
/// wrong — the shape `grep` and friends use, so a script can tell "nothing
/// matched" from "you asked wrong".
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

    fn failed(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: FAILED,
        }
    }
}

impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        Self::failed(said(&error))
    }
}

impl From<std::io::Error> for Failure {
    /// A reader that went away — `| head -1` — has had all it wanted: that
    /// is a clean exit with nothing said, code 0.
    fn from(error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::BrokenPipe {
            return Self {
                message: String::new(),
                code: 0,
            };
        }
        Self::failed(error.to_string())
    }
}

impl From<serde_json::Error> for Failure {
    fn from(error: serde_json::Error) -> Self {
        Self::failed(error.to_string())
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
    /// The inventory, from the cache when both azure tabs are young enough
    /// and from Azure otherwise. A command that has to be current passes
    /// `refresh`.
    fn inventory(&self, refresh: bool) -> Result<Inventory> {
        if !refresh
            && let Some(CachedTab::Secrets { vaults, .. }) = self.cached(SECRETS_TAB)
            && let Some(CachedTab::Registries { registries, .. }) = self.cached(REGISTRIES_TAB)
        {
            return Ok(Inventory { vaults, registries });
        }
        // Read without the allowlist: `narrow` applies it, and has to see
        // what it leaves out to say so.
        let everything = Azure {
            vaults: Vec::new(),
            registries: Vec::new(),
            ..self.azure.clone()
        };
        graph::inventory(self.client, &everything)
    }

    /// One tab of the cache, if there is one and it is younger than the
    /// refresh interval. Older than that and a shell command should go and
    /// look.
    fn cached(&self, tab: &str) -> Option<CachedTab> {
        let entry = cache::load(self.cache?)?.tabs.remove(tab)?;
        let stale_after = self.azure.refresh.unwrap_or(300);
        // `refresh = 0` turns the timer off in the TUI; from a shell it means
        // the cache never goes off by itself.
        if stale_after == 0 {
            return Some(entry);
        }
        let age = entry.read_at().seconds_until(Timestamp::now());
        (age >= 0 && age.unsigned_abs() < stale_after).then_some(entry)
    }
}

/// `az-tui secrets [QUERY] [--vault NAME]…`
///
/// Every row that answered is printed; a vault that would not answer is
/// then the exit code, so a script can tell a whole listing from a partial
/// one.
pub fn secrets(
    out: &mut impl Write,
    context: &Context<'_>,
    query: Option<&str>,
    only: &[String],
    json: bool,
    refresh: bool,
) -> Result<(), Failure> {
    let (rows, failed) = read_secrets(context, only, refresh)?;
    let now = Timestamp::now();
    let parsed = Query::parse(query.unwrap_or_default(), crate::app::secrets::SCHEMA);
    let words = crate::search::Query::new(&parsed.words);
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
        print_json(out, &document)?;
    } else {
        for row in shown {
            writeln!(
                out,
                "{:<16} {:<40} {:<8} {:<10} {}",
                row.vault,
                row.name,
                if row.enabled { "enabled" } else { "disabled" },
                // The table's own Expires cell: `expired`, or the age and a
                // mark inside the window. An age alone loses which side of
                // now it is on.
                crate::app::secrets::Expiry::of(row.expires, now).cell(row.expires, now),
                crate::timestamp::age(row.updated, now),
            )?;
        }
    }
    partial(failed)
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
    let vaults = narrow(
        context.inventory(false)?.vaults,
        &context.azure.vaults,
        only.map(str::to_owned).as_slice(),
        "vault",
        |vault| vault.name.as_str(),
    )?;
    if vaults.is_empty() {
        return Err(none_reachable("vault", &context.azure.vaults));
    }

    // Which vaults actually hold it. A name in more than one and no --vault
    // is ambiguous, and guessing would be the worst possible answer.
    let mut holding = Vec::new();
    let mut failed = Vec::new();
    let listed = parallel::map(&vaults, context.azure.threads(), |vault| {
        vault::secrets(context.client, vault)
    });
    for (vault, listing) in vaults.iter().zip(listed) {
        match listing {
            Ok(rows) => {
                if let Some(row) = rows.into_iter().find(|row| row.name == name) {
                    holding.push((vault.clone(), row));
                }
            }
            Err(error) => failed.push(format!("{error:#}")),
        }
    }
    // A vault that would not answer cannot be ruled in or out: with nothing
    // found, the failures are the answer rather than a silent "not found".
    if holding.is_empty() {
        partial(failed)?;
    }
    let [(held, row)] = holding.as_slice() else {
        return Err(if holding.is_empty() {
            Failure::failed(format!(
                "no secret called {name} in {}",
                joined(vaults.iter().map(|vault| vault.name.as_str()))
            ))
        } else {
            Failure::arguments(format!(
                "{name} is in more than one vault ({}); name one with --vault",
                joined(holding.iter().map(|(vault, _)| vault.name.as_str()))
            ))
        });
    };

    let (secret, read) = vault::value(context.client, held, name, version)?;
    // The one place outside the TUI that reads a value out, and the third
    // and last call to `Secret::expose` in the crate.
    let value = secret.expose();
    if json {
        let document = json!({
            "vault": held.name,
            "name": name,
            "version": read,
            "content_type": row.content_type,
            "value": value,
        });
        writeln!(out, "{document}")?;
    } else {
        // No newline of its own: a value that ends in one keeps it, and one
        // that does not is not given one.
        out.write_all(value.as_bytes())?;
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
    let (rows, failed) = read_repositories(context, only, refresh)?;
    let now = Timestamp::now();
    let parsed = Query::parse(
        query.unwrap_or_default(),
        crate::app::registries::REPOSITORY_SCHEMA,
    );
    let words = crate::search::Query::new(&parsed.words);
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
        print_json(out, &document)?;
    } else {
        for row in shown {
            writeln!(
                out,
                "{:<16} {:<40} {:>6} {}",
                row.registry,
                row.name,
                row.tag_count
                    .map_or_else(|| "—".to_owned(), |count| count.to_string()),
                crate::timestamp::age(row.updated, now),
            )?;
        }
    }
    partial(failed)
}

/// `az-tui tags REPO [--registry NAME]`
pub fn tags(
    out: &mut impl Write,
    context: &Context<'_>,
    repo: &str,
    only: Option<&str>,
    json: bool,
) -> Result<(), Failure> {
    let registries = narrow(
        context.inventory(false)?.registries,
        &context.azure.registries,
        only.map(str::to_owned).as_slice(),
        "registry",
        |registry| registry.name.as_str(),
    )?;
    if registries.is_empty() {
        return Err(none_reachable("registry", &context.azure.registries));
    }

    let mut holding = Vec::new();
    let mut failed = Vec::new();
    let catalogs = parallel::map(&registries, context.azure.threads(), |registry| {
        acr::repositories(context.client, registry)
    });
    for (registry, catalog) in registries.iter().zip(catalogs) {
        match catalog {
            Ok(names) if names.iter().any(|held| held == repo) => holding.push(registry.clone()),
            Ok(_) => {}
            Err(error) => failed.push(format!("{error:#}")),
        }
    }
    if holding.is_empty() {
        partial(failed)?;
    }
    let [held] = holding.as_slice() else {
        return Err(if holding.is_empty() {
            Failure::failed(format!(
                "no repository called {repo} in {}",
                joined(registries.iter().map(|registry| registry.name.as_str()))
            ))
        } else {
            Failure::arguments(format!(
                "{repo} is in more than one registry ({}); name one with --registry",
                joined(holding.iter().map(|registry| registry.name.as_str()))
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
        print_json(out, &document)?;
    } else {
        for tag in &tags {
            writeln!(
                out,
                "{:<30} {:<20} {}",
                tag.name,
                acr::short_digest(&tag.digest),
                crate::timestamp::age(tag.updated, now),
            )?;
        }
    }
    Ok(())
}

/// Every secret in every allowed vault, from the cache or from Azure, and
/// the vaults that would not answer.
fn read_secrets(
    context: &Context<'_>,
    only: &[String],
    refresh: bool,
) -> Result<(Vec<SecretRow>, Vec<String>), Failure> {
    if !refresh
        && let Some(CachedTab::Secrets {
            vaults, secrets, ..
        }) = context.cached(SECRETS_TAB)
    {
        let vaults = narrow(vaults, &context.azure.vaults, only, "vault", vault_name)?;
        let all = only.is_empty() && context.azure.vaults.is_empty();
        let rows = secrets
            .into_iter()
            .filter(|row| all || vaults.iter().any(|vault| vault.name == row.vault))
            .collect();
        return Ok((rows, Vec::new()));
    }
    let vaults = narrow(
        context.inventory(refresh)?.vaults,
        &context.azure.vaults,
        only,
        "vault",
        vault_name,
    )?;
    let mut rows = Vec::new();
    let mut failed = Vec::new();
    let listed = parallel::map(&vaults, context.azure.threads(), |vault| {
        vault::secrets(context.client, vault)
    });
    for listing in listed {
        match listing {
            Ok(found) => rows.extend(found),
            Err(error) => failed.push(format!("{error:#}")),
        }
    }
    Ok((rows, failed))
}

fn read_repositories(
    context: &Context<'_>,
    only: &[String],
    refresh: bool,
) -> Result<(Vec<Repository>, Vec<String>), Failure> {
    if !refresh
        && let Some(CachedTab::Registries {
            registries,
            repositories,
            ..
        }) = context.cached(REGISTRIES_TAB)
    {
        let registries = narrow(
            registries,
            &context.azure.registries,
            only,
            "registry",
            registry_name,
        )?;
        let all = only.is_empty() && context.azure.registries.is_empty();
        let rows = repositories
            .into_iter()
            .filter(|row| all || registries.iter().any(|held| held.name == row.registry))
            .collect();
        return Ok((rows, Vec::new()));
    }
    let registries = narrow(
        context.inventory(refresh)?.registries,
        &context.azure.registries,
        only,
        "registry",
        registry_name,
    )?;
    let mut rows = Vec::new();
    let mut failed = Vec::new();
    let catalogs = parallel::map(&registries, context.azure.threads(), |registry| {
        acr::repositories(context.client, registry)
    });
    for (registry, catalog) in registries.iter().zip(catalogs) {
        match catalog {
            Ok(names) => rows.extend(
                names
                    .iter()
                    .map(|name| Repository::unfilled(&registry.name, name)),
            ),
            Err(error) => failed.push(format!("{error:#}")),
        }
    }
    Ok((rows, failed))
}

/// The vaults or registries a command reads: the configuration's allowlist
/// first, then the command's own `--vault`/`--registry` on top of it. Naming
/// one the login cannot reach is an argument error that says what it can;
/// naming one the allowlist leaves out says that instead.
fn narrow<T: Clone>(
    found: Vec<T>,
    configured: &[String],
    only: &[String],
    kind: &str,
    name_of: impl Fn(&T) -> &str + Copy,
) -> Result<Vec<T>, Failure> {
    let unknown = missing(&found, only, name_of);
    let reachable = allowed(found, configured, name_of);
    let gone = missing(&reachable, only, name_of);
    if let Some(name) = gone.iter().find(|name| !unknown.contains(name)) {
        let (list, flag) = allowlist(kind);
        return Err(Failure::arguments(format!(
            "{name} is left out by [azure].{list} / {flag} (allowed: {})",
            joined(reachable.iter().map(name_of))
        )));
    }
    if !gone.is_empty() {
        return Err(Failure::arguments(if reachable.is_empty() {
            format!(
                "no {kind} called {}; the login can reach no {kind}",
                gone.join(", ")
            )
        } else {
            format!(
                "no {kind} called {}; the login can reach {}",
                gone.join(", "),
                joined(reachable.iter().map(name_of))
            )
        }));
    }
    Ok(allowed(reachable, only, name_of))
}

/// The allowlist's key and flag for one kind of resource.
fn allowlist(kind: &str) -> (&'static str, &'static str) {
    if kind == "vault" {
        ("vaults", "--vault")
    } else {
        ("registries", "--registry")
    }
}

/// Why a command has nothing to read: the login reaches none at all, or none
/// of the ones the allowlist names.
fn none_reachable(kind: &str, configured: &[String]) -> Failure {
    let (list, flag) = allowlist(kind);
    Failure::arguments(if configured.is_empty() {
        format!("the login can reach no {list}")
    } else {
        format!(
            "the login can reach none of [azure].{list} / {flag} ({})",
            configured.join(", ")
        )
    })
}

/// What a listing that printed its rows exits with: a read that failed is
/// the exit code, however many rows the others answered with.
fn partial(failed: Vec<String>) -> Result<(), Failure> {
    if failed.is_empty() {
        Ok(())
    } else {
        Err(Failure::failed(failed.join("; ")))
    }
}

fn print_json(out: &mut impl Write, document: &impl Serialize) -> Result<(), Failure> {
    writeln!(out, "{}", serde_json::to_string_pretty(document)?)?;
    Ok(())
}

fn vault_name(vault: &Vault) -> &str {
    &vault.name
}

fn registry_name(registry: &Registry) -> &str {
    &registry.name
}

fn joined<'a>(names: impl Iterator<Item = &'a str>) -> String {
    names.collect::<Vec<_>>().join(", ")
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

    /// One thread, so the fake's canned answers land in the order they were
    /// given: every test here reads several vaults and says which is which
    /// by that order.
    fn serial() -> Azure {
        Azure {
            parallel: Some(1),
            ..Azure::default()
        }
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
        let azure = serial();
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
        let azure = serial();
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
        let azure = serial();
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
        let azure = serial();
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
        let azure = serial();
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
        let azure = serial();
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
        let azure = serial();
        let mut out = Vec::new();
        let failure =
            secrets(&mut out, &context(&client, &azure), None, &[], false, true).unwrap_err();
        assert_eq!(failure.message, "not signed in — run `az login` (stack)");
        assert_eq!(failure.code, FAILED);
    }

    #[test]
    fn an_expiry_prints_as_the_table_says_it_expired_or_marked_soon() {
        let azure = serial();
        let soon = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3 * 86_400;
        let (client, _, _) = fake_client([
            inventory_answer(),
            Answer::json(json!({
                "value": [
                    { "id": "https://kv-dev.vault.azure.net/secrets/gone", "attributes": { "exp": 1_000_000_000 } },
                    { "id": "https://kv-dev.vault.azure.net/secrets/soon", "attributes": { "exp": soon } },
                ],
            })),
            listing("kv-prod", &[]),
        ]);
        let mut out = Vec::new();
        secrets(&mut out, &context(&client, &azure), None, &[], false, true).unwrap();
        let printed = text(out);
        let gone = printed
            .lines()
            .find(|line| line.contains(" gone "))
            .unwrap();
        assert!(gone.contains("expired"), "{printed}");
        let soon = printed
            .lines()
            .find(|line| line.contains(" soon "))
            .unwrap();
        assert!(soon.contains("⚠"), "{printed}");
        assert!(!soon.contains("expired"), "{printed}");
    }

    #[test]
    fn a_reader_that_went_away_is_a_clean_exit_with_nothing_said() {
        let failure = Failure::from(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
        assert_eq!(failure.code, 0);
        assert!(failure.message.is_empty());
        let failure = Failure::from(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert_eq!(failure.code, FAILED);
    }

    #[test]
    fn a_vault_the_allowlist_leaves_out_says_so_rather_than_that_the_login_cannot_reach_it() {
        let azure = Azure {
            vaults: vec!["kv-dev".into()],
            ..serial()
        };
        let (client, _, _) = fake_client([inventory_answer()]);
        let mut out = Vec::new();
        let failure = secrets(
            &mut out,
            &context(&client, &azure),
            None,
            &["kv-prod".to_owned()],
            false,
            true,
        )
        .unwrap_err();
        assert_eq!(failure.code, BAD_ARGUMENTS);
        assert!(
            failure
                .message
                .contains("kv-prod is left out by [azure].vaults / --vault (allowed: kv-dev)"),
            "{}",
            failure.message
        );
        assert!(
            !failure.message.contains("login can reach"),
            "{}",
            failure.message
        );
    }

    #[test]
    fn a_name_nothing_holds_is_a_read_failure_rather_than_an_argument_one() {
        let azure = serial();
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
    fn secret_get_with_no_vault_answering_reports_the_refusals_not_a_missing_name() {
        let azure = serial();
        let (client, _, _) = fake_client([
            inventory_answer(),
            Answer::status(403, r#"{"error":{"message":"kv-dev says no"}}"#),
            Answer::status(403, r#"{"error":{"message":"kv-prod says no"}}"#),
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
        assert_eq!(failure.code, FAILED);
        assert!(
            !failure.message.contains("no secret called"),
            "{}",
            failure.message
        );
        assert!(failure.message.contains("kv-dev"), "{}", failure.message);
        assert!(failure.message.contains("kv-prod"), "{}", failure.message);
    }

    #[test]
    fn secret_get_json_names_the_vault_and_the_version_it_read() {
        let azure = serial();
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
        let azure = serial();
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
    fn a_vault_that_would_not_answer_is_the_exit_code_after_the_rows_that_did() {
        let azure = serial();
        let (client, _, _) = fake_client([
            inventory_answer(),
            listing("kv-dev", &["api-key"]),
            Answer::status(403, r#"{"error":{"code":"Forbidden","message":"no"}}"#),
        ]);
        let mut out = Vec::new();
        let failure =
            secrets(&mut out, &context(&client, &azure), None, &[], false, true).unwrap_err();
        assert_eq!(failure.code, FAILED);
        assert!(failure.message.contains("kv-prod"), "{}", failure.message);
        assert!(
            text(out).contains("api-key"),
            "the rows that answered were still printed"
        );
    }

    #[test]
    fn a_vault_the_login_cannot_reach_is_an_argument_error_that_names_the_ones_it_can() {
        let azure = serial();
        let (client, _, _) = fake_client([inventory_answer()]);
        let mut out = Vec::new();
        let failure = secrets(
            &mut out,
            &context(&client, &azure),
            None,
            &["kv-typo".to_owned()],
            false,
            true,
        )
        .unwrap_err();
        assert_eq!(failure.code, BAD_ARGUMENTS);
        assert!(
            failure.message.contains("no vault called kv-typo"),
            "{}",
            failure.message
        );
        assert!(
            failure.message.contains("kv-dev, kv-prod"),
            "{}",
            failure.message
        );

        let (client, _, _) = fake_client([inventory_answer()]);
        let failure = secret_get(
            &mut out,
            &context(&client, &azure),
            "x",
            Some("kv-x"),
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
    }

    /// One Azure read as the cache holds it: the two fixed tabs, no scopes.
    fn azure_cache(
        read_at: Timestamp,
        inventory: crate::azure::Inventory,
        secrets: Vec<SecretRow>,
        repositories: Vec<Repository>,
    ) -> cache::Snapshot {
        cache::Snapshot::new(
            CachedTab::azure(read_at, inventory, secrets, repositories)
                .into_iter()
                .collect(),
        )
    }

    #[test]
    fn a_cache_young_enough_is_read_instead_of_azure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        cache::save(
            &path,
            &azure_cache(
                Timestamp::now(),
                crate::azure::Inventory {
                    vaults: vec![Vault {
                        id: "/vaults/kv-cached".into(),
                        name: "kv-cached".into(),
                        resource_group: "rg".into(),
                        location: "eastus".into(),
                        uri: "https://kv-cached.vault.azure.net/".into(),
                    }],
                    registries: Vec::new(),
                },
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

        let azure = serial();
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

        // `--vault` narrows the cache the way it narrows Azure: without
        // regard to case.
        let mut out = Vec::new();
        secrets(
            &mut out,
            &context,
            None,
            &["KV-CACHED".to_owned()],
            false,
            false,
        )
        .unwrap();
        assert!(text(out).contains("from-the-cache"));

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
            &azure_cache(
                long_ago,
                crate::azure::Inventory::default(),
                Vec::new(),
                Vec::new(),
            ),
        )
        .unwrap();

        let azure = serial();
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

    #[test]
    fn a_cache_with_only_the_other_tab_is_not_an_answer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let mut fresh = azure_cache(
            Timestamp::now(),
            crate::azure::Inventory::default(),
            Vec::new(),
            Vec::new(),
        );
        fresh.tabs.remove(SECRETS_TAB);
        cache::save(&path, &fresh).unwrap();

        let azure = serial();
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
            "a registries-only cache says nothing about secrets"
        );
    }
}
