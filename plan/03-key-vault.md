# 03 — Key Vault: secrets, versions, one value

**Goal:** list every secret in a vault with its attributes, list one secret's
versions, and read one value on demand. `doctor` shows the per-vault counts.

## Read first

- The old `items()` and `secret_value()` in
  `git -C ~/dev/ticket-tui show 6f73eef^:src/arm.rs` (search for
  `VAULT_API_VERSION`), and `vault_item()` under it — how `id` is split into
  a name and how the unix stamps are read.
- Azure REST reference, Key Vault secrets, API version 7.4: *Get Secrets*,
  *Get Secret Versions*, *Get Secret*. The shapes below are from it.

## Build — `src/azure/vault.rs`

All calls carry the **Vault** audience token and `api-version=7.4`. The base
is the vault's `uri` from the inventory, which already ends in `/`.

### Models (in `azure/mod.rs`)

```rust
pub struct SecretRow {
    pub vault: String,            // the vault's name, the row's first column
    pub name: String,             // last path segment of `id`
    pub enabled: bool,
    pub created: Option<Timestamp>,
    pub updated: Option<Timestamp>,
    pub expires: Option<Timestamp>,     // attributes.exp
    pub not_before: Option<Timestamp>,  // attributes.nbf
    pub content_type: Option<String>,
    pub tags: Vec<(String, String)>,    // sorted by key
    pub managed: bool,                  // a certificate's backing secret
}
pub struct SecretVersion { pub version: String, pub enabled: bool, pub created: Option<Timestamp>, pub updated: Option<Timestamp>, pub expires: Option<Timestamp> }
```

`Timestamp` is lifted from ticket-tui's `src/timestamp.rs`, trimmed to:
parse RFC 3339, from unix seconds, format `YYYY-MM-DD`, and `relative_age(now)`
giving `3d`, `40m`, `6mo`, `2y`. Key Vault's attribute stamps are **unix
seconds as integers**; ACR's are RFC 3339 strings. Both go through it.

### Calls

**List.** `GET {uri}secrets?api-version=7.4&maxresults=25`. The answer is
`{ "value": [SecretItem], "nextLink": "<full URL or null>" }`. Follow
`nextLink` as-is (it already carries the api-version and a skip token) until
it is absent or null. `maxresults` **cannot exceed 25** — the service refuses
more — so a vault with 500 secrets is 20 requests; that is why step 05 lists
vaults on a worker and step 05's cache makes the next start free.

```json
{ "id": "https://kv-prod.vault.azure.net/secrets/db-password",
  "contentType": "text/plain",
  "attributes": { "enabled": true, "created": 1709251200, "updated": 1725753600, "exp": 1767225600, "nbf": null, "recoveryLevel": "Recoverable+Purgeable" },
  "tags": { "env": "prod" },
  "managed": null }
```

**Versions.** `GET {uri}secrets/{name}/versions?api-version=7.4&maxresults=25`,
same shape; each `id` ends in `/secrets/{name}/{version}`. Order newest first
by `created`.

**Value.** `GET {uri}secrets/{name}?api-version=7.4` for the current version,
or `GET {uri}secrets/{name}/{version}?api-version=7.4` for one. The answer is
`{ "value": "…", "id": "…/{name}/{version}", "attributes": {…}, "contentType": … }`.
Return `Secret` and the version it came from. This is the **only** function
in the crate that produces a `Secret`; say so in its doc comment.

```rust
pub fn secrets(client: &Client, vault: &Vault) -> Result<Vec<SecretRow>>
pub fn versions(client: &Client, vault: &Vault, name: &str) -> Result<Vec<SecretVersion>>
pub fn value(client: &Client, vault: &Vault, name: &str, version: Option<&str>) -> Result<(Secret, String)>
```

### Errors worth naming

The message the user sees is the body's `error.message`, which Key Vault
writes well. Two are common enough to shorten in `doctor` and in the footer:

- `403` with `innererror.code == "ForbiddenByFirewall"` → "kv-prod: blocked by
  the vault firewall (your IP is not allowed)".
- `403` otherwise → "kv-prod: no permission to list secrets (needs the Key
  Vault Secrets User role or a `list` access policy)".

A vault that fails returns `Err`; the caller (the worker) turns that into a
per-vault status and moves on to the next vault. Never let one vault's `Err`
abort the others.

### `doctor`

Append one line per vault: `kv-prod   412 secrets (3.1 s)` or
`kv-prod   403 no permission to list secrets`. No values are read by
`doctor`, ever.

## Tests (fake transport)

- Paging: a first page with `nextLink`, a second without; the second request's
  URL is the `nextLink` verbatim.
- One item with every attribute present and one with only `enabled`; `name`
  from the `id`; tags sorted; `managed: null` reads as false.
- Versions ordered newest first.
- `value()` returns the `Secret` and the version from the `id`; the fake's
  recorded requests show the vault-audience bearer, and the test asserts the
  `Secret`'s `Debug` output does not contain the value.
- A `403` firewall body produces the firewall message; a plain `403` the
  permission message.

## Done when

- Tests pass without network.
- `az-tui doctor` on a signed-in machine prints a count per vault; with one
  vault removed from your permissions it prints that vault's refusal and the
  others' counts, exit 1.
- `grep -rn "expose()" src/` shows the function's definition and no callers
  yet (they arrive in 07 and 10).

## Not in this step

Keys and certificates: not listed at all in v1. They share this listing shape
(`GET keys`, `GET certificates`, 25 a page) and can be added as a `kind`
column later; note that in a `// ponytail:` comment on `SecretRow`.
