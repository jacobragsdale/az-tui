# 05 — The worker thread and the cache

**Goal:** one background thread that reads everything, reports each piece as
it lands, and never blocks the UI; and one JSON file that lets the next start
paint before any network call.

## Read first

- `git -C ~/dev/ticket-tui show 6f73eef^:src/arm_watch.rs` — the request /
  event enums, the edge-triggered focus reads, `Stop`, and how a newer request
  supersedes a queued one.
- ticket-tui `src/search.rs` `search_worker()` — draining a channel to the
  newest command before working.
- ticket-tui `src/db.rs` — the atomic save (`tempfile::NamedTempFile` +
  `persist`) used for the session; the cache is written the same way.

## Build

### `src/worker.rs`

```rust
pub enum Request {
    Refresh,                                                     // inventory, then every vault, then every registry
    Versions { vault: String, name: String },
    Value    { vault: String, name: String, version: Option<String> },
    Tags     { registry: String, repo: String },
    Manifest { registry: String, repo: String, digest: String },
    Stop,
}

pub enum Event {
    Inventory(Result<Inventory, String>),
    Secrets      { vault: String, result: Result<Vec<SecretRow>, String> },
    Repositories { registry: String, result: Result<Vec<Repository>, String> },  // names only, counts None
    Repository   { registry: String, repository: Repository },                    // one attributes fill
    Versions     { vault: String, name: String, result: Result<Vec<SecretVersion>, String> },
    Value        { vault: String, name: String, result: Result<(Secret, String), String> },
    Tags         { registry: String, repo: String, result: Result<Vec<Tag>, String> },
    Manifest     { registry: String, repo: String, digest: String, result: Result<Manifest, String> },
    Progress(String),        // "reading kv-prod (3/3)…", "acrprod asked us to wait 30 s"
    Idle,                    // the refresh has finished, success or not
}

pub struct Worker { requests: Sender<Request>, events: Receiver<Event>, handle: Option<JoinHandle<()>> }
impl Worker {
    pub fn start(cfg: config::Azure, client: Client) -> Worker;
    pub fn send(&self, r: Request);
    pub fn try_recv(&self) -> Option<Event>;   // non-blocking; the run loop drains it every turn
}
impl Drop for Worker { /* send Stop, join */ }
```

The thread is `Worker::start`'s; `Client` is built on the calling thread but
mints its first tokens on the worker (so a missing `az` never delays the
first frame). The loop:

1. `recv()` one request, then drain the channel; collapse duplicates
   (two `Refresh`es are one; a `Value` for the same secret twice is one).
   A `Stop` anywhere in the drained batch returns.
2. Serve `Value`, `Versions`, `Tags`, `Manifest` first — someone is waiting
   on those — then `Refresh`.
3. `Refresh`: send `Inventory`; then for each vault, in config order, send
   `Secrets` as each vault finishes; then for each registry send
   `Repositories` from the catalog; then walk every repository's attributes
   sending one `Repository` each. **Between every call** `try_recv` the
   request channel: a `Value` or `Tags` that arrived mid-refresh is served
   before the next attributes call, so the fill never makes a copy wait.
   Send `Idle` at the end.
4. A per-vault or per-registry failure is that item's `Err(String)`; the loop
   goes on. A `SignedOut` error from the client stops the refresh and is sent
   as `Inventory(Err("not signed in — run az login"))`.
5. Nothing here holds a `Secret` after sending it. `Value` is the only event
   carrying one; the worker does not cache values.

Errors are `String`s in the events because they cross a thread and the UI
only prints them. Format them with `{error:#}` at the source so the chain of
causes survives.

### `src/cache.rs`

One file, `data_dir()/cache.json`, written **after** a refresh finishes and
read **before** the first frame:

```json
{ "version": 1, "read_at": "2026-09-11T20:14:00Z",
  "vaults": [Vault], "registries": [Registry],
  "secrets": [SecretRow], "repositories": [Repository] }
```

- Never versions, tags, manifests or values. Only what the two flat tables
  show on open.
- Written atomically (`NamedTempFile` in the same directory, then `persist`)
  with mode `0600` on unix — secret *names* are mildly sensitive.
- `version` other than 1, or a file that will not parse, is ignored (start
  empty, log nothing) and overwritten by the next refresh.
- `--no-cache` neither reads nor writes it; `--cache PATH` moves it.
- A vault that failed in the refresh keeps its rows from the previous cache
  in memory and in the next write, marked stale: the status bar says
  "kv-prod: <error> — showing rows from 2 h ago". A vault that answered
  replaces its rows entirely (deleted secrets disappear).

```rust
pub struct Snapshot { pub read_at: Timestamp, pub inventory: Inventory, pub secrets: Vec<SecretRow>, pub repositories: Vec<Repository> }
pub fn load(path: &Path) -> Option<Snapshot>
pub fn save(path: &Path, snapshot: &Snapshot) -> Result<()>
```

### `src/store.rs` — the in-memory model both screens read

```rust
pub struct Store {
    pub inventory: Inventory,
    pub secrets: Vec<SecretRow>,                     // every vault, concatenated in config order
    pub repositories: Vec<Repository>,               // every registry
    pub versions: HashMap<(String, String), Vec<SecretVersion>>,
    pub tags: HashMap<(String, String), Vec<Tag>>,
    pub manifests: HashMap<(String, String, String), Manifest>,
    pub problems: Vec<(String, String)>,             // (vault or registry, message), cleared per refresh
    pub read_at: Option<Timestamp>,                  // when the newest complete refresh finished
    pub refreshing: bool,
    pub progress: Option<String>,
}
impl Store {
    pub fn apply(&mut self, event: Event) -> Applied   // what changed: Secrets, Repositories, Detail, Status — the screens use it to keep the cursor on the same row
    pub fn snapshot(&self) -> cache::Snapshot
}
```

`apply` on `Event::Value` does **not** store the secret; it returns it in
`Applied::Value(Secret)` for the screen, which is the one place that holds it
(step 07). `Secrets { vault, Ok(rows) }` replaces that vault's rows in place,
keeping other vaults' order.

### `run.rs`

- Read the cache before `ratatui::init()`; build the `Store` from it.
- Start the worker; send `Refresh` at once, and again every `refresh` seconds
  (`config.azure.refresh`, `--refresh`, default 300; 0 disables the timer).
- Every loop turn: drain `worker.try_recv()` into `store.apply()`; redraw if
  anything applied. `event::poll` timeout: 100 ms while `store.refreshing`
  (the spinner turns), otherwise until the next timer tick.
- On `Idle`: `cache::save` unless `--no-cache`; a save failure is a status,
  not an exit.
- Until step 06 lands, the placeholder screen prints the counts: `3 vaults ·
  412 secrets · 3 registries · 183 repositories · read 12 s ago` and the
  first problem, if any. That is enough to see the worker work.

## Tests

- Worker with the fake transport: a `Refresh` produces `Inventory`, one
  `Secrets` per vault, one `Repositories` per registry, N `Repository`
  fills, then `Idle`, in that order; a `Value` sent mid-refresh is answered
  before the remaining fills.
- Two queued `Refresh`es run once. `Stop` mid-batch returns.
- A vault that answers `403` yields `Secrets { Err }` and the next vault
  still lands.
- Cache round-trip; a `version: 2` file loads as `None`; a missing file loads
  as `None`; the written file's mode is `0600` on unix.
- `Store::apply`: a vault's rows replaced in place; a failed vault keeps its
  old rows and gains a problem; `Applied::Value` carries the secret and the
  store holds none afterwards.

## Done when

- Tests pass without network.
- Signed in: start, see counts fill in within seconds, quit, start again —
  the counts are there on the first frame and `read 12 s ago` says when.
- Not signed in: the first frame says `not signed in — run az login` and
  still shows whatever the cache held.
- `grep -cw Secret src/cache.rs` is 0 (the cache module cannot even name the type; `SecretRow` is a different word).
