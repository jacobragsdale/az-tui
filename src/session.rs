//! What the app remembers between runs: which tab was open, how each table
//! was sorted, and how wide its columns were.
//!
//! Not the query, not the cursor, not a revealed value, and never a secret's
//! name. A session file is a layout, and a layout is the only thing worth
//! putting back.
//!
//! Anything this build does not recognise — a column key from a newer
//! version, a sort direction spelled some other way — is dropped rather than
//! refused: a session is a convenience, and an unreadable one costs a default
//! layout, not a start.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cache::write_private;

/// Bumped when a stored layout would read wrong: 2 hid the vault and
/// registry columns behind the environment; 3 added the AKS tabs and each
/// tab's `kind`.
const VERSION: u32 = 3;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Session {
    /// Stamped by [`Session::save`]; a file of any other value is ignored on
    /// the way in.
    #[serde(default)]
    pub version: u32,
    /// The open tab's key — `qa/dev`, `prod/prod`, `secrets`, `registries`
    /// ([`crate::config::Tab::key`]). One this run does not have falls back
    /// to the first tab.
    #[serde(default)]
    pub tab: Option<String>,
    /// One entry per tab, under the same keys.
    #[serde(default)]
    pub tabs: BTreeMap<String, TabSession>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TabSession {
    /// `"pods"`, `"events"`, `"configmaps"` or `"secrets"` on an AKS tab;
    /// absent elsewhere.
    #[serde(default)]
    pub kind: Option<String>,
    /// `["name", "asc"]`. Kept as two strings so a column this build does
    /// not know is dropped on the way in rather than refused.
    #[serde(default)]
    pub sort: Option<(String, String)>,
    #[serde(default)]
    pub columns: Vec<SessionColumn>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionColumn {
    pub key: String,
    #[serde(default)]
    pub width: Option<u16>,
    #[serde(default)]
    pub visible: Option<bool>,
}

impl Session {
    /// The session at `path`, or a default one. A file of another version,
    /// or one that will not parse, is simply not there.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|source| serde_json::from_str::<Self>(&source).ok())
            .filter(|session| session.version == VERSION)
            .unwrap_or_default()
    }

    /// Writes the session atomically, `0600`, through the cache's writer. A
    /// failure is worth saying once in the status bar and nothing more: the
    /// layout is not the work.
    pub fn save(&mut self, path: &Path) -> Result<()> {
        self.version = VERSION;
        let bytes = serde_json::to_vec_pretty(self).context("failed to write the session")?;
        write_private(path, &bytes)
    }

    pub fn tab(&mut self, name: &str) -> &mut TabSession {
        self.tabs.entry(name.to_owned()).or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        let mut session = Session {
            tab: Some("secrets".into()),
            ..Session::default()
        };
        let tab = session.tab("secrets");
        tab.sort = Some(("name".into(), "asc".into()));
        tab.columns = vec![
            SessionColumn {
                key: "vault".into(),
                width: Some(12),
                visible: Some(true),
            },
            SessionColumn {
                key: "type".into(),
                width: None,
                visible: Some(false),
            },
        ];
        session.tab("qa/dev").kind = Some("pods".into());
        session
    }

    #[test]
    fn a_session_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("session.json");
        session().save(&path).unwrap();

        let read = Session::load(&path);
        assert_eq!(read.tab.as_deref(), Some("secrets"));
        let tab = &read.tabs["secrets"];
        assert_eq!(
            tab.sort
                .as_ref()
                .map(|(key, way)| (key.as_str(), way.as_str())),
            Some(("name", "asc"))
        );
        assert_eq!(tab.columns.len(), 2);
        assert_eq!(tab.columns[0].width, Some(12));
        assert_eq!(tab.columns[1].visible, Some(false));
        assert_eq!(read.tabs["qa/dev"].kind.as_deref(), Some("pods"));
        assert!(tab.kind.is_none(), "an Azure tab has no kind");
    }

    #[test]
    fn a_file_this_build_cannot_read_is_the_default_layout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        assert!(Session::load(&path).tabs.is_empty(), "a missing file");

        std::fs::write(&path, "{ not json").unwrap();
        assert!(Session::load(&path).tabs.is_empty());

        std::fs::write(&path, r#"{"version":99,"tab":"secrets"}"#).unwrap();
        assert!(
            Session::load(&path).tab.is_none(),
            "a version this build does not know"
        );
    }

    #[test]
    fn a_column_key_this_build_does_not_know_is_carried_but_never_applied() {
        // Dropping it on the way in would lose a newer build's layout every
        // time an older one ran; the screens simply ignore what they cannot
        // match, which is where the dropping belongs.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        std::fs::write(
            &path,
            r#"{"version":3,"tab":"secrets","tabs":{"secrets":{"columns":[{"key":"from_the_future","width":9}]}}}"#,
        )
        .unwrap();
        let read = Session::load(&path);
        assert_eq!(read.tabs["secrets"].columns[0].key, "from_the_future");
    }

    #[test]
    fn nothing_that_was_typed_or_read_reaches_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let mut session = session();
        session.save(&path).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        // There is no field for any of these, which is the point; this is
        // the check from the outside that keeps it that way. ("secrets" is
        // in there — it is the tab's name, and a tab name is a layout.)
        for absent in [
            "query",
            "cursor",
            "db-password",
            "orders-api",
            "\"value\"",
            "log",
        ] {
            assert!(!written.contains(absent), "{absent} in {written}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_session_is_readable_only_by_the_user_who_wrote_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        session().save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
