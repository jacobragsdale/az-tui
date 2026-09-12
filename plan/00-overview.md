# az-tui — implementation plan

Read this file first, then the steps in order. Each step is one commit or a
few, gated the same way, and ends with a "Done when" list that is the whole
acceptance test. Nothing in a later step is needed to finish an earlier one.

- [01 Scaffold](01-scaffold.md) — the crate, CI, theme, config, an empty screen
- [02 Azure client](02-azure-client.md) — tokens from `az`, the transport, the inventory, `doctor`
- [03 Key Vault](03-key-vault.md) — secrets, versions, one value
- [04 Container Registry](04-container-registry.md) — token exchange, catalog, tags, manifests
- [05 Worker and cache](05-worker-and-cache.md) — the background thread and the JSON cache
- [06 Shell and the Secrets table](06-shell-and-table.md) — tabs, table, search, sort, help
- [07 Details, reveal, copy](07-details-reveal-copy.md) — the details pane, `v`, `y`, the clipboard
- [08 Registries tab](08-registries-tab.md) — repositories, then tags
- [09 Session, docs, release](09-session-docs-release.md) — the session file, README, the live checklist
- [10 CLI (optional)](10-cli.md) — `secrets`, `secret get`, `repos`, `tags`

## What it is

One terminal program, read only, two tabs:

1. **Secrets** — every secret in every Key Vault the login can reach, as one
   flat table. A secret named `db-password` in dev, qa and prod is three rows
   next to each other, which is the question this tab exists to answer.
2. **Registries** — every repository in every container registry, as one flat
   table; `Enter` opens one and the table becomes its tags, newest first.

Find the thing, copy it, leave. `y` copies a secret's value without ever
showing it, or an image reference (`acrprod.azurecr.io/payments-api:1.42.0`).
`v` shows a secret's value in the details pane for sixty seconds.

Nothing is ever written to Azure. The only requests that are not `GET` are the
Resource Graph query and the two ACR token-exchange calls, all three of which
read.

## Why it is fast

- **It opens from a cache.** Names and metadata from the last run live in one
  JSON file (never values); the first frame is painted from it before any
  network call, and a worker thread refreshes behind it.
- **Search is literal and in memory.** Every keystroke re-filters every row on
  the main thread with substring matching. Ten thousand rows is under a
  millisecond, so there is no debounce, no worker and no "pending" state.
- **Details are read on selection**, once per run: a secret's versions, a
  repository's tags, a tag's manifest. Moving the cursor back to a row costs
  nothing.
- **Every read is one thread away from the UI.** The screen never waits on the
  network, and a vault that will not answer is a line in the status bar, not
  a frozen table.

## The stack

The same as ticket-tui, on purpose: a person who knows one knows the other,
and whole modules can be lifted.

| Crate | Version | Used for |
|---|---|---|
| `ratatui` | 0.30 | every frame |
| `crossterm` | 0.29 | terminal, keys, mouse |
| `nucleo-matcher` | 0.3 | literal substring matching and match highlighting |
| `ureq` | 3, `json` feature | every HTTPS call |
| `serde`, `serde_json` | 1 | Azure's answers, the cache, the session |
| `toml` | 0.9 | `config.toml` |
| `clap` | 4, `derive` | flags and subcommands |
| `anyhow` | 1 | errors |
| `time` | 0.3, `formatting parsing macros` | RFC 3339 and unix stamps |
| `tempfile` | 3 | atomic writes of the cache and the session |

Rust edition 2024, `rust-version = "1.88"`. No SQLite: the cache is one JSON
file. No `base64`, `dirs`, `url` or `zeroize` crates: each is a few lines of
std here, and ticket-tui has them to copy.

## Layout

Wide (≥ 110 columns): table left, details right. Narrow: details below the
table. Under 70 columns the details pane is hidden until `Tab` asks for it.

```
 1 Secrets ⚠2  2 Registries                                                    ?
/ db-pass                                                    vault:kv-prod ×
╭ Secrets 3/412 · Name ↑ ─────────────────────────────╮╭ Details ─────────────────────────╮
│   Vault     Name             Enabled  Expires  Updated││ db-password                      │
│──────────────────────────────────────────────────────││ kv-prod · secret · enabled       │
│› kv-prod   db-password       ✓        —        3d     ││                                  │
│  kv-qa     db-password       ✓        —        3d     ││ Value         ••••••••   v · y   │
│  kv-dev    db-password       ✓        12d ⚠    3d     ││ Content type  text/plain         │
│                                                      ││ Expires       —                  │
│                                                      ││ Not before    —                  │
│                                                      ││ Created       2026-03-01 · 6mo   │
│                                                      ││ Updated       2026-09-08 · 3d    │
│                                                      ││ Tags          env=prod           │
│                                                      ││ Id            https://kv-prod.…  │
│                                                      ││                                  │
│                                                      ││ ── Versions ──────────────────── │
│                                                      ││ 8f3a2c…  3d   enabled  current   │
│                                                      ││ 1c2b90…  4mo  enabled            │
╰──────────────────────────────────────────────────────╯╰──────────────────────────────────╯
 ↑↓/jk move  / search  Enter/v reveal  y copy value  Y copy name  r refresh  ? help   ● 3 vaults · 412 secrets · 12 s ago
```

Registries, at the repository level and then inside one:

```
╭ Repositories 2/183 · Updated ↓ ─────────────────────╮╭ Details ─────────────────────────╮
│   Registry   Repository          Tags  Updated       ││ payments-api                     │
│──────────────────────────────────────────────────────││ acrprod.azurecr.io · 48 tags     │
│› acrprod    payments-api        48    2h            ││ Pull   acrprod.azurecr.io/paymen…│
│  acrqa      payments-api        112   40m           ││                                  │
│                                                      ││ ── Tags ─────────────────────── │
│                                                      ││ 1.42.0   sha256:ab12…  2h        │
│                                                      ││ 1.41.3   sha256:9f01…  3d        │
╰──────────────────────────────────────────────────────╯╰──────────────────────────────────╯

╭ payments-api · acrprod 48/48 · Updated ↓ ───────────╮╭ Details ─────────────────────────╮
│   Tag        Digest          Created   Updated       ││ payments-api:1.42.0              │
│──────────────────────────────────────────────────────││ acrprod.azurecr.io               │
│› 1.42.0     sha256:ab12…    2h        2h            ││ Digest  sha256:ab12…ef (y/Y)     │
│  1.41.3     sha256:9f01…    3d        3d            ││ Size    84.2 MB · linux/amd64    │
╰──────────────────────────────────────────────────────╯╰──────────────────────────────────╯
```

## Keys

| Key | Does |
|---|---|
| `1` `2` | Secrets, Registries |
| `j`/`k`, `↑`/`↓`, `PgUp`/`PgDn`, `Home`/`End` | move the cursor in the focused pane |
| `Tab` | focus the table or the details pane |
| `/` | search; `Esc` or `Enter` leaves the box and keeps the filter; `Esc` again clears it; `Ctrl-U` clears the box |
| `Enter` | Secrets: reveal (same as `v`). Registries: open the repository's tags |
| `Backspace`, `h` | Registries: back up to the repositories |
| `v` | show the secret's value in the details pane for 60 s; again hides it |
| `y` | copy the value (secret), the pull reference (repository or tag) |
| `Y` | copy the name (secret), the digest reference `repo@sha256:…` (tag) |
| `o` | open the vault or registry in the Azure portal |
| `s` / `S` | next sort column / flip direction; a header click does the same |
| `r` | refresh now |
| `?` | help, generated from the key table |
| `q`, `Ctrl-C` | quit |

The mouse: click a row, a tab, a column header or the search row; the wheel
scrolls the pane under it. No drag, no text selection in v1.

## Rules that never bend

1. **A secret's value never touches disk.** Not the cache, not the session
   file, not a log, not a panic message, not the status line. It lives in a
   `Secret` newtype whose `Debug` and `Display` print `[redacted]`, is read out
   in exactly two places (the line that draws it and the key that copies it),
   and is dropped sixty seconds after it was read, on `r`, on a tab switch
   and on quit.
2. **Read only.** No `PUT`, `PATCH` or `DELETE` anywhere in the crate. The
   `Transport` trait has no method that could send one. `grep -rn 'PUT\|DELETE\|PATCH' src` is clean.
3. **Every failure is a status, not a crash.** A vault that answers 403 is a
   line in the footer and a `!` on the tab; the other vaults' rows stay on
   screen. The TUI starts with no `az login` and says so.
4. **Search is literal.** Every whitespace-separated word must appear as a
   substring; scattered letters match nothing. Rows keep the table's sort
   order — no relevance score.
5. **The UI thread never blocks on the network.** Every HTTPS call and every
   `az` shell-out happens on the worker thread.
6. **Tests need no network and no `az`.** The client is tested through a fake
   `Transport`; `az` is a closure the tests hand in.

## Module layout

Mirrors ticket-tui so its files can be read side by side.

```
src/main.rs              hands to run::run(), prints the error, exits 1
src/lib.rs               the module list
src/run.rs               the terminal loop: poll the worker, draw, handle input
src/cli.rs               clap: flags, `doctor`, later the subcommands
src/config.rs            ~/.config/az-tui/config.toml, AZ_TUI_* overrides
src/paths.rs             config dir, data dir (cache, session)
src/azure/mod.rs         models (Vault, Registry, SecretRow, …), Secret, Inventory
src/azure/auth.rs        `az account get-access-token` per audience, a TokenSource trait
src/azure/transport.rs   Transport trait, the ureq implementation, throttling, the fake
src/azure/graph.rs       Resource Graph inventory
src/azure/vault.rs       Key Vault data plane: list, versions, value
src/azure/acr.rs         ACR: token exchange, catalog, attributes, tags, manifests
src/worker.rs            the one background thread: Request in, Event out
src/cache.rs             the JSON snapshot, read at startup, written after a refresh
src/store.rs             the in-memory model both screens read; applies worker events
src/session.rs           the JSON session: tab, sort, column widths
src/search.rs            literal matching and match highlighting (nucleo)
src/filter.rs            `key:value` grammar per tab
src/columns.rs           ColumnId, TableLayout (lifted)
src/text_input.rs        the one-line editor (lifted)
src/timestamp.rs         Timestamp, relative ages (lifted, trimmed)
src/clipboard.rs         OSC 52 plus the external commands
src/desktop.rs           open a URL in the browser (lifted)
src/app/mod.rs           App, AppAction, Focus, the key dispatch
src/app/shell.rs         what every screen shares: focus, status, notifications, hit regions
src/app/screen.rs        the Screen trait and TabId
src/app/cursor.rs        ListCursor (lifted)
src/app/secrets.rs       the Secrets screen
src/app/registries.rs    the Registries screen (two levels)
src/ui/mod.rs            render(): tab bar, the active screen, the status bar
src/ui/theme.rs          tokens and presets (lifted)
src/ui/table.rs          TableSpec and render_list_table (lifted)
src/ui/widgets.rs        search row, status bar, modal frame, scrollbar, help
src/ui/secrets.rs        the Secrets tab's panes
src/ui/registries.rs     the Registries tab's panes
```

Keep every file under 1,000 lines; split when one grows past it.

## Configuration

`$XDG_CONFIG_HOME/az-tui/config.toml`, `~/.config/az-tui/config.toml` when
that is unset (on macOS too). Optional; every key optional. A flag wins over an
`AZ_TUI_*` variable, which wins over the file.

```toml
[azure]
subscriptions = ["<guid>", "<guid>"]   # left out: every subscription the login can see
vaults = ["kv-dev", "kv-qa", "kv-prod"]   # only these, in this order; left out: all
registries = ["acrdev", "acrqa", "acrprod"]
refresh = 300                           # seconds between background refreshes; 0 = only `r`

[theme]                                 # identical to ticket-tui's, so the `theme` tool can write it
preset = "terminal"                     # terminal · terminal-light · mono · custom

[theme.custom]                          # see ticket-tui's config.example.toml for the full palette
name = "neon-void"
appearance = "dark"
bg = "#05060a"
# …
```

Flags: `--subscription <guid>` (repeatable), `--vault <name>` (repeatable),
`--registry <name>` (repeatable), `--refresh <secs>`, `--theme <preset>`,
`--config <path>`, `--cache <path>`, `--no-cache`. Variables: `AZ_TUI_THEME`,
`AZ_TUI_CONFIG`, `AZ_TUI_CACHE`, `NO_COLOR`.

## Prior art — read before writing

**ticket-tui**, at `~/dev/ticket-tui` on this machine and public at
<https://github.com/jacobragsdale/ticket-tui>. Lift from it rather than
re-inventing; each step names what to take.

**ticket-tui once had these two tabs.** They were built as tabs 6 and 7 and
torn out in commit `6f73eef` ("Tear out the AKS, ACR, Key Vault and
Environments tabs"). The complete tree from before the tear-out is on the
`azure-infra` branch at `c84ca7f`. To read a file as it was:

```console
git -C ~/dev/ticket-tui show 6f73eef^:src/arm.rs          # the ARM/Key Vault/ACR client, 69 KB
git -C ~/dev/ticket-tui show 6f73eef^:src/arm_watch.rs    # the worker thread
git -C ~/dev/ticket-tui show 6f73eef^:src/app/key_vault/mod.rs
git -C ~/dev/ticket-tui show 6f73eef^:src/app/acr/mod.rs
git -C ~/dev/ticket-tui show 6f73eef^:src/ui/key_vault.rs
git -C ~/dev/ticket-tui show 6f73eef^:src/ui/acr.rs
git -C ~/dev/ticket-tui show 6f73eef^:src/app/key_vault/filters.rs
git -C ~/dev/ticket-tui show 6f73eef^:src/app/acr/tests.rs
```

Two warnings about that code. It was never run against a real subscription
(none was reachable), so treat its endpoint details as a strong hint and
verify each against the step's own list. And it is wired into ticket-tui's
much larger shell; lift functions and shapes (token minting, `Retry-After`
parsing, the `Secret` newtype, the `Transport` trait, paging loops, the
column enums), not whole files.

## Working the plan

- One step at a time, in order. Read the step, read what it says to read,
  then build. Do not start the next step's work early.
- Commit at every working checkpoint and push to `main` after each step.
  Commit messages say what changed and why in plain sentences.
- Decide small things yourself and say so in the commit message. Nobody is
  waiting to answer a question.
- **The gate**, run before every commit, all green:

  ```console
  cargo fmt --all -- --check
  cargo clippy --all-targets --all-features -- -D warnings
  cargo test --all-targets
  NO_COLOR=1 cargo test --all-targets
  AZ_TUI_THEME=terminal-light cargo test --all-targets
  AZ_TUI_THEME=mono cargo test --all-targets
  cargo build --release
  ```

- Every non-trivial branch, loop or parser gets one test that fails if it
  breaks. No test touches the network or runs `az`.
- A deliberate shortcut with a known ceiling gets a `// ponytail:` comment
  naming the ceiling and the upgrade path, e.g.
  `// ponytail: search on the main thread; move to a worker past ~50k rows`.
- The machine the plan is developed on may not be signed in to Azure. Nothing
  in a step's "Done when" needs a live subscription; the live walk-through is
  step 09's checklist, for the day one is reachable.
- Write for the next reader: doc comments say what a thing is for and why it
  is shaped that way, not what the code obviously does.
