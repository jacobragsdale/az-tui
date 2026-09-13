# az-tui

A fast terminal browser for Azure: AKS namespaces, Key Vault secrets and
Container Registry images, in one window. One tab per namespace with pod
health at a glance, logs, describe, YAML, events, configmaps and secrets one
key away, a shell into a pod without leaving the screen; then a tab of every
secret across every vault, and a tab of every repository across every
registry, with the value or the pull reference one key away.

It is the sibling of [ticket-tui](https://github.com/jacobragsdale/ticket-tui):
the same stack (Rust, ratatui, crossterm), the same layout, the same keys.

```
 1 qa/dev ✗ 1  2 qa/qa  3 qa/uat ✗ 1  4 prod  5 Secrets ⚠ 3  6 Registries                                      Pods ▾  ?
/ Type / to search pods, or status:crash owner:orders-api app: node:
╭ Pods ───────────────────────────────────────────────────────────╮╭ Details ──────────────────────────────────────────╮
│  Name ↑                        Ready Status               ↻ Age ││[Logs] [Bash] [Restart] [Scale] [Describe] [YAML]  │
│─────────────────────────────────────────────────────────────────││✗ orders-worker-5c4d3e-q8zt  CrashLoopBackOff      │
│  billing-api-1a2b3c-qq111        2/2 ● Running            0  2d ││qa/dev · Deployment/orders-worker · 3/3 ready · 2d │
│  orders-api-7d9f5b-abc12         1/1 ● Running            2  2d ││                                                   │
│  orders-api-7d9f5b-k9x2p         1/1 ● Running            0  2d ││Ready         0/1                                  │
│› orders-worker-5c4d3e-q8zt       0/1 ✗ CrashLoopBackOff  17  2d ││Restarts      17                                   │
│  redis-0                         1/1 ● Running            0  2d ││Node          aks-np1-vmss000000                   │
│                                                                 │╰───────────────────────────────────────────────────╯
│                                                                 │╭ Log · following · orders-worker-5c4d3e-q8zt · api ╮
│                                                                 ││ 12:04:01 INFO  handled GET /orders                │
│                                                                 ││ 12:04:02 ERROR upstream timeout                   │
│                                                                 ││ 12:04:02 WARN  retrying in 5s                     │
╰ 5 · Name ↑ ─────────────────────────────────────────────────────╯╰───────────────────────────────────────────────────╯
j/k scroll  End follow  / filter  z zoom  P previous  C container  Tab table  Esc close      ● 5 pods · just now
```

```
 1 qa/dev ✗ 1  2 qa/qa  3 qa/uat ✗ 1  4 prod  5 Secrets ⚠ 3  6 Registries                                              ?
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
↑↓/jk move  / search  v reveal  y copy value  Y copy name  S sort  r refresh  ? help  ● 3 vaults · 412 secrets · 12s ago
```

## What it does

- **Tabs are namespaces, then the two Azure tabs.** `1 qa/dev · 2 qa/qa ·
  3 qa/uat · 4 prod`, in `config.toml`'s order, then `5 Secrets · 6
  Registries`; a number, `[` `]`, the arrows or a click switches. Each tab
  keeps its own cursor, search and sort.
- **Four kinds per namespace** — Pods, Events, ConfigMaps, Secrets — on
  `p e m s` or the pill at the right of the tab bar. Each kind keeps its own
  cursor, search, sort and columns.
- **Pod health at a glance.** A pod in trouble reads red across its whole
  row and counts on its tab's `✗ N` badge; a finished one reads muted; the
  status cell carries kubectl's own word and a glyph.
- **Logs** stream into a pane under the details and follow the pod under
  the cursor; **describe** and **YAML** land in the same pane, read once per
  object. `/` inside the pane filters its lines.
- **Bash into a pod**, **restart** it (delete; its owner replaces it),
  **rollout-restart** or **scale** its owner — each one kubectl call, each
  asked about first, each reading the namespace again the moment it went.
- **Events** newest first with `e` on a pod narrowing to it; **configmaps**
  with their keys walked in the details pane and a key's value in the text
  pane; **secrets** read one key at a time, shown for sixty seconds or
  copied unseen, never cached, never logged.
- **Every Key Vault secret in one flat table**, sorted so that one secret's
  dev, qa and prod rows sit next to each other; the `Env ▾` header narrows
  the table to one environment. `y` copies a value without showing it, `v`
  shows it for sixty seconds.
- **Every registry's repositories in one table**; `Enter` opens one
  repository's tags, `y` copies `acrprod.azurecr.io/payments-api:1.42.0`
  and `Y` the digest reference.
- **Searches literally, as you type**, across every namespace, vault or
  registry at once: `db-pass`, `env:prod enabled:no`, `status:crash
  owner:orders-api`. Forty thousand rows re-filter in about two
  milliseconds.
- **Fast.** The first frame paints from a cache of the last pod lists and
  the last secret and repository listings; the open namespace is read every
  five seconds and the others every thirty, the vaults and registries every
  five minutes; every read is one thread away from the UI.
- **Borrows the Azure CLI's login** (`az login`) for all three; `kubectl`
  and `kubelogin` underneath for the clusters. One
  `~/.config/az-tui/config.toml` names clusters, namespaces, subscriptions,
  vaults, registries and a theme.
- **Read only on Azure**, and on AKS it never writes a configmap or a
  secret, never `apply`s, and never deletes anything but a pod, after
  asking.

## Run it

```console
az login
cargo install --git https://github.com/jacobragsdale/az-tui
az-tui setup          # az aks list, get-credentials, kubelogin, a [[clusters]] block per cluster
az-tui doctor         # the login and tokens, every vault and registry, then kubectl and every scope
az-tui
```

Or from a checkout: `cargo run --release`.

An AKS cluster with Entra ID sign-in wants [`kubelogin`](https://azure.github.io/kubelogin/)
on `PATH`, and its kubeconfig converted once so it borrows the `az login`
rather than asking for a device code on every read; `setup` does the
conversion when `kubelogin` is there:

```console
kubelogin convert-kubeconfig -l azurecli
```

`az-tui doctor` checks the login, both token audiences, what every vault and
registry actually answers, and then that every scope in `config.toml`
answers a `kubectl`:

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

kubectl       v1.31.2
kubelogin     0.1.4
qa/dev        ok (312 ms)
qa/qa         ok (298 ms)
qa/uat        ok (305 ms)
prod/prod     ok (411 ms)
```

It exits 0 when everything answered and 1 otherwise, so it works in a script.
With no `[[clusters]]` in the file the AKS half is one line saying so, and
the exit code is the Azure half's.

## From a shell

The same reads without the screen, for scripts and agents. Each reads the
cache when it is younger than the refresh interval and Azure otherwise; with
`refresh = 0` under `[azure]` the cache is used whatever its age, and
`--refresh` on the subcommand reads Azure regardless. The global flags
(`--config`, `--cache`, `--no-cache`, `--subscription`, `--theme`) may come
before or after the subcommand.

```console
az-tui secrets [QUERY] [--vault NAME]... [--json] [--refresh]
az-tui secret get NAME [--vault NAME] [--version ID] [--json]
az-tui repos [QUERY] [--registry NAME]... [--json] [--refresh]
az-tui tags REPO [--registry NAME] [--json]
az-tui setup [--write]
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

`setup` fetches credentials for every AKS cluster the login can see, converts
the kubeconfig, and prints a `[[clusters]]` block per cluster to trim;
`--write` puts them in `config.toml` when there is no file yet, and never
overwrites one.

Exit codes: **0** it worked, **1** a read failed (the rows that did answer
are still printed first), **2** the arguments were wrong.

## Keys

| Key | Does |
|---|---|
| `1`–`9`, `[` `]`, `←` `→`, click | a tab by number; the previous, the next. AKS namespaces first, then Secrets, then Registries |
| `j`/`k`, `↑`/`↓`, `PgUp`/`PgDn`, `Home`/`End` | move the cursor in the focused pane |
| `Tab` | focus the table, the details, or the text pane under them |
| `/` | search; in a text pane, filter its lines; `Esc` or `Enter` keeps the filter, `Esc` again clears it; `Ctrl-U` empties the box |
| `S`, `R`, header click | sort by the next column; reverse it; the same header again turns it round, a third time is the default |
| `r` | refresh now: this namespace, or every vault and registry |
| `Esc` | back out one step: the modal, the pane filter, the query, then the open pane or repository |
| `p` `e` `m` `s`, the pill | AKS: Pods, Events, ConfigMaps, Secrets; `e` on a pod is that pod's events |
| `Enter`, `l` | AKS: the log, following (pods); the pod (events); the key's value (configmaps, secrets). Secrets: reveal. Registries: the repository's tags |
| `d`, `v` | AKS: describe / YAML; `v` on a configmap or secret is the key's value. Secrets: `v` shows the value for 60 s |
| `P`, `C`, `End`, `z` | AKS: the log before the last restart; the next container; follow again; the pane alone and back |
| `b`, `x`, `X`, `=` | AKS: a shell in the pod; restart it (asks first); rollout-restart its owner; scale its owner |
| `h`, `Backspace` | Registries: back to the repositories |
| `y` | copy: the name (AKS), the key's value unseen (AKS configmap/secret), the secret's value (Secrets), the pull reference (Registries) |
| `Y` | copy: the kubectl line for what the pane shows (AKS), the secret's name (Secrets), the digest reference (Registries, inside a repository) |
| `o` | open the vault or registry in the Azure portal |
| `?` | help: the keys and the search grammar of the open tab, from the same key table |
| `q`, `Ctrl-C` | quit |

The mouse: rows, tabs, the kind pill and its menu, the `Env ▾` header and
its menu, column headers, the search row, the toolbar buttons in the details
pane, the keys of a configmap or secret, a modal's buttons; the wheel scrolls
the pane under it. Anything a key does, something on screen does too.

## Searching

Every whitespace-separated word must appear somewhere in the row as a
substring, case-insensitively. Scattered letters match nothing — `tks` does
not find `ticket search` — so typing part of a name is a reliable way to find
it rather than a guess. Rows keep the table's sort; there is no relevance
score.

A word of the form `key:value` is a filter when the table knows the key, and
an ordinary word otherwise (so `https://kv-prod` and `orders-api:1.2.3`
search for themselves). Filters and words are all ANDed.

| Table | Filters |
|---|---|
| Pods | `name:` `ns:` `status:` `owner:` `app:` `node:` |
| Events | `type:` `reason:` `object:` `kind:` `message:` `ns:` |
| ConfigMaps | `name:` `ns:` `key:` |
| Secrets (AKS) | `name:` `ns:` `type:` `key:` |
| Secrets | `env:dev\|qa\|prod` `vault:` `name:` `type:` (content type) `enabled:yes\|no` `managed:yes\|no` `tag:key` `tag:key=value` `expires:<30d \| >30d \| none \| expired` |
| Registries | `env:dev\|qa\|prod` `registry:` `repo:`/`name:` `updated:<30d` `created:<30d` |
| inside a repository | `tag:`/`name:` `digest:` `updated:` `created:` |

The first column of the Secrets and Registries tables is the environment,
read off the end of the vault's or registry's name: `kv-prod` and `acrprod`
are prod. Clicking its header opens a menu — All, dev, qa, prod — whose choice
writes or clears the `env:` filter in the search box, so `Esc` takes it off
like any other. On AKS the environment is the tab: `qa/dev`, `qa/qa`,
`qa/uat`, `prod`.

A bare number of days means `<`, so `expires:30d` reads the way you meant it.
A filter whose value makes no sense is ignored rather than matching nothing,
so half-typing one never empties the table.

## How it reads

The namespace on screen is read every `refresh` seconds (five by default)
and the moment it is switched to; the other namespaces every thirty, for
their badges. A kind other than pods is read for the open tab only, while it
shows. A scope that fails backs off, doubling to two minutes, and its rows
stand from the last read that worked, with the message in the status bar and
under `?`. Every `kubectl` call carries `--request-timeout=10s` and is killed
after twenty seconds regardless, which is what a credential plugin waiting on
a device-code login looks like.

Describe, YAML and an owner's replica count are read once per object per
run, the owner once the cursor rests on a pod for 150 ms. One
`kubectl logs -f` runs at a time and is killed when the pane leaves it and
when the run ends.

The vaults and registries are refreshed every `[azure].refresh` seconds
(three hundred by default) and on `r` from either of their tabs, `parallel`
vaults or repositories at a time; a secret's versions, a repository's tags
and a tag's manifest are read once per row per run, when the cursor rests
on it.

## Where things live

| | Path |
|---|---|
| Configuration | `$XDG_CONFIG_HOME/az-tui/config.toml`, else `~/.config/az-tui/config.toml` |
| Cache | `$XDG_DATA_HOME/az-tui/cache.json`, else `~/.local/share/az-tui/cache.json` |
| Session | the same directory, `session.json` |

On macOS the data directory is `~/Library/Application Support/az-tui/`; the
config directory is `~/.config/az-tui/` there too, because that is where every
other terminal program keeps its own.

**The cache** is one file keyed by tab — `qa/dev`, `prod/prod`, `secrets`,
`registries` — holding the last pod list per namespace, the vaults and
registries the login can reach, and the names and metadata of every secret
and repository: what the tables show on open. It never holds a secret's
value, a version, a tag, a manifest, a log line, an event, a configmap's data
or a Kubernetes secret's shape. A tab nothing has read is not in it. It is
written `0600` on unix, because a name like `stripe-prod-key` says a good
deal by itself. `--no-cache` neither reads nor writes it; `--cache PATH` moves
it.

**The session** is keyed the same way and holds which tab was open, each
namespace tab's kind, and how each table was sorted and sized. Never the
query, never the cursor, never a name. It is written `0600` too, through the
same writer.

A **value** — a Key Vault secret's, or one key of a Kubernetes secret — is
read only when `v` or `y` asks for one. It lives in a newtype whose `Debug`
and `Display` print `[redacted]` and which cannot be serialised at all, so it
cannot reach the cache, the session, a log or a panic message. It is dropped
when the cursor moves, on `r`, on a kind or tab switch, on quit, and sixty
seconds after it arrived. `y` copies it without ever drawing it.

## Configuration

Every key is optional, and so is the file. See
[`config.example.toml`](config.example.toml); `az-tui setup` prints the
`[[clusters]]` blocks.

```toml
refresh = 5                               # seconds between reads of the open namespace; 0 = only `r`
                                          # (above the first [[clusters]], or TOML gives it to that cluster)
[[clusters]]
name = "qa"
context = "aks-qa"                        # kubeconfig context; left out: the name
namespaces = ["dev", "qa", "uat"]         # one tab each; left out: one tab over all of them

[[clusters]]
name = "prod"
namespaces = ["prod"]

[azure]
subscriptions = ["<guid>"]                # left out: every subscription the login can see
vaults = ["kv-dev", "kv-qa", "kv-prod"]   # only these, in this order; left out: all
registries = ["acrdev", "acrqa", "acrprod"]
refresh = 300                             # seconds between vault and registry refreshes; 0 = only `r`
parallel = 8                              # vaults or repositories read at once during a refresh

[theme]
preset = "terminal"                       # terminal · terminal-light · mono · custom

[theme.custom]                            # the same table ticket-tui reads, so one file themes both
name = "neon-void"
appearance = "dark"
bg = "#05060a"
# …
```

Every namespace is a tab, ahead of Secrets and Registries. A cluster with no
`namespaces` is one tab over all of them, with a Namespace column. No
`[[clusters]]` at all: no AKS tabs. A flag beats an `AZ_TUI_*` variable,
which beats the file.

**Flags:** `--subscription <guid>` (repeatable), `--vault <name>` (repeatable),
`--registry <name>` (repeatable), `--refresh <secs>` (the AKS cadence),
`--theme <preset>`, `--config <path>`, `--cache <path>`, `--no-cache`.
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
   not a log, not a panic message, not the status line. Key Vault and
   Kubernetes secrets alike.
2. **Read only on Azure.** No `PUT`, `PATCH` or `DELETE` anywhere in the
   transport. The only two verbs it can spell are `GET` and `POST`, and the
   three `POST`s are the Resource Graph query and the two registry token
   exchanges, all of which read.
3. **On AKS: never writes a configmap or a secret, never `apply`s, never
   deletes anything but a pod** — and that, a rollout restart and a scale
   are each asked about first.
4. **Every failure is a status, not a crash.** A vault that answers 403 or a
   namespace that answers Forbidden is a line in the footer; the other rows
   stay on screen and are marked stale. It starts with no `az login` at all
   and says so.
5. **The UI thread never blocks on the network.** Every HTTPS call, every
   `az` and `kubectl` shell-out happens off it, and a refresh reads up to
   `parallel` vaults and repositories at once (eight, unless `config.toml`
   says otherwise).

## Without a cluster

`scripts/fake-kubectl` answers `kubectl` from fixtures — two clusters, four
namespaces, a pod in a crash loop, one that will not pull, events,
configmaps, secrets, a namespace that refuses them — and `scripts/walk.py`
drives the release binary under a pty against it and asserts what it
painted, every key and every kind:

```console
cargo build --release
scripts/walk.py --show          # needs uv; or: pip install pyte && python3 scripts/walk.py
```

## How it was built

[`plan/`](plan/00-overview.md) is the implementation plan for the Azure half,
one file per step, written before any code. It is kept as-is; the commit
messages say where the code and the plan disagreed and why. The AKS half was
built as its own program and merged in once the two had converged
on the same layout and keys.

Both halves were built without a reachable subscription or cluster: every
Azure endpoint was checked against the published REST reference, and every
`kubectl` call against a fake. [`plan/CHECKLIST.md`](plan/CHECKLIST.md) is
the walk-through for the first run against real ones, both halves, including
the details the documentation left open.

## License

MIT, see [LICENSE](LICENSE).
