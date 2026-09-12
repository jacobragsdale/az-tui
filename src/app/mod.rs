//! The application: which tab is open, what the keys do before a screen sees
//! them, and how a frame is put together.

pub mod cursor;
pub mod keys;
pub mod registries;
pub mod screen;
pub mod secrets;
pub mod shell;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

use screen::{AppAction, TabId, Target};
use secrets::SecretsScreen;
use shell::{Focus, Shell};

use crate::store::{Applied, Store};
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
            KeyCode::Char('q') | KeyCode::Esc if !self.shell.help_open => {
                if key.code == KeyCode::Esc {
                    // Esc out of the table clears the query rather than
                    // quitting; only `q` quits.
                    self.clear_query();
                    return AppAction::None;
                }
                AppAction::Quit
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

    /// While the box has focus every key is a character, except the two that
    /// leave it.
    fn key_in_search(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Enter => {
                self.shell.focus = Focus::Table;
                AppAction::None
            }
            KeyCode::Esc => {
                self.shell.focus = Focus::Table;
                AppAction::None
            }
            _ => {
                let input = match self.tab {
                    TabId::Secrets => &mut self.secrets.input,
                    TabId::Registries => self.registries.input_mut(),
                };
                input.handle_key(key);
                AppAction::None
            }
        }
    }

    /// `Esc` out of the table: the filter goes, and the table comes back
    /// whole.
    fn clear_query(&mut self) {
        match self.tab {
            TabId::Secrets => self.secrets.input.clear(),
            TabId::Registries => self.registries.input_mut().clear(),
        }
    }

    fn screen_key(&mut self, key: KeyEvent) -> AppAction {
        let store = &self.store;
        match self.tab {
            TabId::Secrets => self.secrets.handle_key(&mut self.shell, store, key),
            TabId::Registries => self.registries.handle_key(&mut self.shell, store, key),
        }
    }

    fn switch_to(&mut self, tab: TabId) {
        if self.tab != tab {
            self.tab = tab;
            self.shell.focus = Focus::Table;
            self.on_tab_switch();
        }
    }

    /// A click, resolved against what was drawn last frame.
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
                Some(target) => {
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

    /// A worker event, and whatever it changed on the screen looking at it.
    pub fn apply(&mut self, event: worker::Event) -> Applied {
        let secrets_was = self.secrets.cursor_identity(&self.store);
        let registries_was = self.registries.cursor_identity(&self.store);
        let applied = self.store.apply(event);
        match applied {
            Applied::Secrets => {
                self.secrets.invalidate();
                self.secrets.keep_cursor(&self.store, secrets_was);
            }
            Applied::Repositories => {
                self.registries.invalidate();
                self.registries.keep_cursor(&self.store, registries_was);
            }
            _ => {}
        }
        applied
    }

    /// `r`: everything a screen was holding that a refresh makes stale.
    fn on_refresh(&mut self) {
        self.secrets.on_refresh();
    }

    /// Switching tabs drops a revealed value, like every other way of
    /// looking away from it.
    fn on_tab_switch(&mut self) {
        self.secrets.on_refresh();
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
        ui::widgets::render_status_bar(frame, &mut self.shell, status, &hint, &self.store, millis);
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
