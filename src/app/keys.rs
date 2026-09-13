//! Every key, in one table, which is what `?` renders.

use super::screen::TabId;

/// One row of the help modal, and one case of the dispatch.
pub struct Key {
    /// As the help prints it.
    pub keys: &'static str,
    pub does: &'static str,
    /// The tab this key belongs to, or `None` when every tab has it.
    pub tab: Option<TabId>,
}

pub const KEYS: &[Key] = &[
    Key {
        keys: "1 2",
        does: "Secrets, Registries",
        tab: None,
    },
    Key {
        keys: "j k  ↑ ↓",
        does: "move the cursor in the focused pane",
        tab: None,
    },
    Key {
        keys: "PgUp PgDn",
        does: "a screenful at a time",
        tab: None,
    },
    Key {
        keys: "Home End",
        does: "the first row, the last row",
        tab: None,
    },
    Key {
        keys: "Tab",
        does: "focus the table or the details pane",
        tab: None,
    },
    Key {
        keys: "/",
        does: "search; Esc or Enter keeps the filter, Esc again clears it",
        tab: None,
    },
    Key {
        keys: "Ctrl-U",
        does: "clear the search box",
        tab: None,
    },
    Key {
        keys: "s  S",
        does: "next sort column, flip the direction",
        tab: None,
    },
    Key {
        keys: "r",
        does: "refresh now",
        tab: None,
    },
    Key {
        keys: "o",
        does: "open the vault or registry in the Azure portal",
        tab: None,
    },
    Key {
        keys: "?",
        does: "this help",
        tab: None,
    },
    Key {
        keys: "q  Ctrl-C",
        does: "quit",
        tab: None,
    },
    Key {
        keys: "Enter  v",
        does: "show the value for 60 seconds; again hides it",
        tab: Some(TabId::Secrets),
    },
    Key {
        keys: "y",
        does: "copy the value without showing it",
        tab: Some(TabId::Secrets),
    },
    Key {
        keys: "Y",
        does: "copy the secret's name",
        tab: Some(TabId::Secrets),
    },
    Key {
        keys: "Enter",
        does: "open the repository's tags",
        tab: Some(TabId::Registries),
    },
    Key {
        keys: "h  Esc  Bksp",
        does: "back to the repositories",
        tab: Some(TabId::Registries),
    },
    Key {
        keys: "y",
        does: "copy the pull reference",
        tab: Some(TabId::Registries),
    },
    Key {
        keys: "Y",
        does: "copy the digest reference (inside a repository)",
        tab: Some(TabId::Registries),
    },
];

/// The keys worth showing on this tab: the shared ones and its own.
pub fn for_tab(tab: TabId) -> impl Iterator<Item = &'static Key> {
    KEYS.iter()
        .filter(move |key| key.tab.is_none_or(|held| held == tab))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_says_what_it_does_and_the_widest_column_stays_narrow() {
        for key in KEYS {
            assert!(!key.keys.is_empty() && !key.does.is_empty());
            assert!(
                key.keys.len() <= 12,
                "the help's left column is 12 wide: {:?}",
                key.keys
            );
        }
    }

    #[test]
    fn a_tab_sees_the_shared_keys_and_its_own() {
        let secrets: Vec<&str> = for_tab(TabId::Secrets).map(|key| key.does).collect();
        assert!(secrets.iter().any(|does| does.contains("60 seconds")));
        assert!(
            !secrets
                .iter()
                .any(|does| does.contains("repository's tags"))
        );
        assert!(secrets.iter().any(|does| does.contains("refresh now")));

        let registries: Vec<&str> = for_tab(TabId::Registries).map(|key| key.does).collect();
        assert!(
            registries
                .iter()
                .any(|does| does.contains("repository's tags"))
        );
        assert!(registries.iter().any(|does| does.contains("refresh now")));
    }
}
