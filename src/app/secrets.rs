//! The Secrets tab: every secret in every vault as one flat table.
//!
//! The point of the tab is the question "what is `db-password` in each
//! environment?", so the default sort is by name with the vault as the
//! tiebreak — dev, qa and prod end up adjacent — and the vault tiebreak
//! follows the inventory's order rather than the alphabet, because that is
//! the order the configuration named them in.

use std::collections::HashMap;

use super::cursor::ListCursor;
use super::screen::{AppAction, Target};
use super::shell::{Focus, Shell};
use crate::azure::SecretRow;
use crate::columns::{ColumnId, SECRET_COLUMNS, TableLayout};
use crate::filter::{self, Query, When};
use crate::store::Store;
use crate::text_input::TextInput;
use crate::timestamp::Timestamp;

/// Inside this many days, an expiry is worth a colour and a badge.
pub const EXPIRING_SOON: i64 = 30;

/// The `key:` filters this tab knows. Everything else typed is a word.
pub const SCHEMA: &[&str] = &[
    "vault", "name", "type", "enabled", "managed", "expires", "tag",
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

    /// What the Expires cell says.
    #[must_use]
    pub fn cell(self, expires: Option<Timestamp>, now: Timestamp) -> String {
        match self {
            Self::None => "—".to_owned(),
            Self::Expired => "expired".to_owned(),
            Self::Soon | Self::Later => expires
                .map(|expires| expires.relative_age(now))
                .unwrap_or_default(),
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
    _now: Timestamp,
) {
    let flip = |ordering: std::cmp::Ordering| {
        if descending {
            ordering.reverse()
        } else {
            ordering
        }
    };
    indices.sort_by(|a, b| {
        let (left, right) = (&rows[*a], &rows[*b]);
        let ordering = match by {
            ColumnId::Vault => flip(
                order
                    .get(left.vault.as_str())
                    .cmp(&order.get(right.vault.as_str())),
            ),
            ColumnId::Enabled => flip(left.enabled.cmp(&right.enabled)),
            ColumnId::Expires => stamps(left.expires, right.expires, descending),
            ColumnId::Updated => stamps(left.updated, right.updated, descending),
            ColumnId::Created => stamps(left.created, right.created, descending),
            ColumnId::Type => flip(left.content_type.cmp(&right.content_type)),
            _ => std::cmp::Ordering::Equal,
        };
        ordering
            .then_with(|| {
                flip(
                    left.name
                        .to_ascii_lowercase()
                        .cmp(&right.name.to_ascii_lowercase()),
                )
            })
            .then_with(|| {
                order
                    .get(left.vault.as_str())
                    .cmp(&order.get(right.vault.as_str()))
            })
    });
}

/// Two stamps, with a missing one always last.
///
/// The absence is compared outside the flip on purpose: `S` should turn the
/// dates over, not move "has no expiry" to the top of a list of things that
/// are expiring.
fn stamps(
    left: Option<Timestamp>,
    right: Option<Timestamp>,
    descending: bool,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (left, right) {
        (None, None) => Ordering::Equal,
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (Some(left), Some(right)) => {
            if descending {
                right.cmp(&left)
            } else {
                left.cmp(&right)
            }
        }
    }
}

/// The table a Secrets tab opens with.
#[must_use]
pub fn default_layout() -> TableLayout {
    TableLayout::new(SECRET_COLUMNS)
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
}

impl Default for SecretsScreen {
    fn default() -> Self {
        Self {
            cursor: ListCursor::default(),
            layout: default_layout(),
            input: TextInput::default(),
            sort: ColumnId::Name,
            descending: false,
            visible: Vec::new(),
            sorted: Vec::new(),
            haystacks: Vec::new(),
            ordered_for: None,
            built_for: None,
            available: 0,
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
            Timestamp::now(),
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
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.cursor.move_by(1, count),
            KeyCode::Char('k') | KeyCode::Up => self.cursor.move_by(-1, count),
            KeyCode::PageDown => self.cursor.page(1, count),
            KeyCode::PageUp => self.cursor.page(-1, count),
            KeyCode::Home => self.cursor.focus(0),
            KeyCode::End => self.cursor.move_by(isize::MAX, count),
            KeyCode::Char('s') => self.next_sort(),
            KeyCode::Char('S') => self.descending = !self.descending,
            KeyCode::Char('o') => {
                return self.open_in_portal(shell, store);
            }
            _ => {}
        }
        AppAction::None
    }

    pub fn handle_click(
        &mut self,
        _shell: &mut Shell,
        _store: &Store,
        target: Target,
    ) -> AppAction {
        match target {
            Target::Row(index) => self
                .cursor
                .focus(index.min(self.visible.len().saturating_sub(1))),
            Target::Header(key) => {
                if let Some(column) = ColumnId::from_key(key) {
                    self.sort_by(column);
                }
            }
            _ => {}
        }
        AppAction::None
    }

    pub fn handle_wheel(&mut self, _shell: &mut Shell, _target: Option<Target>, delta: i32) {
        self.cursor.scroll.scroll_by(delta);
        // The cursor follows the viewport rather than being left behind it,
        // so what `v` acts on is always something on screen.
        let first = self.cursor.scroll.offset;
        let last = first + self.cursor.scroll.viewport.saturating_sub(1);
        self.cursor.index = self
            .cursor
            .index
            .clamp(first, last.min(self.visible.len().saturating_sub(1)));
    }

    /// The vault's secrets blade in the portal.
    fn open_in_portal(&self, shell: &mut Shell, store: &Store) -> AppAction {
        let Some(row) = self.selected(store) else {
            return AppAction::None;
        };
        let Some(vault) = store
            .inventory
            .vaults
            .iter()
            .find(|vault| vault.name == row.vault)
        else {
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
        "↑↓/jk move  / search  s sort  y copy value  v reveal  r refresh  ? help".to_owned()
    }

    /// `r`, or a tab switch: whatever the screen was holding that is now
    /// older than the rows under it.
    pub fn on_refresh(&mut self) {
        // Step 07 drops the revealed value here. Nothing else is held.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timestamp::ts;

    fn now() -> Timestamp {
        ts("2026-09-11T20:00:00Z")
    }

    fn row(vault: &str, name: &str) -> SecretRow {
        SecretRow {
            vault: vault.to_owned(),
            name: name.to_owned(),
            enabled: true,
            created: None,
            updated: None,
            expires: None,
            not_before: None,
            content_type: None,
            tags: Vec::new(),
            managed: false,
        }
    }

    #[test]
    fn the_haystack_holds_every_cell_a_person_might_type_part_of() {
        let mut held = row("kv-prod", "db-password");
        held.content_type = Some("text/plain".into());
        held.tags = vec![("env".into(), "prod".into())];
        let text = haystack(&held);
        assert!(text.contains("kv-prod"));
        assert!(text.contains("db-password"));
        assert!(text.contains("text/plain"));
        assert!(text.contains("env=prod"));
    }

    #[test]
    fn a_field_filter_narrows_and_an_unknown_one_is_ignored() {
        let mut held = row("kv-prod", "db-password");
        held.content_type = Some("text/plain".into());
        held.tags = vec![("env".into(), "prod".into())];

        let query = |raw: &str| Query::parse(raw, SCHEMA);
        assert!(passes(&held, &query("vault:kv-prod"), now()));
        assert!(passes(&held, &query("vault:PROD"), now()));
        assert!(!passes(&held, &query("vault:kv-qa"), now()));
        assert!(passes(&held, &query("type:plain"), now()));
        assert!(passes(&held, &query("tag:env=prod"), now()));
        assert!(!passes(&held, &query("tag:env=qa"), now()));
        assert!(
            passes(&held, &query("vault:kv-prod tag:env"), now()),
            "every filter is ANDed"
        );
    }

    #[test]
    fn a_half_typed_boolean_does_not_empty_the_table() {
        let held = row("kv-prod", "db-password");
        let query = Query::parse("enabled:", SCHEMA);
        assert!(query.fields.is_empty(), "a bare key is still a word");

        let query = Query::parse("enabled:y", SCHEMA);
        assert!(passes(&held, &query, now()));
        let query = Query::parse("enabled:whatever", SCHEMA);
        assert!(
            passes(&held, &query, now()),
            "a value that is not a yes or a no is no opinion"
        );
        let query = Query::parse("enabled:no", SCHEMA);
        assert!(!passes(&held, &query, now()));
    }

    #[test]
    fn an_expiry_is_told_four_ways_and_the_boundary_lands_where_it_says() {
        assert_eq!(Expiry::of(None, now()), Expiry::None);
        assert_eq!(
            Expiry::of(Some(ts("2026-09-08T20:00:00Z")), now()),
            Expiry::Expired
        );
        assert_eq!(
            Expiry::of(Some(ts("2026-10-11T20:00:00Z")), now()),
            Expiry::Soon,
            "exactly 30 days is still soon"
        );
        assert_eq!(
            Expiry::of(Some(ts("2026-10-12T20:01:00Z")), now()),
            Expiry::Later
        );
        assert_eq!(
            Expiry::of(Some(ts("2026-09-11T20:00:00Z")), now()),
            Expiry::Soon,
            "today is soon, not expired"
        );
    }

    #[test]
    fn an_expiry_cell_says_the_age_or_a_dash_or_that_it_has_gone() {
        let soon = Some(ts("2026-09-23T20:00:00Z"));
        assert_eq!(Expiry::None.cell(None, now()), "—");
        assert_eq!(Expiry::Soon.cell(soon, now()), "12d");
        assert_eq!(
            Expiry::Expired.cell(Some(ts("2026-09-08T20:00:00Z")), now()),
            "expired"
        );
    }

    fn order() -> HashMap<&'static str, usize> {
        [("kv-dev", 0), ("kv-qa", 1), ("kv-prod", 2)]
            .into_iter()
            .collect()
    }

    fn sorted(rows: &[SecretRow], by: ColumnId, descending: bool) -> Vec<String> {
        let mut indices: Vec<usize> = (0..rows.len()).collect();
        sort(&mut indices, rows, by, descending, &order(), now());
        indices
            .into_iter()
            .map(|at| format!("{}/{}", rows[at].vault, rows[at].name))
            .collect()
    }

    #[test]
    fn the_default_sort_puts_one_secrets_three_environments_next_to_each_other() {
        let rows = vec![
            row("kv-prod", "db-password"),
            row("kv-dev", "api-key"),
            row("kv-dev", "db-password"),
            row("kv-qa", "db-password"),
        ];
        assert_eq!(
            sorted(&rows, ColumnId::Name, false),
            [
                "kv-dev/api-key",
                "kv-dev/db-password",
                "kv-qa/db-password",
                "kv-prod/db-password",
            ],
            "by name, then the configuration's vault order — not the alphabet"
        );
    }

    #[test]
    fn sorting_by_the_vault_column_follows_the_configuration_not_the_alphabet() {
        let rows = vec![row("kv-prod", "a"), row("kv-dev", "a"), row("kv-qa", "a")];
        assert_eq!(
            sorted(&rows, ColumnId::Vault, false),
            ["kv-dev/a", "kv-qa/a", "kv-prod/a"]
        );
        assert_eq!(
            sorted(&rows, ColumnId::Vault, true),
            ["kv-prod/a", "kv-qa/a", "kv-dev/a"]
        );
    }

    #[test]
    fn a_stamp_that_is_not_set_sorts_last_whichever_way_the_sort_points() {
        let mut rows = vec![row("kv-dev", "a"), row("kv-dev", "b"), row("kv-dev", "c")];
        rows[0].expires = Some(ts("2026-12-01T00:00:00Z"));
        rows[1].expires = None;
        rows[2].expires = Some(ts("2026-10-01T00:00:00Z"));

        assert_eq!(
            sorted(&rows, ColumnId::Expires, false),
            ["kv-dev/c", "kv-dev/a", "kv-dev/b"],
            "soonest first, and the one with no expiry last"
        );
        assert_eq!(
            sorted(&rows, ColumnId::Expires, true),
            ["kv-dev/a", "kv-dev/c", "kv-dev/b"],
            "flipped, and still last"
        );
    }

    #[test]
    fn only_the_columns_on_screen_can_be_sorted_by() {
        let layout = default_layout();
        let wide = sortable(&layout, 200);
        assert!(wide.contains(&ColumnId::Vault));
        assert!(wide.contains(&ColumnId::Name));
        assert!(
            !wide.contains(&ColumnId::Type),
            "Type is hidden by default, so `s` does not stop on it"
        );

        let narrow = sortable(&layout, 30);
        assert!(narrow.len() < wide.len(), "{narrow:?}");
        assert!(
            narrow.contains(&ColumnId::Name) && narrow.contains(&ColumnId::Vault),
            "the pinned columns survive any width: {narrow:?}"
        );
    }

    fn stocked() -> Store {
        use crate::azure::{Inventory, Vault};
        let vault = |name: &str| Vault {
            id: format!("/vaults/{name}"),
            name: name.to_owned(),
            subscription_id: "s".into(),
            resource_group: "rg".into(),
            location: "eastus".into(),
            sku: "standard".into(),
            uri: format!("https://{name}.vault.azure.net/"),
        };
        let mut store = Store::default();
        store.apply(crate::worker::Event::Inventory(Ok(Inventory {
            vaults: vec![vault("kv-dev"), vault("kv-prod")],
            registries: Vec::new(),
        })));
        store.apply(crate::worker::Event::Secrets {
            vault: "kv-dev".into(),
            result: Ok(vec![row("kv-dev", "api-key"), row("kv-dev", "db-password")]),
        });
        store.apply(crate::worker::Event::Secrets {
            vault: "kv-prod".into(),
            result: Ok(vec![row("kv-prod", "db-password")]),
        });
        store
    }

    fn shown(screen: &SecretsScreen, store: &Store) -> Vec<String> {
        screen
            .visible()
            .iter()
            .map(|at| format!("{}/{}", store.secrets[*at].vault, store.secrets[*at].name))
            .collect()
    }

    #[test]
    fn a_query_narrows_the_rows_and_the_border_says_how_many_are_left() {
        let store = stocked();
        let mut screen = SecretsScreen::default();
        screen.refilter(&store);
        assert_eq!(screen.visible().len(), 3);
        assert_eq!(screen.status(&store), "3 · Name ↑");

        screen.input.set_text("db");
        screen.refilter(&store);
        assert_eq!(
            shown(&screen, &store),
            ["kv-dev/db-password", "kv-prod/db-password"]
        );
        assert_eq!(screen.status(&store), "2/3 · Name ↑");

        screen.input.set_text("vault:kv-prod");
        screen.refilter(&store);
        assert_eq!(shown(&screen, &store), ["kv-prod/db-password"]);

        screen.input.set_text("zzz");
        screen.refilter(&store);
        assert!(screen.visible().is_empty());
        assert_eq!(screen.cursor.index, 0, "the cursor comes back onto nothing");
    }

    #[test]
    fn the_cursor_stays_on_the_same_secret_when_a_refresh_reorders_the_rows() {
        let mut store = stocked();
        let mut screen = SecretsScreen::default();
        screen.refilter(&store);
        screen.cursor.focus(2);
        assert_eq!(
            screen.cursor_identity(&store),
            Some(("kv-prod".to_owned(), "db-password".to_owned()))
        );

        let was = screen.cursor_identity(&store);
        // kv-dev grows a secret that sorts before the one under the cursor.
        store.apply(crate::worker::Event::Secrets {
            vault: "kv-dev".into(),
            result: Ok(vec![
                row("kv-dev", "aaa-new"),
                row("kv-dev", "api-key"),
                row("kv-dev", "db-password"),
            ]),
        });
        screen.invalidate();
        screen.keep_cursor(&store, was);
        assert_eq!(
            screen.cursor_identity(&store),
            Some(("kv-prod".to_owned(), "db-password".to_owned())),
            "the row moved down one and the cursor went with it"
        );

        // And when the secret under it is gone, the cursor lands on the list.
        let was = screen.cursor_identity(&store);
        store.apply(crate::worker::Event::Secrets {
            vault: "kv-prod".into(),
            result: Ok(Vec::new()),
        });
        screen.invalidate();
        screen.keep_cursor(&store, was);
        assert!(screen.cursor.index < screen.visible().len());
    }

    #[test]
    fn s_walks_the_columns_and_a_header_click_cycles_one() {
        let store = stocked();
        let mut screen = SecretsScreen::default();
        screen.note_width(120);
        assert_eq!(screen.sort, ColumnId::Name);

        screen.next_sort();
        assert_eq!(screen.sort, ColumnId::Enabled);
        assert!(!screen.descending, "a new column starts ascending");

        screen.sort_by(ColumnId::Expires);
        assert_eq!((screen.sort, screen.descending), (ColumnId::Expires, false));
        screen.sort_by(ColumnId::Expires);
        assert_eq!((screen.sort, screen.descending), (ColumnId::Expires, true));
        screen.sort_by(ColumnId::Expires);
        assert_eq!(
            (screen.sort, screen.descending),
            (ColumnId::Name, false),
            "a third click on the same header goes back to the default"
        );
        let _ = &store;
    }

    /// Prints the cost of one keystroke over forty thousand rows. Ignored by
    /// default because it is a measurement, not an assertion about
    /// correctness, and because a debug build is ten times slower than the
    /// one anybody runs:
    ///
    /// ```console
    /// cargo test --release -- --ignored --nocapture filters_forty_thousand
    /// ```
    #[test]
    #[ignore = "a measurement; run it in release"]
    fn filters_forty_thousand_rows_between_keystrokes() {
        use crate::azure::{Inventory, Vault};
        let vaults: Vec<String> = (0..8).map(|n| format!("kv-{n}")).collect();
        let mut store = Store::default();
        store.apply(crate::worker::Event::Inventory(Ok(Inventory {
            vaults: vaults
                .iter()
                .map(|name| Vault {
                    id: format!("/vaults/{name}"),
                    name: name.clone(),
                    subscription_id: "s".into(),
                    resource_group: "rg".into(),
                    location: "eastus".into(),
                    sku: "standard".into(),
                    uri: format!("https://{name}.vault.azure.net/"),
                })
                .collect(),
            registries: Vec::new(),
        })));
        for vault in &vaults {
            store.apply(crate::worker::Event::Secrets {
                vault: vault.clone(),
                result: Ok((0..5000)
                    .map(|n| row(vault, &format!("service-{n:05}-connection-string")))
                    .collect()),
            });
        }
        assert_eq!(store.secrets.len(), 40_000);

        let mut screen = SecretsScreen::default();
        let built = std::time::Instant::now();
        screen.refilter(&store);
        println!("first filter (builds the haystacks): {:?}", built.elapsed());

        // What a person typing `service-04` actually costs, one character at
        // a time, with the haystacks already built.
        let mut worst = std::time::Duration::ZERO;
        for typed in 1..="service-04".len() {
            screen.input.set_text(&"service-04"[..typed]);
            let started = std::time::Instant::now();
            screen.refilter(&store);
            let took = started.elapsed();
            println!(
                "  {:>12} -> {:>6} rows in {took:?}",
                screen.input.text(),
                screen.visible().len()
            );
            worst = worst.max(took);
        }
        println!("worst keystroke: {worst:?}");
        assert!(
            worst < std::time::Duration::from_millis(200),
            "even a debug build should not be this slow: {worst:?}"
        );
    }

    #[test]
    fn the_badge_counts_only_enabled_secrets_that_are_running_out() {
        let mut rows = vec![
            row("kv-a", "fine"),
            row("kv-a", "soon"),
            row("kv-a", "gone"),
        ];
        rows[1].expires = Some(ts("2026-09-23T20:00:00Z"));
        rows[2].expires = Some(ts("2026-09-08T20:00:00Z"));
        assert_eq!(expiring(&rows, now()), 2);

        rows[2].enabled = false;
        assert_eq!(
            expiring(&rows, now()),
            1,
            "a disabled secret's expiry is nobody's problem"
        );
    }
}
