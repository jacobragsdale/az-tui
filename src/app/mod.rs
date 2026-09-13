//! The application: which tab is open, what the keys do before a screen sees
//! them, and how a frame is put together.

pub mod cursor;
pub mod keys;
pub mod list;
pub mod registries;
pub mod scope;
pub mod screen;
pub mod secrets;
pub mod shell;

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;

use registries::RegistriesScreen;
use scope::ScopeScreen;
use screen::{AppAction, Button, Tab, Target};
use secrets::{REST, SecretsScreen};
use shell::{Focus, Menu, Shell};

use crate::columns::{ColumnId, TableLayout};
use crate::config;
use crate::filter::{self, ENV_CHOICES, Env};
use crate::kube::{self, Kind, LogFollow, TextKind};
use crate::session::{Session, SessionColumn};
use crate::store::{Applied, ScopeData, Store, azure};
use crate::text_input::TextInput;
use crate::timestamp::Timestamp;
use crate::ui;
use crate::ui::widgets::{TabLabel, spinner_frame};
use crate::worker;

/// One tab's screen. A scope tab is boxed because it carries four lists and
/// the text pane; the two Azure screens are a fraction of that.
pub enum Screen {
    Scope(Box<ScopeScreen>),
    Secrets(SecretsScreen),
    Registries(RegistriesScreen),
}

impl Screen {
    #[must_use]
    pub fn scope(&self) -> Option<&ScopeScreen> {
        match self {
            Self::Scope(screen) => Some(screen),
            _ => None,
        }
    }

    pub fn scope_mut(&mut self) -> Option<&mut ScopeScreen> {
        match self {
            Self::Scope(screen) => Some(screen),
            _ => None,
        }
    }

    #[must_use]
    pub fn secrets(&self) -> Option<&SecretsScreen> {
        match self {
            Self::Secrets(screen) => Some(screen),
            _ => None,
        }
    }

    pub fn secrets_mut(&mut self) -> Option<&mut SecretsScreen> {
        match self {
            Self::Secrets(screen) => Some(screen),
            _ => None,
        }
    }

    #[must_use]
    pub fn registries(&self) -> Option<&RegistriesScreen> {
        match self {
            Self::Registries(screen) => Some(screen),
            _ => None,
        }
    }

    pub fn registries_mut(&mut self) -> Option<&mut RegistriesScreen> {
        match self {
            Self::Registries(screen) => Some(screen),
            _ => None,
        }
    }

    /// The search box of the table this screen is showing.
    #[must_use]
    pub fn input(&self) -> &TextInput {
        match self {
            Self::Scope(screen) => &screen.list().input,
            Self::Secrets(screen) => &screen.input,
            Self::Registries(screen) => screen.input(),
        }
    }

    pub fn input_mut(&mut self) -> &mut TextInput {
        match self {
            Self::Scope(screen) => &mut screen.list_mut().input,
            Self::Secrets(screen) => &mut screen.input,
            Self::Registries(screen) => screen.input_mut(),
        }
    }

    /// Which of the help's sections this screen's keys are in.
    #[must_use]
    pub const fn section(&self) -> keys::Section {
        match self {
            Self::Scope(_) => keys::Section::Aks,
            Self::Secrets(_) => keys::Section::Secrets,
            Self::Registries(_) => keys::Section::Registries,
        }
    }
}

pub struct App {
    pub shell: Shell,
    /// The tabs in the bar's order: every AKS scope, then Secrets, then
    /// Registries. One screen each.
    pub tabs: Vec<Tab>,
    pub tab: usize,
    pub screens: Vec<Screen>,
    pub store: Store,
    /// Whether a read has landed since the cache was last written.
    pub cache_dirty: bool,
    /// What the kube worker was last told to follow, so the tick only
    /// speaks when that changes.
    following: Option<LogFollow>,
    /// Where the cursor is on a scope tab and when it got there, for the
    /// rest interval that gates the owner read.
    rested: Option<(usize, usize, Instant)>,
}

impl App {
    #[must_use]
    pub fn new(tabs: Vec<Tab>, mut store: Store) -> Self {
        let screens = tabs
            .iter()
            .map(|tab| match tab {
                Tab::Scope(tab) => {
                    Screen::Scope(Box::new(ScopeScreen::new(tab.scope.namespace.is_none())))
                }
                Tab::Secrets => Screen::Secrets(SecretsScreen::default()),
                Tab::Registries => Screen::Registries(RegistriesScreen::default()),
            })
            .collect();
        // One store slot per scope tab, whether or not the cache had it.
        let scopes = tabs
            .iter()
            .filter(|tab| matches!(tab, Tab::Scope(_)))
            .count();
        store.scopes.resize_with(scopes, ScopeData::default);
        Self {
            shell: Shell::default(),
            tabs,
            tab: 0,
            screens,
            store,
            cache_dirty: false,
            following: None,
            rested: None,
        }
    }

    /// The open tab when it is a scope, borrowed apart. `None` on Secrets
    /// and Registries.
    fn scope_parts(&mut self) -> Option<(&config::Tab, &mut ScopeScreen, &ScopeData, &mut Shell)> {
        let Tab::Scope(tab) = self.tabs.get(self.tab)? else {
            return None;
        };
        let screen = self.screens.get_mut(self.tab)?.scope_mut()?;
        // A scope tab's index is its kube scope: scope tabs come first in
        // config order and `store.scopes` was built from that same list.
        // Nothing remaps.
        let data = self.store.scopes.get(self.tab)?;
        Some((tab, screen, data, &mut self.shell))
    }

    /// The kind the open tab shows, when it is a scope tab.
    #[must_use]
    pub fn kind(&self) -> Option<Kind> {
        self.screens
            .get(self.tab)
            .and_then(Screen::scope)
            .map(|screen| screen.kind)
    }

    /// One key. The global keys are matched here first; everything else goes
    /// to the tab, except while a box has focus, which takes everything but
    /// `Esc`, `Enter` and `Tab`.
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
        let tab = self.tab;
        if let Some(screen) = self.screens.get_mut(tab).and_then(Screen::scope_mut)
            && screen.modal.is_some()
        {
            // The modal takes every key: its own answer it, any other closes
            // it and is not otherwise acted on.
            return screen
                .modal_key(&mut self.shell, tab, key)
                .map_or(AppAction::None, AppAction::Kube);
        }
        if let Some((menu, at)) = self.shell.menu {
            return self.menu_key(menu, at, key);
        }
        if self.shell.focus == Focus::Search {
            return self.key_in_search(key);
        }
        if self.shell.focus == Focus::PaneSearch {
            return self.key_in_pane_search(key);
        }
        let pane_open = self
            .screens
            .get(tab)
            .and_then(Screen::scope)
            .is_some_and(|screen| screen.pane_open);
        match key.code {
            KeyCode::Char(number @ '1'..='9') => {
                let index = usize::from(u8::try_from(number).unwrap_or(b'1') - b'1');
                self.switch_to(index)
            }
            KeyCode::Char('[') | KeyCode::Left => self.step_tab(-1),
            KeyCode::Char(']') | KeyCode::Right => self.step_tab(1),
            KeyCode::Char('?') => {
                self.shell.help_open = true;
                AppAction::None
            }
            KeyCode::Char('q') => AppAction::Quit,
            KeyCode::Esc => self.escape(key),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('/') if pane_open && self.shell.focus == Focus::Details => {
                self.shell.focus = Focus::PaneSearch;
                AppAction::None
            }
            KeyCode::Char('/') => {
                self.shell.focus = Focus::Search;
                AppAction::None
            }
            KeyCode::Tab => {
                self.shell.toggle_focus();
                AppAction::None
            }
            _ => match self.kind() {
                Some(kind) => self.scope_key(kind, key),
                None => self.screen_key(key),
            },
        }
    }

    /// The keys a scope tab has on top of the shared ones: the kinds, the
    /// toolbar, the pane, and the copies.
    fn scope_key(&mut self, kind: Kind, key: KeyEvent) -> AppAction {
        let tab = self.tab;
        match key.code {
            // The kinds. `e` on a pod is that pod's events.
            KeyCode::Char('p') => self.set_kind(Kind::Pods),
            KeyCode::Char('e') if kind == Kind::Pods => {
                if let Some((_, screen, data, _)) = self.scope_parts() {
                    screen.events_for_selected_pod(data);
                }
                self.shell.focus = Focus::Table;
                AppAction::Kube(kube::Request::Showing(tab, Kind::Events))
            }
            KeyCode::Char('e') => self.set_kind(Kind::Events),
            KeyCode::Char('m') => self.set_kind(Kind::ConfigMaps),
            KeyCode::Char('s') => self.set_kind(Kind::Secrets),
            KeyCode::Enter => match kind {
                Kind::Pods => self.button(Button::Logs),
                Kind::Events => self.button(Button::Pod),
                Kind::ConfigMaps | Kind::Secrets => self.button(Button::Value),
            },
            KeyCode::Char('l') if kind == Kind::Pods => self.button(Button::Logs),
            KeyCode::Char('d') => self.button(Button::Describe),
            KeyCode::Char('v') => match kind {
                Kind::Pods | Kind::Events => self.button(Button::Yaml),
                Kind::ConfigMaps | Kind::Secrets => self.button(Button::Value),
            },
            KeyCode::Char('b') if kind == Kind::Pods => self.button(Button::Bash),
            KeyCode::Char('x') if kind == Kind::Pods => self.button(Button::Restart),
            KeyCode::Char('X') if kind == Kind::Pods => {
                if let Some((_, screen, data, shell)) = self.scope_parts() {
                    screen.rollout_prompt(shell, data);
                }
                AppAction::None
            }
            KeyCode::Char('=') if kind == Kind::Pods => self.button(Button::Scale),
            KeyCode::Char('b' | 'x' | 'X' | '=' | 'l') => {
                self.shell.set_status("That is a pod's key: p for the pods");
                AppAction::None
            }
            KeyCode::Char('P') if kind == Kind::Pods => {
                if let Some((_, screen, _, shell)) = self.scope_parts() {
                    screen.toggle_previous(shell);
                }
                AppAction::None
            }
            KeyCode::Char('C') if kind == Kind::Pods => {
                if let Some((_, screen, data, shell)) = self.scope_parts() {
                    screen.next_container(shell, data);
                }
                AppAction::None
            }
            KeyCode::Char('z') => {
                if let Some((_, screen, _, _)) = self.scope_parts() {
                    screen.toggle_zoom();
                }
                AppAction::None
            }
            KeyCode::Char('y') => match kind {
                Kind::ConfigMaps | Kind::Secrets => self.button(Button::Copy),
                _ => self
                    .scope_parts()
                    .and_then(|(_, screen, data, _)| screen.selected_name(data))
                    .map_or(AppAction::None, |name| AppAction::Copy {
                        label: format!("Copied {name}"),
                        text: name,
                    }),
            },
            KeyCode::Char('Y') => self
                .scope_parts()
                .and_then(|(tab, screen, data, _)| screen.kubectl_line(tab, data))
                .map_or(AppAction::None, |line| AppAction::Copy {
                    label: format!("Copied `{line}`"),
                    text: line,
                }),
            _ => self.screen_key(key),
        }
    }

    /// The open drop-down takes the keys that walk it; `Enter` chooses, any
    /// other key closes it and is not otherwise acted on.
    fn menu_key(&mut self, menu: Menu, at: usize, key: KeyEvent) -> AppAction {
        let count = match menu {
            Menu::Env => ENV_CHOICES.len(),
            Menu::Kind => Kind::ALL.len(),
        };
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.shell.menu = Some((menu, (at + 1) % count));
                AppAction::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.shell.menu = Some((menu, (at + count - 1) % count));
                AppAction::None
            }
            KeyCode::Enter => {
                self.shell.menu = None;
                match menu {
                    Menu::Env => {
                        self.choose_env(ENV_CHOICES[at]);
                        AppAction::None
                    }
                    Menu::Kind => self.set_kind(Kind::ALL[at]),
                }
            }
            _ => {
                self.shell.menu = None;
                AppAction::None
            }
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
                if let Some(input) = self.input() {
                    input.handle_key(key);
                }
            }
        }
        AppAction::None
    }

    /// The pane's filter takes every key but the three that leave it.
    fn key_in_pane_search(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Enter | KeyCode::Esc | KeyCode::Tab => {
                self.shell.focus = Focus::Details;
            }
            _ => {
                if let Some(screen) = self.screens.get_mut(self.tab).and_then(Screen::scope_mut) {
                    screen.pane_filter.handle_key(key);
                    screen.scroll_pane(0);
                }
            }
        }
        AppAction::None
    }

    /// `Esc` takes things off in the order they went on. The modal was taken
    /// before this is reached.
    fn escape(&mut self, key: KeyEvent) -> AppAction {
        // A scope tab: the pane's filter, then the query, then the pane.
        if let Some(screen) = self.screens.get_mut(self.tab).and_then(Screen::scope_mut) {
            if screen.pane_open && !screen.pane_filter.is_empty() {
                screen.pane_filter.clear();
            } else if !screen.list().input.is_empty() {
                screen.list_mut().input.clear();
            } else if screen.pane_open {
                screen.close_pane();
                self.shell.focus = Focus::Table;
            }
            return AppAction::None;
        }
        // An Azure tab: the query first; with none, the screen has it, which
        // is how Registries backs out of a repository.
        if self.clear_query() {
            AppAction::None
        } else {
            self.screen_key(key)
        }
    }

    /// One toolbar button, whether clicked or pressed as its key.
    fn button(&mut self, button: Button) -> AppAction {
        let tab = self.tab;
        let Some((tab_ref, screen, data, shell)) = self.scope_parts() else {
            return AppAction::None;
        };
        match button {
            Button::Logs => {
                let open = screen.toggle_log();
                shell.focus = if open { Focus::Details } else { Focus::Table };
                AppAction::None
            }
            Button::Describe => {
                let request = screen.show_text(tab, TextKind::Describe, data);
                shell.focus = Focus::Details;
                request.map_or(AppAction::None, AppAction::Kube)
            }
            Button::Yaml => {
                let request = screen.show_text(tab, TextKind::Yaml, data);
                shell.focus = Focus::Details;
                request.map_or(AppAction::None, AppAction::Kube)
            }
            Button::Bash => screen.bash_target(tab_ref, data).unwrap_or_else(|| {
                shell.set_error("No pod is selected");
                AppAction::None
            }),
            Button::Restart => {
                screen.restart_prompt(shell, data);
                AppAction::None
            }
            Button::Scale => screen
                .scale_prompt(shell, tab, data)
                .map_or(AppAction::None, AppAction::Kube),
            Button::Pod => {
                screen.jump_to_object(shell, data);
                shell.focus = Focus::Table;
                if screen.kind == Kind::Pods {
                    AppAction::Kube(kube::Request::Showing(tab, Kind::Pods))
                } else {
                    AppAction::None
                }
            }
            Button::Value => {
                let request = screen.show_value(tab, data);
                shell.focus = Focus::Details;
                request.map_or(AppAction::None, AppAction::Kube)
            }
            Button::Copy => screen.copy_value(shell, tab, data),
        }
    }

    /// A paste, which bracketed paste hands over whole. It goes into
    /// whichever box has focus, and nowhere otherwise.
    pub fn handle_paste(&mut self, text: &str) -> AppAction {
        match self.shell.focus {
            Focus::Search => {
                if let Some(input) = self.input() {
                    input.paste(text);
                }
            }
            Focus::PaneSearch => {
                if let Some(screen) = self.screens.get_mut(self.tab).and_then(Screen::scope_mut) {
                    screen.pane_filter.paste(text);
                }
            }
            _ => {}
        }
        AppAction::None
    }

    /// The search box of whichever table is showing.
    fn input(&mut self) -> Option<&mut TextInput> {
        self.screens.get_mut(self.tab).map(Screen::input_mut)
    }

    /// What that box says.
    fn query(&self) -> &str {
        self.screens
            .get(self.tab)
            .map_or("", |screen| screen.input().text())
    }

    /// The environment the query is filtered to, for the menu's tick.
    fn current_env(&self) -> Option<Env> {
        filter::env_of_query(self.query())
    }

    /// A click on the Env header: the menu opens on the line the query is
    /// already at.
    fn open_env_menu(&mut self) {
        let current = self.current_env();
        self.shell.menu = ENV_CHOICES
            .iter()
            .position(|held| *held == current)
            .map(|at| (Menu::Env, at));
    }

    /// A choice from the menu goes into the search box as `env:prod`, where
    /// it can be seen, typed, and taken off with `Esc` like any filter.
    fn choose_env(&mut self, env: Option<Env>) {
        if let Some(input) = self.input() {
            let text = filter::with_env(input.text(), env);
            input.set_text(text);
        }
        self.shell.menu = None;
    }

    /// `Esc` out of the table, or the `×`: the filter goes, and the table
    /// comes back whole. Says whether there was one to take off.
    fn clear_query(&mut self) -> bool {
        let Some(input) = self.input() else {
            return false;
        };
        let had = !input.is_empty();
        input.clear();
        had
    }

    fn screen_key(&mut self, key: KeyEvent) -> AppAction {
        let tab = self.tab;
        let shell = &mut self.shell;
        let store = &self.store;
        match self.screens.get_mut(tab) {
            Some(Screen::Scope(screen)) => store
                .scopes
                .get(tab)
                .map_or(AppAction::None, |data| screen.handle_key(shell, data, key)),
            Some(Screen::Secrets(screen)) => screen.handle_key(shell, &store.azure, key),
            Some(Screen::Registries(screen)) => screen.handle_key(shell, &store.azure, key),
            None => AppAction::None,
        }
    }

    /// `r`: the open tab's half read again now. A scope is one namespace;
    /// the Azure half is every vault and registry.
    fn refresh(&mut self) -> AppAction {
        let tab = self.tab;
        match self.tabs.get(tab) {
            Some(Tab::Scope(scope)) => {
                self.shell
                    .set_status(format!("Reading {}…", scope.scope.describe()));
                if let Some(screen) = self.screens.get_mut(tab).and_then(Screen::scope_mut) {
                    screen.on_refresh();
                }
                AppAction::Kube(kube::Request::Refresh(tab))
            }
            Some(_) => {
                self.on_azure_refresh();
                AppAction::Azure(worker::Request::Refresh)
            }
            None => AppAction::None,
        }
    }

    /// Everything the two Azure screens hold that a refresh makes stale, the
    /// revealed value first. Switching tabs drops it too, like every other
    /// way of looking away from it.
    fn on_azure_refresh(&mut self) {
        for screen in &mut self.screens {
            match screen {
                Screen::Secrets(screen) => screen.on_refresh(),
                Screen::Registries(screen) => screen.on_refresh(),
                Screen::Scope(_) => {}
            }
        }
    }

    /// Another tab. A scope tab tells the kube worker, so it is read at once
    /// and kept fresh while it shows.
    fn switch_to(&mut self, tab: usize) -> AppAction {
        if self.tab == tab || tab >= self.tabs.len() {
            return AppAction::None;
        }
        self.tab = tab;
        self.shell.focus = Focus::Table;
        self.on_azure_refresh();
        // ponytail: leaving for an Azure tab says nothing to the kube
        // worker, so the last scope keeps its fast cadence. A `Showing`
        // of nothing is the fix, when the traffic shows.
        self.kind().map_or(AppAction::None, |kind| {
            AppAction::Kube(kube::Request::Showing(tab, kind))
        })
    }

    /// `[` and `]`: the previous and the next tab, round the ends.
    fn step_tab(&mut self, by: isize) -> AppAction {
        let count = self.tabs.len();
        if count == 0 {
            return AppAction::None;
        }
        let next = (self.tab as isize + by).rem_euclid(count as isize) as usize;
        self.switch_to(next)
    }

    /// Another kind on this scope tab: the worker reads it at once and keeps
    /// it fresh while it shows.
    fn set_kind(&mut self, kind: Kind) -> AppAction {
        let tab = self.tab;
        let Some(screen) = self.screens.get_mut(tab).and_then(Screen::scope_mut) else {
            return AppAction::None;
        };
        if screen.kind == kind {
            return AppAction::None;
        }
        screen.set_kind(kind);
        self.shell.focus = Focus::Table;
        AppAction::Kube(kube::Request::Showing(tab, kind))
    }

    /// A click, resolved against what was drawn last frame. A click lands
    /// focus where it lands: on a row, the table; on the pane, the pane.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> AppAction {
        let target = self.shell.hit(event.column, event.row).cloned();
        let tab = self.tab;
        if let Some(screen) = self.screens.get_mut(tab).and_then(Screen::scope_mut)
            && screen.modal.is_some()
        {
            // The open modal takes the pointer: its yes answers it, its body
            // does nothing, anywhere else closes it. The wheel is ignored.
            if event.kind != MouseEventKind::Down(crossterm::event::MouseButton::Left) {
                return AppAction::None;
            }
            return match target {
                Some(Target::Confirm) => screen
                    .confirm(&mut self.shell, tab)
                    .map_or(AppAction::None, AppAction::Kube),
                Some(Target::Modal) => AppAction::None,
                _ => {
                    screen.dismiss();
                    AppAction::None
                }
            };
        }
        if let MouseEventKind::Down(_) = event.kind
            && let Some((menu, _)) = self.shell.menu.take()
        {
            // The open menu takes the click: one of its lines chooses,
            // anywhere else closes it and does nothing more.
            return match (menu, target) {
                (Menu::Env, Some(Target::EnvOption(env))) => {
                    self.choose_env(env);
                    AppAction::None
                }
                (Menu::Kind, Some(Target::KindOption(kind))) => self.set_kind(kind),
                _ => AppAction::None,
            };
        }
        match event.kind {
            MouseEventKind::Down(_) => match target {
                Some(Target::Tab(index)) => self.switch_to(index),
                Some(Target::Header(ColumnId::Env)) => {
                    self.shell.focus = Focus::Table;
                    self.open_env_menu();
                    AppAction::None
                }
                Some(Target::KindPill) => {
                    self.shell.menu = self
                        .kind()
                        .and_then(|kind| Kind::ALL.iter().position(|held| *held == kind))
                        .map(|at| (Menu::Kind, at));
                    AppAction::None
                }
                Some(Target::KindOption(kind)) => self.set_kind(kind),
                Some(Target::Button(button)) => self.button(button),
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
                Some(Target::Details | Target::TextPane) => {
                    self.shell.focus = Focus::Details;
                    AppAction::None
                }
                Some(target) => {
                    self.shell.focus = Focus::Table;
                    let shell = &mut self.shell;
                    let store = &self.store.azure;
                    match self.screens.get_mut(tab) {
                        Some(Screen::Scope(screen)) => screen.handle_click(shell, target),
                        Some(Screen::Secrets(screen)) => screen.handle_click(shell, store, target),
                        Some(Screen::Registries(screen)) => {
                            screen.handle_click(shell, store, target)
                        }
                        None => AppAction::None,
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
                let shell = &mut self.shell;
                match self.screens.get_mut(tab) {
                    Some(Screen::Scope(screen)) => screen.handle_wheel(shell, target, delta),
                    Some(Screen::Secrets(screen)) => screen.handle_wheel(shell, target, delta),
                    Some(Screen::Registries(screen)) => screen.handle_wheel(shell, target, delta),
                    None => {}
                }
                AppAction::None
            }
            _ => AppAction::None,
        }
    }

    /// An Azure worker event, and whatever it asks the run loop to do next.
    ///
    /// `Applied::Value` is the one that does not stay here: it goes to the
    /// Secrets screen that asked for it, which either shows it or copies it
    /// and keeps nothing either way.
    pub fn apply_azure(&mut self, event: worker::Event, now: Instant) -> AppAction {
        let store = &self.store.azure;
        let secrets_was = self
            .screens
            .iter()
            .find_map(Screen::secrets)
            .and_then(|screen| screen.cursor_identity(store));
        let registries_was = self
            .screens
            .iter()
            .find_map(Screen::registries)
            .and_then(|screen| screen.cursor_identity(store));
        match self.store.azure.apply(event) {
            azure::Applied::Secrets => {
                if let Some(screen) = self.screens.iter_mut().find_map(Screen::secrets_mut) {
                    screen.invalidate();
                    screen.keep_cursor(&self.store.azure, secrets_was);
                }
            }
            azure::Applied::Repositories => {
                if let Some(screen) = self.screens.iter_mut().find_map(Screen::registries_mut) {
                    screen.invalidate();
                    screen.keep_cursor(&self.store.azure, registries_was);
                }
            }
            azure::Applied::Value {
                vault,
                name,
                result,
            } => {
                let shell = &mut self.shell;
                let store = &self.store.azure;
                return self
                    .screens
                    .iter_mut()
                    .find_map(Screen::secrets_mut)
                    .map_or(AppAction::None, |screen| {
                        screen.on_value(shell, store, &vault, &name, result, now)
                    });
            }
            // A repository's tags re-read with the same count would otherwise
            // keep the old order under the new list.
            azure::Applied::Detail => {
                if let Some(screen) = self.screens.iter_mut().find_map(Screen::registries_mut) {
                    screen.invalidate();
                }
            }
            azure::Applied::Nothing | azure::Applied::Status => {}
        }
        AppAction::None
    }

    /// A kube worker event, and whatever it asks the run loop to do next. A
    /// read that landed re-sorts its list under a cursor that stays on its
    /// own row; a read that failed is said once in the status bar when it is
    /// the open tab's and the kind on screen; a secret's value goes to the
    /// one screen that asked, which shows it or copies it and keeps nothing
    /// either way.
    pub fn apply_kube(&mut self, event: kube::Event) -> AppAction {
        // The pane's and the modal's own events go to the tab that asked and
        // touch no rows.
        match event {
            kube::Event::LogLines {
                target,
                lines,
                finished,
            } => {
                if let Some(screen) = self
                    .screens
                    .get_mut(target.scope)
                    .and_then(Screen::scope_mut)
                {
                    screen.append_log(&target, lines, finished);
                }
                return AppAction::None;
            }
            kube::Event::Text {
                scope,
                kind,
                object,
                text,
            } => {
                if let Some(screen) = self.screens.get_mut(scope).and_then(Screen::scope_mut) {
                    screen.set_text(kind, object, text);
                }
                return AppAction::None;
            }
            kube::Event::Deleted { scope, key, error } => {
                if let Some((screen, data)) = self
                    .screens
                    .get_mut(scope)
                    .and_then(Screen::scope_mut)
                    .zip(self.store.scopes.get(scope))
                {
                    screen.deleted(&mut self.shell, data, &key, error);
                }
                return AppAction::None;
            }
            kube::Event::Acted {
                scope,
                verb,
                object,
                error,
            } => {
                if let Some(screen) = self.screens.get_mut(scope).and_then(Screen::scope_mut) {
                    screen.acted(&mut self.shell, verb, &object, error);
                }
                return AppAction::None;
            }
            kube::Event::Owner {
                scope,
                object,
                replicas,
            } => {
                if let Some(screen) = self.screens.get_mut(scope).and_then(Screen::scope_mut) {
                    screen.set_owner(object, replicas);
                }
                return AppAction::None;
            }
            kube::Event::SecretValue {
                scope,
                object,
                key,
                copy,
                value,
            } => {
                if let Some((screen, data)) = self
                    .screens
                    .get_mut(scope)
                    .and_then(Screen::scope_mut)
                    .zip(self.store.scopes.get(scope))
                {
                    return screen.on_secret_value(&mut self.shell, data, object, key, copy, value);
                }
                return AppAction::None;
            }
            _ => {}
        }
        // What the tab held before the event: the cursor's row, by identity,
        // and the message the last failure left. Both are read against the
        // rows as they were, which a read is about to replace.
        let landing = match &event {
            kube::Event::Pods { scope, .. } => Some((*scope, Kind::Pods)),
            kube::Event::Events { scope, .. } => Some((*scope, Kind::Events)),
            kube::Event::ConfigMaps { scope, .. } => Some((*scope, Kind::ConfigMaps)),
            kube::Event::Secrets { scope, .. } => Some((*scope, Kind::Secrets)),
            _ => None,
        };
        let (was_error, was_cursor) = landing
            .and_then(|(scope, kind)| {
                let data = self.store.scope(scope)?;
                let screen = self.screens.get(scope)?.scope()?;
                Some((
                    data.listing(kind).error.cloned(),
                    screen.cursor_identity(kind, data),
                ))
            })
            .unwrap_or((None, None));
        match self.store.apply_kube(event) {
            Applied::Rows(index, kind) => {
                if kind == Kind::Pods {
                    self.cache_dirty = true;
                }
                if let Some((screen, data)) = self
                    .screens
                    .get_mut(index)
                    .and_then(Screen::scope_mut)
                    .zip(self.store.scopes.get(index))
                {
                    screen.rows_changed(kind, data, was_cursor);
                }
            }
            Applied::Failed(index, kind) => {
                if index == self.tab
                    && self.kind() == Some(kind)
                    && let Some(Tab::Scope(tab)) = self.tabs.get(index)
                    && let Some(message) = self.store.scopes[index].listing(kind).error.cloned()
                    && was_error.as_deref() != Some(message.as_str())
                {
                    self.shell.set_error(format!(
                        "{} {}: {message}",
                        tab.scope.describe(),
                        kind.noun()
                    ));
                }
            }
            Applied::Status | Applied::Nothing => {}
        }
        AppAction::None
    }

    /// One turn of the clock: what the open Azure screen has run out of or
    /// settled on; whatever the pane should be following now, if that has
    /// changed since the kube worker was last told; the owner of a pod the
    /// cursor has settled on; and a revealed value that has run out. Called
    /// after every frame, once the rows the cursor counts over are settled.
    pub fn tick(&mut self, now: Instant) -> Vec<AppAction> {
        let tab = self.tab;
        let mut actions = Vec::new();
        {
            let store = &self.store.azure;
            match self.screens.get_mut(tab) {
                Some(Screen::Scope(screen)) => screen.tick_reveal(now),
                Some(Screen::Secrets(screen)) => {
                    screen.refilter(store);
                    actions.extend(screen.tick(store, now).map(AppAction::Azure));
                }
                Some(Screen::Registries(screen)) => {
                    screen.refilter(store);
                    actions.extend(screen.tick(store, now).map(AppAction::Azure));
                }
                None => {}
            }
        }
        // The follow. An Azure tab follows nothing, so leaving a scope tab
        // says so once.
        let desired = self
            .screens
            .get(tab)
            .and_then(Screen::scope)
            .zip(self.store.scopes.get(tab))
            .and_then(|(screen, data)| screen.log_target(tab, data));
        if desired != self.following {
            self.following.clone_from(&desired);
            if let Some(screen) = self.screens.get_mut(tab).and_then(Screen::scope_mut) {
                screen.begin_follow(desired.clone());
            }
            actions.push(AppAction::Kube(
                desired.map_or(kube::Request::Unfollow, kube::Request::Follow),
            ));
        }
        // The owner, once the cursor has rested on a pod. Where it is now is
        // read before the borrow that asks.
        let here = self
            .screens
            .get(tab)
            .and_then(Screen::scope)
            .map(|screen| (tab, screen.list().cursor.index));
        match (self.rested, here) {
            (Some((t, c, since)), Some(here)) if (t, c) == here => {
                if now.saturating_duration_since(since) >= REST
                    && let Some((_, screen, data, _)) = self.scope_parts()
                    && let Some(request) = screen.owner_request(tab, data)
                {
                    actions.push(AppAction::Kube(request));
                }
            }
            (_, Some((t, c))) => self.rested = Some((t, c, now)),
            (_, None) => self.rested = None,
        }
        actions
    }

    /// Whether the cursor has landed on a pod in the last [`REST`], so the
    /// loop comes back in time to ask about it.
    #[must_use]
    pub fn is_resting(&self) -> bool {
        self.rested
            .is_some_and(|(_, _, since)| since.elapsed() < REST)
    }

    /// How long the run loop may sleep: a tenth of a second while a read is
    /// in flight, so its answer is painted the moment it lands; a second
    /// while a value is counting down; the rest interval while the cursor has
    /// just landed somewhere; and whatever the caller wanted otherwise.
    #[must_use]
    pub fn poll_for(&self, settled: Duration) -> Duration {
        if self.store.azure.refreshing || self.store.reading() {
            return Duration::from_millis(100);
        }
        let ticking = match self.screens.get(self.tab) {
            Some(Screen::Scope(screen)) => screen.is_ticking(),
            Some(Screen::Secrets(screen)) => screen.is_ticking(),
            _ => false,
        };
        if ticking {
            return Duration::from_secs(1);
        }
        let resting = self.screens.iter().any(|screen| match screen {
            Screen::Secrets(screen) => screen.is_resting(),
            Screen::Registries(screen) => screen.is_resting(),
            Screen::Scope(_) => false,
        });
        if self.is_resting() || resting {
            return REST;
        }
        settled
    }

    // ── The session ────────────────────────────────────────────────────

    /// The layout as it stands, for the file: the tab, each scope tab's kind
    /// and its pods table, and each Azure table's sort and columns. Never
    /// the query, never the cursor, and nothing a vault or a cluster
    /// answered.
    #[must_use]
    pub fn session(&self) -> Session {
        let mut session = Session {
            tab: self.tabs.get(self.tab).map(Tab::key),
            ..Session::default()
        };
        for (tab, screen) in self.tabs.iter().zip(&self.screens) {
            let held = session.tab(&tab.key());
            match screen {
                Screen::Scope(screen) => {
                    held.kind = Some(screen.kind.session_key().to_owned());
                    let pods = screen.list_of(Kind::Pods);
                    held.sort = Some(sort_of(pods.sort, pods.descending));
                    held.columns = columns_of(&pods.layout);
                }
                Screen::Secrets(screen) => {
                    held.sort = Some(sort_of(screen.sort, screen.descending));
                    held.columns = columns_of(&screen.layout);
                }
                Screen::Registries(screen) => {
                    let table = &screen.repositories;
                    held.sort = Some(sort_of(table.sort, table.descending));
                    held.columns = columns_of(&table.layout);
                }
            }
        }
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
            .and_then(|name| self.tabs.iter().position(|held| held.key() == name))
        {
            self.tab = tab;
        }
        for (tab, screen) in self.tabs.iter().zip(&mut self.screens) {
            let Some(held) = session.tabs.get(&tab.key()) else {
                continue;
            };
            let sort = read_sort(held.sort.as_ref());
            match screen {
                Screen::Scope(screen) => {
                    if let Some(kind) = held.kind.as_deref().and_then(Kind::from_session_key) {
                        screen.set_kind(kind);
                    }
                    let pods = screen.list_of_mut(Kind::Pods);
                    if let Some((column, way)) = sort {
                        pods.sort = column;
                        pods.descending = way;
                    }
                    apply_columns(&mut pods.layout, &held.columns);
                }
                Screen::Secrets(screen) => {
                    if let Some((column, way)) = sort {
                        screen.sort = column;
                        screen.descending = way;
                    }
                    apply_columns(&mut screen.layout, &held.columns);
                    screen.invalidate();
                }
                Screen::Registries(screen) => {
                    if let Some((column, way)) = sort {
                        screen.repositories.sort = column;
                        screen.repositories.descending = way;
                    }
                    apply_columns(&mut screen.repositories.layout, &held.columns);
                    screen.invalidate();
                }
            }
        }
    }

    /// What the status bar says on the left when nothing has just happened.
    #[must_use]
    pub fn footer_hint(&self) -> String {
        match self.screens.get(self.tab) {
            Some(Screen::Scope(screen)) => screen.footer_hint(&self.shell),
            Some(Screen::Secrets(screen)) => screen.footer_hint(&self.shell),
            Some(Screen::Registries(screen)) => screen.footer_hint(&self.shell),
            None => String::new(),
        }
    }

    /// What the status bar says on the right: what the open tab is doing, or
    /// what is wrong with it, or what it holds and how old that is.
    fn store_state(&self, millis: u128) -> (String, Style) {
        let palette = ui::theme::theme();
        match self.tabs.get(self.tab) {
            Some(Tab::Scope(tab)) => {
                let Some((data, kind)) = self.store.scope(self.tab).zip(self.kind()) else {
                    return (String::new(), Style::default());
                };
                let listing = data.listing(kind);
                let scope = tab.scope.describe();
                let noun = kind.noun();
                if data.reading && listing.reads == 0 && listing.count == 0 {
                    return (
                        format!("{} reading {scope} {noun}…", spinner_frame(millis)),
                        Style::default().fg(palette.info),
                    );
                }
                if let Some(message) = listing.error {
                    return (
                        format!("! {scope} {noun}: {message}"),
                        Style::default().fg(palette.error),
                    );
                }
                let age = age_of(listing.read_at);
                let spinner = if data.reading {
                    format!("{} ", spinner_frame(millis))
                } else {
                    "● ".to_owned()
                };
                (
                    format!("{spinner}{} {noun} · {age}", listing.count),
                    Style::default().fg(palette.muted),
                )
            }
            Some(tab) => {
                let store = &self.store.azure;
                if store.refreshing {
                    let said = store.progress.clone().unwrap_or_else(|| "reading…".into());
                    return (
                        format!("{} {said}", spinner_frame(millis)),
                        Style::default().fg(palette.info),
                    );
                }
                if let Some(problem) = store.first_problem() {
                    return (
                        format!("! {}", crate::store::problem_line(problem)),
                        Style::default().fg(palette.error),
                    );
                }
                let counts = if matches!(tab, Tab::Registries) {
                    format!(
                        "{} registries · {} repositories",
                        store.inventory.registries.len(),
                        store.repositories.len()
                    )
                } else {
                    format!(
                        "{} vaults · {} secrets",
                        store.inventory.vaults.len(),
                        store.secrets.len()
                    )
                };
                (
                    format!("● {counts} · {}", age_of(store.read_at)),
                    Style::default().fg(palette.muted),
                )
            }
            None => (String::new(), Style::default()),
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

        let labels: Vec<TabLabel> = self
            .tabs
            .iter()
            .zip(&self.screens)
            .enumerate()
            .map(|(index, (tab, screen))| TabLabel {
                label: tab.label().to_owned(),
                short: tab.short_label().to_owned(),
                badge: match screen {
                    Screen::Scope(_) => self.store.scope(index).and_then(ScopeScreen::badge),
                    Screen::Secrets(screen) => screen.badge(&self.store.azure),
                    Screen::Registries(screen) => screen.badge(&self.store.azure),
                },
            })
            .collect();
        let kind = self.kind();
        ui::widgets::render_tab_bar(frame, &mut self.shell, tabs, self.tab, &labels, kind);
        self.render_body(frame, body);
        let hint = self.footer_hint();
        let (right, right_style) = self.store_state(millis);
        ui::widgets::render_status_bar(frame, &mut self.shell, status, &hint, &right, right_style);
        match self.shell.menu {
            Some((Menu::Env, highlighted)) => {
                if let Some(anchor) = self.shell.find(&Target::Header(ColumnId::Env)) {
                    let current = self.current_env();
                    ui::widgets::render_env_menu(
                        frame,
                        &mut self.shell,
                        anchor,
                        current,
                        highlighted,
                    );
                }
            }
            Some((Menu::Kind, highlighted)) => {
                if let Some((anchor, kind)) = self.shell.find(&Target::KindPill).zip(kind) {
                    ui::widgets::render_kind_menu(
                        frame,
                        &mut self.shell,
                        anchor,
                        kind,
                        highlighted,
                    );
                }
            }
            None => {}
        }
        if let Some(modal) = self
            .screens
            .get(self.tab)
            .and_then(Screen::scope)
            .and_then(|screen| screen.modal.as_ref())
        {
            ui::modal::render_modal(frame, &mut self.shell, modal, area);
        }
        if self.shell.help_open {
            let section = self
                .screens
                .get(self.tab)
                .map_or(keys::Section::Secrets, Screen::section);
            let problems = self.store.problems(&self.tabs);
            ui::widgets::render_help(frame, &mut self.shell, area, section, &problems);
        }
    }

    fn render_body(&mut self, frame: &mut Frame, area: Rect) {
        let tab = self.tab;
        let shell = &mut self.shell;
        let store = &self.store;
        match (self.tabs.get(tab), self.screens.get_mut(tab)) {
            (Some(Tab::Scope(tab_ref)), Some(Screen::Scope(screen))) => {
                if let Some(data) = store.scopes.get(tab) {
                    screen.refilter(data);
                    ui::scope::render(frame, shell, screen, tab_ref, data, area);
                }
            }
            (_, Some(Screen::Secrets(screen))) => {
                screen.refilter(&store.azure);
                ui::secrets::render(frame, shell, screen, &store.azure, area);
            }
            (_, Some(Screen::Registries(screen))) => {
                screen.refilter(&store.azure);
                ui::registries::render(frame, shell, screen, &store.azure, area);
            }
            _ => {}
        }
    }
}

/// "just now", "12s ago", or "never read".
fn age_of(read_at: Option<Timestamp>) -> String {
    read_at.map_or_else(
        || "never read".to_owned(),
        |read_at| match read_at.relative_age(Timestamp::now()).as_str() {
            "now" => "just now".to_owned(),
            age => format!("{age} ago"),
        },
    )
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
pub(crate) mod tests {
    use super::*;
    use crate::kube::tests::{crashing, pod};
    use crate::kube::{ConfigMap, Event, K8sEvent, ObjectRef, Request, Secret, SecretMeta};
    use crate::store::AzureStore;
    use screen::tabs;

    /// The two Azure tabs and nothing else: Secrets at 0, Registries at 1.
    fn azure_only() -> App {
        App::new(tabs(Vec::new()), Store::default())
    }

    fn azure_only_with(azure: AzureStore) -> App {
        App::new(
            tabs(Vec::new()),
            Store {
                azure,
                ..Store::default()
            },
        )
    }

    /// Two clusters, four scope tabs, then Secrets at 4 and Registries at 5.
    pub(crate) fn two_clusters() -> App {
        let tabs = tabs(config::parse(config::tests::TWO_CLUSTERS).unwrap().tabs());
        let store = Store::new(4);
        App::new(tabs, store)
    }

    /// The two clusters with pods read into qa/dev and prod, and qa/dev's
    /// events, configmaps and secrets besides.
    pub(crate) fn stocked() -> App {
        let mut app = two_clusters();
        app.apply_kube(Event::Pods {
            scope: 0,
            pods: Ok(vec![
                pod("qa", "dev", "orders-api-7d9f5b-abc12", "Running"),
                crashing("qa", "dev", "orders-api-7d9f5b-def34"),
            ]),
        });
        app.apply_kube(Event::Pods {
            scope: 3,
            pods: Ok(vec![pod(
                "prod",
                "prod",
                "orders-api-9a1c2d-ghi56",
                "Running",
            )]),
        });
        app.apply_kube(Event::Events {
            scope: 0,
            events: Ok(vec![
                K8sEvent::from_json(&serde_json::json!({
                    "metadata": {"name": "w1", "namespace": "dev"},
                    "lastTimestamp": "2026-09-12T12:00:00Z", "type": "Warning", "reason": "BackOff", "count": 9,
                    "involvedObject": {"kind": "Pod", "name": "orders-api-7d9f5b-def34", "namespace": "dev"},
                    "message": "Back-off restarting failed container"
                }))
                .unwrap(),
                K8sEvent::from_json(&serde_json::json!({
                    "metadata": {"name": "n1", "namespace": "dev"},
                    "lastTimestamp": "2026-09-12T11:00:00Z", "type": "Normal", "reason": "Pulled",
                    "involvedObject": {"kind": "Pod", "name": "orders-api-7d9f5b-abc12", "namespace": "dev"},
                    "message": "Pulled image"
                }))
                .unwrap(),
            ]),
        });
        app.apply_kube(Event::ConfigMaps {
            scope: 0,
            configmaps: Ok(vec![
                ConfigMap::from_json(&serde_json::json!({
                    "metadata": {"name": "orders-config", "namespace": "dev"},
                    "data": {"LOG_LEVEL": "info"}
                }))
                .unwrap(),
            ]),
        });
        app.apply_kube(Event::Secrets {
            scope: 0,
            secrets: Ok(vec![
                SecretMeta::from_json(&serde_json::json!({
                    "metadata": {"name": "db", "namespace": "dev"}, "type": "Opaque",
                    "data": {"password": "aHVudGVyMg=="}
                }))
                .unwrap(),
                SecretMeta::from_json(&serde_json::json!({
                    "metadata": {"name": "tls", "namespace": "dev"}, "type": "kubernetes.io/tls",
                    "data": {"tls.crt": "LS0t", "tls.key": "LS0t"}
                }))
                .unwrap(),
            ]),
        });
        app
    }

    fn press(app: &mut App, code: KeyCode) -> AppAction {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// What the tick says about the follow, the owner reads left aside.
    fn follow_tick(app: &mut App) -> Option<Request> {
        app.tick(Instant::now())
            .into_iter()
            .find_map(|action| match action {
                AppAction::Kube(request @ (Request::Follow(_) | Request::Unfollow)) => {
                    Some(request)
                }
                _ => None,
            })
    }

    fn draw(app: &mut App) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 50)).unwrap();
        terminal.draw(|frame| app.render(frame, 0)).unwrap();
        crate::ui::screen_text(terminal.backend().buffer())
    }

    fn click(app: &mut App, column: u16, row: u16) -> AppAction {
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn scope(app: &App, tab: usize) -> &ScopeScreen {
        app.screens[tab].scope().expect("a scope tab")
    }

    fn secrets(app: &App, tab: usize) -> &SecretsScreen {
        app.screens[tab].secrets().expect("the Secrets tab")
    }

    fn registries(app: &App, tab: usize) -> &RegistriesScreen {
        app.screens[tab].registries().expect("the Registries tab")
    }

    fn selected_pod_name(app: &App) -> Option<String> {
        scope(app, app.tab)
            .selected_pod(&app.store.scopes[app.tab])
            .map(|pod| pod.key.name.clone())
    }

    // ── The tabs ───────────────────────────────────────────────────────

    #[test]
    fn a_scope_tabs_index_is_its_kube_scope_and_the_azure_tabs_come_after() {
        let mut app = two_clusters();
        assert_eq!(app.tabs.len(), 6);
        assert_eq!(app.store.scopes.len(), 4);
        assert!(matches!(app.tabs[4], Tab::Secrets));
        assert!(matches!(app.tabs[5], Tab::Registries));
        assert_eq!(
            press(&mut app, KeyCode::Char('4')),
            AppAction::Kube(Request::Showing(3, Kind::Pods))
        );
        assert_eq!(press(&mut app, KeyCode::Char('5')), AppAction::None);
        assert_eq!(app.tab, 4);
        assert_eq!(app.kind(), None);
        assert_eq!(
            press(&mut app, KeyCode::Char('r')),
            AppAction::Azure(worker::Request::Refresh)
        );
        app.apply_kube(Event::Pods {
            scope: 3,
            pods: Ok(vec![pod("prod", "prod", "a", "Running")]),
        });
        assert_eq!(app.store.scopes[3].pods.rows.len(), 1);
        assert_eq!(scope(&app, 3).list().cursor.index, 0);
        assert_eq!(
            press(&mut app, KeyCode::Char('6')),
            AppAction::None,
            "Registries asks the kube worker for nothing"
        );
        assert_eq!(press(&mut app, KeyCode::Char('7')), AppAction::None);
        assert_eq!(app.tab, 5, "a number with no tab does nothing");
    }

    #[test]
    fn a_tab_is_reached_by_number_by_bracket_and_by_click_and_the_worker_is_told() {
        let mut app = two_clusters();
        assert_eq!(app.tab, 0);
        assert_eq!(
            press(&mut app, KeyCode::Char('3')),
            AppAction::Kube(Request::Showing(2, Kind::Pods))
        );
        assert_eq!(app.tabs[app.tab].label(), "qa/uat");
        assert_eq!(press(&mut app, KeyCode::Char('9')), AppAction::None);
        assert_eq!(app.tab, 2, "a number with no tab does nothing");
        for _ in 0..4 {
            press(&mut app, KeyCode::Char(']'));
        }
        assert_eq!(app.tab, 0, "round the end");
        press(&mut app, KeyCode::Char('['));
        assert_eq!(app.tabs[app.tab].label(), "Registries");
        assert_eq!(press(&mut app, KeyCode::Left), AppAction::None);
        assert_eq!(app.tabs[app.tab].label(), "Secrets");
        assert_eq!(
            press(&mut app, KeyCode::Left),
            AppAction::Kube(Request::Showing(3, Kind::Pods))
        );
        assert_eq!(app.tabs[app.tab].label(), "prod");
        press(&mut app, KeyCode::Left);
        assert_eq!(app.tab, 2);
        assert_eq!(
            press(&mut app, KeyCode::Char('3')),
            AppAction::None,
            "the same tab is not a switch"
        );

        app.shell.begin_frame();
        app.shell.region(Rect::new(0, 0, 8, 1), Target::Tab(1));
        assert_eq!(
            click(&mut app, 2, 0),
            AppAction::Kube(Request::Showing(1, Kind::Pods))
        );
        assert_eq!(app.tab, 1);
    }

    #[test]
    fn no_clusters_at_all_leaves_the_two_azure_tabs() {
        let mut app = App::new(tabs(Vec::new()), Store::new(0));
        assert_eq!(app.tabs.len(), 2);
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('e'));
        assert_eq!(
            press(&mut app, KeyCode::Char('r')),
            AppAction::Azure(worker::Request::Refresh)
        );
        assert_eq!(app.session().tab.as_deref(), Some("secrets"));
        press(&mut app, KeyCode::Char(']'));
        assert_eq!(app.session().tab.as_deref(), Some("registries"));
        assert!(app.tick(Instant::now()).is_empty());
        let drawn = draw(&mut app);
        assert!(drawn.contains("1 Secrets"), "{drawn}");
        assert!(drawn.contains("2 Registries"), "{drawn}");
        assert!(!drawn.contains("Pods \u{25be}"), "no kind pill: {drawn}");
    }

    #[test]
    fn one_menu_at_a_time_and_each_on_its_own_tab() {
        let mut app = two_clusters();
        app.store.azure = crate::app::secrets::tests::stocked();
        press(&mut app, KeyCode::Char('5'));
        let drawn = draw(&mut app);
        assert!(drawn.contains("Env \u{25be}"), "{drawn}");
        assert!(!drawn.contains("Pods \u{25be}"), "{drawn}");
        let header = app
            .shell
            .find(&Target::Header(ColumnId::Env))
            .expect("an Env header");
        click(&mut app, header.x, header.y);
        assert_eq!(app.shell.menu, Some((Menu::Env, 0)));
        // A click on a tab while the menu is open only closes the menu.
        app.shell.region(Rect::new(0, 0, 8, 1), Target::Tab(0));
        assert_eq!(click(&mut app, 2, 0), AppAction::None);
        assert_eq!(app.shell.menu, None);
        assert_eq!(app.tab, 4);

        press(&mut app, KeyCode::Char('1'));
        let drawn = draw(&mut app);
        assert!(drawn.contains("Pods \u{25be}"), "{drawn}");
        assert!(!drawn.contains("Env \u{25be}"), "{drawn}");
        let pill = app.shell.find(&Target::KindPill).expect("the pill");
        click(&mut app, pill.x + 1, pill.y);
        assert_eq!(app.shell.menu, Some((Menu::Kind, 0)));
        // A number closes the menu and is not otherwise acted on.
        assert_eq!(press(&mut app, KeyCode::Char('5')), AppAction::None);
        assert_eq!(app.shell.menu, None);
        assert_eq!(app.tab, 0);
        press(&mut app, KeyCode::Char('5'));
        assert_eq!(app.tab, 4);
        let drawn = draw(&mut app);
        assert!(drawn.contains("Env \u{25be}"), "{drawn}");
        assert!(!drawn.contains("Pods \u{25be}"), "{drawn}");
    }

    #[test]
    fn esc_takes_things_off_in_order_on_every_kind_of_tab() {
        // A scope tab: the pane's filter, the query, the pane, then nothing.
        let mut app = stocked();
        press(&mut app, KeyCode::Enter);
        assert!(scope(&app, 0).pane_open);
        assert_eq!(app.shell.focus, Focus::Details);
        press(&mut app, KeyCode::Char('/'));
        assert_eq!(app.shell.focus, Focus::PaneSearch);
        app.handle_paste("INFO");
        press(&mut app, KeyCode::Enter);
        app.screens[0]
            .scope_mut()
            .unwrap()
            .list_mut()
            .input
            .set_text("orders");
        press(&mut app, KeyCode::Esc);
        assert!(scope(&app, 0).pane_filter.is_empty());
        assert_eq!(scope(&app, 0).list().input.text(), "orders");
        assert!(scope(&app, 0).pane_open);
        press(&mut app, KeyCode::Esc);
        assert!(scope(&app, 0).list().input.is_empty());
        assert!(scope(&app, 0).pane_open);
        press(&mut app, KeyCode::Esc);
        assert!(!scope(&app, 0).pane_open);
        assert_eq!(app.shell.focus, Focus::Table);
        assert_eq!(press(&mut app, KeyCode::Esc), AppAction::None);

        // Secrets: the query, then nothing.
        press(&mut app, KeyCode::Char('5'));
        app.screens[4].secrets_mut().unwrap().input.set_text("db");
        press(&mut app, KeyCode::Esc);
        assert!(secrets(&app, 4).input.is_empty());
        assert_eq!(press(&mut app, KeyCode::Esc), AppAction::None);

        // Registries: the query, then back out of a repository, then nothing.
        let mut app = azure_only_with(crate::app::registries::tests::stocked());
        press(&mut app, KeyCode::Char('2'));
        app.screens[1]
            .registries_mut()
            .unwrap()
            .refilter(&app.store.azure);
        press(&mut app, KeyCode::Enter);
        assert!(matches!(
            registries(&app, 1).level,
            registries::Level::Tags { .. }
        ));
        app.screens[1]
            .registries_mut()
            .unwrap()
            .input_mut()
            .set_text("1.4");
        press(&mut app, KeyCode::Esc);
        assert!(registries(&app, 1).input().is_empty());
        assert!(matches!(
            registries(&app, 1).level,
            registries::Level::Tags { .. }
        ));
        press(&mut app, KeyCode::Esc);
        assert_eq!(registries(&app, 1).level, registries::Level::Repositories);
        assert_eq!(press(&mut app, KeyCode::Esc), AppAction::None);
        assert_eq!(registries(&app, 1).level, registries::Level::Repositories);
    }

    // ── The Azure tabs ─────────────────────────────────────────────────

    #[test]
    fn a_layout_survives_a_round_trip_through_the_file() {
        let mut app = azure_only();
        app.tab = 1;
        {
            let secrets = app.screens[0].secrets_mut().unwrap();
            secrets.sort = ColumnId::Expires;
            secrets.descending = true;
            secrets.layout.columns[0].width = 20;
            secrets.layout.columns[1].visible = false;
        }

        let session = app.session();
        let mut fresh = azure_only();
        fresh.restore(&session);

        assert_eq!(fresh.tab, 1);
        let secrets = secrets(&fresh, 0);
        assert_eq!(secrets.sort, ColumnId::Expires);
        assert!(secrets.descending);
        assert_eq!(secrets.layout.columns[0].width, 20);
        assert!(!secrets.layout.columns[1].visible);
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

        let mut app = azure_only();
        let before = secrets(&app, 0).layout.clone();
        app.restore(&session);
        assert_eq!(app.tab, 0, "an unknown tab is the first one");
        assert_eq!(
            secrets(&app, 0).sort,
            ColumnId::Name,
            "and an unknown sort is the default"
        );
        assert_eq!(secrets(&app, 0).layout, before);
    }

    #[test]
    fn a_paste_lands_in_the_search_box_and_nowhere_else() {
        let mut app = azure_only();
        app.handle_paste("db\npass");
        assert!(secrets(&app, 0).input.is_empty(), "the table took nothing");
        app.shell.focus = Focus::Search;
        app.handle_paste("db\npass");
        assert_eq!(secrets(&app, 0).input.text(), "db pass");
    }

    #[test]
    fn tab_leaves_the_search_box_and_esc_backs_out_of_a_repository() {
        let mut app = azure_only();
        app.shell.focus = Focus::Search;
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.shell.focus, Focus::Table);

        // A query is what Esc takes off first; with none, the screen has it.
        app.screens[0].secrets_mut().unwrap().input.set_text("db");
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(secrets(&app, 0).input.is_empty());
        let mut app = azure_only_with(crate::app::registries::tests::stocked());
        app.tab = 1;
        app.screens[1]
            .registries_mut()
            .unwrap()
            .refilter(&app.store.azure);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            registries(&app, 1).level,
            registries::Level::Tags { .. }
        ));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(registries(&app, 1).level, registries::Level::Repositories);
    }

    #[test]
    fn a_click_puts_focus_where_it_landed() {
        let mut app = azure_only();
        app.shell.begin_frame();
        app.shell.region(Rect::new(0, 5, 40, 10), Target::Details);
        app.shell.region(Rect::new(0, 1, 40, 1), Target::Row(0));
        app.shell.focus = Focus::Search;
        click(&mut app, 3, 1);
        assert_eq!(app.shell.focus, Focus::Table);
        click(&mut app, 3, 7);
        assert_eq!(app.shell.focus, Focus::Details);
    }

    #[test]
    fn a_re_read_of_the_open_repositorys_tags_is_shown_in_its_new_order() {
        use crate::azure::Tag;
        use crate::timestamp::ts;

        let mut app = azure_only_with(crate::app::registries::tests::stocked());
        app.tab = 1;
        app.screens[1]
            .registries_mut()
            .unwrap()
            .refilter(&app.store.azure);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.screens[1]
            .registries_mut()
            .unwrap()
            .refilter(&app.store.azure);
        let first = |app: &App| {
            registries(app, 1)
                .selected_tag(&app.store.azure)
                .map(|tag| tag.name.clone())
        };
        let was = first(&app).expect("a tag under the cursor");
        // The same two names come back with their stamps swapped: the other
        // one is now the newest, and `Updated ↓` must put it first.
        let (registry, repo) = registries(&app, 1)
            .open_repository()
            .map(|(r, p)| (r.to_owned(), p.to_owned()))
            .unwrap();
        let Ok(tags) = app.store.azure.tags[&(registry.clone(), repo.clone())].clone() else {
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
        app.apply_azure(
            worker::Event::Tags {
                registry,
                repo,
                result: Ok(swapped),
            },
            Instant::now(),
        );
        app.screens[1]
            .registries_mut()
            .unwrap()
            .refilter(&app.store.azure);
        assert_ne!(first(&app).unwrap(), was, "the newest tag is first again");
    }

    #[test]
    fn the_env_header_opens_a_menu_whose_choice_lands_in_the_search_box() {
        let mut app = azure_only_with(crate::app::secrets::tests::stocked());
        app.screens[0].secrets_mut().unwrap().input.set_text("db");
        let drawn = draw(&mut app);
        assert!(drawn.contains("Env \u{25be}"), "{drawn}");
        assert!(!drawn.contains("All"), "closed until asked for: {drawn}");
        let header = app
            .shell
            .find(&Target::Header(ColumnId::Env))
            .expect("the table drew an Env header");
        click(&mut app, header.x, header.y);
        assert_eq!(app.shell.menu, Some((Menu::Env, 0)), "open, on All");
        assert_eq!(secrets(&app, 0).sort, ColumnId::Name, "and it did not sort");
        let drawn = draw(&mut app);
        assert!(drawn.contains("\u{2713} All"), "{drawn}");
        assert!(drawn.contains("  prod"), "{drawn}");
        let prod = app
            .shell
            .find(&Target::EnvOption(Some(Env::Prod)))
            .expect("a line for prod");
        assert!(prod.y > header.y, "under the header");

        // Down to prod and Enter: the filter is in the box, the menu is gone.
        for _ in 0..3 {
            press(&mut app, KeyCode::Char('j'));
        }
        press(&mut app, KeyCode::Enter);
        assert_eq!(secrets(&app, 0).input.text(), "db env:prod");
        assert_eq!(app.shell.menu, None);
        app.screens[0]
            .secrets_mut()
            .unwrap()
            .refilter(&app.store.azure);
        assert!(
            secrets(&app, 0)
                .visible()
                .iter()
                .all(|at| app.store.azure.secrets[*at].vault == "kv-prod"),
            "{:?}",
            secrets(&app, 0).visible()
        );

        // Opened again it starts on prod; a click on a line chooses it, and
        // a click anywhere else only closes it.
        app.shell.begin_frame();
        app.shell
            .region(Rect::new(2, 2, 6, 1), Target::Header(ColumnId::Env));
        click(&mut app, 3, 2);
        assert_eq!(app.shell.menu, Some((Menu::Env, 3)));
        app.shell
            .region(Rect::new(2, 4, 6, 1), Target::EnvOption(None));
        app.shell.region(Rect::new(2, 9, 20, 1), Target::Row(1));
        click(&mut app, 3, 9);
        assert_eq!(app.shell.menu, None);
        assert_eq!(
            secrets(&app, 0).cursor.index,
            0,
            "the row under it was not taken"
        );
        assert_eq!(secrets(&app, 0).input.text(), "db env:prod");
        click(&mut app, 3, 2);
        click(&mut app, 3, 4);
        assert_eq!(
            secrets(&app, 0).input.text(),
            "db",
            "All takes the filter off"
        );

        // Any other key closes it and is not otherwise acted on.
        click(&mut app, 3, 2);
        assert!(app.shell.menu.is_some());
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.shell.menu, None);
    }

    #[test]
    fn the_session_never_carries_the_query_or_the_cursor() {
        let mut app = azure_only();
        let secrets = app.screens[0].secrets_mut().unwrap();
        secrets.input.set_text("vault:kv-prod db-password");
        secrets.cursor.focus(7);
        let written = serde_json::to_string(&app.session()).unwrap();
        assert!(!written.contains("db-password"), "{written}");
        assert!(!written.contains("kv-prod"), "{written}");
        assert!(!written.contains("cursor"), "{written}");
    }

    #[test]
    fn the_status_bar_shows_the_spinner_while_reading_and_the_problem_after() {
        use crate::azure::Inventory;

        let mut app = azure_only();
        app.apply_azure(
            worker::Event::Inventory(Ok(Inventory::default())),
            Instant::now(),
        );
        app.apply_azure(
            worker::Event::Progress("reading kv-prod (2/3)…".into()),
            Instant::now(),
        );
        assert_eq!(
            app.poll_for(Duration::from_secs(1)),
            Duration::from_millis(100)
        );
        let drawn = draw(&mut app);
        assert!(drawn.contains("reading kv-prod (2/3)"), "{drawn}");

        app.apply_azure(
            worker::Event::Secrets {
                vault: "kv-prod".into(),
                result: Err("no permission to read secrets".into()),
            },
            Instant::now(),
        );
        app.apply_azure(worker::Event::Idle, Instant::now());
        let drawn = draw(&mut app);
        assert!(drawn.contains("! kv-prod: no permission"), "{drawn}");
        app.shell.help_open = true;
        let drawn = draw(&mut app);
        assert!(drawn.contains("Problems"), "{drawn}");
        assert!(drawn.contains("kv-prod: no permission"), "{drawn}");
    }

    #[test]
    fn the_status_bar_counts_what_the_open_tab_holds() {
        let mut app = azure_only();
        let drawn = draw(&mut app);
        assert!(
            drawn.contains("0 vaults · 0 secrets · never read"),
            "{drawn}"
        );
        app.tab = 1;
        let drawn = draw(&mut app);
        assert!(
            drawn.contains("0 registries · 0 repositories · never read"),
            "{drawn}"
        );
        assert!(!drawn.contains("vaults"), "{drawn}");
    }

    // ── The scope tabs ─────────────────────────────────────────────────

    #[test]
    fn the_kinds_are_reached_by_key_and_by_the_pill_and_each_tab_remembers_its_own() {
        let mut app = stocked();
        assert_eq!(
            press(&mut app, KeyCode::Char('m')),
            AppAction::Kube(Request::Showing(0, Kind::ConfigMaps))
        );
        assert_eq!(app.kind(), Some(Kind::ConfigMaps));
        let drawn = draw(&mut app);
        assert!(drawn.contains("ConfigMaps ▾"), "{drawn}");
        assert!(drawn.contains("orders-config"), "{drawn}");
        assert!(drawn.contains("● 1 configmaps · just now"), "{drawn}");
        assert_eq!(
            press(&mut app, KeyCode::Char('m')),
            AppAction::None,
            "already there"
        );

        // Another tab keeps pods; back here keeps configmaps.
        assert_eq!(
            press(&mut app, KeyCode::Char('4')),
            AppAction::Kube(Request::Showing(3, Kind::Pods))
        );
        assert_eq!(
            press(&mut app, KeyCode::Char('1')),
            AppAction::Kube(Request::Showing(0, Kind::ConfigMaps))
        );

        // The pill opens a menu; j, Enter chooses; a click on a line chooses.
        // The pill moves with its label, so it is found again after each draw.
        let pill = |app: &mut App| {
            draw(app);
            app.shell.find(&Target::KindPill).expect("the pill")
        };
        let at = pill(&mut app);
        click(&mut app, at.x + 1, at.y);
        assert_eq!(app.shell.menu, Some((Menu::Kind, 2)), "open, on ConfigMaps");
        let drawn = draw(&mut app);
        assert!(drawn.contains("\u{2713} ConfigMaps"), "{drawn}");
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            AppAction::Kube(Request::Showing(0, Kind::Secrets))
        );
        assert_eq!(app.shell.menu, None);
        assert_eq!(app.kind(), Some(Kind::Secrets));
        let at = pill(&mut app);
        click(&mut app, at.x + 1, at.y);
        draw(&mut app);
        let events = app
            .shell
            .find(&Target::KindOption(Kind::Events))
            .expect("a line for events");
        assert_eq!(
            click(&mut app, events.x + 2, events.y),
            AppAction::Kube(Request::Showing(0, Kind::Events))
        );
        // Any other key closes the menu and is not otherwise acted on.
        let at = pill(&mut app);
        click(&mut app, at.x + 1, at.y);
        assert!(app.shell.menu.is_some());
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.shell.menu, None);
        assert_eq!(app.kind(), Some(Kind::Events));

        // A pod's key on another kind says so.
        press(&mut app, KeyCode::Char('x'));
        assert!(
            app.shell
                .notification()
                .is_some_and(|(said, _)| said.contains("pod's key"))
        );
        assert_eq!(
            press(&mut app, KeyCode::Char('s')),
            AppAction::Kube(Request::Showing(0, Kind::Secrets))
        );
        assert_eq!(
            press(&mut app, KeyCode::Char('p')),
            AppAction::Kube(Request::Showing(0, Kind::Pods))
        );
    }

    #[test]
    fn e_on_a_pod_shows_its_events_and_enter_on_one_goes_back_to_the_pod() {
        let mut app = stocked();
        app.screens[0]
            .scope_mut()
            .unwrap()
            .refilter(&app.store.scopes[0]);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(
            selected_pod_name(&app).as_deref(),
            Some("orders-api-7d9f5b-def34")
        );
        assert_eq!(
            press(&mut app, KeyCode::Char('e')),
            AppAction::Kube(Request::Showing(0, Kind::Events))
        );
        assert_eq!(app.kind(), Some(Kind::Events));
        let drawn = draw(&mut app);
        assert!(drawn.contains("BackOff"), "{drawn}");
        assert!(!drawn.contains("Pulled"), "narrowed to the pod: {drawn}");
        assert!(drawn.contains("[Pod] [Describe] [YAML]"), "{drawn}");

        press(&mut app, KeyCode::Esc);
        app.screens[0]
            .scope_mut()
            .unwrap()
            .refilter(&app.store.scopes[0]);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            AppAction::Kube(Request::Showing(0, Kind::Pods))
        );
        assert_eq!(app.kind(), Some(Kind::Pods));
        assert_eq!(
            selected_pod_name(&app).as_deref(),
            Some("orders-api-7d9f5b-abc12")
        );

        // d on an event describes what the event is about.
        press(&mut app, KeyCode::Char('e'));
        press(&mut app, KeyCode::Esc);
        app.screens[0]
            .scope_mut()
            .unwrap()
            .refilter(&app.store.scopes[0]);
        let action = press(&mut app, KeyCode::Char('d'));
        assert!(
            matches!(
                action,
                AppAction::Kube(Request::Describe { ref object, .. }) if object.slash() == "pod/orders-api-7d9f5b-def34"
            ),
            "{action:?}"
        );
    }

    #[test]
    fn a_secrets_value_is_read_on_v_shown_for_sixty_seconds_and_y_copies_it_unseen() {
        let mut app = stocked();
        press(&mut app, KeyCode::Char('s'));
        app.screens[0]
            .scope_mut()
            .unwrap()
            .refilter(&app.store.scopes[0]);
        let drawn = draw(&mut app);
        assert!(drawn.contains("› password  7 bytes"), "{drawn}");
        assert!(!drawn.contains("hunter2"), "{drawn}");
        let object = ObjectRef {
            kind: "secret".to_owned(),
            namespace: "dev".to_owned(),
            name: "db".to_owned(),
        };
        assert_eq!(
            press(&mut app, KeyCode::Char('v')),
            AppAction::Kube(Request::SecretValue {
                scope: 0,
                object: object.clone(),
                key: "password".to_owned(),
                copy: false,
            })
        );
        assert_eq!(
            app.poll_for(Duration::from_secs(9)),
            Duration::from_secs(1),
            "counting"
        );
        let drawn = draw(&mut app);
        assert!(drawn.contains("Value · db · password"), "{drawn}");
        assert!(drawn.contains("Reading…"), "{drawn}");
        let action = app.apply_kube(Event::SecretValue {
            scope: 0,
            object: object.clone(),
            key: "password".to_owned(),
            copy: false,
            value: Ok(Secret::new("hunter2")),
        });
        assert_eq!(action, AppAction::None);
        let drawn = draw(&mut app);
        assert!(drawn.contains("hunter2"), "{drawn}");
        assert!(drawn.contains("clears in"), "{drawn}");

        // Moving the cursor hides it; y reads it for the clipboard alone.
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.shell.focus, Focus::Table);
        press(&mut app, KeyCode::Char('j'));
        let drawn = draw(&mut app);
        assert!(!drawn.contains("hunter2"), "{drawn}");
        assert!(drawn.contains("tls.crt"), "{drawn}");
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(
            press(&mut app, KeyCode::Char('y')),
            AppAction::Kube(Request::SecretValue {
                scope: 0,
                object: object.clone(),
                key: "password".to_owned(),
                copy: true,
            })
        );
        let action = app.apply_kube(Event::SecretValue {
            scope: 0,
            object,
            key: "password".to_owned(),
            copy: true,
            value: Ok(Secret::new("hunter2")),
        });
        assert_eq!(
            action,
            AppAction::Copy {
                text: "hunter2".to_owned(),
                label: "Copied password of db".to_owned()
            }
        );
        let written = serde_json::to_string(&app.store.snapshot(&app.tabs)).unwrap();
        assert!(
            !written.contains("hunter2") && !written.contains("password"),
            "{written}"
        );
        assert!(!format!("{:?}", scope(&app, 0).modal).contains("hunter2"));
    }

    #[test]
    fn a_configmaps_value_shows_on_enter_and_y_copies_it() {
        let mut app = stocked();
        press(&mut app, KeyCode::Char('m'));
        app.screens[0]
            .scope_mut()
            .unwrap()
            .refilter(&app.store.scopes[0]);
        assert_eq!(press(&mut app, KeyCode::Enter), AppAction::None);
        let drawn = draw(&mut app);
        assert!(
            drawn.contains("Value · orders-config · LOG_LEVEL"),
            "{drawn}"
        );
        assert!(drawn.contains("info"), "{drawn}");
        assert_eq!(
            press(&mut app, KeyCode::Char('y')),
            AppAction::Copy {
                text: "info".to_owned(),
                label: "Copied LOG_LEVEL of orders-config".to_owned()
            }
        );
        assert!(matches!(
            press(&mut app, KeyCode::Char('d')),
            AppAction::Kube(Request::Describe { .. })
        ));
    }

    #[test]
    fn r_reads_the_open_tab_again_and_says_so() {
        let mut app = two_clusters();
        press(&mut app, KeyCode::Char('4'));
        assert_eq!(
            press(&mut app, KeyCode::Char('r')),
            AppAction::Kube(Request::Refresh(3))
        );
        assert_eq!(
            app.shell.notification().map(|(said, _)| said),
            Some("Reading prod/prod…")
        );
    }

    #[test]
    fn each_tab_and_kind_keeps_its_own_search_box() {
        let mut app = two_clusters();
        app.shell.focus = Focus::Search;
        app.handle_paste("orders");
        press(&mut app, KeyCode::Esc);
        assert_eq!(scope(&app, 0).list().input.text(), "orders");
        press(&mut app, KeyCode::Char('2'));
        assert!(
            scope(&app, 1).list().input.is_empty(),
            "the other tab's box is its own"
        );
        press(&mut app, KeyCode::Char('5'));
        assert!(secrets(&app, 4).input.is_empty(), "and so is Secrets'");
        press(&mut app, KeyCode::Char('1'));
        assert_eq!(
            scope(&app, 0).list().input.text(),
            "orders",
            "and comes back as it was"
        );
        press(&mut app, KeyCode::Char('e'));
        assert!(
            scope(&app, 0).list().input.is_empty(),
            "the events' box is its own"
        );
        press(&mut app, KeyCode::Char('p'));
        press(&mut app, KeyCode::Esc);
        assert!(
            scope(&app, 0).list().input.is_empty(),
            "Esc out of the table clears it"
        );
    }

    #[test]
    fn a_read_lands_on_its_own_tab_keeps_the_cursor_and_marks_the_cache_dirty() {
        let mut app = stocked();
        assert!(app.cache_dirty);
        app.screens[0]
            .scope_mut()
            .unwrap()
            .refilter(&app.store.scopes[0]);
        app.screens[0]
            .scope_mut()
            .unwrap()
            .list_mut()
            .cursor
            .focus(1);
        let chosen = scope(&app, 0)
            .cursor_identity(Kind::Pods, &app.store.scopes[0])
            .unwrap();
        // A pod that sorts ahead of both pushes the chosen row down a line.
        app.apply_kube(Event::Pods {
            scope: 0,
            pods: Ok(vec![
                crashing("qa", "dev", "orders-api-7d9f5b-def34"),
                pod("qa", "dev", "orders-api-7d9f5b-abc12", "Running"),
                pod("qa", "dev", "orders-api-7d9f5b-aaa01", "Running"),
            ]),
        });
        assert_eq!(app.store.scopes[0].pods.rows.len(), 3);
        assert_eq!(app.store.scopes[3].pods.rows.len(), 1, "prod is untouched");
        assert_eq!(scope(&app, 0).list().cursor.index, 2, "the row moved down");
        assert_eq!(
            scope(&app, 0).cursor_identity(Kind::Pods, &app.store.scopes[0]),
            Some(chosen)
        );
    }

    #[test]
    fn a_failed_read_is_said_once_for_the_open_tab_and_kind_and_its_rows_stand() {
        let mut app = stocked();
        app.apply_kube(Event::Pods {
            scope: 0,
            pods: Err("Unable to connect to the server".into()),
        });
        assert_eq!(
            app.shell.notification().map(|(said, _)| said),
            Some("qa/dev pods: Unable to connect to the server")
        );
        assert_eq!(
            app.store.scopes[0].pods.rows.len(),
            2,
            "yesterday's rows stand"
        );

        let mut app = stocked();
        app.apply_kube(Event::Pods {
            scope: 3,
            pods: Err("Unable to connect to the server".into()),
        });
        assert!(
            app.shell.notification().is_none(),
            "a hidden tab's trouble is on its badge and in ?, not the status bar"
        );
        app.tab = 3;
        app.apply_kube(Event::Pods {
            scope: 3,
            pods: Err("Unable to connect to the server".into()),
        });
        assert!(
            app.shell.notification().is_none(),
            "the same refusal is not said twice"
        );
        app.apply_kube(Event::Pods {
            scope: 3,
            pods: Err("context \"aks-prod\" does not exist".into()),
        });
        assert!(app.shell.notification().is_some(), "a different one is");
        app.apply_kube(Event::Secrets {
            scope: 3,
            secrets: Err("secrets is forbidden".into()),
        });
        let drawn = draw(&mut app);
        assert!(
            !drawn.contains("! prod/prod secrets"),
            "not the kind on screen: {drawn}"
        );
        press(&mut app, KeyCode::Char('s'));
        let drawn = draw(&mut app);
        assert!(
            drawn.contains("! prod/prod secrets: secrets is forbidden"),
            "{drawn}"
        );
    }

    #[test]
    fn the_frame_wears_the_badge_the_counts_and_the_problems() {
        let mut app = stocked();
        let drawn = draw(&mut app);
        assert!(drawn.contains("1 qa/dev \u{2717} 1"), "{drawn}");
        assert!(drawn.contains("4 prod"), "{drawn}");
        assert!(drawn.contains("5 Secrets"), "{drawn}");
        assert!(drawn.contains("orders-api-7d9f5b-def34"), "{drawn}");
        assert!(drawn.contains("● 2 pods · just now"), "{drawn}");

        app.apply_kube(Event::Pods {
            scope: 0,
            pods: Err("Unable to connect to the server".into()),
        });
        app.apply_azure(
            worker::Event::Inventory(Err("not signed in".into())),
            Instant::now(),
        );
        let drawn = draw(&mut app);
        assert!(
            drawn.contains("! qa/dev pods: Unable to connect"),
            "{drawn}"
        );
        app.shell.help_open = true;
        let drawn = draw(&mut app);
        assert!(drawn.contains("Problems"), "{drawn}");
        assert!(
            drawn.contains("qa/dev pods: Unable to connect to the server"),
            "{drawn}"
        );
        assert!(drawn.contains("not signed in"), "both halves: {drawn}");

        let mut app = two_clusters();
        app.apply_kube(Event::Reading(0));
        assert_eq!(
            app.poll_for(Duration::from_secs(1)),
            Duration::from_millis(100)
        );
        let drawn = draw(&mut app);
        assert!(drawn.contains("reading qa/dev pods…"), "{drawn}");
    }

    #[test]
    fn enter_opens_the_log_and_the_tick_tells_the_worker_what_to_follow() {
        let mut app = stocked();
        assert_eq!(
            follow_tick(&mut app),
            None,
            "nothing followed with the pane closed"
        );
        assert_eq!(press(&mut app, KeyCode::Enter), AppAction::None);
        assert_eq!(app.shell.focus, Focus::Details, "the pane takes the keys");
        let request = follow_tick(&mut app).expect("a follow");
        let Request::Follow(target) = request else {
            panic!("expected a follow, got {request:?}");
        };
        assert_eq!(target.scope, 0);
        assert_eq!(target.key.name, "orders-api-7d9f5b-abc12");
        assert_eq!(follow_tick(&mut app), None, "said once");

        app.apply_kube(Event::LogLines {
            target: target.clone(),
            lines: vec!["2026-09-12T12:00:00Z INFO up".to_owned()],
            finished: false,
        });
        assert_eq!(scope(&app, 0).log_lines().len(), 1);
        let drawn = draw(&mut app);
        assert!(
            drawn.contains("Log · following · orders-api-7d9f5b-abc12"),
            "{drawn}"
        );
        assert!(drawn.contains("12:00:00 INFO up"), "{drawn}");

        // Back to the table and down a row: the follow moves with the cursor.
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('j'));
        app.screens[0]
            .scope_mut()
            .unwrap()
            .refilter(&app.store.scopes[0]);
        let Some(Request::Follow(next)) = follow_tick(&mut app) else {
            panic!("the next pod's log");
        };
        assert_eq!(next.key.name, "orders-api-7d9f5b-def34");
        assert!(
            scope(&app, 0).log_lines().is_empty(),
            "the lines were the last pod's"
        );

        // Another kind, another tab: nothing on this one is followed any more.
        press(&mut app, KeyCode::Char('e'));
        assert_eq!(follow_tick(&mut app), Some(Request::Unfollow));
        press(&mut app, KeyCode::Char('p'));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(follow_tick(&mut app), Some(Request::Follow(_))));
        press(&mut app, KeyCode::Char('4'));
        assert_eq!(follow_tick(&mut app), Some(Request::Unfollow));
        press(&mut app, KeyCode::Char('1'));
        assert!(
            matches!(follow_tick(&mut app), Some(Request::Follow(_))),
            "and back again"
        );
        // An Azure tab follows nothing either.
        press(&mut app, KeyCode::Char('5'));
        assert_eq!(follow_tick(&mut app), Some(Request::Unfollow));
        press(&mut app, KeyCode::Char('1'));
        assert!(matches!(follow_tick(&mut app), Some(Request::Follow(_))));

        // Esc closes the pane; the tick says so.
        press(&mut app, KeyCode::Esc);
        assert!(!scope(&app, 0).pane_open);
        assert_eq!(app.shell.focus, Focus::Table);
        assert_eq!(follow_tick(&mut app), Some(Request::Unfollow));
    }

    #[test]
    fn d_asks_for_a_describe_once_and_the_pane_shows_it_when_it_lands() {
        let mut app = stocked();
        let AppAction::Kube(Request::Describe {
            scope: index,
            object,
        }) = press(&mut app, KeyCode::Char('d'))
        else {
            panic!("a describe request");
        };
        assert_eq!(index, 0);
        assert_eq!(object.slash(), "pod/orders-api-7d9f5b-abc12");
        assert_eq!(follow_tick(&mut app), None, "a describe follows nothing");
        let drawn = draw(&mut app);
        assert!(
            drawn.contains("Describe · orders-api-7d9f5b-abc12"),
            "{drawn}"
        );
        assert!(drawn.contains("Describe…"), "{drawn}");

        app.apply_kube(Event::Text {
            scope: 0,
            kind: TextKind::Describe,
            object: object.clone(),
            text: Ok(vec![
                "Name:  orders-api-7d9f5b-abc12".to_owned(),
                "Node:  aks-np1".to_owned(),
            ]),
        });
        let drawn = draw(&mut app);
        assert!(drawn.contains("Node:  aks-np1"), "{drawn}");
        assert_eq!(
            press(&mut app, KeyCode::Char('d')),
            AppAction::None,
            "on file now"
        );

        // The filter inside the pane.
        press(&mut app, KeyCode::Char('/'));
        assert_eq!(app.shell.focus, Focus::PaneSearch);
        app.handle_paste("Node");
        press(&mut app, KeyCode::Enter);
        let drawn = draw(&mut app);
        assert!(drawn.contains("/ Node · 1/2"), "{drawn}");
        assert!(!drawn.contains("Name:  orders"), "{drawn}");
        press(&mut app, KeyCode::Esc);
        assert!(
            scope(&app, 0).pane_filter.is_empty(),
            "Esc takes the filter off first"
        );
        assert!(scope(&app, 0).pane_open);

        // Y copies the line for what the pane shows; y the name.
        assert_eq!(
            press(&mut app, KeyCode::Char('Y')),
            AppAction::Copy {
                text: "kubectl --context aks-qa -n dev describe pod/orders-api-7d9f5b-abc12"
                    .to_owned(),
                label:
                    "Copied `kubectl --context aks-qa -n dev describe pod/orders-api-7d9f5b-abc12`"
                        .to_owned(),
            }
        );
        assert!(matches!(
            press(&mut app, KeyCode::Char('y')),
            AppAction::Copy { text, .. } if text == "orders-api-7d9f5b-abc12"
        ));

        // v: the YAML, asked for; r forgets both.
        assert!(matches!(
            press(&mut app, KeyCode::Char('v')),
            AppAction::Kube(Request::Yaml { .. })
        ));
        press(&mut app, KeyCode::Char('r'));
        assert!(matches!(
            press(&mut app, KeyCode::Char('d')),
            AppAction::Kube(Request::Describe { .. })
        ));
    }

    #[test]
    fn x_asks_first_and_the_second_x_is_the_one_delete_while_a_bare_pod_is_refused() {
        let mut app = stocked();
        assert_eq!(press(&mut app, KeyCode::Char('x')), AppAction::None);
        assert!(scope(&app, 0).modal.is_some(), "asked, not deleted");
        let drawn = draw(&mut app);
        assert!(
            drawn.contains("Restart orders-api-7d9f5b-abc12?"),
            "{drawn}"
        );
        assert!(
            drawn.contains("Deployment orders-api replaces it"),
            "{drawn}"
        );
        assert!(drawn.contains("x again to restart it"), "{drawn}");
        // Any other key closes it and is not otherwise acted on.
        assert_eq!(press(&mut app, KeyCode::Char('j')), AppAction::None);
        assert!(scope(&app, 0).modal.is_none());
        assert_eq!(
            scope(&app, 0).list().cursor.index,
            0,
            "j did not move the cursor"
        );

        press(&mut app, KeyCode::Char('x'));
        let action = press(&mut app, KeyCode::Char('x'));
        let AppAction::Kube(Request::Delete { scope: 0, key }) = action else {
            panic!("the delete, got {action:?}");
        };
        assert_eq!(key.name, "orders-api-7d9f5b-abc12");
        assert!(scope(&app, 0).modal.is_none());
        app.apply_kube(Event::Deleted {
            scope: 0,
            key,
            error: None,
        });
        assert_eq!(
            app.shell.notification().map(|(said, _)| said),
            Some("Deleted orders-api-7d9f5b-abc12; Deployment orders-api is putting a new one up")
        );

        // The modal takes the pointer: a click on its yes answers, a click
        // anywhere else closes it, and nothing underneath is reached.
        press(&mut app, KeyCode::Char('x'));
        draw(&mut app);
        let yes = app.shell.find(&Target::Confirm).expect("a yes button");
        assert!(matches!(
            click(&mut app, yes.x, yes.y),
            AppAction::Kube(Request::Delete { .. })
        ));
        press(&mut app, KeyCode::Char('x'));
        draw(&mut app);
        assert_eq!(
            click(&mut app, 2, 4),
            AppAction::None,
            "a row under the modal"
        );
        assert!(scope(&app, 0).modal.is_none(), "closed");
        assert_eq!(
            scope(&app, 0).list().cursor.index,
            0,
            "and the row was not taken"
        );

        // A pod nothing put there is refused outright.
        let mut bare = pod("qa", "dev", "debug-shell", "Running");
        bare.owner = None;
        app.apply_kube(Event::Pods {
            scope: 0,
            pods: Ok(vec![bare]),
        });
        app.screens[0]
            .scope_mut()
            .unwrap()
            .refilter(&app.store.scopes[0]);
        press(&mut app, KeyCode::Char('x'));
        assert!(scope(&app, 0).modal.is_none());
        assert!(
            app.shell
                .notification()
                .is_some_and(|(said, _)| said.contains("no controller"))
        );
        press(&mut app, KeyCode::Char('X'));
        assert!(scope(&app, 0).modal.is_none());
        assert!(
            app.shell
                .notification()
                .is_some_and(|(said, _)| said.contains("nothing to roll"))
        );
    }

    #[test]
    fn the_owner_is_read_once_the_cursor_rests_and_equals_scales_it() {
        let mut app = stocked();
        app.screens[0]
            .scope_mut()
            .unwrap()
            .refilter(&app.store.scopes[0]);
        let now = Instant::now();
        assert!(app.tick(now).is_empty(), "the rest has just started");
        assert!(app.is_resting());
        let actions = app.tick(now + REST);
        let deployment = ObjectRef {
            kind: "deployment".to_owned(),
            namespace: "dev".to_owned(),
            name: "orders-api".to_owned(),
        };
        assert_eq!(
            actions,
            vec![AppAction::Kube(Request::Owner {
                scope: 0,
                object: deployment.clone()
            })]
        );
        assert!(app.tick(now + REST * 2).is_empty(), "asked once");
        app.apply_kube(Event::Owner {
            scope: 0,
            object: deployment.clone(),
            replicas: Ok(crate::kube::Replicas {
                desired: 3,
                ready: 3,
            }),
        });
        let drawn = draw(&mut app);
        assert!(
            drawn.contains("Deployment/orders-api · 3/3 ready"),
            "{drawn}"
        );

        // = opens the scale modal filled with the count; Enter sends it.
        assert_eq!(
            press(&mut app, KeyCode::Char('=')),
            AppAction::None,
            "on file: nothing to ask"
        );
        let drawn = draw(&mut app);
        assert!(drawn.contains("Replicas  [ 3"), "{drawn}");
        assert!(drawn.contains("now 3 desired · 3 ready"), "{drawn}");
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Char('5'));
        let action = press(&mut app, KeyCode::Enter);
        assert_eq!(
            action,
            AppAction::Kube(Request::Scale {
                scope: 0,
                object: deployment.clone(),
                replicas: 5
            })
        );
        app.apply_kube(Event::Acted {
            scope: 0,
            verb: "scale",
            object: deployment.clone(),
            error: None,
        });
        assert_eq!(
            app.shell.notification().map(|(said, _)| said),
            Some("deployment/orders-api scale sent")
        );

        // Not a number: the modal stays and says so.
        press(&mut app, KeyCode::Char('='));
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(press(&mut app, KeyCode::Enter), AppAction::None);
        assert!(scope(&app, 0).modal.is_some());
        assert!(
            app.shell
                .notification()
                .is_some_and(|(said, _)| said.contains("whole number"))
        );
        press(&mut app, KeyCode::Esc);
        assert!(scope(&app, 0).modal.is_none());

        // X: the rollout, confirmed with X.
        press(&mut app, KeyCode::Char('X'));
        assert_eq!(
            press(&mut app, KeyCode::Char('X')),
            AppAction::Kube(Request::RolloutRestart {
                scope: 0,
                object: deployment
            })
        );

        // A pod on a StatefulSet: scalable too; on a Job: not.
        let mut redis = pod("qa", "dev", "redis-0", "Running");
        redis.owner = Some(("StatefulSet".to_owned(), "redis".to_owned()));
        let mut job = pod("qa", "dev", "report-x1", "Completed");
        job.owner = Some(("Job".to_owned(), "report".to_owned()));
        app.apply_kube(Event::Pods {
            scope: 0,
            pods: Ok(vec![job, redis]),
        });
        app.screens[0]
            .scope_mut()
            .unwrap()
            .refilter(&app.store.scopes[0]);
        // By name: redis-0 first, report-x1 second.
        assert!(
            matches!(
                press(&mut app, KeyCode::Char('=')),
                AppAction::Kube(Request::Owner { .. })
            ),
            "not on file yet: asked, and the box fills when it lands"
        );
        assert!(scope(&app, 0).modal.is_some());
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('='));
        assert!(scope(&app, 0).modal.is_none());
        assert!(
            app.shell
                .notification()
                .is_some_and(|(said, _)| said.contains("nothing to scale"))
        );
    }

    #[test]
    fn b_hands_the_terminal_to_kubectl_exec_on_the_pod_and_its_followed_container() {
        let mut app = stocked();
        assert_eq!(
            press(&mut app, KeyCode::Char('b')),
            AppAction::Exec {
                context: "aks-qa".to_owned(),
                namespace: "dev".to_owned(),
                pod: "orders-api-7d9f5b-abc12".to_owned(),
                container: None,
            }
        );
        // The toolbar button is the same key.
        draw(&mut app);
        let bash = app
            .shell
            .find(&Target::Button(Button::Bash))
            .expect("a Bash button");
        assert!(matches!(
            click(&mut app, bash.x + 1, bash.y),
            AppAction::Exec { .. }
        ));
        let logs = app
            .shell
            .find(&Target::Button(Button::Logs))
            .expect("a Logs button");
        click(&mut app, logs.x + 1, logs.y);
        assert!(scope(&app, 0).pane_open, "the Logs button opens the pane");
    }

    #[test]
    fn a_layout_survives_a_round_trip_through_the_file_by_the_tabs_key() {
        let mut app = two_clusters();
        app.tab = 3;
        {
            let prod = app.screens[3].scope_mut().unwrap();
            prod.set_kind(Kind::Events);
            prod.list_of_mut(Kind::Pods).sort = ColumnId::Age;
            prod.list_of_mut(Kind::Pods).descending = true;
            prod.list_of_mut(Kind::Pods).layout.columns[0].width = 20;
        }
        app.screens[0]
            .scope_mut()
            .unwrap()
            .list_of_mut(Kind::Pods)
            .layout
            .set_visible(ColumnId::Node, true);
        app.screens[5].registries_mut().unwrap().repositories.sort = ColumnId::Created;

        let session = app.session();
        assert!(session.tabs.contains_key("prod/prod"), "{session:?}");
        assert!(session.tabs.contains_key("registries"), "{session:?}");
        let mut fresh = two_clusters();
        fresh.restore(&session);

        assert_eq!(fresh.tabs[fresh.tab].label(), "prod");
        assert_eq!(fresh.tabs[fresh.tab].key(), "prod/prod");
        let prod = scope(&fresh, 3);
        assert_eq!(prod.kind, Kind::Events);
        assert_eq!(prod.list_of(Kind::Pods).sort, ColumnId::Age);
        assert!(prod.list_of(Kind::Pods).descending);
        assert_eq!(prod.list_of(Kind::Pods).layout.columns[0].width, 20);
        let node_shown = |app: &App, tab: usize| {
            scope(app, tab)
                .list_of(Kind::Pods)
                .layout
                .columns
                .iter()
                .any(|column| column.id == ColumnId::Node && column.visible)
        };
        assert!(node_shown(&fresh, 0));
        assert!(!node_shown(&fresh, 1));
        assert_eq!(registries(&fresh, 5).repositories.sort, ColumnId::Created);

        let written = serde_json::to_string(&session).unwrap();
        assert!(!written.contains("cursor"), "{written}");
        assert!(!written.contains("query"), "{written}");
    }

    #[test]
    fn a_tab_or_column_this_build_does_not_know_is_skipped_rather_than_fatal() {
        let mut session = Session {
            tab: Some("staging/blue".into()),
            ..Session::default()
        };
        let held = session.tab("qa/dev");
        held.kind = Some("nodes".into());
        held.sort = Some(("from_the_future".into(), "asc".into()));
        held.columns = vec![SessionColumn {
            key: "from_the_future".into(),
            width: Some(9),
            visible: Some(false),
        }];
        let mut app = two_clusters();
        let before = scope(&app, 0).list_of(Kind::Pods).layout.clone();
        app.restore(&session);
        assert_eq!(app.tab, 0, "an unknown tab is the first one");
        assert_eq!(scope(&app, 0).kind, Kind::Pods, "an unknown kind is pods");
        assert_eq!(scope(&app, 0).list_of(Kind::Pods).sort, ColumnId::Name);
        assert_eq!(scope(&app, 0).list_of(Kind::Pods).layout, before);
    }

    #[test]
    fn a_tab_over_every_namespace_shows_which_one_a_row_is_in() {
        let tabs = tabs(
            config::parse("[[clusters]]\nname = \"lab\"\n")
                .unwrap()
                .tabs(),
        );
        let app = App::new(tabs, Store::new(1));
        for kind in Kind::ALL {
            assert!(
                scope(&app, 0)
                    .list_of(kind)
                    .layout
                    .columns
                    .iter()
                    .any(|column| column.id == ColumnId::Namespace && column.visible),
                "{kind:?}"
            );
        }
        let app = two_clusters();
        assert!(
            !scope(&app, 0)
                .list_of(Kind::Pods)
                .layout
                .columns
                .iter()
                .any(|column| column.id == ColumnId::Namespace && column.visible)
        );
    }
}
