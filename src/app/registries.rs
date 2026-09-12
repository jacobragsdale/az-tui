//! The Registries tab: every repository across every registry as one flat
//! table, and `Enter` to turn that table into one repository's tags.
//!
//! The same shape as the Secrets tab with a second level on top. Each level
//! keeps its own search box, so going down into a repository and coming back
//! out puts each query back the way it was — which is the whole reason the
//! two are separate fields rather than one shared one.

use std::collections::HashSet;
use std::time::Instant;

use super::cursor::{ListCursor, ScrollState};
use super::screen::{AppAction, Target};
use super::secrets::REST;
use super::shell::{Focus, Shell};
use crate::azure::{Repository, Tag, acr};
use crate::columns::{ColumnId, REPOSITORY_COLUMNS, TAG_COLUMNS, TableLayout};
use crate::filter::{self, Query, When};
use crate::store::Store;
use crate::text_input::TextInput;
use crate::timestamp::Timestamp;
use crate::worker::Request;

/// The `key:` filters the repository table knows.
pub const REPOSITORY_SCHEMA: &[&str] = &["registry", "repo", "name", "updated", "created"];
/// The `key:` filters the tag table knows.
pub const TAG_SCHEMA: &[&str] = &["tag", "name", "digest", "updated", "created"];
/// How many tags the repository pane lists before it says how many more there
/// are. Any more and the pane is a table, which is what `Enter` is for.
pub const TAGS_IN_PANE: usize = 20;

/// Which of the two tables the tab is showing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Level {
    Repositories,
    /// One repository's tags. The registry and the repository are held here
    /// rather than read off the cursor, so a refresh that moves the rows
    /// underneath cannot change which repository is open.
    Tags {
        registry: String,
        repo: String,
    },
}

pub struct RegistriesScreen {
    pub level: Level,
    pub repositories: Table,
    pub tags: Table,
    /// Every repository's searchable text, built when the rows change.
    haystacks: Vec<String>,
    /// What the repository rows were last built from.
    built_for: Option<(String, ColumnId, bool, usize)>,
    ordered_for: Option<(ColumnId, bool, usize)>,
    /// Which rows of `store.repositories` are shown, in order.
    visible: Vec<usize>,
    sorted: Vec<usize>,
    /// Which tags of the open repository are shown, in order.
    tag_visible: Vec<usize>,
    tag_built_for: Option<(String, ColumnId, bool, usize)>,
    /// The last repository opened, so coming straight back into it keeps its
    /// cursor and its query while opening a different one does not.
    was: Level,
    /// Where the cursor is and when it landed, for the rest interval.
    rested: Option<(usize, Instant)>,
    /// What has been asked for this run, so coming back to a row is free.
    asked: HashSet<(String, String, String)>,
    pub details_scroll: ScrollState,
    available: u16,
}

/// One of the two tables: its cursor, its box, its columns and its sort.
pub struct Table {
    pub cursor: ListCursor,
    pub input: TextInput,
    pub layout: TableLayout,
    pub sort: ColumnId,
    pub descending: bool,
}

impl Table {
    fn new(defaults: &[crate::columns::ColumnConfig], sort: ColumnId) -> Self {
        Self {
            cursor: ListCursor::default(),
            input: TextInput::default(),
            layout: TableLayout::new(defaults),
            sort,
            // Newest first: what changed last is what anybody is looking for.
            descending: true,
        }
    }
}

impl Default for RegistriesScreen {
    fn default() -> Self {
        Self {
            level: Level::Repositories,
            repositories: Table::new(REPOSITORY_COLUMNS, ColumnId::Updated),
            tags: Table::new(TAG_COLUMNS, ColumnId::Updated),
            haystacks: Vec::new(),
            built_for: None,
            ordered_for: None,
            visible: Vec::new(),
            sorted: Vec::new(),
            tag_visible: Vec::new(),
            tag_built_for: None,
            was: Level::Repositories,
            rested: None,
            asked: HashSet::new(),
            details_scroll: ScrollState::default(),
            available: 0,
        }
    }
}

impl RegistriesScreen {
    /// The table the keys and the cursor belong to right now.
    #[must_use]
    pub const fn table(&self) -> &Table {
        match self.level {
            Level::Repositories => &self.repositories,
            Level::Tags { .. } => &self.tags,
        }
    }

    pub const fn table_mut(&mut self) -> &mut Table {
        match self.level {
            Level::Repositories => &mut self.repositories,
            Level::Tags { .. } => &mut self.tags,
        }
    }

    pub const fn input_mut(&mut self) -> &mut TextInput {
        &mut self.table_mut().input
    }

    #[must_use]
    pub const fn input(&self) -> &TextInput {
        &self.table().input
    }

    /// Which rows of `store.repositories` the repository table is showing.
    #[must_use]
    pub fn visible(&self) -> &[usize] {
        &self.visible
    }

    /// Which of the open repository's tags the tag table is showing.
    #[must_use]
    pub fn tag_visible(&self) -> &[usize] {
        &self.tag_visible
    }

    #[must_use]
    pub fn selected_repository<'a>(&self, store: &'a Store) -> Option<&'a Repository> {
        store
            .repositories
            .get(*self.visible.get(self.repositories.cursor.index)?)
    }

    /// The repository the tag level is inside, whichever table has the
    /// cursor.
    #[must_use]
    pub fn open_repository(&self) -> Option<(&str, &str)> {
        match &self.level {
            Level::Repositories => None,
            Level::Tags { registry, repo } => Some((registry, repo)),
        }
    }

    /// The open repository's tags, as the store holds them.
    #[must_use]
    pub fn tags_of<'a>(&self, store: &'a Store) -> Option<&'a Result<Vec<Tag>, String>> {
        let (registry, repo) = self.open_repository()?;
        store.tags.get(&(registry.to_owned(), repo.to_owned()))
    }

    #[must_use]
    pub fn selected_tag<'a>(&self, store: &'a Store) -> Option<&'a Tag> {
        let Ok(tags) = self.tags_of(store)? else {
            return None;
        };
        tags.get(*self.tag_visible.get(self.tags.cursor.index)?)
    }

    /// Where the registry a row belongs to lives, for the pull references
    /// and the portal link.
    #[must_use]
    pub fn login_server<'a>(&self, store: &'a Store, registry: &str) -> Option<&'a str> {
        store
            .inventory
            .registries
            .iter()
            .find(|held| held.name == registry)
            .map(|held| held.login_server.as_str())
    }

    pub fn note_width(&mut self, available: u16) {
        self.available = available;
    }

    pub fn invalidate(&mut self) {
        self.built_for = None;
        self.ordered_for = None;
        self.tag_built_for = None;
        self.haystacks.clear();
    }

    /// Rebuilds whichever table is on screen. Cheap to call every frame.
    pub fn refilter(&mut self, store: &Store) {
        self.refilter_repositories(store);
        self.refilter_tags(store);
    }

    fn refilter_repositories(&mut self, store: &Store) {
        let order_key = (
            self.repositories.sort,
            self.repositories.descending,
            store.repositories.len(),
        );
        if self.ordered_for != Some(order_key) {
            self.ordered_for = Some(order_key);
            self.built_for = None;
            if self.haystacks.len() != store.repositories.len() {
                self.haystacks = store.repositories.iter().map(haystack).collect();
            }
            self.sorted = (0..store.repositories.len()).collect();
            sort_repositories(
                &mut self.sorted,
                &store.repositories,
                self.repositories.sort,
                self.repositories.descending,
            );
        }
        let key = (
            self.repositories.input.text().to_owned(),
            self.repositories.sort,
            self.repositories.descending,
            store.repositories.len(),
        );
        if self.built_for.as_ref() == Some(&key) {
            return;
        }
        self.built_for = Some(key);
        let now = Timestamp::now();
        let parsed = Query::parse(self.repositories.input.text(), REPOSITORY_SCHEMA);
        let mut words = crate::search::Query::new(&parsed.words);
        self.visible = self
            .sorted
            .iter()
            .copied()
            .filter(|at| {
                repository_passes(&store.repositories[*at], &parsed, now)
                    && words.matches(&self.haystacks[*at])
            })
            .collect();
        self.repositories.cursor.clamp(self.visible.len());
    }

    fn refilter_tags(&mut self, store: &Store) {
        let Some(Ok(tags)) = self.tags_of(store) else {
            self.tag_visible.clear();
            self.tag_built_for = None;
            return;
        };
        let key = (
            self.tags.input.text().to_owned(),
            self.tags.sort,
            self.tags.descending,
            tags.len(),
        );
        if self.tag_built_for.as_ref() == Some(&key) {
            return;
        }
        self.tag_built_for = Some(key);
        let now = Timestamp::now();
        let parsed = Query::parse(self.tags.input.text(), TAG_SCHEMA);
        let mut words = crate::search::Query::new(&parsed.words);
        let mut visible: Vec<usize> = (0..tags.len())
            .filter(|at| {
                let tag = &tags[*at];
                tag_passes(tag, &parsed, now)
                    && words.matches(&format!("{} {}", tag.name, tag.digest))
            })
            .collect();
        sort_tags(&mut visible, tags, self.tags.sort, self.tags.descending);
        self.tag_visible = visible;
        self.tags.cursor.clamp(self.tag_visible.len());
    }

    /// `Enter`: the table becomes that repository's tags.
    fn open_tags(&mut self, store: &Store) -> AppAction {
        let Some(repository) = self.selected_repository(store) else {
            return AppAction::None;
        };
        let (registry, repo) = (repository.registry.clone(), repository.name.clone());
        let opening = Level::Tags {
            registry: registry.clone(),
            repo: repo.clone(),
        };
        // Coming back into the repository just left puts the cursor back on
        // the tag it was on; opening a different one starts at the top,
        // because a tag index means nothing across two repositories.
        let elsewhere = self.was != opening;
        self.level = opening.clone();
        self.was = opening;
        if elsewhere {
            self.tags.cursor.reset();
            self.tags.input.clear();
        }
        self.tag_built_for = None;
        self.details_scroll.scroll_to(0);
        self.rested = None;
        AppAction::Send(Request::Tags { registry, repo })
    }

    /// `Backspace` or `h`: back to the repositories, cursor where it was.
    fn close_tags(&mut self) {
        self.level = Level::Repositories;
        self.details_scroll.scroll_to(0);
        self.rested = None;
    }

    /// One turn of the clock: what the cursor has settled on long enough to
    /// be worth asking about.
    pub fn tick(&mut self, store: &Store, now: Instant) -> Option<Request> {
        let here = self.table().cursor.index;
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
        match &self.level {
            Level::Repositories => {
                let repository = self.selected_repository(store)?;
                let (registry, repo) = (repository.registry.clone(), repository.name.clone());
                let key = (registry.clone(), repo.clone(), String::new());
                if store.tags.contains_key(&(registry.clone(), repo.clone()))
                    || !self.asked.insert(key)
                {
                    return None;
                }
                Some(Request::Tags { registry, repo })
            }
            Level::Tags { registry, repo } => {
                let tag = self.selected_tag(store)?;
                let digest = tag.digest.clone();
                if digest.is_empty() {
                    return None;
                }
                let key = (registry.clone(), repo.clone(), digest.clone());
                if store.manifests.contains_key(&key) || !self.asked.insert(key) {
                    return None;
                }
                Some(Request::Manifest {
                    registry: registry.clone(),
                    repo: repo.clone(),
                    digest,
                })
            }
        }
    }

    #[must_use]
    pub const fn is_resting(&self) -> bool {
        self.rested.is_some()
    }

    /// A refresh, or a tab switch. Nothing here is secret, so only the
    /// once-per-run bookkeeping resets.
    pub fn on_refresh(&mut self) {
        self.asked.clear();
    }

    /// What the bottom border says.
    #[must_use]
    pub fn status(&self, store: &Store) -> String {
        let arrow = if self.table().descending {
            "↓"
        } else {
            "↑"
        };
        let label = self.table().sort.label();
        match self.level {
            Level::Repositories => {
                let filling = store
                    .repositories
                    .iter()
                    .filter(|row| row.tag_count.is_none())
                    .count();
                let counts = if self.visible.len() == store.repositories.len() {
                    format!("{}", store.repositories.len())
                } else {
                    format!("{}/{}", self.visible.len(), store.repositories.len())
                };
                if filling > 0 && !store.repositories.is_empty() {
                    format!(
                        "{counts} · filling {}/{} · {label} {arrow}",
                        store.repositories.len() - filling,
                        store.repositories.len()
                    )
                } else {
                    format!("{counts} · {label} {arrow}")
                }
            }
            Level::Tags { .. } => {
                let total = match self.tags_of(store) {
                    Some(Ok(tags)) => tags.len(),
                    _ => 0,
                };
                if self.tag_visible.len() == total {
                    format!("{total} · {label} {arrow}")
                } else {
                    format!("{}/{total} · {label} {arrow}", self.tag_visible.len())
                }
            }
        }
    }

    /// The pane's title: `Repositories`, or the repository and its registry.
    #[must_use]
    pub fn title(&self) -> String {
        match &self.level {
            Level::Repositories => " Repositories ".to_owned(),
            Level::Tags { registry, repo } => format!(" {repo} · {registry} "),
        }
    }

    #[must_use]
    pub fn cursor_identity(&self, store: &Store) -> Option<(String, String)> {
        self.selected_repository(store)
            .map(|row| (row.registry.clone(), row.name.clone()))
    }

    /// After a refresh: back onto the same repository, and out of a
    /// repository that has gone.
    pub fn keep_cursor(&mut self, store: &Store, was: Option<(String, String)>) {
        self.refilter(store);
        if let Level::Tags { registry, repo } = self.level.clone()
            && !store
                .repositories
                .iter()
                .any(|held| held.registry == registry && held.name == repo)
        {
            self.close_tags();
        }
        let Some((registry, name)) = was else {
            self.repositories.cursor.clamp(self.visible.len());
            return;
        };
        match self.visible.iter().position(|at| {
            store.repositories[*at].registry == registry && store.repositories[*at].name == name
        }) {
            Some(at) => self.repositories.cursor.focus(at),
            None => self.repositories.cursor.clamp(self.visible.len()),
        }
    }

    /// The sortable columns of whichever table is showing.
    fn sortable(&self) -> Vec<ColumnId> {
        self.table()
            .layout
            .visible_columns(self.available)
            .into_iter()
            .map(|column| column.id)
            .collect()
    }

    pub fn next_sort(&mut self) {
        let columns = self.sortable();
        if columns.is_empty() {
            return;
        }
        let table = self.table_mut();
        let at = columns.iter().position(|held| *held == table.sort);
        table.sort = columns[at.map_or(0, |at| (at + 1) % columns.len())];
        table.descending = false;
    }

    pub fn sort_by(&mut self, column: ColumnId) {
        let default = match self.level {
            Level::Repositories | Level::Tags { .. } => ColumnId::Updated,
        };
        let table = self.table_mut();
        if table.sort == column {
            if table.descending {
                table.sort = default;
                table.descending = true;
            } else {
                table.descending = true;
            }
        } else {
            table.sort = column;
            table.descending = false;
        }
    }

    #[must_use]
    pub fn count(&self) -> usize {
        match self.level {
            Level::Repositories => self.visible.len(),
            Level::Tags { .. } => self.tag_visible.len(),
        }
    }

    pub fn handle_key(
        &mut self,
        shell: &mut Shell,
        store: &Store,
        key: crossterm::event::KeyEvent,
    ) -> AppAction {
        use crossterm::event::KeyCode;
        let count = self.count();
        if shell.focus == Focus::Details {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.details_scroll.scroll_by(1);
                    return AppAction::None;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.details_scroll.scroll_by(-1);
                    return AppAction::None;
                }
                _ => return self.acting_key(shell, store, key),
            }
        }
        let before = self.table().cursor.index;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.table_mut().cursor.move_by(1, count),
            KeyCode::Char('k') | KeyCode::Up => self.table_mut().cursor.move_by(-1, count),
            KeyCode::PageDown => self.table_mut().cursor.page(1, count),
            KeyCode::PageUp => self.table_mut().cursor.page(-1, count),
            KeyCode::Home => self.table_mut().cursor.focus(0),
            KeyCode::End => self.table_mut().cursor.move_by(isize::MAX, count),
            KeyCode::Char('s') => self.next_sort(),
            KeyCode::Char('S') => {
                let table = self.table_mut();
                table.descending = !table.descending;
            }
            _ => return self.acting_key(shell, store, key),
        }
        if self.table().cursor.index != before {
            self.details_scroll.scroll_to(0);
        }
        AppAction::None
    }

    /// The keys that act on the row rather than move to it.
    fn acting_key(
        &mut self,
        shell: &mut Shell,
        store: &Store,
        key: crossterm::event::KeyEvent,
    ) -> AppAction {
        use crossterm::event::KeyCode;
        match (key.code, &self.level) {
            (KeyCode::Enter, Level::Repositories) => self.open_tags(store),
            (KeyCode::Backspace | KeyCode::Char('h'), Level::Tags { .. }) => {
                self.close_tags();
                AppAction::None
            }
            (KeyCode::Char('y'), _) => self.copy(store, false),
            (KeyCode::Char('Y'), _) => self.copy(store, true),
            (KeyCode::Char('o'), _) => self.open_in_portal(shell, store),
            _ => AppAction::None,
        }
    }

    /// `y` and `Y`. At the repository level both copy the reference that
    /// pulls `latest`; at the tag level `y` is the tag and `Y` is the digest,
    /// which is the one that names a build rather than a moving label.
    fn copy(&self, store: &Store, digest: bool) -> AppAction {
        match &self.level {
            Level::Repositories => {
                let Some(repository) = self.selected_repository(store) else {
                    return AppAction::None;
                };
                let Some(login) = self.login_server(store, &repository.registry) else {
                    return AppAction::None;
                };
                let text = format!("{login}/{}", repository.name);
                AppAction::Copy {
                    label: format!("Copied {text}"),
                    text,
                }
            }
            Level::Tags { registry, repo } => {
                let Some(tag) = self.selected_tag(store) else {
                    return AppAction::None;
                };
                let Some(login) = self.login_server(store, registry) else {
                    return AppAction::None;
                };
                let text = if digest {
                    acr::digest_reference(login, repo, &tag.digest)
                } else {
                    acr::pull_reference(login, repo, &tag.name)
                };
                AppAction::Copy {
                    label: format!("Copied {text}"),
                    text,
                }
            }
        }
    }

    fn open_in_portal(&self, shell: &mut Shell, store: &Store) -> AppAction {
        let registry = match &self.level {
            Level::Repositories => self
                .selected_repository(store)
                .map(|row| row.registry.clone()),
            Level::Tags { registry, .. } => Some(registry.clone()),
        };
        let Some(registry) = registry else {
            return AppAction::None;
        };
        let Some(held) = store
            .inventory
            .registries
            .iter()
            .find(|held| held.name == registry)
        else {
            shell.set_error(format!("{registry} is not in the inventory"));
            return AppAction::None;
        };
        AppAction::OpenUrl(crate::azure::portal_url(&held.id))
    }

    pub fn handle_click(
        &mut self,
        _shell: &mut Shell,
        _store: &Store,
        target: Target,
    ) -> AppAction {
        match target {
            Target::Row(index) => {
                let count = self.count();
                let index = index.min(count.saturating_sub(1));
                if index != self.table().cursor.index {
                    self.details_scroll.scroll_to(0);
                }
                self.table_mut().cursor.focus(index);
            }
            Target::Header(key) => {
                if let Some(column) = ColumnId::from_key(key) {
                    self.sort_by(column);
                }
            }
            _ => {}
        }
        AppAction::None
    }

    pub fn handle_wheel(&mut self, _shell: &mut Shell, target: Option<Target>, delta: i32) {
        if target == Some(Target::Details) {
            self.details_scroll.scroll_by(delta);
            return;
        }
        let count = self.count();
        let cursor = &mut self.table_mut().cursor;
        cursor.scroll.scroll_by(delta);
        let first = cursor.scroll.offset;
        let last = first + cursor.scroll.viewport.saturating_sub(1);
        cursor.index = cursor.index.clamp(first, last.min(count.saturating_sub(1)));
    }

    /// Nothing about a registry is urgent, so no badge.
    #[must_use]
    pub const fn badge(&self, _store: &Store) -> Option<String> {
        None
    }

    #[must_use]
    pub fn footer_hint(&self, shell: &Shell) -> String {
        if shell.focus == Focus::Search {
            return "Esc/Enter keep the filter  Esc again clears it  Ctrl-U empties the box"
                .to_owned();
        }
        match self.level {
            Level::Repositories => {
                "↑↓/jk move  Enter tags  / search  y copy pull  s sort  r refresh  ? help"
                    .to_owned()
            }
            Level::Tags { .. } => {
                "↑↓/jk move  h back  y copy pull  Y copy digest  s sort  ? help".to_owned()
            }
        }
    }
}

/// What a repository looks like to the search.
#[must_use]
pub fn haystack(row: &Repository) -> String {
    format!("{} {}", row.registry, row.name)
}

/// Whether one repository answers every `key:value` in the query.
#[must_use]
pub fn repository_passes(row: &Repository, query: &Query, now: Timestamp) -> bool {
    query.fields.iter().all(|(key, value)| match key.as_str() {
        "registry" => filter::contains(&row.registry, value),
        "repo" | "name" => filter::contains(&row.name, value),
        "updated" => When::parse(value).is_none_or(|when| when.holds(row.updated, now)),
        "created" => When::parse(value).is_none_or(|when| when.holds(row.created, now)),
        _ => true,
    })
}

#[must_use]
pub fn tag_passes(tag: &Tag, query: &Query, now: Timestamp) -> bool {
    query.fields.iter().all(|(key, value)| match key.as_str() {
        "tag" | "name" => filter::contains(&tag.name, value),
        "digest" => filter::contains(&tag.digest, value),
        "updated" => When::parse(value).is_none_or(|when| when.holds(tag.updated, now)),
        "created" => When::parse(value).is_none_or(|when| when.holds(tag.created, now)),
        _ => true,
    })
}

/// `None` sorts last in both directions, as it does on the Secrets tab: a
/// count that has not arrived yet is not the most interesting row on screen,
/// and flipping the sort should not make it so.
fn none_last<T: Ord>(left: Option<T>, right: Option<T>, descending: bool) -> std::cmp::Ordering {
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

pub fn sort_repositories(
    indices: &mut [usize],
    rows: &[Repository],
    by: ColumnId,
    descending: bool,
) {
    indices.sort_by(|a, b| {
        let (left, right) = (&rows[*a], &rows[*b]);
        let flip = |ordering: std::cmp::Ordering| {
            if descending {
                ordering.reverse()
            } else {
                ordering
            }
        };
        let ordering = match by {
            ColumnId::Registry => flip(left.registry.cmp(&right.registry)),
            ColumnId::Repository => flip(left.name.cmp(&right.name)),
            ColumnId::Tags => none_last(left.tag_count, right.tag_count, descending),
            ColumnId::Manifests => none_last(left.manifest_count, right.manifest_count, descending),
            ColumnId::Updated => none_last(left.updated, right.updated, descending),
            ColumnId::Created => none_last(left.created, right.created, descending),
            _ => std::cmp::Ordering::Equal,
        };
        ordering
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.registry.cmp(&right.registry))
    });
}

pub fn sort_tags(indices: &mut [usize], tags: &[Tag], by: ColumnId, descending: bool) {
    indices.sort_by(|a, b| {
        let (left, right) = (&tags[*a], &tags[*b]);
        let flip = |ordering: std::cmp::Ordering| {
            if descending {
                ordering.reverse()
            } else {
                ordering
            }
        };
        let ordering = match by {
            ColumnId::Tag => flip(left.name.cmp(&right.name)),
            ColumnId::Digest => flip(left.digest.cmp(&right.digest)),
            ColumnId::Updated => none_last(left.updated, right.updated, descending),
            ColumnId::Created => none_last(left.created, right.created, descending),
            _ => std::cmp::Ordering::Equal,
        };
        ordering.then_with(|| left.name.cmp(&right.name))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::{Inventory, Registry};
    use crate::timestamp::ts;
    use crate::worker::Event;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn registry(name: &str) -> Registry {
        Registry {
            id: format!("/registries/{name}"),
            name: name.to_owned(),
            subscription_id: "s".into(),
            resource_group: "rg".into(),
            location: "eastus".into(),
            sku: "Premium".into(),
            login_server: format!("{name}.azurecr.io"),
        }
    }

    fn repository(
        registry: &str,
        name: &str,
        tags: Option<u64>,
        updated: Option<&str>,
    ) -> Repository {
        Repository {
            registry: registry.to_owned(),
            name: name.to_owned(),
            tag_count: tags,
            manifest_count: tags,
            created: None,
            updated: updated.map(ts),
        }
    }

    fn tag(name: &str, digest: &str, updated: &str) -> Tag {
        Tag {
            name: name.to_owned(),
            digest: digest.to_owned(),
            created: Some(ts(updated)),
            updated: Some(ts(updated)),
        }
    }

    fn stocked() -> Store {
        let mut store = Store::default();
        store.apply(Event::Inventory(Ok(Inventory {
            vaults: Vec::new(),
            registries: vec![registry("acrdev"), registry("acrprod")],
        })));
        store.apply(Event::Repositories {
            registry: "acrprod".into(),
            result: Ok(vec![
                repository(
                    "acrprod",
                    "payments-api",
                    Some(48),
                    Some("2026-09-11T18:00:00Z"),
                ),
                repository(
                    "acrprod",
                    "web-frontend",
                    Some(12),
                    Some("2026-09-01T18:00:00Z"),
                ),
                // Still filling: no counts and no stamps yet.
                repository("acrprod", "notifications", None, None),
            ]),
        });
        store.apply(Event::Tags {
            registry: "acrprod".into(),
            repo: "payments-api".into(),
            result: Ok(vec![
                tag("1.42.0", "sha256:ab12ef0199", "2026-09-11T18:00:00Z"),
                tag("1.41.3", "sha256:9f01aa4288", "2026-09-08T18:00:00Z"),
            ]),
        });
        store
    }

    fn press(screen: &mut RegistriesScreen, store: &Store, code: KeyCode) -> AppAction {
        let mut shell = Shell::default();
        screen.refilter(store);
        let action = screen.handle_key(&mut shell, store, KeyEvent::new(code, KeyModifiers::NONE));
        screen.refilter(store);
        action
    }

    fn shown(screen: &RegistriesScreen, store: &Store) -> Vec<String> {
        screen
            .visible()
            .iter()
            .map(|at| store.repositories[*at].name.clone())
            .collect()
    }

    #[test]
    fn enter_opens_a_repository_and_backspace_comes_back_with_both_cursors_intact() {
        let store = stocked();
        let mut screen = RegistriesScreen::default();
        screen.refilter(&store);
        // Updated descending, so the newest repository is first and the one
        // with no stamp yet is last.
        assert_eq!(
            shown(&screen, &store),
            ["payments-api", "web-frontend", "notifications"]
        );

        let action = press(&mut screen, &store, KeyCode::Enter);
        assert!(
            matches!(&action, AppAction::Send(Request::Tags { repo, .. }) if repo == "payments-api"),
            "{action:?}"
        );
        assert_eq!(screen.open_repository(), Some(("acrprod", "payments-api")));
        assert_eq!(screen.count(), 2, "the two tags");

        press(&mut screen, &store, KeyCode::Char('j'));
        assert_eq!(screen.selected_tag(&store).unwrap().name, "1.41.3");

        press(&mut screen, &store, KeyCode::Char('h'));
        assert_eq!(screen.level, Level::Repositories);
        assert_eq!(
            screen.selected_repository(&store).unwrap().name,
            "payments-api",
            "the level-1 cursor is where it was"
        );

        press(&mut screen, &store, KeyCode::Enter);
        assert_eq!(
            screen.selected_tag(&store).unwrap().name,
            "1.41.3",
            "and so is the level-2 one"
        );
    }

    #[test]
    fn each_level_keeps_its_own_query_across_the_round_trip() {
        let store = stocked();
        let mut screen = RegistriesScreen::default();
        screen.repositories.input.set_text("pay");
        screen.refilter(&store);
        assert_eq!(shown(&screen, &store), ["payments-api"]);

        press(&mut screen, &store, KeyCode::Enter);
        assert!(
            screen.input().is_empty(),
            "a level just opened has no query"
        );
        screen.tags.input.set_text("1.41");
        screen.refilter(&store);
        assert_eq!(screen.count(), 1);

        press(&mut screen, &store, KeyCode::Char('h'));
        assert_eq!(screen.input().text(), "pay", "level 1's query came back");
        press(&mut screen, &store, KeyCode::Enter);
        assert_eq!(screen.input().text(), "1.41", "and so did level 2's");
    }

    #[test]
    fn a_count_that_has_not_arrived_sorts_last_whichever_way_the_sort_points() {
        let store = stocked();
        let mut screen = RegistriesScreen::default();
        screen.note_width(200);
        screen.refilter(&store);
        assert_eq!(shown(&screen, &store).last().unwrap(), "notifications");

        screen.repositories.descending = false;
        screen.refilter(&store);
        assert_eq!(
            shown(&screen, &store).last().unwrap(),
            "notifications",
            "flipped, and still last"
        );

        screen.repositories.sort = ColumnId::Tags;
        screen.refilter(&store);
        assert_eq!(shown(&screen, &store).last().unwrap(), "notifications");
    }

    #[test]
    fn the_border_says_how_many_are_still_filling_in() {
        let store = stocked();
        let mut screen = RegistriesScreen::default();
        screen.refilter(&store);
        let status = screen.status(&store);
        assert!(status.contains("filling 2/3"), "{status}");

        // Once the last attributes call lands, the counter goes.
        let mut store = store;
        store.apply(Event::Repository {
            registry: "acrprod".into(),
            repository: repository(
                "acrprod",
                "notifications",
                Some(3),
                Some("2026-09-10T18:00:00Z"),
            ),
        });
        screen.invalidate();
        screen.refilter(&store);
        let status = screen.status(&store);
        assert!(!status.contains("filling"), "{status}");
        assert!(status.starts_with("3 · Updated"), "{status}");
    }

    #[test]
    fn y_and_capital_y_produce_references_that_pull_at_each_level() {
        let store = stocked();
        let mut screen = RegistriesScreen::default();
        screen.refilter(&store);

        match press(&mut screen, &store, KeyCode::Char('y')) {
            AppAction::Copy { text, .. } => {
                assert_eq!(text, "acrprod.azurecr.io/payments-api");
            }
            other => panic!("{other:?}"),
        }

        press(&mut screen, &store, KeyCode::Enter);
        match press(&mut screen, &store, KeyCode::Char('y')) {
            AppAction::Copy { text, .. } => {
                assert_eq!(text, "acrprod.azurecr.io/payments-api:1.42.0");
            }
            other => panic!("{other:?}"),
        }
        match press(&mut screen, &store, KeyCode::Char('Y')) {
            AppAction::Copy { text, label } => {
                assert_eq!(text, "acrprod.azurecr.io/payments-api@sha256:ab12ef0199");
                assert!(label.contains("sha256:ab12ef0199"), "{label}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_repository_that_leaves_the_registry_closes_the_level_it_had_open() {
        let mut store = stocked();
        let mut screen = RegistriesScreen::default();
        screen.refilter(&store);
        press(&mut screen, &store, KeyCode::Enter);
        assert!(matches!(screen.level, Level::Tags { .. }));

        let was = screen.cursor_identity(&store);
        store.apply(Event::Repositories {
            registry: "acrprod".into(),
            result: Ok(vec![repository(
                "acrprod",
                "web-frontend",
                Some(12),
                Some("2026-09-01T18:00:00Z"),
            )]),
        });
        screen.invalidate();
        screen.keep_cursor(&store, was);
        assert_eq!(
            screen.level,
            Level::Repositories,
            "the repository it was inside is gone"
        );
    }

    #[test]
    fn tags_are_asked_for_once_per_row_after_the_rest_interval() {
        let store = stocked();
        let mut screen = RegistriesScreen::default();
        screen.refilter(&store);
        // The cursor starts on payments-api, whose tags are already in.
        let mut clock = Instant::now();
        assert!(screen.tick(&store, clock).is_none());
        clock += REST;
        assert!(
            screen.tick(&store, clock).is_none(),
            "nothing to ask: the tags are already held"
        );

        press(&mut screen, &store, KeyCode::Char('j'));
        // The first tick after a move only notes where the cursor landed.
        assert!(screen.tick(&store, clock).is_none());
        clock += REST;
        let request = screen.tick(&store, clock);
        assert!(
            matches!(&request, Some(Request::Tags { repo, .. }) if repo == "web-frontend"),
            "{request:?}"
        );
        clock += REST;
        assert!(screen.tick(&store, clock).is_none(), "asked once per run");
    }

    #[test]
    fn a_manifest_is_asked_for_once_per_digest_at_the_tag_level() {
        let store = stocked();
        let mut screen = RegistriesScreen::default();
        screen.refilter(&store);
        press(&mut screen, &store, KeyCode::Enter);

        let mut clock = Instant::now();
        screen.tick(&store, clock);
        clock += REST;
        let request = screen.tick(&store, clock);
        assert!(
            matches!(&request, Some(Request::Manifest { digest, .. }) if digest == "sha256:ab12ef0199"),
            "{request:?}"
        );
        clock += REST;
        assert!(screen.tick(&store, clock).is_none());
    }

    #[test]
    fn each_levels_filters_narrow_their_own_table() {
        let store = stocked();
        let mut screen = RegistriesScreen::default();
        screen.repositories.input.set_text("registry:acrprod web");
        screen.refilter(&store);
        assert_eq!(shown(&screen, &store), ["web-frontend"]);

        screen.repositories.input.set_text("registry:acrdev");
        screen.refilter(&store);
        assert!(screen.visible().is_empty(), "nothing is in acrdev");

        screen.repositories.input.clear();
        screen.refilter(&store);
        press(&mut screen, &store, KeyCode::Enter);
        screen.tags.input.set_text("digest:9f01");
        screen.refilter(&store);
        assert_eq!(screen.count(), 1);
        assert_eq!(screen.selected_tag(&store).unwrap().name, "1.41.3");
    }
}
