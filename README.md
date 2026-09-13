# az-tui

A fast, read-only terminal browser for Azure Key Vault secrets and Azure
Container Registry images. Type part of a name, see it across every vault and
registry you can reach, and copy the value or the image reference with one key.

It is the sibling of [ticket-tui](https://github.com/jacobragsdale/ticket-tui):
the same stack (Rust, ratatui, crossterm), the same layout, the same keys.

```
 1 Secrets ⚠ 3 2 Registries                                                                                           ?
/ db-pass                                                                                                             ×
╭ Secrets ───────────────────────────────────────────────────────╮╭ Details ───────────────────────────────────────────╮
│  Env ▾  Name ↑                               Enabled Expires   ││db-password                                         │
│────────────────────────────────────────────────────────────────││kv-prod · secret · enabled · 2 versions             │
│› prod   db-password                           ✓       —        ││                                                    │
│  qa     db-password                           ✓       —        ││Value         ••••••••                         v · y│
│  dev    db-password                           ✓       12d      ││Content type  text/plain                            │
│                                                                ││Expires       —                                     │
│                                                                ││Created       2026-03-01 · 6mo                      │
│                                                                ││Updated       2026-09-08 · 3d                       │
│                                                                ││Tags          env=prod                              │
│                                                                ││                                                    │
│                                                                ││── Versions ────────────────────────────────────────│
│                                                                ││8f3a2c1d…  3d    enabled  current                   │
│                                                                ││1c2b90aa…  4mo   enabled                            │
╰ 3/412 · Name ↑ ────────────────────────────────────────────────╯╰────────────────────────────────────────────────────╯
↑↓/jk move  / search  v reveal  y copy value  Y copy name  s sort  r refresh  ? help  ● 3 vaults · 412 secrets · 12s ago
```

## What it does

- **Opens instantly** from a local cache of names and metadata, then refreshes
  from Azure in the background. Secret values are never cached, logged or
  written anywhere.
- **Searches literally, as you type**, across every vault or registry at once:
  `db-pass`, `env:prod enabled:no`, `expires:<30d`. Forty thousand rows
  re-filter in about two milliseconds.
- **`y` copies a secret's value without showing it**; `v` shows it for sixty
  seconds. On an image, `y` copies `acrprod.azurecr.io/payments-api:1.42.0`
  and `Y` copies the digest reference.
- **Borrows the Azure CLI's login** (`az login`); nothing else to set up. One
  optional `~/.config/az-tui/config.toml` names subscriptions, vaults,
  registries and a theme.
- **Read only.** It never creates, changes or deletes anything in Azure.

## Run it

```console
az login
cargo install --git https://github.com/jacobragsdale/az-tui
az-tui
```

Or from a checkout: `cargo run --release`.

Start with `az-tui doctor`, which checks the login, both token audiences, and
what every vault and registry actually answers:

```
az            2.90.0
account       jacob@example.com · tenant 72f988bf-…
subscriptions 3 enabled
arm token     ok (412 ms)
vault token   ok (398 ms)
inventory     3 vaults, 3 registries (1.2 s)
  kv-dev      eastus   rg-dev           https://kv-dev.vault.azure.net/
  acrprod     eastus   rg-prod          acrprod.azurecr.io
kv-dev        412 secrets (3.1 s)
acrprod       183 repositories (0.9 s)
```

It exits 0 when everything answered and 1 otherwise, so it works in a script.

## From a shell

The same reads without the screen, for scripts and agents. Each reads the
cache when it is younger than the refresh interval and Azure otherwise; with
`refresh = 0` in `config.toml` the cache is used whatever its age, and
`--refresh` reads Azure regardless. The global flags (`--config`, `--cache`,
`--no-cache`, `--subscription`, `--theme`) may come before or after the
subcommand.

```console
az-tui secrets [QUERY] [--vault NAME]... [--json] [--refresh]
az-tui secret get NAME [--vault NAME] [--version ID] [--json]
az-tui repos [QUERY] [--registry NAME]... [--json] [--refresh]
az-tui tags REPO [--registry NAME] [--json]
az-tui doctor
```

`QUERY` is the same grammar as `/` in the TUI.

```console
$ az-tui secrets 'expires:<30d enabled:yes'
kv-prod          tls-cert-pem      enabled  12d        3d
$ az-tui secrets db --json | jq length
3
$ az-tui secret get db-password --vault kv-prod | wc -c
24
$ az-tui tags payments-api --json | jq -r '.[0].pull'
acrprod.azurecr.io/payments-api:1.42.0
```

`secret get` is the one command that prints a value. It writes it to stdout
with no trailing newline of its own, so `$(az-tui secret get NAME)` is the
value byte for byte, and a value that ends in a newline keeps it. A name that
is in more than one vault is an error listing them rather than a guess.

Exit codes: **0** it worked, **1** a read failed (the rows that did answer
are still printed first), **2** the arguments were wrong.

## Keys

| Key | Does |
|---|---|
| `1` `2` | Secrets, Registries |
| `j`/`k`, `↑`/`↓`, `PgUp`/`PgDn`, `Home`/`End` | move the cursor in the focused pane |
| `Tab` | focus the table or the details pane |
| `/` | search; `Esc` or `Enter` leaves the box and keeps the filter; `Esc` again clears it; `Ctrl-U` clears the box |
| `Enter` | Secrets: reveal (same as `v`). Registries: open the repository's tags |
| `Backspace`, `h`, `Esc` | Registries: back up to the repositories (`Esc` clears a query first) |
| `v` | show the secret's value for 60 s; again hides it |
| `y` | copy the value (secret), the pull reference (repository or tag) |
| `Y` | copy the name (secret), the digest reference `repo@sha256:…` (tag) |
| `o` | open the vault or registry in the Azure portal |
| `s` / `S` | next sort column / flip the direction; a header click does the same |
| `r` | refresh now |
| `?` | help: the keys and the search grammar, generated from the same key table |
| `q`, `Ctrl-C` | quit |

The mouse: click a row, a tab, a column header or the search row; the wheel
scrolls the pane under it.

## Searching

Every whitespace-separated word must appear somewhere in the row as a
substring, case-insensitively. Scattered letters match nothing — `tks` does
not find `ticket search` — so typing part of a name is a reliable way to find
it rather than a guess. Rows keep the table's sort; there is no relevance
score.

A word of the form `key:value` is a filter when the tab knows the key, and an
ordinary word otherwise (so `https://kv-prod` searches for itself). Filters
and words are all ANDed.

**Secrets:** `env:dev|qa|prod` `vault:` `name:` `type:` (content type) `enabled:yes|no`
`managed:yes|no` `tag:key` `tag:key=value`
`expires:<30d | >30d | none | expired`

**Registries:** `env:dev|qa|prod` `registry:` `repo:`/`name:` `updated:<30d` `created:<30d`, and
inside a repository `tag:`/`name:` `digest:` `updated:` `created:`

The first column of both tables is the environment, read off the end of the
vault's or registry's name: `kv-prod` and `acrprod` are prod. Clicking its
header opens a menu — All, dev, qa, prod — whose choice writes or clears the
`env:` filter in the search box, so `Esc` takes it off like any other.

A bare number of days means `<`, so `expires:30d` reads the way you meant it.
A filter whose value makes no sense is ignored rather than matching nothing,
so half-typing one never empties the table.

## Where things live

| | Path |
|---|---|
| Configuration | `$XDG_CONFIG_HOME/az-tui/config.toml`, else `~/.config/az-tui/config.toml` |
| Cache | `$XDG_DATA_HOME/az-tui/cache.json`, else `~/.local/share/az-tui/cache.json` |
| Session | the same directory, `session.json` |

On macOS the data directory is `~/Library/Application Support/az-tui/`; the
config directory is `~/.config/az-tui/` there too, because that is where every
other terminal program keeps its own.

**The cache** holds the vaults and registries the login can reach, and the
names and metadata of every secret and repository — what the two tables show
on open. It never holds a secret's value, a version, a tag or a manifest. It
is written `0600` on unix, because a name like `stripe-prod-key` says a good
deal by itself. `--no-cache` neither reads nor writes it; `--cache PATH` moves
it.

**The session** holds which tab was open and how each table was sorted and
sized. Never the query, never the cursor, never a name.

A **value** is read out of Azure only when `v` or `y` asks for one. It lives
in a newtype whose `Debug` and `Display` print `[redacted]` and which cannot
be serialised at all, so it cannot reach the cache, the session, a log or a
panic message. It is read out in exactly two places in the crate, and it is
dropped when the cursor moves, on `r`, on a tab switch, on quit, and sixty
seconds after it arrived.

## Configuration

Every key is optional, and so is the file. See
[`config.example.toml`](config.example.toml).

```toml
[azure]
subscriptions = ["<guid>"]                # left out: every subscription the login can see
vaults = ["kv-dev", "kv-qa", "kv-prod"]   # only these, in this order; left out: all
registries = ["acrdev", "acrqa", "acrprod"]
refresh = 300                             # seconds between background refreshes; 0 = only `r`
parallel = 8                              # vaults or repositories read at once during a refresh

[theme]
preset = "terminal"                       # terminal · terminal-light · mono · custom

[theme.custom]                            # the same table ticket-tui reads, so one file themes both
name = "neon-void"
appearance = "dark"
bg = "#05060a"
# …
```

A flag beats an `AZ_TUI_*` variable, which beats the file.

**Flags:** `--subscription <guid>` (repeatable), `--vault <name>` (repeatable),
`--registry <name>` (repeatable), `--refresh <secs>`, `--theme <preset>`,
`--config <path>`, `--cache <path>`, `--no-cache`.
**Variables:** `AZ_TUI_THEME`, `AZ_TUI_CONFIG`, `AZ_TUI_CACHE`, `NO_COLOR`.

## Copying over SSH

`y` writes an **OSC 52** escape to the terminal as well as trying `pbcopy`,
`wl-copy`, `xclip`, `xsel` and `clip.exe`. The escape is what makes copying
work over SSH and inside a VDI, where nothing on the far side can reach the
clipboard your eyes are next to.

Under tmux the sequence has to be passed through — add this to `~/.tmux.conf`
(tmux 3.3 or newer):

```tmux
set -s set-clipboard on
```

Terminals that speak OSC 52: kitty, WezTerm, Alacritty, foot, iTerm2, Windows
Terminal, and VS Code's.

## What it never does

1. **A secret's value never touches disk.** Not the cache, not the session,
   not a log, not a panic message, not the status line.
2. **Read only.** No `PUT`, `PATCH` or `DELETE` anywhere in the crate. The
   only two verbs the transport can spell are `GET` and `POST`, and the three
   `POST`s are the Resource Graph query and the two registry token exchanges,
   all of which read.
3. **Every failure is a status, not a crash.** A vault that answers 403 is a
   line in the footer; the other vaults' rows stay on screen and are marked
   stale. It starts with no `az login` at all and says so.
4. **The UI thread never blocks on the network.** Every HTTPS call and every
   `az` shell-out happens off it, and a refresh reads up to `parallel` vaults
   and repositories at once (eight, unless `config.toml` says otherwise).

## How it was built

[`plan/`](plan/00-overview.md) is the implementation plan, one file per step,
written before any code. It is kept as-is; the commit messages say where the
code and the plan disagreed and why.

az-tui was built without a reachable subscription, so every endpoint was
checked against the published Azure REST reference rather than against a live
tenant. [`plan/CHECKLIST.md`](plan/CHECKLIST.md) is the walk-through for the
first run against a real one, including the three details the documentation
left open.

## License

MIT, see [LICENSE](LICENSE).
