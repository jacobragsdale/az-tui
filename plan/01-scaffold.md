# 01 — Scaffold

**Goal:** a crate that builds, passes the gate in CI, reads `config.toml`,
paints a themed empty screen, and quits on `q`.

## Read first

- ticket-tui `Cargo.toml`, `.github/workflows/ci.yml`, `src/main.rs`
- ticket-tui `src/ui/theme.rs` — the whole file; you are lifting most of it
- ticket-tui `src/config.rs` — the `ThemeSection`, `Palette`, `Appearance`
  types and `load()`, `default_path()`; skip the DevOps/agents/Herdr tables
- ticket-tui `src/db.rs` `default_database_path()` — how the data directory
  is found with std alone

## Build

### Cargo.toml

```toml
[package]
name = "az-tui"
version = "0.1.0"
edition = "2024"
rust-version = "1.88"
description = "A fast read-only terminal browser for Azure Key Vault secrets and Container Registry images"
license = "MIT"
repository = "https://github.com/jacobragsdale/az-tui"

[dependencies]
anyhow = "1"
clap = { version = "4", features = ["derive"] }
crossterm = "0.29"
nucleo-matcher = "0.3"
ratatui = { version = "0.30", features = ["unstable-rendered-line-info"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tempfile = "3"
time = { version = "0.3", features = ["formatting", "macros", "parsing"] }
toml = "0.9"
ureq = { version = "3", features = ["json"] }

[profile.release]
lto = "thin"
strip = "symbols"
```

Run `cargo update` once so `Cargo.lock` pins what resolves today, and commit
the lock file.

### Files

- `src/main.rs` — exactly ticket-tui's: call `run::run()`, print `error: {e:#}`
  to stderr, exit 1.
- `src/lib.rs` — `pub mod` for `cli`, `config`, `paths`, `ui`.
- `src/cli.rs` — a clap `Cli` with the flags from the overview (all optional)
  and a `Command` enum holding only `Doctor` for now (it does nothing yet;
  step 02 fills it). `--theme` accepts `terminal`, `terminal-light`, `mono`,
  `custom`.
- `src/paths.rs` — `config_dir()` and `data_dir()`:
  `$XDG_CONFIG_HOME/az-tui` else `~/.config/az-tui`; `$XDG_DATA_HOME/az-tui`
  else `~/.local/share/az-tui` on Linux and `~/Library/Application Support/az-tui`
  on macOS. `AZ_TUI_CONFIG` and `--config` name the file directly. Tested with
  the environment passed in as a closure, not read inside.
- `src/config.rs` — `Config { azure: Azure, theme: ThemeSection }` with serde
  `#[serde(default)]` on everything so an empty file and a missing file both
  give `Config::default()`. `Azure { subscriptions: Vec<String>, vaults: Vec<String>, registries: Vec<String>, refresh: Option<u64> }`.
  Unknown keys are ignored, never an error. A file that does not parse is an
  error that names the path and the line. Lift `ThemeSection`/`Palette`/
  `Appearance` verbatim so a `[theme.custom]` table written by the `theme`
  tool for ticket-tui works here unchanged.
- `src/ui/theme.rs` — lift ticket-tui's. Keep these tokens: `accent`, `muted`,
  `text`, `body`, `link`, `header`, `border`, `border_focused`, `surface`,
  `selected_background`, `selection_fg`, `hover_background`, `info`,
  `success`, `warning`, `error`, `scrollbar`, `search_match`, `tag_palette`,
  `border_type`, `dim_behind_modals`. Drop the work-item state, type and
  priority tokens. Keep the four presets, `from_env` reading `AZ_TUI_THEME`
  and `NO_COLOR`, `chosen_theme`, `set_theme`, `theme()`, and the custom
  palette mapping. Keep its tests.
- `src/ui/mod.rs` — `render(frame, &App)` that paints a bordered block titled
  ` az-tui ` in the theme's border colour with the text "Nothing read yet —
  press r" in `muted`, and the "Terminal too small" guard at 36 × 11.
- `src/run.rs` — load the config, parse the CLI with its defaults, resolve the
  theme, `ratatui::init()`, loop: draw, `event::poll(250 ms)`, quit on `q` or
  `Ctrl-C`; `ratatui::restore()` on every exit path including a panic (a
  guard struct whose `Drop` restores, as ticket-tui's `TerminalRestore`).
  Enable mouse capture and bracketed paste now so later steps do not have to.
- `.github/workflows/ci.yml` — ticket-tui's, with `TICKET_TUI_THEME` renamed
  `AZ_TUI_THEME`. Ubuntu and macOS. Runs on push to `main` and on
  `workflow_dispatch`.
- `config.example.toml` at the root: the `[azure]` table with every key
  commented, the `[theme]` table, and a full `[theme.custom]` palette copied
  from ticket-tui's example.

## Tests

- `paths`: each directory resolves from `XDG_*`, from `HOME`, and `AZ_TUI_CONFIG`
  wins over both.
- `config`: an empty string, a file with only `[azure]`, a file with unknown
  keys, and a broken file (error names the line).
- `theme`: the lifted tests, adjusted to the trimmed token set.

## Done when

- `cargo run` shows the empty framed screen in the terminal's colours; `q`
  quits and the terminal is restored (prompt on a clean line, no stray mouse
  escape codes when you move the pointer afterwards).
- `AZ_TUI_THEME=mono cargo run` paints without colour; `--theme terminal-light`
  paints the light palette.
- `cargo run -- --config /nonexistent` starts with defaults; a config file with
  `preset = "nope"` is refused with the path in the message.
- The gate is green locally and CI is green on GitHub for the pushed commit.

## Not in this step

No network, no `az`, no tables, no search. `doctor` is a stub.
