# 10 — CLI (optional)

**Goal:** the same reads from a shell, for scripts and agents. Build this
only if it is wanted; nothing before it depends on it.

## Build — `src/cli.rs`

Every subcommand runs on the calling thread with the same `Client`, reads
the cache first when it is fresh enough, and prints a table or `--json`.

```
az-tui secrets [QUERY] [--vault NAME]... [--json] [--refresh]
    one line per secret: vault, name, enabled, expires, updated
    QUERY is the same grammar as `/` in the TUI
    --refresh reads Azure instead of the cache

az-tui secret get NAME [--vault NAME] [--version ID] [--json]
    prints the value and nothing else (no trailing newline unless the value has one)
    NAME in more than one vault and no --vault: an error listing the vaults, exit 2
    --json: {"vault","name","version","content_type","value"}

az-tui repos [QUERY] [--registry NAME]... [--json]
az-tui tags REPO [--registry NAME] [--json]
    one line per tag: name, short digest, updated; --json carries the full digest and the pull reference

az-tui doctor        (already exists)
```

`secret get` is the one command that prints a value; say so in its help text,
and never print it in any other command, in errors, or in `--json` output of
`secrets`.

Exit codes: 0 ok, 1 a read failed, 2 the arguments were wrong.

## Tests

- Each command against the fake transport: the table shape, the `--json`
  shape, the ambiguity error for `secret get`, exit codes.
- `secrets --json` output does not contain the key `"value"`.

## Done when

- `az-tui secret get db-password --vault kv-prod | wc -c` matches the
  value's length; `az-tui secrets db --json | jq length` counts the rows.
