//! What a tab is, and what the shell may ask one to do.

use crate::columns::ColumnId;
use crate::worker;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TabId {
    Secrets,
    Registries,
}

impl TabId {
    pub const ALL: [Self; 2] = [Self::Secrets, Self::Registries];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Secrets => "Secrets",
            Self::Registries => "Registries",
        }
    }

    /// What the tab bar falls back to when the terminal is narrow.
    #[must_use]
    pub const fn short_label(self) -> &'static str {
        match self {
            Self::Secrets => "Sec",
            Self::Registries => "Reg",
        }
    }

    #[must_use]
    pub const fn number(self) -> char {
        match self {
            Self::Secrets => '1',
            Self::Registries => '2',
        }
    }

    #[must_use]
    pub const fn from_number(key: char) -> Option<Self> {
        match key {
            '1' => Some(Self::Secrets),
            '2' => Some(Self::Registries),
            _ => None,
        }
    }
}

/// Something on screen a click can land on.
///
/// The shell keeps a `Vec<(Rect, Target)>` rebuilt every frame and resolves a
/// click to the **last** region containing the point — drawn last is on top,
/// which is what puts a modal over the table under it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    Tab(TabId),
    /// A row of the table, by its index among the rows currently shown.
    Row(usize),
    /// A column header.
    Header(ColumnId),
    SearchField,
    ClearSearch,
    Details,
    Help,
}

/// What a screen wants the run loop to do next. A screen never talks to the
/// worker, the clipboard or the browser itself: it says what it wants and the
/// loop does it, which is what keeps every screen testable without either.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppAction {
    None,
    /// Put this on the clipboard and say `label` in the status bar. The label
    /// never contains what was copied.
    Copy {
        text: String,
        label: String,
    },
    Send(worker::Request),
    OpenUrl(String),
    Quit,
}

// ponytail: no `Screen` trait. `App` holds both screens as named fields and
// dispatches with a two-armed match, so a trait object would buy one
// indirection and cost every screen-specific method a place in a shared
// vocabulary — the reveal state belongs to Secrets and the second level
// belongs to Registries. A third tab is when this is worth revisiting.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tab_is_named_by_its_number_and_knows_a_short_name() {
        assert_eq!(TabId::from_number('1'), Some(TabId::Secrets));
        assert_eq!(TabId::from_number('2'), Some(TabId::Registries));
        assert_eq!(TabId::from_number('3'), None);
        for tab in TabId::ALL {
            assert_eq!(TabId::from_number(tab.number()), Some(tab));
            assert!(tab.short_label().len() <= tab.label().len());
        }
    }
}
