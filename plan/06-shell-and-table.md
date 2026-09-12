# 06 — The shell and the Secrets table

**Goal:** the frame every screen shares — tab bar, status bar, the list table,
the search row — and the Secrets tab as its first user: a sortable, searchable
table of every secret across every vault. No details pane yet.

## Read first

- ticket-tui `src/app/screen.rs` — the `Screen` trait and `TabId`. Lift the
  trait's shape, trimmed (below); leave out the mouse-drag machinery.
- ticket-tui `src/columns.rs` — `ColumnId`, `ColumnConfig`, `TableLayout`,
  `visible_columns()`, the width constants. Lift whole.
- ticket-tui `src/ui/table.rs` — `TableGeometry`, `TableSpec`,
  `render_list_table`, `highlight_line`. Lift, dropping the marker gutter and
  the work-item-specific colour helpers.
- ticket-tui `src/ui/widgets.rs` — `SearchRow`/`render_search_row`,
  `render_status_bar`, `render_modal_frame`, `render_scrollbar`,
  `spinner_frame`, `dim_behind`. Lift what these need and nothing else.
- ticket-tui `src/text_input.rs` — lift whole (it is the search box).
- ticket-tui `src/search.rs` — `QueryHighlighter` and the `Pattern::new(…,
  CaseMatching::Ignore, Normalization::Smart, AtomKind::Substring)` usage. Do
  **not** lift the worker thread or the score sort.
- ticket-tui `src/ui/mod.rs` `render_tab_bar` — names shorten before they
  drop, badges paint in `warning`.
- ticket-tui `src/app/cursor.rs` — `ListCursor` (index, offset, viewport,
  `move_by`, `page`, `clamp`). Lift whole.

## Build

### `src/app/screen.rs`

```rust
pub enum TabId { Secrets, Registries }   // label(), short_label(), number() '1' '2', ALL, from_number()

pub trait Screen {
    fn handle_key(&mut self, shell: &mut Shell, store: &Store, key: KeyEvent) -> AppAction;
    fn handle_click(&mut self, shell: &mut Shell, store: &Store, target: Target) -> AppAction;
    fn handle_wheel(&mut self, shell: &mut Shell, target: Option<Target>, delta: i32);
    fn on_store_changed(&mut self, store: &Store, applied: &Applied);   // keep the cursor on the same row after a refresh
    fn badge(&self, store: &Store) -> Option<String>;
    fn footer_hint(&self, shell: &Shell) -> String;
    fn render(&mut self, frame: &mut Frame, shell: &mut Shell, store: &Store, area: Rect);
}

pub enum AppAction { None, Copy { text: String, label: String }, Send(worker::Request), OpenUrl(String), Quit }
```

`Target` is the hit-region enum: `Tab(TabId)`, `Row(usize)`, `Header(&'static str)`,
`SearchField`, `ClearSearch`, `Details`, `Help`. The shell keeps a
`Vec<(Rect, Target)>` rebuilt every frame; a click resolves the **last**
region containing the point (drawn last = on top). Ten lines, not
`pointer.rs`.

### `src/app/shell.rs`

What every screen shares and no screen owns: `focus: Focus { Table, Details, Search }`,
`should_quit`, the hit regions, `notification: Option<(String, Instant, Level)>`
(shown in the status bar for 4 s: `Copied value of db-password (kv-prod)`),
`set_status`, `set_error`, `help_open`, the pane split (wide/narrow/hidden
from the terminal width; a fixed 55/45 split — no drag in v1, a
`// ponytail:` comment says so).

### `src/app/mod.rs`

`App { shell, store, tab, secrets: SecretsScreen, registries: RegistriesScreen }`.
Global keys handled here before the screen sees them: `1`/`2` switch tabs,
`?` toggles help, `q`/`Ctrl-C` quit — unless the search box has focus, in
which case every key goes to the box except `Esc`/`Enter`. `r` sends
`Request::Refresh` and clears any revealed value (07).

### `src/search.rs` and `src/filter.rs`

`filter::parse(query) -> Query { words: Vec<String>, fields: Vec<(Field, String)> }`:
split on whitespace; a token `key:value` whose key the tab's schema knows is
a field filter, anything else a word. Quoted values are not supported in v1
(`// ponytail:` note).

`search::matches(row_text: &str, words: &[String]) -> bool` builds one nucleo
`Pattern` per query (not per row) with `AtomKind::Substring` and asks
`pattern.score(Utf32Str)` — `Some` means every atom matched literally. The
screen filters `store.secrets` into `visible: Vec<usize>` on every change of
query, rows or sort. Build the `Utf32String` of each row once per store
change, not per keystroke.

`QueryHighlighter::indices(cell_text)` lights the matched characters in the
Name and Vault cells in `theme().search_match`.

Secrets schema for `filter`: `vault:` (substring of the vault name),
`name:`, `type:` (content type substring), `enabled:yes|no|true|false`,
`managed:yes|no`, `expires:<Nd | >Nd | none | expired`, `tag:key` or
`tag:key=value`. All ANDed with the words.

### `src/app/secrets.rs` — the Secrets screen

Columns (`ColumnId`): `Vault` 12, `Name` flexible, `Enabled` 7, `Expires` 9,
`Updated` 8, `Type` 14 (hidden by default), `Created` 8 (hidden). Default
sort: `Name` ascending, then `Vault` in config order as the tiebreak so
`db-password` reads dev, qa, prod.

- `Expires` cell: `—` for none; `12d` in `warning` when ≤ 30 days; `expired`
  in `error`; otherwise the plain relative age. `Enabled`: `✓` or `✗` in
  `muted`.
- Rows of a vault whose last read failed paint in `muted` (stale).
- Sort: `s` moves to the next visible column, `S` flips; a header click
  cycles asc → desc → default the way ticket-tui does. The bottom border
  says `412/412 · Name ↑` (or `3/412` under a query).
- Badge: `⚠ N` where N is secrets expiring within 30 days (or already
  expired) that are enabled.
- `on_store_changed`: keep the cursor on the same `(vault, name)` if it is
  still visible, else clamp.
- `footer_hint`: `↑↓/jk move  / search  s sort  r refresh  ? help` (07 adds
  the copy keys).

### `src/ui/mod.rs`, `src/ui/widgets.rs`, `src/ui/secrets.rs`

- `render()`: the "too small" guard; one row for the tab bar; the screen; one
  row for the status bar. The status bar shows the notification if there is
  one, else the footer hint on the left and the store's state on the right:
  `● 3 vaults · 412 secrets · 12 s ago`, `◐ reading kv-prod (2/3)…`, or
  `! kv-prod: no permission to list secrets` in `error` (the first problem;
  `?` lists them all).
- The search row is the screen's first line: `/ ` glyph, the field, a `×`
  clear button once there is text, and the placeholder `Type / to search, or
  vault:kv-prod enabled:no expires:<30d`.
- Help (`?`): a centred modal listing every key from one static table (the
  same table `handle_key` matches on, so they cannot drift), then the
  problems list. `Esc`, `?` or a click outside closes it.

## Tests

- `filter::parse`: words vs fields, unknown key stays a word, `expires:<30d`.
- `search::matches`: literal, case-insensitive, all words required,
  scattered letters rejected (`tks` does not match `ticket search`).
- Secrets screen with a fixture store: sort by each column both ways; a query
  narrows and the count reads `3/412`; cursor survives a refresh that
  reorders; the badge counts only enabled expiring secrets.
- Renderer tests through `ratatui::backend::TestBackend` at 120 × 30 and
  60 × 20: the header row, a highlighted match, the stale row style, the tab
  badge, the status bar's three states. Assert through `theme()` tokens so
  the theme matrix in the gate is meaningful.
- Hit regions: a click on row 3 selects it; on a header sorts; on `2`
  switches tabs; on `×` clears the query.

## Done when

- Signed in or from cache: open, type `/db-pass`, see the rows narrow as you
  type with no visible lag; `Esc`, `s`, `S`, header clicks, `1`/`2`, `?`,
  `q` all do what the key table says.
- 40k fixture rows (a test builds them) filter in under 5 ms per keystroke
  in a release build — assert it in an `#[ignore]` test that prints the time.
- The gate is green, including the theme matrix.

## Not in this step

The details pane, reveal, copy (07). The Registries screen renders a
placeholder line (08).
