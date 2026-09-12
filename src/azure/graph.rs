//! Resource Graph: one query for every vault and registry the login can
//! reach, rather than one list call per provider per subscription. That is
//! the whole reason Resource Graph exists, and it is why the first frame
//! costs one round trip instead of dozens.

use anyhow::Result;
use serde_json::{Value, json};

use super::auth::Audience;
use super::transport::{Client, Request};
use super::{Inventory, Registry, Vault, allowed};
use crate::config::Azure;

const URL: &str = "https://management.azure.com/providers/Microsoft.ResourceGraph/resources?api-version=2021-03-01";

/// The two resource types the tabs know, with the fields each needs projected
/// under one name. `sku` sits in a different place for the two providers and
/// `coalesce` picks whichever is there.
const QUERY: &str = r"resources
| where type in~ ('microsoft.keyvault/vaults', 'microsoft.containerregistry/registries')
| project id, name, type, subscriptionId, resourceGroup, location,
          sku = tostring(coalesce(sku.name, properties.sku.name)),
          loginServer = tostring(properties.loginServer),
          vaultUri = tostring(properties.vaultUri)
| order by name asc";

const VAULT_TYPE: &str = "microsoft.keyvault/vaults";
const REGISTRY_TYPE: &str = "microsoft.containerregistry/registries";
/// The most rows one page may carry, which is also Resource Graph's own cap.
const PAGE: usize = 1000;

/// Every vault and registry the configuration allows, in the order it names
/// them.
pub fn inventory(client: &Client, azure: &Azure) -> Result<Inventory> {
    let mut inventory = Inventory::default();
    let mut skip: Option<String> = None;
    loop {
        let answer = client
            .call(
                &Audience::Arm,
                Request::post_json(URL, body(azure, skip.as_deref())),
            )
            .map_err(explain)?;
        for row in answer["data"].as_array().into_iter().flatten() {
            match string(&row["type"]).to_ascii_lowercase().as_str() {
                VAULT_TYPE => inventory.vaults.push(vault(row)),
                REGISTRY_TYPE => inventory.registries.push(registry(row)),
                _ => {}
            }
        }
        let next = text(&answer["$skipToken"]);
        // A repeated token would page for ever; absent or unchanged is the
        // end of the answer either way.
        if next.is_none() || next == skip {
            inventory.vaults =
                allowed(inventory.vaults, &azure.vaults, |vault| vault.name.as_str());
            inventory.registries = allowed(inventory.registries, &azure.registries, |registry| {
                registry.name.as_str()
            });
            return Ok(inventory);
        }
        skip = next;
    }
}

/// The query body. `subscriptions` is left out entirely when nothing named
/// any: the query then runs over every subscription the login can reach,
/// which is what an empty list means everywhere else in the configuration
/// too. An empty *array* would mean the opposite, and answer nothing.
fn body(azure: &Azure, skip: Option<&str>) -> Value {
    let mut body = json!({ "query": QUERY, "options": { "$top": PAGE } });
    let named: Vec<&String> = azure
        .subscriptions
        .iter()
        .filter(|held| !held.trim().is_empty())
        .collect();
    if !named.is_empty() {
        body["subscriptions"] = json!(named);
    }
    if let Some(token) = skip {
        body["options"]["$skipToken"] = json!(token);
    }
    body
}

/// The one refusal worth rewording: it reads as an argument error and is
/// really "this login has nothing".
fn explain(error: anyhow::Error) -> anyhow::Error {
    if format!("{error:#}").contains("NoValidSubscriptionsInQueryRequest") {
        return error.context("the login can see no subscriptions; run `az account list`");
    }
    error
}

fn vault(row: &Value) -> Vault {
    Vault {
        id: string(&row["id"]),
        name: string(&row["name"]),
        subscription_id: string(&row["subscriptionId"]),
        resource_group: string(&row["resourceGroup"]),
        location: string(&row["location"]),
        sku: string(&row["sku"]),
        uri: string(&row["vaultUri"]),
    }
}

fn registry(row: &Value) -> Registry {
    Registry {
        id: string(&row["id"]),
        name: string(&row["name"]),
        subscription_id: string(&row["subscriptionId"]),
        resource_group: string(&row["resourceGroup"]),
        location: string(&row["location"]),
        sku: string(&row["sku"]),
        login_server: string(&row["loginServer"]),
    }
}

/// One string that was actually there: trimmed, and blank counts as absent.
pub(crate) fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|held| !held.is_empty())
        .map(str::to_owned)
}

pub(crate) fn string(value: &Value) -> String {
    text(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::transport::Body;
    use crate::azure::transport::fake::{Answer, client as fake_client};

    fn row(kind: &str, name: &str) -> Value {
        json!({
            "id": format!("/subscriptions/sub-1/resourceGroups/rg/providers/{kind}/{name}"),
            "name": name,
            "type": kind.to_ascii_lowercase(),
            "subscriptionId": "sub-1",
            "resourceGroup": "rg",
            "location": "eastus",
            "sku": "standard",
            "loginServer": if kind.to_ascii_lowercase().contains("containerregistry") { format!("{name}.azurecr.io") } else { String::new() },
            "vaultUri": if kind.to_ascii_lowercase().contains("keyvault") { format!("https://{name}.vault.azure.net/") } else { String::new() },
        })
    }

    fn sent_body(request: &Request) -> Value {
        match &request.body {
            Body::Json(body) => body.clone(),
            other => panic!("expected a JSON body, got {other:?}"),
        }
    }

    #[test]
    fn two_pages_are_joined_and_the_skip_token_is_followed() {
        let (client, transport, _) = fake_client([
            Answer::json(json!({
                "data": [row("Microsoft.KeyVault/vaults", "kv-a")],
                "$skipToken": "page-2",
            })),
            Answer::json(json!({
                "data": [row("Microsoft.ContainerRegistry/registries", "acrb")],
            })),
        ]);
        let inventory = inventory(&client, &Azure::default()).unwrap();
        assert_eq!(inventory.vaults.len(), 1);
        assert_eq!(inventory.vaults[0].uri, "https://kv-a.vault.azure.net/");
        assert_eq!(inventory.vaults[0].subscription_id, "sub-1");
        assert_eq!(inventory.registries.len(), 1);
        assert_eq!(inventory.registries[0].login_server, "acrb.azurecr.io");

        let sent = transport.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent_body(&sent[1])["options"]["$skipToken"],
            json!("page-2"),
            "the second page carries the token the first answered with"
        );
    }

    #[test]
    fn a_repeated_skip_token_ends_the_paging_rather_than_looping() {
        let (client, transport, _) = fake_client([
            Answer::json(json!({ "data": [], "$skipToken": "same" })),
            Answer::json(json!({ "data": [], "$skipToken": "same" })),
        ]);
        inventory(&client, &Azure::default()).unwrap();
        assert_eq!(transport.sent().len(), 2);
    }

    #[test]
    fn subscriptions_are_named_only_when_the_configuration_names_them() {
        let (client, transport, _) = fake_client([Answer::json(json!({ "data": [] }))]);
        inventory(&client, &Azure::default()).unwrap();
        let body = sent_body(&transport.sent()[0]);
        assert!(
            body.get("subscriptions").is_none(),
            "an empty list means every subscription, which is said by saying nothing: {body}"
        );
        assert_eq!(body["options"]["$top"], json!(1000));

        let azure = Azure {
            subscriptions: vec!["sub-1".into(), "  ".into()],
            ..Azure::default()
        };
        let (client, transport, _) = fake_client([Answer::json(json!({ "data": [] }))]);
        inventory(&client, &azure).unwrap();
        assert_eq!(
            sent_body(&transport.sent()[0])["subscriptions"],
            json!(["sub-1"]),
            "a blank entry is not a subscription"
        );
    }

    #[test]
    fn an_allowlist_fixes_the_order_of_both_kinds() {
        let (client, _, _) = fake_client([Answer::json(json!({
            "data": [
                row("Microsoft.KeyVault/vaults", "kv-dev"),
                row("Microsoft.KeyVault/vaults", "kv-prod"),
                row("Microsoft.ContainerRegistry/registries", "acrdev"),
                row("Microsoft.ContainerRegistry/registries", "acrprod"),
            ],
        }))]);
        let azure = Azure {
            vaults: vec!["KV-PROD".into(), "kv-dev".into()],
            registries: vec!["acrprod".into()],
            ..Azure::default()
        };
        let inventory = inventory(&client, &azure).unwrap();
        let names: Vec<&str> = inventory.vaults.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["kv-prod", "kv-dev"]);
        assert_eq!(inventory.registries.len(), 1);
        assert_eq!(inventory.registries[0].name, "acrprod");
    }

    #[test]
    fn a_login_with_no_subscriptions_is_told_what_to_run() {
        let (client, _, _) = fake_client([Answer::status(
            400,
            r#"{"error":{"code":"BadRequest","message":"NoValidSubscriptionsInQueryRequest: no subscriptions"}}"#,
        )]);
        let error = inventory(&client, &Azure::default()).unwrap_err();
        assert!(
            format!("{error:#}").contains("az account list"),
            "{error:#}"
        );
    }

    #[test]
    fn a_row_of_an_unknown_type_is_ignored_rather_than_guessed_at() {
        let (client, _, _) = fake_client([Answer::json(json!({
            "data": [row("Microsoft.Compute/virtualMachines", "vm-1")],
        }))]);
        let inventory = inventory(&client, &Azure::default()).unwrap();
        assert!(inventory.vaults.is_empty() && inventory.registries.is_empty());
    }
}
