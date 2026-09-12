//! The Registries tab: every repository across every registry, and one
//! repository's tags inside it.
//!
//! Step 08 fills this in. Until then it draws what the store holds so the
//! tab bar, the tab switch and the worker's registry events can all be seen
//! working.

use crate::store::Store;
use crate::text_input::TextInput;

use super::screen::{AppAction, Target};
use super::shell::Shell;

#[derive(Default)]
pub struct RegistriesScreen {
    input: TextInput,
}

impl RegistriesScreen {
    pub fn input_mut(&mut self) -> &mut TextInput {
        &mut self.input
    }

    #[must_use]
    pub const fn input(&self) -> &TextInput {
        &self.input
    }

    pub fn refilter(&mut self, _store: &Store) {}

    pub fn invalidate(&mut self) {}

    pub fn keep_cursor(&mut self, _store: &Store, _was: Option<(String, String)>) {}

    #[must_use]
    pub fn cursor_identity(&self, _store: &Store) -> Option<(String, String)> {
        None
    }

    pub fn handle_key(
        &mut self,
        _shell: &mut Shell,
        _store: &Store,
        _key: crossterm::event::KeyEvent,
    ) -> AppAction {
        AppAction::None
    }

    pub fn handle_click(
        &mut self,
        _shell: &mut Shell,
        _store: &Store,
        _target: Target,
    ) -> AppAction {
        AppAction::None
    }

    pub fn handle_wheel(&mut self, _shell: &mut Shell, _target: Option<Target>, _delta: i32) {}

    #[must_use]
    pub fn badge(&self, _store: &Store) -> Option<String> {
        // Nothing about a registry is urgent.
        None
    }

    #[must_use]
    pub fn footer_hint(&self, _shell: &Shell) -> String {
        "the Registries tab arrives in step 08".to_owned()
    }
}
