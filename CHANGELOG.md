# Changelog

## 0.1.0 — unreleased

The first version. Two tabs, read only.

- **Secrets**: every secret in every Key Vault the `az` login can reach, as
  one flat table sorted so that one secret's dev, qa and prod rows are
  adjacent. `v` shows a value for sixty seconds, `y` copies one without
  showing it, `Y` copies the name. The details pane carries the attributes,
  the tags and the versions.
- **Registries**: every repository across every container registry, with the
  tag and manifest counts filling in behind the names. `Enter` opens one
  repository's tags; `y` and `Y` copy references that pull.
- Literal search as you type, with a `key:value` grammar per tab. Forty
  thousand rows re-filter in about two milliseconds.
- Opens from a JSON cache of names and metadata — never values — and refreshes
  on one background thread. The session remembers the tab, the sort and the
  column widths.
- `az-tui doctor` checks the login, both token audiences, and what every vault
  and registry actually answers, and exits non-zero if anything refused.
- Four themes, `NO_COLOR`, and the same `[theme.custom]` palette table
  ticket-tui reads.
- Copying works over SSH and under tmux through OSC 52.

Built without a reachable Azure subscription: every endpoint was verified
against the published REST reference instead. `plan/CHECKLIST.md` is the live
walk-through, and it has not been ticked yet.
