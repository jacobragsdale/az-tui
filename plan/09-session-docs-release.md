# 09 — Session, docs, release

**Goal:** the app reopens the way it was left, the README tells a new user
everything, the live checklist is written down for the day a subscription is
reachable, and `cargo install` works from the repo.

## Read first

- ticket-tui `src/session.rs` — `Session`, `TabSession`, `SessionColumn`,
  the "unknown kind is dropped, not fatal" deserialisers, and the atomic
  save; ticket-tui `README.md` and the top of `DESIGN.md` for the tone.
- The old "ARM tabs — integration checklist" in
  `git -C ~/dev/ticket-tui show 6f73eef^:HANDOFF.md` (search for it): the
  order it walks things in is right.

## Build

### `src/session.rs`

`data_dir()/session.json`, written on quit and whenever a sort, a column
width or the tab changes (debounced 500 ms, like ticket-tui's settle):

```json
{ "version": 1, "tab": "secrets",
  "secrets":    { "sort": ["name", "asc"], "columns": [{"key":"vault","width":12,"visible":true}, …] },
  "registries": { "sort": ["updated", "desc"], "columns": [...] } }
```

Never the query, the cursor, a revealed value or a secret name. A file of
another version or one that will not parse is ignored. Restore before the
first frame.

### Docs

- `README.md` — replace the planning README with the real one: what it is,
  the screenshot-as-text, **Run it** (`az login`, `cargo install --git
  https://github.com/jacobragsdale/az-tui`, or `cargo run --release`), the
  key table from the overview, **Where things live** (config, cache, session
  paths; what the cache holds and does not), the `config.toml` reference
  with every key, the OSC 52 / tmux note, and the rule list from the overview
  ("what it never does"). Keep the plan directory; link it as "how it was
  built".
- `config.example.toml` — final, every key commented.
- `plan/CHECKLIST.md` — the live walk-through below.

### The live checklist (write it into `plan/CHECKLIST.md`)

For the day a real subscription is reachable, in order, each a line to tick:

1. `az login`; `az account list -o table` shows the subscriptions you expect.
2. `az-tui doctor`: every vault and registry named in `config.toml` is found;
   counts per vault and registry; the two token lines under a second each.
3. `az-tui --no-cache`: the first frame within a second, the vaults landing
   one by one, `Idle` → the status bar says `N vaults · M secrets · just now`.
4. Quit, start without `--no-cache`: the same rows on the first frame,
   `read 30 s ago`.
5. `/` a secret name that exists in two vaults: both rows, adjacent.
6. `v` on one you may read: shown, counts down, gone at 0. `y`: pasted
   elsewhere it is byte-identical (check a value with a trailing newline and
   one with `=` padding characters).
7. `v` on one you may not read: the refusal under Value; the table intact.
8. `grep -c <the value you revealed> ~/.local/share/az-tui/*` is 0 for both
   files.
9. `2`, a repository, `Enter`, `y`, `docker pull <paste>` succeeds.
10. Over SSH into that machine from a terminal that speaks OSC 52: `y` still
    lands on the local clipboard. Inside tmux with `set-clipboard on`: same.
11. Wrong on purpose: `--subscription 00000000-0000-0000-0000-000000000000`
    says so in the status bar and shows the cache; a vault name in
    `config.toml` that does not exist is listed by `doctor` as not found.
12. Throttle: hold `r` for a while; when Azure answers 429 the status bar says
    `kv-prod asked us to wait 30 s` and the app stays responsive.

### Release

- `Cargo.toml` `version = "0.1.0"` stays; tag `v0.1.0` after the checklist is
  ticked, not before.
- CI already builds release on both OSes; add a `cargo install --path .`
  smoke to the workflow so the install path is proven.
- A `CHANGELOG.md` with one entry: what 0.1.0 does.

## Tests

- Session round-trip; an unknown column key is dropped; a `version: 2` file
  is ignored; the file never contains the query (a test sets a query, saves,
  and greps the JSON).

## Done when

- Start, change the sort and the tab, quit, start: both restored.
- `cargo install --git https://github.com/jacobragsdale/az-tui` produces a
  working `az-tui` on a clean checkout.
- README reads top to bottom without a reference to the plan being needed.
- The gate is green; CI is green.
