# 04 — Container Registry: token exchange, catalog, tags, manifests

**Goal:** list every repository in a registry, one repository's attributes,
its tags newest first, and one manifest. `doctor` shows repository counts.

## Read first

- `acr_token()` and the `repositories()`, `repository()`, `tags()`,
  `manifest()` impls in `git -C ~/dev/ticket-tui show 6f73eef^:src/arm.rs`.
- Azure Container Registry REST reference: *Access Tokens*, *Refresh Tokens*,
  *Repository → Get List*, *Repository → Get Attributes*, *Tag → Get List*,
  *Manifests → Get List / Get Attributes*. The endpoints below are the
  `/acr/v1/` ones, which carry attributes; the `/v2/` ones do not.

## Build — `src/azure/acr.rs`

### Tokens — a registry mints its own

ARM does not sign data-plane calls; the registry trades an ARM token for one
of its own, in two `POST`s with `application/x-www-form-urlencoded` bodies:

1. **Exchange**, once per registry per run:
   `POST https://{login_server}/oauth2/exchange` with
   `grant_type=access_token&service={login_server}&access_token={ARM token}`
   → `{ "refresh_token": "…" }`. Cache the refresh token per `login_server`
   in the client; it lives about three hours.
2. **Access**, per scope: `POST https://{login_server}/oauth2/token` with
   `grant_type=refresh_token&service={login_server}&scope={scope}&refresh_token={…}`
   → `{ "access_token": "…" }`. Scopes: `registry:catalog:*` for the catalog,
   `repository:{name}:metadata_read` for everything about one repository.
   Cache per `(login_server, scope)`; on a `401` from a data-plane call drop
   both caches for that registry and redo both steps once.

The client gets an `Audience::Acr { login_server, scope }` alongside `Arm` and
`Vault`, so `client.call()` keeps owning the 401/429 policy and the two
`POST`s above are the only places the exchange happens.

### Models

```rust
pub struct Repository { pub registry: String, pub name: String, pub tag_count: Option<u64>, pub manifest_count: Option<u64>, pub created: Option<Timestamp>, pub updated: Option<Timestamp> }
pub struct Tag        { pub name: String, pub digest: String, pub created: Option<Timestamp>, pub updated: Option<Timestamp> }
pub struct Manifest   { pub digest: String, pub size: Option<u64>, pub architecture: Option<String>, pub os: Option<String>, pub created: Option<Timestamp>, pub tags: Vec<String> }
```

### Calls

**Catalog.** `GET https://{login}/acr/v1/_catalog?n=100` → `{ "repositories": ["a/b", …] }`.
Page with `&last={last name of the previous page}` until a page is shorter
than `n`. A catalog is names only; the counts arrive from the next call.

**Attributes.** `GET /acr/v1/{name}` →
`{ "registry", "imageName", "createdTime", "lastUpdateTime", "manifestCount", "tagCount", "changeableAttributes": {…} }`.
This is one call per repository, so step 05 runs it as a low-priority fill
after the catalog has been shown.

**Tags.** `GET /acr/v1/{name}/_tags?n=100&orderby=timedesc` →
`{ "registry", "imageName", "tags": [ { "name", "digest", "createdTime", "lastUpdateTime", "signed", "changeableAttributes": {…} } ] }`.
Page with `&last={last tag name}`. Keep the service's order (newest first).

**Manifests.** `GET /acr/v1/{name}/_manifests/{digest}` →
`{ "registry", "imageName", "manifest": { "digest", "imageSize", "createdTime", "lastUpdateTime", "architecture", "os", "mediaType", "tags": [...] } }`.
Note the attributes sit under `manifest`, unlike the list call
`GET /acr/v1/{name}/_manifests?n=100&orderby=timedesc` whose items sit in
`manifests[]` directly. A multi-arch index has no `architecture`; leave it
`None` and let the UI print `index`.

```rust
pub fn repositories(client: &Client, registry: &Registry) -> Result<Vec<String>>
pub fn attributes(client: &Client, registry: &Registry, name: &str) -> Result<Repository>
pub fn tags(client: &Client, registry: &Registry, name: &str) -> Result<Vec<Tag>>
pub fn manifest(client: &Client, registry: &Registry, name: &str, digest: &str) -> Result<Manifest>
```

### References the copy keys build

Pure functions in this module, tested:

- `pull_reference(login, repo, tag)` → `acrprod.azurecr.io/payments-api:1.42.0`
- `digest_reference(login, repo, digest)` → `acrprod.azurecr.io/payments-api@sha256:…`
- `short_digest(digest)` → `sha256:ab12ef01` (the prefix plus eight hex chars),
  for cells; the details pane prints the full digest.
- `human_size(bytes)` → `84.2 MB` (SI, one decimal, as `docker images` prints).

### Errors worth naming

ACR errors are `{ "errors": [ { "code": "…", "message": "…" } ] }`; step 02's
extractor already reads that shape. A `401` on the exchange itself (not a
data-plane call) means the login has no role on the registry: say "acrprod:
no permission (needs AcrPull or Reader)" and move on to the next registry.

### `doctor`

One line per registry: `acrprod   183 repositories (0.9 s)` or its refusal.
No attributes calls in `doctor` — one call per repository is a listing's job.

## Tests (fake transport)

- The exchange runs once per registry however many scopes are asked; a `401`
  on a data-plane call redoes it once and then gives up.
- Catalog paging with `last`; a page shorter than `n` ends it.
- Tags parsed in the order given; a manifest under `manifest`; a list item
  without `architecture` reads `None`.
- The reference builders and `human_size` on 0, 999, 84_200_000, 3 GB.
- The recorded requests carry the registry's own bearer, not the ARM one.

## Done when

- Tests pass without network.
- `az-tui doctor` on a signed-in machine lists a count per registry.
- `grep -rn "PUT\|DELETE\|PATCH" src/` finds nothing.

## Not in this step

No UI. The Registries tab is step 08.
