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
use super::{flip, none_last};
use crate::azure::{Repository, Tag, acr};
use crate::columns::{ColumnId, REPOSITORY_COLUMNS, TAG_COLUMNS, TableLayout};
use crate::filter::{self, Env, Query, When};
use crate::store::AzureStore;
use crate::text_input::TextInput;
use crate::timestamp::Timestamp;
use crate::worker::Request;

/// The `key:` filters the repository table knows.
pub const REPOSITORY_SCHEMA: &[&str] = &["env", "registry", "repo", "name", "updated", "created"];
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
    /// A fill landed since the rows were last sorted. They sort again once,
    /// at the next `refilter`, rather than once a fill.
    filled: bool,
    /// Which rows of `store.repositories` are shown, in order.
    visible: Vec<usize>,
    sorted: Vec<usize>,
    /// Which tags of the open repository are shown, in order.
    tag_visible: Vec<usize>,
    tag_built_for: Option<(String, ColumnId, bool, usize)>,
    /// The last repository opened, so coming straight back into it keeps its
    /// cursor and its query while opening a different one does not.
    was: Level,
    /// The row under the cursor and when it landed, for the rest interval.
    /// By identity, as `asked` is keyed: a keystroke in the search box puts
    /// another row under the same index.
    rested: Option<((String, String, String), Instant)>,
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
            filled: false,
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
    pub fn selected_repository<'a>(&self, store: &'a AzureStore) -> Option<&'a Repository> {
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
    pub fn tags_of<'a>(&self, store: &'a AzureStore) -> Option<&'a Result<Vec<Tag>, String>> {
        let (registry, repo) = self.open_repository()?;
        store.tags.get(&(registry.to_owned(), repo.to_owned()))
    }

    #[must_use]
    pub fn selected_tag<'a>(&self, store: &'a AzureStore) -> Option<&'a Tag> {
        let Ok(tags) = self.tags_of(store)? else {
            return None;
        };
        tags.get(*self.tag_visible.get(self.tags.cursor.index)?)
    }

    pub fn note_width(&mut self, available: u16) {
        self.available = available;
    }

    pub fn invalidate(&mut self) {
        self.built_for = None;
        self.ordered_for = None;
        self.tag_built_for = None;
        self.haystacks.clear();
        // The caller puts the cursor back itself, from before the rows moved.
        self.filled = false;
    }

    /// One repository filled in. The haystacks hold only the registry and
    /// the name, which a fill does not change, so they stand; the sort and
    /// the filter wait for the next `refilter`.
    pub const fn on_fill(&mut self) {
        self.filled = true;
    }

    /// Rebuilds whichever table is on screen. Cheap to call every frame.
    pub fn refilter(&mut self, store: &AzureStore) {
        // A fill keeps every row at its index, so the cursor's row can still
        // be read off the shown rows as they were before it.
        if std::mem::take(&mut self.filled) {
            let was = self.cursor_identity(store);
            self.ordered_for = None;
            self.keep_cursor(store, was);
            return;
        }
        self.refilter_repositories(store);
        self.refilter_tags(store);
    }

    fn refilter_repositories(&mut self, store: &AzureStore) {
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
        let words = crate::search::Query::new(&parsed.words);
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

    fn refilter_tags(&mut self, store: &AzureStore) {
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
        let words = crate::search::Query::new(&parsed.words);
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
    fn open_tags(&mut self, store: &AzureStore) -> AppAction {
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
        AppAction::Azure(Request::Tags { registry, repo })
    }

    /// `Backspace` or `h`: back to the repositories, cursor where it was.
    fn close_tags(&mut self) {
        self.level = Level::Repositories;
        self.details_scroll.scroll_to(0);
        self.rested = None;
    }

    /// One turn of the clock: what the cursor has settled on long enough to
    /// be worth asking about.
    pub fn tick(&mut self, store: &AzureStore, now: Instant) -> Option<Request> {
        let key = match &self.level {
            Level::Repositories => {
                let repository = self.selected_repository(store)?;
                (
                    repository.registry.clone(),
                    repository.name.clone(),
                    String::new(),
                )
            }
            Level::Tags { registry, repo } => {
                let tag = self.selected_tag(store)?;
                (registry.clone(), repo.clone(), tag.digest.clone())
            }
        };
        match &self.rested {
            Some((at, since)) if *at == key => {
                if now.saturating_duration_since(*since) < REST {
                    return None;
                }
            }
            _ => {
                self.rested = Some((key, now));
                return None;
            }
        }
        let (registry, repo, digest) = key;
        if matches!(self.level, Level::Repositories) {
            if store.tags.contains_key(&(registry.clone(), repo.clone()))
                || !self.asked.insert((registry.clone(), repo.clone(), digest))
            {
                return None;
            }
            return Some(Request::Tags { registry, repo });
        }
        if digest.is_empty() {
            return None;
        }
        let key = (registry.clone(), repo.clone(), digest.clone());
        if store.manifests.contains_key(&key) || !self.asked.insert(key) {
            return None;
        }
        Some(Request::Manifest {
            registry,
            repo,
            digest,
        })
    }

    /// Whether the cursor has landed somewhere in the last [`REST`], so the
    /// loop comes back in time to ask about it. It closes when the window
    /// does, rather than leaving the loop awake for the rest of the run.
    #[must_use]
    pub fn is_resting(&self) -> bool {
        self.rested
            .as_ref()
            .is_some_and(|(_, since)| since.elapsed() < REST)
    }

    /// A refresh, or a tab switch. Nothing here is secret, so only the
    /// once-per-run bookkeeping resets.
    pub fn on_refresh(&mut self) {
        self.asked.clear();
    }

    /// What the bottom border says.
    #[must_use]
    pub fn status(&self, store: &AzureStore) -> String {
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
    pub fn cursor_identity(&self, store: &AzureStore) -> Option<(String, String)> {
        self.selected_repository(store)
            .map(|row| (row.registry.clone(), row.name.clone()))
    }

    /// After a refresh: back onto the same repository, and out of a
    /// repository that has gone.
    pub fn keep_cursor(&mut self, store: &AzureStore, was: Option<(String, String)>) {
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

    pub fn next_sort(&mut self) {
        let available = self.available;
        let table = self.table_mut();
        if let Some(next) = table.layout.next_sort(table.sort, available) {
            table.sort = next;
            table.descending = false;
        }
    }

    /// A header click: the same column cycles ascending, descending, then
    /// back to the default; a different column starts ascending. The default
    /// column opens descending, so on it a click simply turns the sort over
    /// — otherwise no number of clicks would ever make it ascending.
    pub fn sort_by(&mut self, column: ColumnId) {
        const DEFAULT: ColumnId = ColumnId::Updated;
        let table = self.table_mut();
        if table.sort == column {
            if table.descending && column != DEFAULT {
                table.sort = DEFAULT;
                table.descending = true;
            } else {
                table.descending = !table.descending;
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
        store: &AzureStore,
        key: crossterm::event::KeyEvent,
    ) -> AppAction {
        use crossterm::event::KeyCode;
        let count = self.count();
        if shell.focus == Focus::Details {
            let page = i32::try_from(self.details_scroll.page_step()).unwrap_or(1);
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => self.details_scroll.scroll_by(1),
                KeyCode::Char('k') | KeyCode::Up => self.details_scroll.scroll_by(-1),
                KeyCode::PageDown => self.details_scroll.scroll_by(page),
                KeyCode::PageUp => self.details_scroll.scroll_by(-page),
                KeyCode::Home => {
                    self.details_scroll.scroll_to(0);
                    true
                }
                KeyCode::End => {
                    self.details_scroll.scroll_to(usize::MAX);
                    true
                }
                _ => return self.acting_key(shell, store, key),
            };
            return AppAction::None;
        }
        let before = self.table().cursor.index;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.table_mut().cursor.move_by(1, count),
            KeyCode::Char('k') | KeyCode::Up => self.table_mut().cursor.move_by(-1, count),
            KeyCode::PageDown => self.table_mut().cursor.page(1, count),
            KeyCode::PageUp => self.table_mut().cursor.page(-1, count),
            KeyCode::Home => self.table_mut().cursor.focus(0),
            KeyCode::End => self.table_mut().cursor.move_by(isize::MAX, count),
            KeyCode::Char('S') => self.next_sort(),
            KeyCode::Char('R') => {
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
        store: &AzureStore,
        key: crossterm::event::KeyEvent,
    ) -> AppAction {
        use crossterm::event::KeyCode;
        match (key.code, &self.level) {
            (KeyCode::Enter | KeyCode::Char('l'), Level::Repositories) => self.open_tags(store),
            // `Esc` reaches here only once there was no query to clear.
            (KeyCode::Backspace | KeyCode::Char('h') | KeyCode::Esc, Level::Tags { .. }) => {
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
    fn copy(&self, store: &AzureStore, digest: bool) -> AppAction {
        match &self.level {
            Level::Repositories => {
                let Some(repository) = self.selected_repository(store) else {
                    return AppAction::None;
                };
                let Some(login) = store
                    .registry(&repository.registry)
                    .map(|held| &held.login_server)
                else {
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
                let Some(login) = store.registry(registry).map(|held| &held.login_server) else {
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

    fn open_in_portal(&self, shell: &mut Shell, store: &AzureStore) -> AppAction {
        let registry = match &self.level {
            Level::Repositories => self
                .selected_repository(store)
                .map(|row| row.registry.clone()),
            Level::Tags { registry, .. } => Some(registry.clone()),
        };
        let Some(registry) = registry else {
            return AppAction::None;
        };
        let Some(held) = store.registry(&registry) else {
            shell.set_error(format!("{registry} is not in the inventory"));
            return AppAction::None;
        };
        AppAction::OpenUrl(crate::azure::portal_url(&held.id))
    }

    pub fn handle_click(
        &mut self,
        _shell: &mut Shell,
        _store: &AzureStore,
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
        let count = self.count();
        self.table_mut().cursor.wheel(delta, count);
    }

    #[must_use]
    pub fn footer_hint(&self, shell: &Shell) -> String {
        if shell.focus == Focus::Search {
            return "Esc/Enter keep the filter  Esc again clears it  Ctrl-U empties the box"
                .to_owned();
        }
        match self.level {
            Level::Repositories => {
                "↑↓/jk move  Enter tags  / search  y copy pull  S sort  r refresh  ? help"
                    .to_owned()
            }
            Level::Tags { .. } => {
                "↑↓/jk move  h back  y copy pull  Y copy digest  S sort  ? help".to_owned()
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
        "env" => Env::of(value).is_none_or(|want| Env::of(&row.registry) == Some(want)),
        "registry" => filter::contains(&row.registry, value),
        "repo" | "name" => filter::contains(&row.name, value),
        "updated" => When::parse(value).is_none_or(|when| when.holds_age(row.updated, now)),
        "created" => When::parse(value).is_none_or(|when| when.holds_age(row.created, now)),
        _ => true,
    })
}

#[must_use]
pub fn tag_passes(tag: &Tag, query: &Query, now: Timestamp) -> bool {
    query.fields.iter().all(|(key, value)| match key.as_str() {
        "tag" | "name" => filter::contains(&tag.name, value),
        "digest" => filter::contains(&tag.digest, value),
        "updated" => When::parse(value).is_none_or(|when| when.holds_age(tag.updated, now)),
        "created" => When::parse(value).is_none_or(|when| when.holds_age(tag.created, now)),
        _ => true,
    })
}

pub fn sort_repositories(
    indices: &mut [usize],
    rows: &[Repository],
    by: ColumnId,
    descending: bool,
) {
    indices.sort_by(|a, b| {
        let (left, right) = (&rows[*a], &rows[*b]);
        let ordering = match by {
            ColumnId::Env => none_last(
                Env::of(&left.registry),
                Env::of(&right.registry),
                descending,
            ),
            ColumnId::Registry => flip(left.registry.cmp(&right.registry), descending),
            ColumnId::Repository => flip(left.name.cmp(&right.name), descending),
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
        let ordering = match by {
            ColumnId::Tag => flip(left.name.cmp(&right.name), descending),
            ColumnId::Digest => flip(left.digest.cmp(&right.digest), descending),
            ColumnId::Updated => none_last(left.updated, right.updated, descending),
            ColumnId::Created => none_last(left.created, right.created, descending),
            _ => std::cmp::Ordering::Equal,
        };
        ordering.then_with(|| left.name.cmp(&right.name))
    });
}

#[cfg(test)]
pub(crate) mod tests;
