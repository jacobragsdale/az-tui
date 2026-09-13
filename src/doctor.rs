//! `az-tui doctor` and `az-tui setup`: everything the TUI needs, checked one
//! line at a time on the calling thread — the Azure half first, stopping at
//! the first thing that is not there, then the AKS half.
//!
//! Each check is one function returning `Result<String>`, so the steps that
//! add planes append their own line without touching the ones before it. No
//! secret value is ever read here — `doctor` reads names and counts only.
//!
//! `setup` does the first-day AKS work: `az aks list`, `az aks
//! get-credentials` for each cluster, `kubelogin convert-kubeconfig -l
//! azurecli` so kubectl borrows the `az login` rather than asking for a
//! device code on every read, and a `[[clusters]]` block per cluster with its
//! namespaces, printed to trim into `config.toml` — or written there with
//! `--write` when the file does not exist yet.

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::azure::auth::{self, Audience, AzCli, TokenSource};
use crate::azure::transport::{Client, Https};
use crate::azure::{Inventory, acr, graph, missing, vault};
use crate::config::{Azure, Config, Tab};
use crate::kube::run_capped;

/// The bound on every call the AKS half makes. `az aks list` over a slow
/// tenant is the one that comes close.
const CALL_CAP: Duration = Duration::from_secs(60);

/// Namespaces every AKS cluster has that nobody wants a tab for.
const SYSTEM_NAMESPACES: &[&str] = &[
    "default",
    "kube-system",
    "kube-public",
    "kube-node-lease",
    "gatekeeper-system",
    "calico-system",
    "tigera-operator",
    "azure-arc",
    "app-routing-system",
    "aks-command",
    "aks-istio-system",
    "aks-istio-ingress",
    "aks-istio-egress",
];

/// Runs every check, printing as it goes: the Azure half, then the AKS
/// half. Exit 0 when everything answered.
pub fn run(out: &mut impl Write, config: &Config) -> Result<bool> {
    let azure = azure_checks(out, &config.azure)?;
    writeln!(out)?;
    let aks = aks_checks(out, config)?;
    Ok(azure && aks)
}

/// The login, the tokens, and what every vault and registry answers.
fn azure_checks(out: &mut impl Write, azure: &Azure) -> Result<bool> {
    let mut ok = true;

    match auth::az_version() {
        Ok(version) => line(out, "az", &version)?,
        Err(error) => {
            fail(out, "az", &error, "install the Azure CLI")?;
            return Ok(false);
        }
    }
    match auth::account() {
        Ok((user, tenant)) => line(out, "account", &format!("{user} · tenant {tenant}"))?,
        Err(_) => {
            // The CLI's own words here are three lines of stack; what the
            // reader needs is which of the two things went wrong.
            line(out, "account", crate::azure::transport::SIGNED_OUT)?;
            return Ok(false);
        }
    }
    let named = !azure.subscriptions.is_empty();
    match subscriptions_line(azure) {
        Ok(said) => line(out, "subscriptions", &said)?,
        Err(error) if named => fail(out, "subscriptions", &error, "check config.toml")?,
        Err(error) => {
            fail(out, "subscriptions", &error, "run `az login`")?;
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
            Ok(_) => line(out, label, &format!("ok ({})", took(started)))?,
            Err(error) => {
                fail(out, label, &error, "run `az login`")?;
                return Ok(false);
            }
        }
    }

    let client = Client::new(Box::new(AzCli), Box::new(Https::new()));
    let started = Instant::now();
    let inventory = match graph::inventory(&client, azure) {
        Ok(inventory) => {
            line(
                out,
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
            fail(out, "inventory", &error, "check the subscription")?;
            return Ok(false);
        }
    };
    for vault in &inventory.vaults {
        indented(
            out,
            &vault.name,
            &format!(
                "{:8} {:16} {}",
                vault.location, vault.resource_group, vault.uri
            ),
        )?;
    }
    for registry in &inventory.registries {
        indented(
            out,
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
        line(out, &vault.name, &said)?;
        ok &= answered;
    }
    for registry in &inventory.registries {
        let (said, answered) = timed(
            "repositories",
            &format!("{}: ", registry.login_server),
            || acr::repositories(&client, registry).map(|names| names.len()),
        );
        line(out, &registry.name, &said)?;
        ok &= answered;
    }
    if !report_missing(out, &inventory, azure)? {
        ok = false;
    }
    Ok(ok)
}

/// `kubectl` and `kubelogin` on `PATH`, and every scope in `config.toml`
/// answering `get pods`. No clusters is a line, not a failure: the Azure
/// tabs stand on their own.
fn aks_checks(out: &mut impl Write, config: &Config) -> Result<bool> {
    let tabs = config.tabs();
    if tabs.is_empty() {
        line(
            out,
            "clusters",
            "none in config.toml — `az-tui setup` prints a [[clusters]] block per cluster",
        )?;
        return Ok(true);
    }
    let mut ok = true;

    match run_capped(
        command("kubectl", &["version", "--client", "-o", "json"]),
        CALL_CAP,
    ) {
        Ok(raw) => {
            let version: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
            let said = version["clientVersion"]["gitVersion"]
                .as_str()
                .unwrap_or("ok")
                .to_owned();
            line(out, "kubectl", &said)?;
        }
        Err(error) => {
            ok = false;
            line(out, "kubectl", &format!("FAIL {error:#}"))?;
        }
    }
    match version("kubelogin", &["--version"]) {
        Ok(said) => line(out, "kubelogin", &said)?,
        Err(_) => line(
            out,
            "kubelogin",
            "not on PATH — a cluster with Entra ID sign-in needs it (https://azure.github.io/kubelogin/)",
        )?,
    }

    writeln!(out)?;
    for tab in &tabs {
        let started = Instant::now();
        let (said, fine) = match probe(tab) {
            Ok(()) => (format!("ok ({})", took(started)), true),
            Err(error) => {
                let message = format!("{error:#}");
                let word = if message.contains("Forbidden") {
                    "forbidden"
                } else {
                    "FAIL"
                };
                (format!("{word} — {message}"), false)
            }
        };
        ok &= fine;
        line(out, &tab.scope.describe(), &said)?;
    }
    Ok(ok)
}

/// One cheap read of the scope: the first pod's name, or nothing.
fn probe(tab: &Tab) -> Result<()> {
    let mut arguments = vec![
        "--context",
        tab.scope.context.as_str(),
        "--request-timeout=10s",
        "get",
        "pods",
        "--limit=1",
        "-o",
        "name",
    ];
    match &tab.scope.namespace {
        Some(namespace) => arguments.extend(["-n", namespace]),
        None => arguments.push("--all-namespaces"),
    }
    run_capped(command("kubectl", &arguments), CALL_CAP).map(drop)
}

/// One cluster `az aks list` named.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListedCluster {
    pub name: String,
    pub resource_group: String,
    pub namespaces: Vec<String>,
}

/// Whether a namespace is one AKS put there rather than the team.
#[must_use]
pub fn is_system_namespace(name: &str) -> bool {
    SYSTEM_NAMESPACES.contains(&name)
}

/// The `[[clusters]]` block for one cluster, as `config.toml` takes it.
#[must_use]
pub fn cluster_block(cluster: &ListedCluster) -> String {
    let namespaces: Vec<String> = cluster
        .namespaces
        .iter()
        .map(|namespace| format!("{namespace:?}"))
        .collect();
    format!(
        "[[clusters]]\nname = {:?}\ncontext = {:?}\nnamespaces = [{}]\n",
        cluster.name,
        cluster.name,
        namespaces.join(", ")
    )
}

/// Fetches credentials for every AKS cluster the login can see, converts
/// the kubeconfig to borrow the `az login`, and prints a `[[clusters]]`
/// block per cluster. With `write`, the blocks go to `config_path` when no
/// file is there yet.
pub fn setup(out: &mut impl Write, write: bool, config_path: &Path) -> Result<bool> {
    let raw = run_capped(command("az", &["aks", "list", "-o", "json"]), CALL_CAP)
        .context("az aks list — is there an `az login`?")?;
    let listed: Value =
        serde_json::from_str(&raw).context("az answered with something other than JSON")?;
    let mut clusters: Vec<ListedCluster> = listed
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some(ListedCluster {
                name: item["name"].as_str()?.to_owned(),
                resource_group: item["resourceGroup"].as_str()?.to_owned(),
                namespaces: Vec::new(),
            })
        })
        .collect();
    if clusters.is_empty() {
        bail!(
            "az aks list found no clusters in the current subscription; `az account set --subscription …` and try again"
        );
    }
    for cluster in &clusters {
        let started = Instant::now();
        run_capped(
            command(
                "az",
                &[
                    "aks",
                    "get-credentials",
                    "--resource-group",
                    &cluster.resource_group,
                    "--name",
                    &cluster.name,
                    "--overwrite-existing",
                ],
            ),
            CALL_CAP,
        )
        .with_context(|| format!("az aks get-credentials for {}", cluster.name))?;
        line(
            out,
            &cluster.name,
            &format!("credentials written ({})", took(started)),
        )?;
    }
    match run_capped(
        command("kubelogin", &["convert-kubeconfig", "-l", "azurecli"]),
        CALL_CAP,
    ) {
        Ok(_) => line(out, "kubelogin", "kubeconfig converted to use the az login")?,
        Err(error) => line(
            out,
            "kubelogin",
            &format!(
                "not converted — {error:#} (a cluster with Entra ID sign-in will ask for a device code until it is)"
            ),
        )?,
    }
    for cluster in &mut clusters {
        let names = run_capped(
            command(
                "kubectl",
                &[
                    "--context",
                    &cluster.name,
                    "--request-timeout=10s",
                    "get",
                    "namespaces",
                    "-o",
                    "name",
                ],
            ),
            CALL_CAP,
        );
        match names {
            Ok(raw) => {
                cluster.namespaces = raw
                    .lines()
                    .filter_map(|held| held.trim().strip_prefix("namespace/"))
                    .filter(|name| !is_system_namespace(name))
                    .map(str::to_owned)
                    .collect();
            }
            Err(error) => line(
                out,
                &cluster.name,
                &format!("namespaces not read — {error:#}; list them by hand"),
            )?,
        }
    }
    let blocks: Vec<String> = clusters.iter().map(cluster_block).collect();
    let text = blocks.join("\n");
    writeln!(out)?;
    if write {
        if config_path.exists() {
            writeln!(
                out,
                "{} exists; not touched. The blocks it would have had:\n",
                config_path.display()
            )?;
            write!(out, "{text}")?;
            return Ok(false);
        }
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to make {}", parent.display()))?;
        }
        // The AKS cadence is a top-level key: below the first [[clusters]]
        // TOML would hand it to that cluster, so it goes first.
        std::fs::write(
            config_path,
            format!(
                "# Seconds between reads of the open AKS tab; keep it above the first [[clusters]].\n# refresh = 5\n\n{text}\n[theme]\n# preset = \"terminal\"\n"
            ),
        )
        .with_context(|| format!("failed to write {}", config_path.display()))?;
        writeln!(
            out,
            "wrote {} — trim the namespaces to the ones you want tabs for",
            config_path.display()
        )?;
    } else {
        writeln!(
            out,
            "# Trim to the namespaces you want tabs for and put in {}\n# (or run `az-tui setup --write` to write it):\n",
            config_path.display()
        )?;
        write!(out, "{text}")?;
    }
    Ok(true)
}

fn command(program: &str, arguments: &[&str]) -> Command {
    let mut command = Command::new(program);
    command.args(arguments);
    command
}

/// The tool's own version line, or why it will not answer.
fn version(program: &str, arguments: &[&str]) -> Result<String> {
    let raw = run_capped(command(program, arguments), CALL_CAP)?;
    Ok(raw.lines().next().unwrap_or_default().trim().to_owned())
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

    #[test]
    fn a_cluster_block_is_config_toml_the_file_reads_back() {
        let block = cluster_block(&ListedCluster {
            name: "aks-qa".into(),
            resource_group: "rg-qa".into(),
            namespaces: vec!["dev".into(), "qa".into(), "uat".into()],
        });
        assert_eq!(
            block,
            "[[clusters]]\nname = \"aks-qa\"\ncontext = \"aks-qa\"\nnamespaces = [\"dev\", \"qa\", \"uat\"]\n"
        );
        let config = crate::config::parse(&block).unwrap();
        assert_eq!(config.tabs().len(), 3);
        assert_eq!(config.tabs()[0].label, "aks-qa/dev");
    }

    #[test]
    fn the_namespaces_aks_puts_there_are_left_out() {
        assert!(is_system_namespace("kube-system"));
        assert!(is_system_namespace("gatekeeper-system"));
        assert!(is_system_namespace("default"));
        assert!(!is_system_namespace("dev"));
        assert!(!is_system_namespace("prod"));
    }

    #[test]
    fn no_clusters_is_a_line_not_a_failure() {
        // A Key Vault-only configuration must not fail the doctor, and must
        // not go looking for kubectl either.
        let mut out = Vec::new();
        let ok = aks_checks(&mut out, &Config::default()).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(ok);
        assert!(said.contains("none in config.toml"), "{said}");
        assert!(!said.contains("kubectl"), "{said}");
    }

    #[test]
    fn setup_without_az_says_so_rather_than_panicking() {
        // Whatever this box has, `az aks list` either answers or the error
        // names the command; both are a result, never a panic.
        let dir = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let outcome = setup(&mut out, false, &dir.path().join("config.toml"));
        if let Err(error) = outcome {
            assert!(format!("{error:#}").contains("az"), "{error:#}");
        }
    }
}
