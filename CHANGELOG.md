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
  in the background, `parallel` vaults or repositories at a time (eight by
  default). The session remembers the tab, the sort and the
  column widths.
- `az-tui doctor` checks the login, both token audiences, and what every vault
  and registry actually answers, and exits non-zero if anything refused.
- Four themes, `NO_COLOR`, and the same `[theme.custom]` palette table
  ticket-tui reads.
- Copying works over SSH and under tmux through OSC 52, and the status bar
  says when the terminal was the only channel that took it.
- A review pass over correctness, resiliency, performance, ergonomics and
  design: a registry's token chain is rebuilt from a fresh CLI token when it
  is spent, so a TUI left open past the hour keeps its registries; a throttle
  wait is said on screen before it is taken; a quit never waits on a request
  in flight; a refresh that read nothing writes no cache; the cache is
  written in one call rather than one per token; pasting into the search box
  works; `Esc` backs out of a repository; the help wraps and lists the search
  grammar; a listing with a vault that would not answer exits 1 after the
  rows that did; naming a vault the login cannot reach exits 2 and says which
  it can.

Built without a reachable Azure subscription: every endpoint was verified
against the published REST reference instead. `plan/CHECKLIST.md` is the live
walk-through, and it has not been ticked yet.
