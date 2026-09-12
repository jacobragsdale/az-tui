# 07 — Details, reveal, copy

**Goal:** the details pane beside the table; a secret's versions on
selection; `v` to show a value for sixty seconds; `y`/`Y`/`o`; a clipboard
that works over SSH.

## Read first

- `git -C ~/dev/ticket-tui show 6f73eef^:src/app/key_vault/mod.rs` — search
  for `REVEAL_FOR`, `revealed`, `reveal_pending`, `fn reveal`, `fn copy_value`,
  `fn tick`. This is the reveal state machine; lift its shape.
- `git -C ~/dev/ticket-tui show 6f73eef^:src/ui/key_vault.rs` —
  `render_revealed`, `MASK`, and the details layout.
- ticket-tui `src/run/desktop.rs` — `copy_to_clipboard`, `clipboard_commands`,
  `write_to_command`, `open_in_browser`, `browser_commands`, `is_wsl`,
  `cmd_escape`. Lift whole into `src/clipboard.rs` and `src/desktop.rs`.
- ticket-tui `src/ui/panes.rs` `PanePair` and `src/ui/details.rs`
  `field_line`/`section_line` — the two-pane split and the label/value rows.

## Build

### The pane split

Wide (≥ 110): table 55 % left, details right, a shared vertical border.
Narrow (< 110): table above, details below, 60/40. Under 70 columns: details
hidden; `Tab` shows it full-width in place of the table and `Tab`/`Esc`
brings the table back. `Tab` otherwise moves focus between the two; the
focused pane's border is `border_focused`. `j`/`k` in the details pane
scroll it.

### The Secrets details pane

For the row under the cursor:

```
db-password                                   ← bold, `text`
kv-prod · secret · enabled · 3 versions       ← `muted`; "disabled" in `error`

Value         ••••••••                 v · y  ← the mask, always eight dots
Content type  text/plain
Expires       —                               ← or `2027-01-01 · in 12d` in `warning` / `expired 3d ago` in `error`
Not before    —
Created       2026-03-01 · 6mo
Updated       2026-09-08 · 3d
Tags          env=prod  owner=platform         ← each a chip in `tag_palette[hash]`
Managed       no
Id            https://kv-prod.vault.azure.net/secrets/db-password   ← `link`

── Versions ────────────────────────
8f3a2c1d…   3d    enabled   current           ← first eight chars of the version id
1c2b90aa…   4mo   enabled
                                               ← "reading…" with a spinner until they land; the error if they do not
```

Versions are requested (`Request::Versions`) when the cursor lands on a row
whose versions the store does not hold, **after** 150 ms of the cursor
resting there (holding `j` down must not fire four hundred requests). Once
read they stay for the run.

### Reveal

- `v` or `Enter` on a secret: if the store holds no value for it, send
  `Request::Value` and show `reading…` in the Value line with a spinner. When
  `Applied::Value(secret)` arrives for the row still under the cursor, keep
  it as `revealed: Option<Revealed { vault, name, version, value: Secret, at: Instant }>`
  on the screen — the one field in the crate that holds a `Secret` — and
  paint `value.expose()` in the Value line, with `clears in 47 s` after it in
  `muted`, counting down (the run loop wakes every second while something is
  revealed).
- `v` again, or moving the cursor, or `r`, or switching tabs, or sixty
  seconds: `revealed = None`. The mask comes back.
- A value that arrives for a row the cursor has left is dropped on the floor.
- A long value wraps inside the pane; a multi-line value (a PEM) shows its
  first line and `(+14 lines)`, and `y` copies all of it.
- The refusal (`403`) shows in the Value line in `error` and stays until the
  cursor moves.

### Copy

- `y` on a secret: if a value is revealed, copy it; otherwise send
  `Request::Value` and copy when it lands (the status bar says `reading…`
  then `Copied value of db-password (kv-prod)`). The value is **not** shown
  by `y`; copying blind is the common case.
- `Y`: copy the secret's name. `o`: open `portal_url(vault.id) + "/secrets"`.
- The status line names what was copied, never its content.

### `src/clipboard.rs`

Two channels, both tried, in this order:

1. **OSC 52** to the terminal: write `ESC ] 52 ; c ; <base64 of the text> BEL`
   to stdout and flush. This is what makes copying work over SSH and inside
   a VDI, where no `xclip` can reach the real clipboard. Under tmux the
   sequence has to be passed through: tell the user in the README to set
   `set -s set-clipboard on` (tmux ≥ 3.3). The base64 encoder is the
   fifteen-line function from ticket-tui's `azure.rs`; keep it there rather
   than adding a crate.
2. The external command that works: `pbcopy` on macOS; `wl-copy
   --trim-newline`, `xclip -selection clipboard`, `xsel --clipboard --input`
   on Linux; `clip.exe` under WSL. Lifted from ticket-tui, plus the WSL entry.

Success is "OSC 52 was written" — the terminal gives no receipt — so the
notification says `Copied value of db-password (kv-prod)` when either channel
went through, and `Could not copy: no clipboard command found and the
terminal may not support OSC 52` only when the external commands all fail
(OSC 52 is still written, so it may in fact have worked). Neither channel
logs the text.

`copy(text: &str) -> Result<()>` takes `&str`; the `Secret` is exposed at the
call site in `secrets.rs` and nowhere else.

### The run loop

- Poll timeout 1 s while `revealed.is_some()` so the countdown repaints, and
  on the 150 ms rest timer while a versions request is pending.
- `AppAction::Copy` → `clipboard::copy`; `AppAction::OpenUrl` →
  `desktop::open_in_browser` (HTTPS only, as ticket-tui checks).
- `q` and `Ctrl-C` drop `revealed` before restoring the terminal (the field
  goes with the `App`, but say it in a comment so nobody adds a
  "remember the last reveal" feature).

## Tests

- Reveal state machine with a fake clock: `v` sends one request; the value
  lands and is shown; 60 s later it is gone; a value landing for another row
  is discarded; `r` clears it; `j` clears it.
- `y` without a reveal sends the request and copies on arrival; `y` with a
  reveal copies at once and sends nothing.
- The rendered details pane (`TestBackend`) shows eight dots before a reveal
  and the value after, and the `Debug` of the whole `App` never contains the
  value (`format!("{app:?}")` — give `App` a `Debug` that goes through
  `Secret`'s).
- The versions request fires once per row after the rest interval, not per
  keystroke: hold `j` across ten rows with a fake clock and assert one
  request.
- `clipboard`: the OSC 52 bytes for `hello` are exactly
  `\x1b]52;c;aGVsbG8=\x07`; the base64 encoder on the RFC 4648 vectors.
- `human` ages and the expiry colouring at 31 d, 30 d, 0 d, −1 d.

## Done when

- Signed in: select a secret you may read, `v` shows it and counts down, `y`
  copies it (paste somewhere to check), `Y` copies the name, `o` opens the
  vault's secrets blade in the browser.
- Over an SSH session in a terminal that supports OSC 52 (kitty, WezTerm,
  Alacritty, foot, iTerm2, Windows Terminal, VS Code's), `y` still lands the
  value on the local clipboard.
- A secret you may not read shows the refusal under Value and nothing else
  changes.
- `grep -rn "expose()" src/` shows exactly two call sites: the Value line and
  `copy` (step 10's `secret get` adds a third, and no more).
- The gate is green.
