# az-tui

A fast, read-only terminal browser for Azure Key Vault secrets and Azure
Container Registry images. Type part of a name, see it across every vault and
registry you can reach, and copy the value or the image reference with one key.

It is the sibling of [ticket-tui](https://github.com/jacobragsdale/ticket-tui):
the same stack (Rust, ratatui, crossterm), the same layout, the same keys.

**Status: planning.** The implementation plan is in [plan/](plan/00-overview.md).
Each numbered file is one step, meant to be picked up in order by a coding
agent or a person; the overview says how the steps fit and what never bends.

```
 1 Secrets  2 Registries                                                      ?
/ db-pass
╭ Secrets 3/412 · Name ↑ ──────────────────────────╮╭ Details ─────────────────────╮
│  Vault     Name          Enabled  Expires  Updated││ db-password                  │
│──────────────────────────────────────────────────││ kv-prod · secret · enabled   │
│› kv-prod   db-password   ✓        —        3d     ││ Value        ••••••••        │
│  kv-qa     db-password   ✓        —        3d     ││ Content type text/plain      │
│  kv-dev    db-password   ✓        12d      3d     ││ Expires      —               │
│                                                  ││ Updated      3d              │
╰──────────────────────────────────────────────────╯╰──────────────────────────────╯
 ↑↓/jk move  / search  y copy value  v reveal  r refresh   ● 3 vaults · 12 s ago
```

## What it will do

- Open instantly from a local cache of names and metadata, then refresh from
  Azure in the background. Secret values are never cached, logged or written
  anywhere.
- Search literally, as you type, across every vault or registry at once:
  `db-pass`, `vault:kv-prod enabled:no`, `expires:<30d`.
- `y` copies a secret's value without showing it; `v` shows it for 60 seconds.
  On an image, `y` copies `registry.azurecr.io/repo:tag`.
- Borrow the Azure CLI's login (`az login`); nothing else to set up. One
  optional `~/.config/az-tui/config.toml` names subscriptions, vaults,
  registries and a theme.
- Read only. It never creates, changes or deletes anything in Azure.

## License

MIT, see [LICENSE](LICENSE).
