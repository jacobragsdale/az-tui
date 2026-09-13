//! Every key, in one table, which is what `?` renders.

/// Which kind of tab a key belongs to — not a tab index. Every AKS
/// namespace tab is one section; Secrets and Registries are one each.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Section {
    Aks,
    Secrets,
    Registries,
}

/// One row of the help modal, and one case of the dispatch.
pub struct Key {
    /// As the help prints it.
    pub keys: &'static str,
    pub does: &'static str,
    /// The section this key belongs to, or `None` when every tab has it.
    pub tab: Option<Section>,
}

pub const KEYS: &[Key] = &[
    Key {
        keys: "1-9  [ ] ← →",
        does: "a tab by number; the previous, the next",
        tab: None,
    },
    Key {
        keys: "j k  ↑ ↓",
        does: "move the cursor in the focused pane",
        tab: None,
    },
    Key {
        keys: "PgUp PgDn",
        does: "a screenful at a time; Home and End the ends",
        tab: None,
    },
    Key {
        keys: "Tab",
        does: "focus the table, the details, or the text pane under them",
        tab: None,
    },
    Key {
        keys: "/",
        does: "search; Esc or Enter keeps the filter, Esc again clears it; in a text pane, filters its lines",
        tab: None,
    },
    Key {
        keys: "Ctrl-U",
        does: "clear the search box",
        tab: None,
    },
    Key {
        keys: "S  R",
        does: "sort by the next column; reverse the direction. A header click sorts too",
        tab: None,
    },
    Key {
        keys: "r",
        does: "refresh now: this namespace, or every vault and registry",
        tab: None,
    },
    Key {
        keys: "Esc",
        does: "back out one step: the modal, the pane filter, the query, then the open pane or repository",
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
        keys: "p e m s",
        does: "Pods, Events, ConfigMaps, Secrets; e on a pod is that pod's events",
        tab: Some(Section::Aks),
    },
    Key {
        keys: "Enter  l",
        does: "pods: the log, following, in the text pane; again closes it. Events: the pod. ConfigMaps and Secrets: the key's value",
        tab: Some(Section::Aks),
    },
    Key {
        keys: "d  v",
        does: "describe / YAML of what is under the cursor; v on a configmap or secret is the key's value",
        tab: Some(Section::Aks),
    },
    Key {
        keys: "P  C",
        does: "the log before the last restart; the pod's next container",
        tab: Some(Section::Aks),
    },
    Key {
        keys: "End  z",
        does: "follow the log again; the text pane alone, and back",
        tab: Some(Section::Aks),
    },
    Key {
        keys: "b",
        does: "a shell in the pod: bash, or sh when there is none",
        tab: Some(Section::Aks),
    },
    Key {
        keys: "x  X",
        does: "restart the pod (delete it; its owner replaces it) / rollout-restart its owner",
        tab: Some(Section::Aks),
    },
    Key {
        keys: "=",
        does: "scale the pod's deployment or statefulset",
        tab: Some(Section::Aks),
    },
    Key {
        keys: "y  Y",
        does: "copy the name — on a configmap or secret, the key's value, unseen; copy the kubectl line for what the pane shows",
        tab: Some(Section::Aks),
    },
    Key {
        keys: "Enter  v",
        does: "show the value for 60 seconds; again hides it",
        tab: Some(Section::Secrets),
    },
    Key {
        keys: "y",
        does: "copy the value without showing it",
        tab: Some(Section::Secrets),
    },
    Key {
        keys: "Y",
        does: "copy the secret's name",
        tab: Some(Section::Secrets),
    },
    Key {
        keys: "o",
        does: "open the vault in the Azure portal",
        tab: Some(Section::Secrets),
    },
    Key {
        keys: "Enter",
        does: "open the repository's tags",
        tab: Some(Section::Registries),
    },
    Key {
        keys: "h  Bksp",
        does: "back to the repositories",
        tab: Some(Section::Registries),
    },
    Key {
        keys: "y",
        does: "copy the pull reference",
        tab: Some(Section::Registries),
    },
    Key {
        keys: "Y",
        does: "copy the digest reference (inside a repository)",
        tab: Some(Section::Registries),
    },
    Key {
        keys: "o",
        does: "open the registry in the Azure portal",
        tab: Some(Section::Registries),
    },
];

/// The keys worth showing in this section: the shared ones and its own.
pub fn for_section(section: Section) -> impl Iterator<Item = &'static Key> {
    KEYS.iter()
        .filter(move |key| key.tab.is_none_or(|held| held == section))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_says_what_it_does_and_the_widest_column_stays_narrow() {
        for key in KEYS {
            assert!(!key.keys.is_empty() && !key.does.is_empty());
            assert!(
                key.keys.chars().count() <= 12,
                "the help's left column is 12 wide: {:?}",
                key.keys
            );
        }
    }

    #[test]
    fn a_section_sees_the_shared_keys_and_its_own() {
        let does = |section| -> Vec<&str> { for_section(section).map(|key| key.does).collect() };
        let has = |list: &[&str], word: &str| list.iter().any(|does| does.contains(word));

        let secrets = does(Section::Secrets);
        assert!(has(&secrets, "60 seconds"));
        assert!(!has(&secrets, "bash"));
        assert!(!has(&secrets, "repository's tags"));

        let aks = does(Section::Aks);
        assert!(has(&aks, "bash"));
        assert!(!has(&aks, "60 seconds"));

        let registries = does(Section::Registries);
        assert!(has(&registries, "repository's tags"));

        for list in [secrets, aks, registries] {
            assert!(has(&list, "refresh now"));
        }
    }
}
