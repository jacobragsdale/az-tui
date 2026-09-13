//! The Secrets tab: every secret in every vault as one flat table.
//!
//! The point of the tab is the question "what is `db-password` in each
//! environment?", so the default sort is by name with the vault as the
//! tiebreak — dev, qa and prod end up adjacent — and the vault tiebreak
//! follows the inventory's order rather than the alphabet, because that is
//! the order the configuration named them in.

use std::collections::HashMap;

use std::time::{Duration, Instant};

use super::cursor::ListCursor;
use super::screen::{AppAction, Target};
use super::shell::{Focus, Shell};
use super::{flip, none_last};
use crate::azure::{Secret, SecretRow};
use crate::columns::{ColumnId, SECRET_COLUMNS, TableLayout};
use crate::filter::{self, Env, Query, When};
use crate::store::Store;
use crate::text_input::TextInput;
use crate::timestamp::Timestamp;

/// Inside this many days, an expiry is worth a colour and a badge.
pub const EXPIRING_SOON: i64 = 30;

/// How long a revealed value stays on screen. Long enough to read one off
/// and type it somewhere, short enough that a walked-away-from terminal is
/// not showing a production password.
pub const REVEAL_FOR: Duration = Duration::from_secs(60);

/// How long the cursor has to sit on a row before its versions are asked
/// for. Holding `j` down across four hundred rows must not be four hundred
/// requests.
pub const REST: Duration = Duration::from_millis(150);

/// The `key:` filters this tab knows. Everything else typed is a word.
pub const SCHEMA: &[&str] = &[
    "env", "vault", "name", "type", "enabled", "managed", "expires", "tag",
];

/// What a row looks like to the search: every cell a person might type part
/// of, joined once per store change rather than per keystroke.
#[must_use]
pub fn haystack(row: &SecretRow) -> String {
    let mut text = String::with_capacity(row.vault.len() + row.name.len() + 32);
    text.push_str(&row.vault);
    text.push(' ');
    text.push_str(&row.name);
    if let Some(content_type) = &row.content_type {
        text.push(' ');
        text.push_str(content_type);
    }
    for (key, value) in &row.tags {
        text.push(' ');
        text.push_str(key);
        text.push('=');
        text.push_str(value);
    }
    text
}

/// Whether one row answers every `key:value` in the query. The words are
/// [`crate::search`]'s job; this is only the fields.
#[must_use]
pub fn passes(row: &SecretRow, query: &Query, now: Timestamp) -> bool {
    for (key, value) in &query.fields {
        let holds = match key.as_str() {
            // Half-typed `env:p` names nothing yet and passes everything,
            // like a boolean that is not yet a yes or a no.
            "env" => Env::of(value).is_none_or(|want| Env::of(&row.vault) == Some(want)),
            "vault" => filter::contains(&row.vault, value),
            "name" => filter::contains(&row.name, value),
            "type" => row
                .content_type
                .as_deref()
                .is_some_and(|held| filter::contains(held, value)),
            // A filter whose value is not a yes or a no is ignored rather
            // than matching nothing: half-typed `enabled:` should not empty
            // the table.
            "enabled" => filter::boolean(value).is_none_or(|want| row.enabled == want),
            "managed" => filter::boolean(value).is_none_or(|want| row.managed == want),
            "expires" => When::parse(value).is_none_or(|when| when.holds(row.expires, now)),
            "tag" => filter::tag_matches(&row.tags, value),
            _ => true,
        };
        if !holds {
            return false;
        }
    }
    true
}

/// How an expiry reads in its cell, and whether it is worth a colour.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Expiry {
    /// No expiry set. The common case, and not a problem.
    None,
    /// Already past.
    Expired,
    /// Inside [`EXPIRING_SOON`] days.
    Soon,
    /// Further off than that.
    Later,
}

impl Expiry {
    #[must_use]
    pub fn of(expires: Option<Timestamp>, now: Timestamp) -> Self {
        let Some(expires) = expires else {
            return Self::None;
        };
        let seconds = now.seconds_until(expires);
        if seconds < 0 {
            Self::Expired
        } else if seconds / 86_400 <= EXPIRING_SOON {
            Self::Soon
        } else {
            Self::Later
        }
    }

    /// What the Expires cell says. An expiry inside the window carries its
    /// own mark as well as its colour, so it still reads under `NO_COLOR`.
    #[must_use]
    pub fn cell(self, expires: Option<Timestamp>, now: Timestamp) -> String {
        let age = expires.map(|expires| expires.relative_age(now));
        match self {
            Self::None => "—".to_owned(),
            Self::Expired => "expired".to_owned(),
            Self::Soon => format!("{} ⚠", age.unwrap_or_default()),
            Self::Later => age.unwrap_or_default(),
        }
    }
}

/// How many enabled secrets are expiring or already expired — the number on
/// the tab's badge. A disabled secret's expiry is nobody's problem.
#[must_use]
pub fn expiring(rows: &[SecretRow], now: Timestamp) -> usize {
    rows.iter()
        .filter(|row| {
            row.enabled && matches!(Expiry::of(row.expires, now), Expiry::Expired | Expiry::Soon)
        })
        .count()
}

/// The columns this table can be sorted by, in the order `s` walks them.
/// Only what is on screen: sorting by a hidden column would move the rows for
/// a reason nobody could see.
#[must_use]
pub fn sortable(layout: &TableLayout, available: u16) -> Vec<ColumnId> {
    layout
        .visible_columns(available)
        .into_iter()
        .map(|column| column.id)
        .collect()
}

/// Where each vault sits in the inventory, so `db-password` reads dev, qa,
/// prod rather than alphabetically. The configuration's order is an opinion;
/// the alphabet is not.
#[must_use]
pub fn vault_order(store: &Store) -> HashMap<&str, usize> {
    store
        .inventory
        .vaults
        .iter()
        .enumerate()
        .map(|(at, vault)| (vault.name.as_str(), at))
        .collect()
}

/// Sorts `indices` into `rows` by one column, with the name and then the
/// vault as the tiebreaks — so the rows of one secret always end up adjacent
/// whatever the sort, which is the whole reason this table is flat.
pub fn sort(
    indices: &mut [usize],
    rows: &[SecretRow],
    by: ColumnId,
    descending: bool,
    order: &HashMap<&str, usize>,
) {
    indices.sort_by(|a, b| {
        let (left, right) = (&rows[*a], &rows[*b]);
        let ordering = match by {
            ColumnId::Env => none_last(Env::of(&left.vault), Env::of(&right.vault), descending),
            ColumnId::Vault => flip(
                order
                    .get(left.vault.as_str())
                    .cmp(&order.get(right.vault.as_str())),
                descending,
            ),
            ColumnId::Enabled => flip(left.enabled.cmp(&right.enabled), descending),
            ColumnId::Expires => none_last(left.expires, right.expires, descending),
            ColumnId::Updated => none_last(left.updated, right.updated, descending),
            ColumnId::Created => none_last(left.created, right.created, descending),
            ColumnId::Type => flip(left.content_type.cmp(&right.content_type), descending),
            _ => std::cmp::Ordering::Equal,
        };
        ordering
            .then_with(|| flip(cmp_ignore_ascii_case(&left.name, &right.name), descending))
            .then_with(|| {
                order
                    .get(left.vault.as_str())
                    .cmp(&order.get(right.vault.as_str()))
            })
    });
}

/// Two names, compared without regard to ASCII case and without allocating:
/// this runs a million times in a sort of forty thousand rows.
fn cmp_ignore_ascii_case(left: &str, right: &str) -> std::cmp::Ordering {
    left.bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .cmp(right.bytes().map(|byte| byte.to_ascii_lowercase()))
}

/// A value, on screen, and when it got there.
///
/// **This is the one field in the crate that holds a [`Secret`].** It is
/// dropped when the cursor moves, on `r`, on a tab switch, on `v` again, and
/// sixty seconds after it arrived — and it goes with the `App` on quit, which
/// is why there is nothing here that could remember it for next time.
pub struct Revealed {
    pub vault: String,
    pub name: String,
    /// The version the vault actually handed over.
    pub version: String,
    value: Secret,
    at: Instant,
}

impl Revealed {
    /// The value, for the one line that is about to draw it or the one key
    /// that is about to copy it.
    ///
    /// This and the blind-copy arm of [`SecretsScreen::on_value`] are the
    /// only two calls to [`Secret::expose`] in the crate outside the tests —
    /// a grep for that method name over `src/` is the audit — and this one
    /// funnels the two places that draw and copy a value already on screen.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.value.expose()
    }

    /// How many lines the value has, without reading it out.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.value.line_count()
    }

    /// Whole seconds until it goes.
    #[must_use]
    pub fn clears_in(&self, now: Instant) -> u64 {
        REVEAL_FOR
            .saturating_sub(now.saturating_duration_since(self.at))
            .as_secs()
    }

    #[must_use]
    pub fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.at) >= REVEAL_FOR
    }
}

impl std::fmt::Debug for Revealed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Goes through `Secret`'s, so `format!("{app:?}")` cannot print one.
        formatter
            .debug_struct("Revealed")
            .field("vault", &self.vault)
            .field("name", &self.name)
            .field("version", &self.version)
            .field("value", &self.value)
            .finish()
    }
}

/// The Secrets tab.
pub struct SecretsScreen {
    pub cursor: ListCursor,
    pub layout: TableLayout,
    pub input: TextInput,
    pub sort: ColumnId,
    pub descending: bool,
    /// Which rows of `store.secrets` are shown, in the order they are shown.
    visible: Vec<usize>,
    /// Every row, in sort order. A query filters this rather than the store,
    /// so the shown rows come out sorted without being sorted again — which
    /// is the difference between a keystroke costing a substring scan and a
    /// keystroke costing a sort of forty thousand rows.
    sorted: Vec<usize>,
    /// One searchable string per row of `store.secrets`, built when the rows
    /// change rather than when the query does.
    haystacks: Vec<String>,
    /// What `sorted` was last built from.
    ordered_for: Option<(ColumnId, bool, usize)>,
    /// What `visible` was last built from, so a redraw that changed nothing
    /// does not rebuild it.
    built_for: Option<(String, ColumnId, bool, usize)>,
    /// The width the columns were last solved at, which is what `s` walks.
    available: u16,
    /// The one place a value lives. See [`Revealed`].
    revealed: Option<Revealed>,
    /// A value request that has gone out and not come back, and whether it
    /// was `y` rather than `v` that sent it.
    reading: Option<Reading>,
    /// What the vault said when it would not hand one over. Cleared when the
    /// cursor moves.
    refusal: Option<String>,
    /// Where the cursor is and when it got there, for the rest interval that
    /// gates the versions request.
    rested: Option<(usize, Instant)>,
    /// The rows whose versions have already been asked for this run, so a
    /// cursor coming back to one costs nothing.
    asked: std::collections::HashSet<(String, String)>,
    /// How far down the details pane is scrolled.
    pub details_scroll: super::cursor::ScrollState,
}

/// A value request in flight.
#[derive(Debug)]
struct Reading {
    vault: String,
    name: String,
    /// `y` sent it, so the answer goes to the clipboard rather than the
    /// screen. `v` sent it otherwise.
    copy: bool,
}

impl Default for SecretsScreen {
    fn default() -> Self {
        Self {
            cursor: ListCursor::default(),
            layout: TableLayout::new(SECRET_COLUMNS),
            input: TextInput::default(),
            sort: ColumnId::Name,
            descending: false,
            visible: Vec::new(),
            sorted: Vec::new(),
            haystacks: Vec::new(),
            ordered_for: None,
            built_for: None,
            available: 0,
            revealed: None,
            reading: None,
            refusal: None,
            rested: None,
            asked: std::collections::HashSet::new(),
            details_scroll: super::cursor::ScrollState::default(),
        }
    }
}

impl SecretsScreen {
    /// The rows on screen, as indices into `store.secrets`.
    #[must_use]
    pub fn visible(&self) -> &[usize] {
        &self.visible
    }

    /// The row under the cursor.
    #[must_use]
    pub fn selected<'a>(&self, store: &'a Store) -> Option<&'a SecretRow> {
        store.secrets.get(*self.visible.get(self.cursor.index)?)
    }

    /// Rebuilds the shown rows when the query, the sort or the rows have
    /// moved. Cheap to call every frame: it compares first, and a keystroke
    /// only ever re-runs the filter.
    pub fn refilter(&mut self, store: &Store) {
        let order_key = (self.sort, self.descending, store.secrets.len());
        if self.ordered_for != Some(order_key) {
            self.ordered_for = Some(order_key);
            self.built_for = None;
            self.reorder(store);
        }
        let key = (
            self.input.text().to_owned(),
            self.sort,
            self.descending,
            store.secrets.len(),
        );
        if self.built_for.as_ref() == Some(&key) {
            return;
        }
        self.built_for = Some(key);

        let now = Timestamp::now();
        let parsed = Query::parse(self.input.text(), SCHEMA);
        let mut words = crate::search::Query::new(&parsed.words);
        self.visible = self
            .sorted
            .iter()
            .copied()
            .filter(|at| {
                passes(&store.secrets[*at], &parsed, now) && words.matches(&self.haystacks[*at])
            })
            .collect();
        self.cursor.clamp(self.visible.len());
    }

    /// Every row, in sort order, and the searchable text of each. Run when
    /// the rows or the sort change — once a refresh and once a keypress of
    /// `s`, not once a keystroke of the query.
    fn reorder(&mut self, store: &Store) {
        if self.haystacks.len() != store.secrets.len() {
            self.haystacks = store.secrets.iter().map(haystack).collect();
        }
        self.sorted = (0..store.secrets.len()).collect();
        sort(
            &mut self.sorted,
            &store.secrets,
            self.sort,
            self.descending,
            &vault_order(store),
        );
    }

    /// Forces the next `refilter` to do the work, after the rows underneath
    /// have moved.
    pub fn invalidate(&mut self) {
        self.built_for = None;
        self.ordered_for = None;
        self.haystacks.clear();
    }

    /// `s`: the next column on screen. `S`: the same column the other way.
    pub fn next_sort(&mut self) {
        let columns = sortable(&self.layout, self.available);
        if columns.is_empty() {
            return;
        }
        let at = columns.iter().position(|held| *held == self.sort);
        self.sort = columns[at.map_or(0, |at| (at + 1) % columns.len())];
        self.descending = false;
    }

    /// A header click: the same column cycles ascending, descending, then
    /// back to the default; a different column starts ascending.
    pub fn sort_by(&mut self, column: ColumnId) {
        if self.sort == column {
            if self.descending {
                self.sort = ColumnId::Name;
                self.descending = false;
            } else {
                self.descending = true;
            }
        } else {
            self.sort = column;
            self.descending = false;
        }
    }

    /// What the bottom border says: how many rows of how many, and the sort.
    #[must_use]
    pub fn status(&self, store: &Store) -> String {
        let arrow = if self.descending { "↓" } else { "↑" };
        if self.visible.len() == store.secrets.len() {
            format!("{} · {} {arrow}", store.secrets.len(), self.sort.label())
        } else {
            format!(
                "{}/{} · {} {arrow}",
                self.visible.len(),
                store.secrets.len(),
                self.sort.label()
            )
        }
    }

    /// Remembers the width the columns were solved at, so `s` walks the
    /// columns that are actually on screen.
    pub fn note_width(&mut self, available: u16) {
        self.available = available;
    }

    /// After a refresh: back onto the same secret if it is still shown.
    pub fn keep_cursor(&mut self, store: &Store, was: Option<(String, String)>) {
        self.refilter(store);
        let Some((vault, name)) = was else {
            self.cursor.clamp(self.visible.len());
            return;
        };
        match self
            .visible
            .iter()
            .position(|at| store.secrets[*at].vault == vault && store.secrets[*at].name == name)
        {
            Some(at) => self.cursor.focus(at),
            None => self.cursor.clamp(self.visible.len()),
        }
    }

    /// What the cursor is on, by identity rather than by position, for
    /// putting it back after the rows have moved.
    #[must_use]
    pub fn cursor_identity(&self, store: &Store) -> Option<(String, String)> {
        self.selected(store)
            .map(|row| (row.vault.clone(), row.name.clone()))
    }

    /// One key the shell did not take. Movement, sorting, and — from step 07
    /// — the keys that reveal and copy.
    pub fn handle_key(
        &mut self,
        shell: &mut Shell,
        store: &Store,
        key: crossterm::event::KeyEvent,
    ) -> AppAction {
        use crossterm::event::KeyCode;
        let count = self.visible.len();
        // `j` and `k` scroll the details pane when that is what has focus,
        // so a long value or a long list of versions can be read.
        if shell.focus == Focus::Details {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.details_scroll.scroll_by(1);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.details_scroll.scroll_by(-1);
                }
                KeyCode::PageDown => {
                    self.details_scroll
                        .scroll_by(i32::try_from(self.details_scroll.page_step()).unwrap_or(1));
                }
                KeyCode::PageUp => {
                    self.details_scroll
                        .scroll_by(-i32::try_from(self.details_scroll.page_step()).unwrap_or(1));
                }
                _ => return self.acting_key(shell, store, key),
            }
            return AppAction::None;
        }
        let before = self.cursor.index;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.cursor.move_by(1, count),
            KeyCode::Char('k') | KeyCode::Up => self.cursor.move_by(-1, count),
            KeyCode::PageDown => self.cursor.page(1, count),
            KeyCode::PageUp => self.cursor.page(-1, count),
            KeyCode::Home => self.cursor.focus(0),
            KeyCode::End => self.cursor.move_by(isize::MAX, count),
            KeyCode::Char('s') => self.next_sort(),
            KeyCode::Char('S') => self.descending = !self.descending,
            _ => return self.acting_key(shell, store, key),
        }
        if self.cursor.index != before {
            self.cursor_moved();
            self.details_scroll.scroll_to(0);
        }
        AppAction::None
    }

    /// The keys that act on the row rather than move to it. They work from
    /// either pane, because which pane has focus is not what `y` is about.
    fn acting_key(
        &mut self,
        shell: &mut Shell,
        store: &Store,
        key: crossterm::event::KeyEvent,
    ) -> AppAction {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Char('v') | KeyCode::Enter => self.reveal(store),
            KeyCode::Char('y') => self.copy_value(shell, store),
            KeyCode::Char('Y') => {
                self.selected(store)
                    .map_or(AppAction::None, |row| AppAction::Copy {
                        text: row.name.clone(),
                        label: format!("Copied the name {} ({})", row.name, row.vault),
                    })
            }
            KeyCode::Char('o') => self.open_in_portal(shell, store),
            _ => AppAction::None,
        }
    }

    /// A click on a row moves the cursor there, and takes a revealed value
    /// off the screen with it, as a key would.
    pub fn handle_click(
        &mut self,
        _shell: &mut Shell,
        _store: &Store,
        target: Target,
    ) -> AppAction {
        match target {
            Target::Row(index) => {
                let before = self.cursor.index;
                self.cursor
                    .focus(index.min(self.visible.len().saturating_sub(1)));
                if self.cursor.index != before {
                    self.cursor_moved();
                    self.details_scroll.scroll_to(0);
                }
            }
            Target::Header(column) => self.sort_by(column),
            _ => {}
        }
        AppAction::None
    }

    pub fn handle_wheel(&mut self, _shell: &mut Shell, target: Option<Target>, delta: i32) {
        if target == Some(Target::Details) {
            self.details_scroll.scroll_by(delta);
            return;
        }
        let before = self.cursor.index;
        // The scroll state is from the last draw of the table, which a
        // refresh may have shortened the list under since; measured again
        // here so the window below cannot come out inside out.
        let count = self.visible.len();
        self.cursor
            .scroll
            .set_viewport(self.cursor.scroll.viewport, count);
        self.cursor.scroll.scroll_by(delta);
        // The cursor follows the viewport rather than being left behind it,
        // so what `v` acts on is always something on screen.
        let last = (self.cursor.scroll.offset + self.cursor.scroll.viewport.saturating_sub(1))
            .min(count.saturating_sub(1));
        let first = self.cursor.scroll.offset.min(last);
        self.cursor.index = self.cursor.index.clamp(first, last);
        if self.cursor.index != before {
            self.cursor_moved();
            self.details_scroll.scroll_to(0);
        }
    }

    /// The vault's secrets blade in the portal.
    fn open_in_portal(&self, shell: &mut Shell, store: &Store) -> AppAction {
        let Some(row) = self.selected(store) else {
            return AppAction::None;
        };
        let Some(vault) = store.vault(&row.vault) else {
            shell.set_error(format!("{} is not in the inventory", row.vault));
            return AppAction::None;
        };
        AppAction::OpenUrl(format!("{}/secrets", crate::azure::portal_url(&vault.id)))
    }

    /// `⚠ N`, where N is the enabled secrets running out inside thirty days.
    #[must_use]
    pub fn badge(&self, store: &Store) -> Option<String> {
        let count = expiring(&store.secrets, Timestamp::now());
        (count > 0).then(|| format!("⚠ {count}"))
    }

    #[must_use]
    pub fn footer_hint(&self, shell: &Shell) -> String {
        if shell.focus == Focus::Search {
            return "Esc/Enter keep the filter  Esc again clears it  Ctrl-U empties the box"
                .to_owned();
        }
        "↑↓/jk move  / search  v reveal  y copy value  Y copy name  s sort  r refresh  ? help"
            .to_owned()
    }

    // ── Reveal, copy, and the rest timer ────────────────────────────────

    /// Whether a value for this row has been asked for and not come back.
    #[must_use]
    pub fn is_reading(&self, row: &SecretRow) -> bool {
        self.reading
            .as_ref()
            .is_some_and(|held| held.vault == row.vault && held.name == row.name && !held.copy)
    }

    /// The revealed value, if it is this row's.
    #[must_use]
    pub fn revealed_here(&self, row: &SecretRow) -> Option<&Revealed> {
        self.revealed
            .as_ref()
            .filter(|held| held.vault == row.vault && held.name == row.name)
    }

    #[must_use]
    pub const fn refusal(&self) -> Option<&String> {
        self.refusal.as_ref()
    }

    /// `v` or `Enter`: show it, or hide it if it is already showing.
    fn reveal(&mut self, store: &Store) -> AppAction {
        let Some(row) = self.selected(store) else {
            return AppAction::None;
        };
        if self.revealed_here(row).is_some() {
            self.revealed = None;
            return AppAction::None;
        }
        let (vault, name) = (row.vault.clone(), row.name.clone());
        self.refusal = None;
        self.reading = Some(Reading {
            vault: vault.clone(),
            name: name.clone(),
            copy: false,
        });
        AppAction::Send(crate::worker::Request::Value {
            vault,
            name,
            version: None,
        })
    }

    /// `y`: copy the value without showing it. A value already on screen is
    /// copied at once and nothing is asked for.
    fn copy_value(&mut self, shell: &mut Shell, store: &Store) -> AppAction {
        let Some(row) = self.selected(store) else {
            return AppAction::None;
        };
        if let Some(held) = self.revealed_here(row) {
            return AppAction::Copy {
                // The one other place a value is read out. Nothing about it
                // reaches the label.
                text: held.expose().to_owned(),
                label: format!("Copied value of {} ({})", held.name, held.vault),
            };
        }
        let (vault, name) = (row.vault.clone(), row.name.clone());
        self.refusal = None;
        self.reading = Some(Reading {
            vault: vault.clone(),
            name: name.clone(),
            copy: true,
        });
        shell.set_status(format!("reading {name}…"));
        AppAction::Send(crate::worker::Request::Value {
            vault,
            name,
            version: None,
        })
    }

    /// A value has come back. It is kept only if the cursor is still on the
    /// row that asked; a value for a row somebody has left is dropped on the
    /// floor rather than shown next to the wrong name.
    pub fn on_value(
        &mut self,
        shell: &mut Shell,
        store: &Store,
        vault: &str,
        name: &str,
        result: Result<(Secret, String), String>,
        now: Instant,
    ) -> AppAction {
        // Taken only if it is the one asked for: an answer for a row the
        // cursor has left must not cancel the ask still out for the row it
        // is on now.
        let Some(asked) = self
            .reading
            .take_if(|held| held.vault == vault && held.name == name)
        else {
            return AppAction::None;
        };
        let still_here = self
            .selected(store)
            .is_some_and(|row| row.vault == vault && row.name == name);
        match result {
            Err(message) => {
                if asked.copy {
                    shell.set_error(message.clone());
                }
                if still_here {
                    self.refusal = Some(message);
                }
                AppAction::None
            }
            // The second and last call to `Secret::expose`: `y` pressed with
            // nothing on screen, copying blind, which is the common case.
            Ok((secret, _version)) if asked.copy => AppAction::Copy {
                text: secret.expose().to_owned(),
                label: format!("Copied value of {name} ({vault})"),
            },
            Ok((secret, version)) => {
                if !still_here {
                    // Nothing keeps it. It is dropped here, unread.
                    return AppAction::None;
                }
                self.revealed = Some(Revealed {
                    vault: vault.to_owned(),
                    name: name.to_owned(),
                    version,
                    value: secret,
                    at: now,
                });
                AppAction::None
            }
        }
    }

    /// One turn of the clock: drops a value that has run out, and asks for
    /// the versions of a row the cursor has settled on.
    pub fn tick(&mut self, store: &Store, now: Instant) -> Option<crate::worker::Request> {
        if self.revealed.as_ref().is_some_and(|held| held.expired(now)) {
            self.revealed = None;
        }
        let row = self.selected(store)?;
        let (vault, name) = (row.vault.clone(), row.name.clone());
        let here = self.cursor.index;
        match self.rested {
            Some((at, since)) if at == here => {
                if now.saturating_duration_since(since) < REST {
                    return None;
                }
            }
            _ => {
                self.rested = Some((here, now));
                return None;
            }
        }
        let key = (vault.clone(), name.clone());
        if store.versions.contains_key(&key) || !self.asked.insert(key) {
            return None;
        }
        Some(crate::worker::Request::Versions { vault, name })
    }

    /// Whether the run loop should wake every second: something is counting
    /// down, or something is being waited for.
    #[must_use]
    pub const fn is_ticking(&self) -> bool {
        self.revealed.is_some() || self.reading.is_some()
    }

    /// Whether the cursor has landed somewhere in the last [`REST`], so the
    /// loop comes back in time to ask about it.
    ///
    /// It closes when the window does. Left open, the loop would wake six
    /// times a second for the rest of the run over a cursor that stopped
    /// moving minutes ago.
    #[must_use]
    pub fn is_resting(&self) -> bool {
        self.rested.is_some_and(|(_, since)| since.elapsed() < REST)
    }

    /// Moving the cursor takes the value off the screen with it, and clears
    /// whatever the last row refused with.
    fn cursor_moved(&mut self) {
        self.revealed = None;
        self.refusal = None;
    }

    /// `r`, or a tab switch: the value goes. Rows that are about to be read
    /// again should not be sitting next to a value read before them, and a
    /// tab switch is looking away.
    pub fn on_refresh(&mut self) {
        self.revealed = None;
        self.reading = None;
        self.refusal = None;
        self.asked.clear();
    }
}

#[cfg(test)]
pub(crate) mod tests;
