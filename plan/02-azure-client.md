# 02 — Azure client: tokens, transport, inventory, `doctor`

**Goal:** a tested HTTPS client that borrows the Azure CLI's login, survives
throttling and expired tokens, and lists every Key Vault and Container Registry
the login can see. `az-tui doctor` prints what it found.

## Read first

- The torn-out client: `git -C ~/dev/ticket-tui show 6f73eef^:src/arm.rs`.
  Its first 200 lines are the constants, `ArmConfig::resolve_with`, the `az`
  shell-out and token minting; lines ~390–700 are `Transport`, `Https`,
  `ArmClient::request`, `throttled_for`, `failure_message`, `retry_after`;
  the `ArmSource for ArmClient` impl at the end has the Resource Graph paging
  loop. Its `tests` module has `FakeTransport` with `answering(&[Answer])`,
  `sent()`, `urls()`, `mints()` — the shape of every test in this step.
- ticket-tui `src/azure.rs` `authorization_header()` — the `az account
  get-access-token` call as it runs today, and `base64()` under it.

## Build

### `src/azure/mod.rs` — models

```rust
pub struct Vault    { pub id: String, pub name: String, pub subscription_id: String, pub resource_group: String, pub location: String, pub sku: String, pub uri: String }      // uri: "https://kv-prod.vault.azure.net/"
pub struct Registry { pub id: String, pub name: String, pub subscription_id: String, pub resource_group: String, pub location: String, pub sku: String, pub login_server: String } // "acrprod.azurecr.io"
#[derive(Default)] pub struct Inventory { pub vaults: Vec<Vault>, pub registries: Vec<Registry> }

/// A secret's value. Prints as `[redacted]` from Debug and Display; `expose()`
/// is the one accessor and is called from exactly two places in the crate.
pub struct Secret(String);
```

Derive `Clone, Debug, Eq, PartialEq, Serialize, Deserialize` on the models
(the cache stores them). `Secret` derives nothing but `Clone` and `Eq`; write
`Debug` and `Display` by hand to print `[redacted]`. Add a test that
`format!("{:?}", Secret::new("x"))` contains no `x`.

`portal_url(id: &str) -> String` = `https://portal.azure.com/#@/resource{id}`.

### `src/azure/auth.rs` — tokens

```rust
pub enum Audience { Arm, Vault }
impl Audience { fn resource(self) -> &'static str }   // "https://management.azure.com/" (trailing slash matters), "https://vault.azure.net"

pub trait TokenSource: Send {
    fn token(&self, audience: Audience) -> Result<String>;
}
pub struct AzCli;      // runs `az account get-access-token --resource <r> --query accessToken -o tsv`
pub fn az(args: &[&str]) -> Result<String>   // stdin null, stdout trimmed, non-zero exit or empty output -> Err that ends with "run `az login`"
pub fn subscriptions() -> Result<Vec<String>> // `az account list --query "[?state=='Enabled'].id" -o tsv`, one per line
```

A test double `FixedTokens` in `#[cfg(test)]` answers a fixed string and
counts calls.

### `src/azure/transport.rs` — HTTPS

```rust
pub struct Request { pub method: Method /* Get | Post */, pub url: String, pub bearer: Option<String>, pub body: Body /* None | Json(Value) | Form(Vec<(String,String)>) */ }
pub struct Response { pub status: u16, pub headers: Vec<(String, String)>, pub body: String }
pub trait Transport: Send { fn send(&self, request: Request) -> Result<Response>; }
pub struct Https;   // ureq::Agent with 30 s timeouts, body capped at 32 MiB, `http_status_as_error(false)` so 4xx/5xx come back as Response
```

`Client<T: TokenSource, H: Transport>` wraps both and owns the one policy
every call shares, in `client.call(audience, request) -> Result<Value>`:

1. Sign with the cached token for the audience, minting on first use.
2. `401` → mint once more, retry once. A second `401` is "signed out: run
   `az login`", a distinct error type the worker can recognise.
3. `429` or `503` → read `Retry-After` (seconds, or an HTTP date), clamp to
   1 s ≤ wait ≤ 3600 s, default 30 s, sleep, retry once. Record the wait in
   `client.last_throttle()` so the status bar can say it.
4. Any other non-2xx → error carrying the status and the message from the
   body's `error.message` (ARM and Key Vault both use `{"error":{"code","message"}}`;
   ACR uses `{"errors":[{"code","message"}]}`), else the first 200 characters
   of the body.
5. Parse JSON; an unparsable 2xx body is an error naming the URL.

The `Retry-After` parser and the error-message extraction are pure functions
with their own tests.

### `src/azure/graph.rs` — the inventory

One `POST https://management.azure.com/providers/Microsoft.ResourceGraph/resources?api-version=2021-03-01`
with the ARM audience and this body:

```json
{ "subscriptions": ["…"], "query": "<below>", "options": { "$top": 1000, "$skipToken": "…" } }
```

Omit `subscriptions` entirely when the config names none: the query then runs
over every subscription the login can access. The query:

```
resources
| where type in~ ('microsoft.keyvault/vaults', 'microsoft.containerregistry/registries')
| project id, name, type, subscriptionId, resourceGroup, location,
          sku = tostring(coalesce(sku.name, properties.sku.name)),
          loginServer = tostring(properties.loginServer),
          vaultUri = tostring(properties.vaultUri)
| order by name asc
```

Page while the answer carries `$skipToken`. Rows with `type` of the vault
kind become `Vault`, the other kind `Registry`. Then apply the allowlists:
an empty `vaults` list keeps every vault; a non-empty one keeps only the named
ones, in the named order, matched case-insensitively (lift `allowed()` from
the old `arm.rs`). A name in the allowlist that was not found is reported by
`doctor`, not an error.

If Resource Graph answers `NoValidSubscriptionsInQueryRequest` the error says
"the login can see no subscriptions; run `az account list`".

```rust
pub fn inventory(client: &Client, cfg: &crate::config::Azure) -> Result<Inventory>
```

### `az-tui doctor`

Runs on the calling thread (no TUI) and prints, in order, stopping at the
first failure with the error and a one-line fix:

```
az            2.90.0                      (az --version, first line)
account       jacob@example.com · tenant …  (az account show --query "[user.name, tenantId]")
subscriptions 3 enabled                   (or the list from config.toml)
arm token     ok (412 ms)
vault token   ok (398 ms)
inventory     3 vaults, 3 registries (1.2 s)
  kv-dev      eastus   rg-dev   https://kv-dev.vault.azure.net/
  …
  acrdev      eastus   rg-dev   acrdev.azurecr.io
not found     kv-staging (named in config.toml)
```

Exit 0 when everything answered, 1 otherwise. Every line is one function that
returns `Result<String>`, so steps 03 and 04 can append their own lines.

## Tests (fake transport, fixed tokens)

- A `401` mints a new token and retries once; two `401`s are `SignedOut`.
- `429` with `Retry-After: 2` waits (inject the sleep as a closure) and
  retries; `Retry-After` as an HTTP date; missing header → 30 s; `7200` → 3600.
- Error message extraction for the ARM shape, the ACR shape and a plain body.
- Resource Graph paging: two pages joined; the `subscriptions` key is absent
  from the sent body when the config names none and present when it does.
- Allowlist: order, case, and a name that is not there.
- `Secret`'s `Debug`/`Display` never contain the value.

## Done when

- The tests above pass without network.
- With `az login` done on some machine: `az-tui doctor` prints the table
  above with real names. Without a login: it prints the `az` line and then
  `account   not signed in — run az login`, exit 1, in under a second.
- `grep -rn "PUT\|DELETE\|PATCH" src/` finds nothing.

## Not in this step

No Key Vault or ACR data-plane calls (03, 04). No thread (05).
