//! What a tab is, what a click can land on, and what a screen may ask the
//! run loop to do.

use crate::columns::ColumnId;
use crate::config::{self, REGISTRIES_TAB, SECRETS_TAB};
use crate::filter::Env;
use crate::kube::{self, Kind};
use crate::worker;

/// One tab of the bar: an AKS namespace, or one of the two Azure tables.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Tab {
    Scope(config::Tab),
    Secrets,
    Registries,
}

impl Tab {
    /// What the cache and the session call this tab: `qa/dev`, `secrets`.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Scope(tab) => tab.key(),
            Self::Secrets => SECRETS_TAB.to_owned(),
            Self::Registries => REGISTRIES_TAB.to_owned(),
        }
    }

    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Scope(tab) => &tab.label,
            Self::Secrets => "Secrets",
            Self::Registries => "Registries",
        }
    }

    /// What the tab bar falls back to when the terminal is narrow.
    #[must_use]
    pub fn short_label(&self) -> &str {
        match self {
            Self::Scope(tab) => tab.scope.short_label(),
            Self::Secrets => "Sec",
            Self::Registries => "Reg",
        }
    }
}

/// The tabs in the order the bar shows them and the number keys reach them:
/// every AKS scope in `config.toml`'s order, then Secrets, then Registries.
/// A scope tab's index is therefore its kube scope index.
#[must_use]
pub fn tabs(scopes: Vec<config::Tab>) -> Vec<Tab> {
    scopes
        .into_iter()
        .map(Tab::Scope)
        .chain([Tab::Secrets, Tab::Registries])
        .collect()
}

/// Something on screen a click can land on.
///
/// The shell keeps a `Vec<(Rect, Target)>` rebuilt every frame and resolves a
/// click to the **last** region containing the point — drawn last is on top,
/// which is what puts a modal over the table under it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    /// A tab, by its index in the bar.
    Tab(usize),
    /// The pill at the right of the tab bar that says which kind shows.
    KindPill,
    /// One line of the kind pill's menu.
    KindOption(Kind),
    /// A row of the table, by its index among the rows currently shown.
    Row(usize),
    /// A column header.
    Header(ColumnId),
    /// One line of the Env header's menu: an environment, or `None` for all.
    EnvOption(Option<Env>),
    /// One key of a configmap or a secret, in the details pane.
    KeyRow(usize),
    SearchField,
    ClearSearch,
    Details,
    /// The text pane under the details.
    TextPane,
    /// One of the toolbar buttons in the details pane.
    Button(Button),
    /// A modal's yes.
    Confirm,
    /// A modal's body: a click there does nothing.
    Modal,
    /// Anywhere that closes a modal.
    Dismiss,
    Help,
}

/// The buttons in the details pane's toolbar. Each stands for the key it
/// names, so clicking one is pressing it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Button {
    Logs,
    Bash,
    Restart,
    Scale,
    Describe,
    Yaml,
    /// Events: the pod the event is about.
    Pod,
    /// ConfigMaps and Secrets: the key's value in the text pane.
    Value,
    /// Secrets: the key's value on the clipboard, unseen.
    Copy,
}

impl Button {
    /// The toolbar for one kind, in order.
    #[must_use]
    pub const fn for_kind(kind: Kind) -> &'static [Self] {
        match kind {
            Kind::Pods => &[
                Self::Logs,
                Self::Bash,
                Self::Restart,
                Self::Scale,
                Self::Describe,
                Self::Yaml,
            ],
            Kind::Events => &[Self::Pod, Self::Describe, Self::Yaml],
            Kind::ConfigMaps => &[Self::Value, Self::Describe],
            Kind::Secrets => &[Self::Value, Self::Copy, Self::Describe],
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Logs => "Logs",
            Self::Bash => "Bash",
            Self::Restart => "Restart",
            Self::Scale => "Scale",
            Self::Describe => "Describe",
            Self::Yaml => "YAML",
            Self::Pod => "Pod",
            Self::Value => "Value",
            Self::Copy => "Copy",
        }
    }
}

/// What a screen wants the run loop to do next. A screen never talks to a
/// worker, the clipboard or the browser itself: it says what it wants and
/// the loop does it, which is what keeps every screen testable without any.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppAction {
    None,
    /// Put this on the clipboard and say `label` in the status bar. The label
    /// never contains what was copied.
    Copy {
        text: String,
        label: String,
    },
    Azure(worker::Request),
    Kube(kube::Request),
    OpenUrl(String),
    /// Hand the terminal to `kubectl exec -it` and take it back after.
    Exec {
        context: String,
        namespace: String,
        pod: String,
        container: Option<String>,
    },
    Quit,
}

// ponytail: no `Screen` trait. `App` holds each screen as one arm of an
// enum and dispatches with a match, so a trait object would buy one
// indirection and cost every screen-specific method a place in a shared
// vocabulary — the reveal state belongs to Secrets, the second level to
// Registries, the text pane to a scope.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tab_knows_its_session_key_and_its_labels() {
        let config = config::parse(config::tests::TWO_CLUSTERS).unwrap();
        let tabs = tabs(config.tabs());
        assert_eq!(tabs.len(), 6);
        let keys: Vec<String> = tabs.iter().map(Tab::key).collect();
        assert_eq!(
            keys,
            [
                "qa/dev",
                "qa/qa",
                "qa/uat",
                "prod/prod",
                "secrets",
                "registries"
            ]
        );
        assert_eq!(tabs[3].label(), "prod");
        assert_eq!(tabs[0].short_label(), "dev");
        assert!(matches!(tabs[4], Tab::Secrets));
        assert_eq!(tabs[5].label(), "Registries");
        assert_eq!(tabs[5].short_label(), "Reg");
        for tab in &tabs {
            assert!(tab.short_label().len() <= tab.label().len());
        }
    }
}
