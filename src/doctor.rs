//! `az-tui doctor`: everything the TUI needs, checked one line at a time on
//! the calling thread, stopping at the first thing that is not there.
//!
//! Each check is one function returning `Result<String>`, so the steps that
//! add planes append their own line without touching the ones before it. No
//! secret value is ever read here — `doctor` reads names and counts only.

use std::io::Write;
use std::time::Instant;

use anyhow::Result;

use crate::azure::auth::{self, Audience, AzCli, TokenSource};
use crate::azure::transport::{Client, Https};
use crate::azure::{Inventory, acr, graph, missing, vault};
use crate::config::Azure;

/// Runs every check, printing as it goes. Exit 0 when everything answered.
pub fn run(azure: &Azure) -> Result<bool> {
    let mut out = std::io::stdout().lock();
    let mut ok = true;

    match auth::az_version() {
        Ok(version) => line(&mut out, "az", &version)?,
        Err(error) => {
            fail(&mut out, "az", &error, "install the Azure CLI")?;
            return Ok(false);
        }
    }
    match auth::account() {
        Ok((user, tenant)) => line(&mut out, "account", &format!("{user} · tenant {tenant}"))?,
        Err(_) => {
            // The CLI's own words here are three lines of stack; what the
            // reader needs is which of the two things went wrong.
            line(&mut out, "account", crate::azure::transport::SIGNED_OUT)?;
            return Ok(false);
        }
    }
    let named = !azure.subscriptions.is_empty();
    match subscriptions_line(azure) {
        Ok(said) => line(&mut out, "subscriptions", &said)?,
        Err(error) if named => fail(&mut out, "subscriptions", &error, "check config.toml")?,
        Err(error) => {
            fail(&mut out, "subscriptions", &error, "run `az login`")?;
            return Ok(false);
        }
    }

    let tokens = AzCli;
    for (label, audience) in [
        ("arm token", Audience::Arm),
        ("vault token", Audience::Vault),
    ] {
        let started = Instant::now();
        match tokens.token(&audience) {
            Ok(_) => line(&mut out, label, &format!("ok ({})", took(started)))?,
            Err(error) => {
                fail(&mut out, label, &error, "run `az login`")?;
                return Ok(false);
            }
        }
    }

    let client = Client::new(Box::new(AzCli), Box::new(Https::new()));
    let started = Instant::now();
    let inventory = match graph::inventory(&client, azure) {
        Ok(inventory) => {
            line(
                &mut out,
                "inventory",
                &format!(
                    "{} vaults, {} registries ({})",
                    inventory.vaults.len(),
                    inventory.registries.len(),
                    took(started)
                ),
            )?;
            inventory
        }
        Err(error) => {
            fail(&mut out, "inventory", &error, "check the subscription")?;
            return Ok(false);
        }
    };
    for vault in &inventory.vaults {
        indented(
            &mut out,
            &vault.name,
            &format!(
                "{:8} {:16} {}",
                vault.location, vault.resource_group, vault.uri
            ),
        )?;
    }
    for registry in &inventory.registries {
        indented(
            &mut out,
            &registry.name,
            &format!(
                "{:8} {:16} {}",
                registry.location, registry.resource_group, registry.login_server
            ),
        )?;
    }

    // What each plane actually answers, which is the half of the check ARM
    // cannot do for you: a vault can be listed by Resource Graph and still
    // refuse every data-plane call. No values and no attributes are read:
    // one listing per vault and per registry is the whole check.
    for vault in &inventory.vaults {
        let (said, answered) = timed("secrets", &format!("{}: ", vault.name), || {
            vault::secrets(&client, vault).map(|rows| rows.len())
        });
        line(&mut out, &vault.name, &said)?;
        ok &= answered;
    }
    for registry in &inventory.registries {
        let (said, answered) = timed(
            "repositories",
            &format!("{}: ", registry.login_server),
            || acr::repositories(&client, registry).map(|names| names.len()),
        );
        line(&mut out, &registry.name, &said)?;
        ok &= answered;
    }
    if !report_missing(&mut out, &inventory, azure)? {
        ok = false;
    }
    Ok(ok)
}

/// One data-plane read, timed: how many `noun` it found, or why it would not
/// say. The label of the line already names the vault or registry, so a
/// refusal that starts by naming it again has that taken off.
fn timed(noun: &str, prefix: &str, read: impl FnOnce() -> Result<usize>) -> (String, bool) {
    let started = Instant::now();
    match read() {
        Ok(count) => (format!("{count} {noun} ({})", took(started)), true),
        Err(error) => {
            let said = format!("{error:#}");
            (said.strip_prefix(prefix).unwrap_or(&said).to_owned(), false)
        }
    }
}

/// A name in the allowlist that the login could not reach. Not an error —
/// a vault may simply not exist yet — but the reason a tab looks empty.
fn report_missing(out: &mut impl Write, inventory: &Inventory, azure: &Azure) -> Result<bool> {
    let gone: Vec<String> = missing(&inventory.vaults, &azure.vaults, |vault| {
        vault.name.as_str()
    })
    .into_iter()
    .chain(missing(&inventory.registries, &azure.registries, |r| {
        r.name.as_str()
    }))
    .collect();
    if gone.is_empty() {
        return Ok(true);
    }
    line(
        out,
        "not found",
        &format!("{} (named in config.toml)", gone.join(", ")),
    )?;
    Ok(false)
}

fn subscriptions_line(azure: &Azure) -> Result<String> {
    if !azure.subscriptions.is_empty() {
        return Ok(format!(
            "{} from config.toml: {}",
            azure.subscriptions.len(),
            azure.subscriptions.join(", ")
        ));
    }
    let found = auth::subscriptions()?;
    Ok(format!("{} enabled", found.len()))
}

/// Every line is `label` padded to a column, then what it found.
fn line(out: &mut impl Write, label: &str, said: &str) -> Result<()> {
    writeln!(out, "{label:<14}{said}")?;
    Ok(())
}

fn indented(out: &mut impl Write, label: &str, said: &str) -> Result<()> {
    writeln!(out, "  {label:<12}{said}")?;
    Ok(())
}

fn fail(out: &mut impl Write, label: &str, error: &anyhow::Error, fix: &str) -> Result<()> {
    line(out, label, &format!("{error:#}"))?;
    line(out, "", &format!("→ {fix}"))?;
    Ok(())
}

/// How long a step took, in whichever unit reads.
fn took(started: Instant) -> String {
    let elapsed = started.elapsed();
    if elapsed.as_secs() >= 1 {
        format!("{:.1} s", elapsed.as_secs_f64())
    } else {
        format!("{} ms", elapsed.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_line_lands_in_the_same_column() {
        let mut out = Vec::new();
        line(&mut out, "az", "2.90.0").unwrap();
        indented(&mut out, "kv-prod", "eastus").unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert_eq!(printed, "az            2.90.0\n  kv-prod     eastus\n");
    }

    #[test]
    fn a_configured_subscription_list_is_reported_without_asking_az() {
        let azure = Azure {
            subscriptions: vec!["sub-1".into(), "sub-2".into()],
            ..Azure::default()
        };
        let said = subscriptions_line(&azure).unwrap();
        assert!(said.contains("2 from config.toml"), "{said}");
        assert!(said.contains("sub-1, sub-2"), "{said}");
    }

    #[test]
    fn a_refusal_is_not_prefixed_with_the_name_the_line_already_carries() {
        let (said, ok) = timed("secrets", "kv-prod: ", || {
            Err(anyhow::anyhow!("kv-prod: no permission to read secrets"))
        });
        assert!(!ok);
        assert_eq!(said, "no permission to read secrets");
        let (said, ok) = timed("secrets", "kv-prod: ", || Ok(3));
        assert!(ok);
        assert!(said.starts_with("3 secrets ("), "{said}");
    }

    #[test]
    fn a_name_that_was_not_found_is_reported_and_is_not_ok() {
        let mut out = Vec::new();
        let azure = Azure {
            vaults: vec!["kv-staging".into()],
            ..Azure::default()
        };
        let ok = report_missing(&mut out, &Inventory::default(), &azure).unwrap();
        assert!(!ok);
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("kv-staging"), "{printed}");
        assert!(printed.contains("named in config.toml"), "{printed}");
    }
}
