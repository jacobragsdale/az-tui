//! Resource Graph: one query for every vault and registry the login can
//! reach, rather than one list call per provider per subscription. That is
//! the whole reason Resource Graph exists, and it is why the first frame
//! costs one round trip instead of dozens.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::auth::Audience;
use super::transport::{Client, Request, api_error};
use super::{Inventory, Registry, Vault, allowed};
use crate::config::Azure;

/// The current stable Resource Graph version. The plan named `2021-03-01`,
/// which still answers; this is the version the REST reference documents
/// today and the one the `$`-prefixed option keys below are specified in.
const URL: &str = "https://management.azure.com/providers/Microsoft.ResourceGraph/resources?api-version=2024-04-01";

/// The two resource types the tabs know, with the fields each needs projected
/// under one name.
///
/// The sort names `id` as well as `name`: a skip token paging over a
/// non-unique sort column can hand back a row twice and miss another, which
/// is Resource Graph's own warning about paging.
const QUERY: &str = r"resources
| where type in~ ('microsoft.keyvault/vaults', 'microsoft.containerregistry/registries')
| project id, name, type, resourceGroup, location,
          loginServer = tostring(properties.loginServer),
          vaultUri = tostring(properties.vaultUri)
| order by name asc, id asc";

const VAULT_TYPE: &str = "microsoft.keyvault/vaults";
const REGISTRY_TYPE: &str = "microsoft.containerregistry/registries";
/// The most rows one page may carry, which is also Resource Graph's own cap.
const PAGE: usize = 1000;

/// Every vault and registry the configuration allows, in the order it names
/// them.
pub fn inventory(client: &Client, azure: &Azure) -> Result<Inventory> {
    let mut inventory = Inventory::default();
    let mut skip: Option<String> = None;
    let mut truncated = false;
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
        // A truncated answer carries no skip token, so paging cannot
        // recover the rest; the tables would simply be short and nobody
        // would know why.
        if answer["resultTruncated"] == json!("true") || answer["resultTruncated"] == json!(true) {
            truncated = true;
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
            if truncated {
                bail!(
                    "Resource Graph truncated the answer at {} vaults and {} registries; name the subscriptions in config.toml to narrow it",
                    inventory.vaults.len(),
                    inventory.registries.len()
                );
            }
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
    // `resultFormat` is set rather than left to the default: the REST
    // reference and the guidance page disagree about which default applies,
    // and `objectArray` is the shape the rows are read in below.
    let mut body = json!({
        "query": QUERY,
        "options": { "$top": PAGE, "resultFormat": "objectArray" },
    });
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

/// The one refusal worth rewording: it reads as an argument error, or as a
/// bare `403`, and is really "this login has nothing to look at".
///
/// Resource Graph answers `403` when none of the subscriptions in scope are
/// ones the caller has rights to, and some tenants answer a `400` naming
/// `NoValidSubscriptionsInQueryRequest` instead. Both mean the same thing and
/// neither is worth showing raw.
fn explain(error: anyhow::Error) -> anyhow::Error {
    let signed_out_of_everything = api_error(&error).is_some_and(|refusal| {
        refusal.status == 403 || refusal.has_code("NoValidSubscriptionsInQueryRequest")
    }) || format!("{error:#}")
        .contains("NoValidSubscriptionsInQueryRequest");
    if signed_out_of_everything {
        return error.context("the login can see no subscriptions; run `az account list`");
    }
    error
}

fn vault(row: &Value) -> Vault {
    Vault {
        id: string(&row["id"]),
        name: string(&row["name"]),
        resource_group: string(&row["resourceGroup"]),
        location: string(&row["location"]),
        uri: string(&row["vaultUri"]),
    }
}

fn registry(row: &Value) -> Registry {
    Registry {
        id: string(&row["id"]),
        name: string(&row["name"]),
        resource_group: string(&row["resourceGroup"]),
        location: string(&row["location"]),
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
    fn a_truncated_answer_is_said_out_loud_rather_than_shown_short() {
        let (client, _, _) = fake_client([Answer::json(json!({
            "data": [row("Microsoft.KeyVault/vaults", "kv-a")],
            "resultTruncated": "true",
        }))]);
        let error = format!("{:#}", inventory(&client, &Azure::default()).unwrap_err());
        assert!(error.contains("truncated"), "{error}");
        assert!(error.contains("config.toml"), "{error}");
    }

    #[test]
    fn a_forbidden_answer_means_the_login_can_reach_nothing() {
        let (client, _, _) = fake_client([Answer::status(
            403,
            r#"{"error":{"code":"Forbidden","message":"no rights"}}"#,
        )]);
        let error = format!("{:#}", inventory(&client, &Azure::default()).unwrap_err());
        assert!(error.contains("az account list"), "{error}");
    }

    #[test]
    fn the_query_sorts_on_a_unique_column_too_and_asks_for_object_rows() {
        let (client, transport, _) = fake_client([Answer::json(json!({ "data": [] }))]);
        inventory(&client, &Azure::default()).unwrap();
        let body = sent_body(&transport.sent()[0]);
        assert!(
            body["query"]
                .as_str()
                .unwrap()
                .contains("order by name asc, id asc"),
            "paging over a non-unique sort column duplicates and drops rows"
        );
        assert_eq!(body["options"]["resultFormat"], json!("objectArray"));
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
