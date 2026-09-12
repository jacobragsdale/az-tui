# 08 — The Registries tab

**Goal:** every repository across every registry as one table; `Enter` turns
the table into that repository's tags; the details pane shows the tags and
then the manifest; `y`/`Y` copy references that pull.

## Read first

- `git -C ~/dev/ticket-tui show 6f73eef^:src/app/acr/mod.rs` — `Level`,
  the two cursors, `open_repositories`/`close_repositories` (the same shape
  as this step's `open_tags`/`close_tags`), `focused`, the copy commands.
- `git -C ~/dev/ticket-tui show 6f73eef^:src/app/acr/columns.rs` and
  `src/ui/acr.rs`.
- This repo's `src/app/secrets.rs` from steps 06–07: the Registries screen
  is the same shape with a second level.

## Build — `src/app/registries.rs`, `src/ui/registries.rs`

### Level 1 — repositories

Columns: `Registry` 10, `Repository` flexible (min 20), `Tags` 5 right-aligned,
`Manifests` 9 right (hidden), `Updated` 8, `Created` 8 (hidden). Default
sort: `Updated` descending, `None` last; while counts are still filling in
(step 05's attributes walk) the cells read `…` in `muted` and the bottom
border says `183 · filling 40/183`.

Schema for `filter`: `registry:`, `repo:`/`name:`, `updated:<Nd | >Nd`.

Details pane for the row under the cursor:

```
payments-api
acrprod.azurecr.io · 48 tags · 51 manifests · updated 2h · created 1y
Pull          acrprod.azurecr.io/payments-api                    Y
Portal        https://portal.azure.com/#@/resource/…               ← `link`

── Tags ─────────────────────────── newest first
1.42.0     sha256:ab12ef01   2h
1.41.3     sha256:9f01aa42   3d
…                                                                   ← "reading…" until they land; the first 20, then `and 28 more — Enter`
```

Tags are requested on the 150 ms rest, like versions in 07, and kept for
the run.

Keys at level 1: `Enter` opens the repository (level 2); `y` copies
`login/repo` (a pullable reference for `latest`); `Y` copies the same; `o`
opens the registry in the portal.

### Level 2 — the tags of one repository

The table is replaced (same pane, same `TableSpec`): title
` payments-api · acrprod `, columns `Tag` flexible (min 16), `Digest` 20
(`short_digest`), `Created` 8, `Updated` 8. Sort default `Updated`
descending. The search box gets its own `TextInput` for this level, so going
down and back up puts each query back the way it was (the old code's
`registry_query`/`repository_query` pair). Schema: `tag:`/`name:`,
`digest:`, `updated:<Nd`.

Details pane at level 2:

```
payments-api:1.42.0
acrprod.azurecr.io
Pull      acrprod.azurecr.io/payments-api:1.42.0                y
Digest    acrprod.azurecr.io/payments-api@sha256:ab12…full…     Y
Size      84.2 MB · linux/amd64                                ← from the manifest, "reading…" until it lands; `index` for a multi-arch list
Created   2026-09-11 · 2h
Updated   2026-09-11 · 2h
Also tags 1.42, stable                                          ← the manifest's other tags
```

The manifest is requested on the rest interval, once per digest per run.

Keys at level 2: `Backspace`/`h` back to level 1 with the level-1 cursor
where it was; `y` copies the pull reference; `Y` the digest reference; `o`
the portal.

### Shared

- `on_store_changed` keeps the level-1 cursor on `(registry, repo)` and the
  level-2 cursor on the tag name across a refresh; a repository that
  disappeared on refresh while its tags are open closes the level with a
  status `payments-api is no longer in acrprod`.
- Badge: none in v1 (nothing on a registry is urgent).
- Stale registries (last read failed) paint `muted`, like stale vaults.
- The footer hint per level, and the help table gains the level-2 keys.

## Tests

- Level switching: `Enter` opens, `Backspace` closes, both cursors survive;
  each level's query survives the round trip.
- Sort by each column at each level; `None` counts sort last.
- Tag and manifest requests fire once per row after the rest interval.
- `y`/`Y` produce the exact reference strings at each level.
- Renderer tests at 120 × 30: the filling counter, the details pane at both
  levels, the `index` size line, the stale style.

## Done when

- Signed in: `2`, type a service name, `Enter`, see the tags newest first,
  `y`, then `docker pull <paste>` (or `crane manifest <paste>`) succeeds on
  a machine with registry access.
- `Y` at level 2 pastes a digest reference the same tool accepts.
- The gate is green.

## Not in this step

Deleting tags, untagged manifests, repository size totals, the `latest`
resolution. None of these are read-only concerns except size totals, which
would be one manifest call per tag — note it in a `// ponytail:` comment.
