//! The application: which tab is open, what the keys do before a screen sees
//! them, and how a frame is put together.

pub mod cursor;
pub mod keys;
pub mod registries;
pub mod screen;
pub mod secrets;
pub mod shell;

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

use screen::{AppAction, TabId, Target};
use secrets::SecretsScreen;
use shell::{Focus, Shell};

use crate::columns::{ColumnId, TableLayout};
use crate::session::{Session, SessionColumn};
use crate::store::{Applied, Store};
use crate::text_input::TextInput;
use crate::worker::Request;
use crate::{ui, worker};

pub struct App {
    pub shell: Shell,
    pub store: Store,
    pub tab: TabId,
    pub secrets: SecretsScreen,
    pub registries: registries::RegistriesScreen,
}

impl App {
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self {
            shell: Shell::default(),
            store,
            tab: TabId::Secrets,
            secrets: SecretsScreen::default(),
            registries: registries::RegistriesScreen::default(),
        }
    }

    /// One key. The global keys are matched here first; everything else goes
    /// to the tab, except while the search box has focus, which takes
    /// everything but `Esc` and `Enter`.
    pub fn handle_key(&mut self, key: KeyEvent) -> AppAction {
        if key.kind != KeyEventKind::Press {
            return AppAction::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return AppAction::Quit;
        }
        if self.shell.help_open {
            // The help takes every key: the one thing it can do is close.
            self.shell.help_open = false;
            return AppAction::None;
        }
        if self.shell.focus == Focus::Search {
            return self.key_in_search(key);
        }
        match key.code {
            KeyCode::Char(number @ ('1' | '2')) => {
                if let Some(tab) = TabId::from_number(number) {
                    self.switch_to(tab);
                }
                AppAction::None
            }
            KeyCode::Char('?') => {
                self.shell.help_open = true;
                AppAction::None
            }
            KeyCode::Char('q') => AppAction::Quit,
            // Esc out of the table clears the query rather than quitting:
            // Esc left the box keeping the filter, and this is the second
            // press that takes it off. With nothing to clear it is the
            // screen's, which is how it backs out of a repository.
            KeyCode::Esc => {
                if self.clear_query() {
                    AppAction::None
                } else {
                    self.screen_key(key)
                }
            }
            KeyCode::Char('r') => {
                self.on_refresh();
                AppAction::Send(Request::Refresh)
            }
            KeyCode::Char('/') => {
                self.shell.focus = Focus::Search;
                AppAction::None
            }
            KeyCode::Tab => {
                self.shell.toggle_focus();
                AppAction::None
            }
            _ => self.screen_key(key),
        }
    }

    /// While the box has focus every key is a character, except the three
    /// that leave it.
    fn key_in_search(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Enter | KeyCode::Esc | KeyCode::Tab => {
                self.shell.focus = Focus::Table;
            }
            _ => {
                self.input().handle_key(key);
            }
        }
        AppAction::None
    }

    /// A paste, which bracketed paste hands over whole. It goes into the
    /// search box when that is what has focus, and nowhere otherwise.
    pub fn handle_paste(&mut self, text: &str) -> AppAction {
        if self.shell.focus == Focus::Search {
            self.input().paste(text);
        }
        AppAction::None
    }

    /// The search box of whichever table is showing.
    fn input(&mut self) -> &mut TextInput {
        match self.tab {
            TabId::Secrets => &mut self.secrets.input,
            TabId::Registries => self.registries.input_mut(),
        }
    }

    /// `Esc` out of the table: the filter goes, and the table comes back
    /// whole. Says whether there was one to take off.
    fn clear_query(&mut self) -> bool {
        let input = self.input();
        let had = !input.is_empty();
        input.clear();
        had
    }

    fn screen_key(&mut self, key: KeyEvent) -> AppAction {
        let store = &self.store;
        match self.tab {
            TabId::Secrets => self.secrets.handle_key(&mut self.shell, store, key),
            TabId::Registries => self.registries.handle_key(&mut self.shell, store, key),
        }
    }

    /// Switching tabs drops a revealed value, like every other way of
    /// looking away from it.
    fn switch_to(&mut self, tab: TabId) {
        if self.tab != tab {
            self.tab = tab;
            self.shell.focus = Focus::Table;
            self.on_refresh();
        }
    }

    /// A click, resolved against what was drawn last frame. A click lands
    /// focus where it lands: on a row, the table; on the pane, the pane.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> AppAction {
        let target = self.shell.hit(event.column, event.row).cloned();
        match event.kind {
            MouseEventKind::Down(_) => match target {
                Some(Target::Tab(tab)) => {
                    self.switch_to(tab);
                    AppAction::None
                }
                Some(Target::Help) => {
                    self.shell.help_open = !self.shell.help_open;
                    AppAction::None
                }
                Some(Target::SearchField) => {
                    self.shell.focus = Focus::Search;
                    AppAction::None
                }
                Some(Target::ClearSearch) => {
                    self.clear_query();
                    AppAction::None
                }
                Some(Target::Details) => {
                    self.shell.focus = Focus::Details;
                    AppAction::None
                }
                Some(target) => {
                    self.shell.focus = Focus::Table;
                    let store = &self.store;
                    match self.tab {
                        TabId::Secrets => self.secrets.handle_click(&mut self.shell, store, target),
                        TabId::Registries => {
                            self.registries.handle_click(&mut self.shell, store, target)
                        }
                    }
                }
                None => AppAction::None,
            },
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let delta = if event.kind == MouseEventKind::ScrollUp {
                    -3
                } else {
                    3
                };
                match self.tab {
                    TabId::Secrets => {
                        self.secrets.handle_wheel(&mut self.shell, target, delta);
                    }
                    TabId::Registries => {
                        self.registries.handle_wheel(&mut self.shell, target, delta);
                    }
                }
                AppAction::None
            }
            _ => AppAction::None,
        }
    }

    /// A worker event, and whatever it asks the run loop to do next.
    ///
    /// `Applied::Value` is the one that does not stay here: it goes to the
    /// screen that asked for it, which either shows it or copies it and
    /// keeps nothing either way.
    pub fn apply(&mut self, event: worker::Event, now: Instant) -> AppAction {
        let secrets_was = self.secrets.cursor_identity(&self.store);
        let registries_was = self.registries.cursor_identity(&self.store);
        match self.store.apply(event) {
            Applied::Secrets => {
                self.secrets.invalidate();
                self.secrets.keep_cursor(&self.store, secrets_was);
            }
            Applied::Repositories => {
                self.registries.invalidate();
                self.registries.keep_cursor(&self.store, registries_was);
            }
            Applied::Value {
                vault,
                name,
                result,
            } => {
                return self.secrets.on_value(
                    &mut self.shell,
                    &self.store,
                    &vault,
                    &name,
                    result,
                    now,
                );
            }
            // A repository's tags re-read with the same count would otherwise
            // keep the old order under the new list.
            Applied::Detail => self.registries.invalidate(),
            Applied::Nothing | Applied::Status => {}
        }
        AppAction::None
    }

    /// One turn of the clock: what has run out, and what the cursor has
    /// settled long enough to be worth asking about.
    pub fn tick(&mut self, now: Instant) -> Option<worker::Request> {
        match self.tab {
            TabId::Secrets => {
                self.secrets.refilter(&self.store);
                self.secrets.tick(&self.store, now)
            }
            TabId::Registries => {
                self.registries.refilter(&self.store);
                self.registries.tick(&self.store, now)
            }
        }
    }

    /// How long the run loop may sleep: a second while something is counting
    /// down or being waited for, the rest interval while the cursor has just
    /// landed somewhere, and whatever the caller wanted otherwise.
    #[must_use]
    pub fn poll_for(&self, settled: Duration) -> Duration {
        if self.store.refreshing {
            return Duration::from_millis(100);
        }
        if self.secrets.is_ticking() {
            return Duration::from_secs(1);
        }
        if self.secrets.is_resting() || self.registries.is_resting() {
            return crate::app::secrets::REST;
        }
        settled
    }

    /// `r`, or a tab switch: everything a screen was holding that a refresh
    /// makes stale, the revealed value first.
    fn on_refresh(&mut self) {
        self.secrets.on_refresh();
        self.registries.on_refresh();
    }

    // ── The session ────────────────────────────────────────────────────

    /// The layout as it stands, for the file. Never the query, never the
    /// cursor, and nothing a vault answered.
    #[must_use]
    pub fn session(&self) -> Session {
        let mut session = Session {
            tab: Some(tab_key(self.tab).to_owned()),
            ..Session::default()
        };

        let secrets = session.tab(tab_key(TabId::Secrets));
        secrets.sort = Some(sort_of(self.secrets.sort, self.secrets.descending));
        secrets.columns = columns_of(&self.secrets.layout);

        let registries = session.tab(tab_key(TabId::Registries));
        registries.sort = Some(sort_of(
            self.registries.repositories.sort,
            self.registries.repositories.descending,
        ));
        registries.columns = columns_of(&self.registries.repositories.layout);
        session
    }

    /// Puts a session back, before the first frame. Anything this build does
    /// not recognise is left where it is: a column key from a newer version
    /// is carried in the file and ignored here, rather than dropped and lost
    /// the next time an older binary runs.
    pub fn restore(&mut self, session: &Session) {
        if let Some(tab) = session
            .tab
            .as_deref()
            .and_then(|name| TabId::ALL.into_iter().find(|held| tab_key(*held) == name))
        {
            self.tab = tab;
        }
        if let Some(held) = session.tabs.get(tab_key(TabId::Secrets)) {
            if let Some((column, way)) = read_sort(held.sort.as_ref()) {
                self.secrets.sort = column;
                self.secrets.descending = way;
            }
            apply_columns(&mut self.secrets.layout, &held.columns);
            self.secrets.invalidate();
        }
        if let Some(held) = session.tabs.get(tab_key(TabId::Registries)) {
            if let Some((column, way)) = read_sort(held.sort.as_ref()) {
                self.registries.repositories.sort = column;
                self.registries.repositories.descending = way;
            }
            apply_columns(&mut self.registries.repositories.layout, &held.columns);
            self.registries.invalidate();
        }
    }

    /// What the status bar says on the left when nothing has just happened.
    #[must_use]
    pub fn footer_hint(&self) -> String {
        match self.tab {
            TabId::Secrets => self.secrets.footer_hint(&self.shell),
            TabId::Registries => self.registries.footer_hint(&self.shell),
        }
    }

    /// One frame.
    pub fn render(&mut self, frame: &mut Frame, millis: u128) {
        let area = frame.area();
        if area.width < ui::MIN_WIDTH || area.height < ui::MIN_HEIGHT {
            ui::render_too_small(frame, area);
            return;
        }
        self.shell.begin_frame();
        let [tabs, body, status] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .areas(area);

        let badges = [
            (TabId::Secrets, self.secrets.badge(&self.store)),
            (TabId::Registries, self.registries.badge(&self.store)),
        ];
        ui::widgets::render_tab_bar(frame, &mut self.shell, tabs, self.tab, &badges);
        self.render_body(frame, body);
        let hint = self.footer_hint();
        ui::widgets::render_status_bar(
            frame,
            &mut self.shell,
            status,
            &hint,
            &self.store,
            self.tab,
            millis,
        );
        if self.shell.help_open {
            ui::widgets::render_help(frame, &mut self.shell, area, self.tab, &self.store);
        }
    }

    fn render_body(&mut self, frame: &mut Frame, area: Rect) {
        match self.tab {
            TabId::Secrets => {
                self.secrets.refilter(&self.store);
                ui::secrets::render(frame, &mut self.shell, &mut self.secrets, &self.store, area);
            }
            TabId::Registries => {
                self.registries.refilter(&self.store);
                ui::registries::render(
                    frame,
                    &mut self.shell,
                    &mut self.registries,
                    &self.store,
                    area,
                );
            }
        }
    }
}

/// `None` sorts last in both directions: a stamp that is not set, or a count
/// that has not arrived, is not the most interesting row on screen, and
/// flipping the sort should not make it so.
pub(crate) fn none_last<T: Ord>(
    left: Option<T>,
    right: Option<T>,
    descending: bool,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (left, right) {
        (None, None) => Ordering::Equal,
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (Some(left), Some(right)) => flip(left.cmp(&right), descending),
    }
}

/// An ordering, turned over when the sort is descending.
pub(crate) const fn flip(ordering: std::cmp::Ordering, descending: bool) -> std::cmp::Ordering {
    if descending {
        ordering.reverse()
    } else {
        ordering
    }
}

/// What the session file calls each tab.
const fn tab_key(tab: TabId) -> &'static str {
    match tab {
        TabId::Secrets => "secrets",
        TabId::Registries => "registries",
    }
}

fn sort_of(column: ColumnId, descending: bool) -> (String, String) {
    (
        column.key().to_owned(),
        if descending { "desc" } else { "asc" }.to_owned(),
    )
}

/// A sort out of the file, if this build knows the column it names.
fn read_sort(sort: Option<&(String, String)>) -> Option<(ColumnId, bool)> {
    let (key, way) = sort?;
    Some((ColumnId::from_key(key)?, way.eq_ignore_ascii_case("desc")))
}

fn columns_of(layout: &TableLayout) -> Vec<SessionColumn> {
    layout
        .columns
        .iter()
        .map(|column| SessionColumn {
            key: column.id.key().to_owned(),
            width: Some(column.width),
            visible: Some(column.visible),
        })
        .collect()
}

/// Widths and visibility out of the file. A key this build does not know is
/// skipped; a column the file does not mention keeps its default.
fn apply_columns(layout: &mut TableLayout, held: &[SessionColumn]) {
    for stored in held {
        let Some(id) = ColumnId::from_key(&stored.key) else {
            continue;
        };
        let Some(column) = layout.columns.iter_mut().find(|column| column.id == id) else {
            continue;
        };
        if let Some(width) = stored.width {
            column.width = width;
        }
        if let Some(visible) = stored.visible {
            column.visible = visible;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_layout_survives_a_round_trip_through_the_file() {
        let mut app = App::new(Store::default());
        app.tab = TabId::Registries;
        app.secrets.sort = ColumnId::Expires;
        app.secrets.descending = true;
        app.secrets.layout.columns[0].width = 20;
        app.secrets.layout.columns[1].visible = false;

        let session = app.session();
        let mut fresh = App::new(Store::default());
        fresh.restore(&session);

        assert_eq!(fresh.tab, TabId::Registries);
        assert_eq!(fresh.secrets.sort, ColumnId::Expires);
        assert!(fresh.secrets.descending);
        assert_eq!(fresh.secrets.layout.columns[0].width, 20);
        assert!(!fresh.secrets.layout.columns[1].visible);
    }

    #[test]
    fn a_key_this_build_does_not_know_is_skipped_rather_than_fatal() {
        let mut session = Session {
            tab: Some("environments".into()),
            ..Session::default()
        };
        let held = session.tab("secrets");
        held.sort = Some(("from_the_future".into(), "asc".into()));
        held.columns = vec![SessionColumn {
            key: "from_the_future".into(),
            width: Some(9),
            visible: Some(false),
        }];

        let mut app = App::new(Store::default());
        let before = app.secrets.layout.clone();
        app.restore(&session);
        assert_eq!(app.tab, TabId::Secrets, "an unknown tab is the first one");
        assert_eq!(
            app.secrets.sort,
            ColumnId::Name,
            "and an unknown sort is the default"
        );
        assert_eq!(app.secrets.layout, before);
    }

    #[test]
    fn a_paste_lands_in_the_search_box_and_nowhere_else() {
        let mut app = App::new(Store::default());
        app.handle_paste("db\npass");
        assert!(app.secrets.input.is_empty(), "the table took nothing");
        app.shell.focus = Focus::Search;
        app.handle_paste("db\npass");
        assert_eq!(app.secrets.input.text(), "db pass");
    }

    #[test]
    fn tab_leaves_the_search_box_and_esc_backs_out_of_a_repository() {
        let mut app = App::new(Store::default());
        app.shell.focus = Focus::Search;
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.shell.focus, Focus::Table);

        // A query is what Esc takes off first; with none, the screen has it.
        app.secrets.input.set_text("db");
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.secrets.input.is_empty());
        let mut app = App::new(crate::app::registries::tests::stocked());
        app.tab = TabId::Registries;
        app.registries.refilter(&app.store);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            app.registries.level,
            registries::Level::Tags { .. }
        ));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.registries.level, registries::Level::Repositories);
    }

    #[test]
    fn a_click_puts_focus_where_it_landed() {
        let mut app = App::new(Store::default());
        app.shell.begin_frame();
        app.shell.region(Rect::new(0, 5, 40, 10), Target::Details);
        app.shell.region(Rect::new(0, 1, 40, 1), Target::Row(0));
        app.shell.focus = Focus::Search;
        let click = |column, row| MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(click(3, 1));
        assert_eq!(app.shell.focus, Focus::Table);
        app.handle_mouse(click(3, 7));
        assert_eq!(app.shell.focus, Focus::Details);
    }

    #[test]
    fn a_re_read_of_the_open_repositorys_tags_is_shown_in_its_new_order() {
        use crate::azure::Tag;
        use crate::timestamp::ts;

        let mut app = App::new(crate::app::registries::tests::stocked());
        app.tab = TabId::Registries;
        app.registries.refilter(&app.store);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.registries.refilter(&app.store);
        let first = |app: &App| {
            app.registries
                .selected_tag(&app.store)
                .map(|tag| tag.name.clone())
        };
        let was = first(&app).expect("a tag under the cursor");
        // The same two names come back with their stamps swapped: the other
        // one is now the newest, and `Updated ↓` must put it first.
        let (registry, repo) = app
            .registries
            .open_repository()
            .map(|(r, p)| (r.to_owned(), p.to_owned()))
            .unwrap();
        let Ok(tags) = app.store.tags[&(registry.clone(), repo.clone())].clone() else {
            panic!("tags");
        };
        let swapped: Vec<Tag> = tags
            .iter()
            .enumerate()
            .map(|(at, tag)| Tag {
                updated: Some(ts(if at == 0 {
                    "2020-01-01T00:00:00Z"
                } else {
                    "2030-01-01T00:00:00Z"
                })),
                ..tag.clone()
            })
            .collect();
        app.apply(
            worker::Event::Tags {
                registry,
                repo,
                result: Ok(swapped),
            },
            Instant::now(),
        );
        app.registries.refilter(&app.store);
        assert_ne!(first(&app).unwrap(), was, "the newest tag is first again");
    }

    #[test]
    fn the_session_never_carries_the_query_or_the_cursor() {
        let mut app = App::new(Store::default());
        app.secrets.input.set_text("vault:kv-prod db-password");
        app.secrets.cursor.focus(7);
        let written = serde_json::to_string(&app.session()).unwrap();
        assert!(!written.contains("db-password"), "{written}");
        assert!(!written.contains("kv-prod"), "{written}");
        assert!(!written.contains("cursor"), "{written}");
    }
}
