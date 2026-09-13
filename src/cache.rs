//! The snapshot the next start paints from, before any network call.
//!
//! Names and metadata only. **No value ever reaches this file**, and it
//! cannot: the newtypes a value lives in derive no `Serialize`, and this
//! module cannot so much as name those types — a word-boundary grep for
//! either over this file comes back empty. `SecretRow` is a different word:
//! it carries a secret's name, not its value. A pod carries names, statuses,
//! images and owners; a configmap's data, a cluster secret's keys and a log
//! line are types this file does not name either.
//!
//! One entry per tab, under the same key the session uses
//! ([`crate::config::Tab::key`], [`SECRETS_TAB`], [`REGISTRIES_TAB`]). A tab
//! nothing has read is not in the file.
//!
//! Names alone are mildly sensitive — `stripe-prod-key` says a good deal — so
//! the file is written `0600` on unix.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::azure::{Inventory, Registry, Repository, SecretRow, Vault};
use crate::config::{REGISTRIES_TAB, SECRETS_TAB};
use crate::kube::Pod;
use crate::timestamp::Timestamp;

/// The schema this build writes. A file of any other version is ignored
/// rather than migrated: it is a cache, and the next refresh rewrites it.
/// 2 keyed the file by tab and merged aks-tui's pod lists in; a version-1
/// file from either program is ignored, and the refresh behind the first
/// frame rewrites it.
const VERSION: u32 = 2;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Snapshot {
    version: u32,
    pub tabs: BTreeMap<String, CachedTab>,
}

/// One tab's last read.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CachedTab {
    Scope {
        read_at: Timestamp,
        pods: Vec<Pod>,
    },
    Secrets {
        read_at: Timestamp,
        vaults: Vec<Vault>,
        secrets: Vec<SecretRow>,
    },
    Registries {
        read_at: Timestamp,
        registries: Vec<Registry>,
        repositories: Vec<Repository>,
    },
}

impl Snapshot {
    #[must_use]
    pub fn new(tabs: BTreeMap<String, CachedTab>) -> Self {
        Self {
            version: VERSION,
            tabs,
        }
    }
}

impl CachedTab {
    #[must_use]
    pub const fn read_at(&self) -> Timestamp {
        match self {
            Self::Scope { read_at, .. }
            | Self::Secrets { read_at, .. }
            | Self::Registries { read_at, .. } => *read_at,
        }
    }

    /// The two fixed tabs from one Azure read, keyed for the map. The
    /// inventory is split between them; `AzureStore::from_cache` puts it
    /// back together.
    #[must_use]
    pub fn azure(
        read_at: Timestamp,
        inventory: Inventory,
        secrets: Vec<SecretRow>,
        repositories: Vec<Repository>,
    ) -> [(String, Self); 2] {
        [
            (
                SECRETS_TAB.to_owned(),
                Self::Secrets {
                    read_at,
                    vaults: inventory.vaults,
                    secrets,
                },
            ),
            (
                REGISTRIES_TAB.to_owned(),
                Self::Registries {
                    read_at,
                    registries: inventory.registries,
                    repositories,
                },
            ),
        ]
    }
}

/// The snapshot at `path`, or nothing at all. A file this build cannot read
/// — another version, half-written, hand-edited — is not an error worth
/// showing anyone: the refresh already running behind the first frame
/// replaces it.
#[must_use]
pub fn load(path: &Path) -> Option<Snapshot> {
    let source = std::fs::read_to_string(path).ok()?;
    let snapshot: Snapshot = serde_json::from_str(&source).ok()?;
    (snapshot.version == VERSION).then_some(snapshot)
}

/// Writes the snapshot atomically: a temporary file beside the real one, then
/// a rename. A start that races a save reads one file or the other, never
/// half of one.
pub fn save(path: &Path, snapshot: &Snapshot) -> Result<()> {
    // Serialised whole and written once: a bare temp file is unbuffered, and
    // `to_writer` on one is a syscall per token — forty thousand rows took a
    // second and a half of the run loop that way.
    let bytes = serde_json::to_vec(snapshot).context("failed to write the cache")?;
    write_private(path, &bytes)
}

/// The one way a file of this program's own reaches disk: a temporary file
/// in the same directory, `0600` on unix, then a rename over `path`. The
/// session goes through here too, so both files carry the same permissions
/// and the same all-or-nothing write.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(directory)
        .with_context(|| format!("failed to make {}", directory.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(directory)
        .with_context(|| format!("failed to write in {}", directory.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.flush()
        .with_context(|| format!("failed to write {}", path.display()))?;
    restrict(file.as_file())?;
    file.persist(path)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn restrict(file: &std::fs::File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .context("failed to restrict the file")
}

#[cfg(not(unix))]
fn restrict(_file: &std::fs::File) -> Result<()> {
    // Windows inherits the user's profile ACL, which is already private.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::Vault;
    use crate::timestamp::ts;

    fn snapshot() -> Snapshot {
        let mut tabs: BTreeMap<String, CachedTab> = CachedTab::azure(
            ts("2026-09-11T20:00:00Z"),
            Inventory {
                vaults: vec![Vault {
                    id: "/id".into(),
                    name: "kv-prod".into(),
                    resource_group: "rg".into(),
                    location: "eastus".into(),
                    uri: "https://kv-prod.vault.azure.net/".into(),
                }],
                registries: Vec::new(),
            },
            vec![SecretRow {
                vault: "kv-prod".into(),
                name: "db-password".into(),
                enabled: true,
                created: Some(ts("2026-03-01T00:00:00Z")),
                updated: None,
                expires: None,
                not_before: None,
                content_type: Some("text/plain".into()),
                tags: vec![("env".into(), "prod".into())],
                managed: false,
            }],
            Vec::new(),
        )
        .into_iter()
        .collect();
        tabs.insert(
            "qa/dev".into(),
            CachedTab::Scope {
                read_at: ts("2026-09-11T20:00:05Z"),
                pods: vec![crate::kube::tests::pod(
                    "qa",
                    "dev",
                    "orders-api-7d9f5b-abc12",
                    "Running",
                )],
            },
        );
        Snapshot::new(tabs)
    }

    #[test]
    fn a_snapshot_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("cache.json");
        save(&path, &snapshot()).unwrap();

        let read = load(&path).unwrap();
        assert_eq!(
            read.tabs.keys().collect::<Vec<_>>(),
            ["qa/dev", "registries", "secrets"]
        );
        match &read.tabs["secrets"] {
            CachedTab::Secrets {
                read_at,
                vaults,
                secrets,
            } => {
                assert_eq!(*read_at, ts("2026-09-11T20:00:00Z"));
                assert_eq!(vaults[0].name, "kv-prod");
                assert_eq!(secrets[0].name, "db-password");
                assert_eq!(secrets[0].tags, [("env".to_owned(), "prod".to_owned())]);
            }
            other => panic!("expected the secrets tab, got {other:?}"),
        }
        assert!(matches!(
            &read.tabs["registries"],
            CachedTab::Registries { registries, repositories, .. }
                if registries.is_empty() && repositories.is_empty()
        ));
        match &read.tabs["qa/dev"] {
            CachedTab::Scope { read_at, pods } => {
                assert_eq!(*read_at, ts("2026-09-11T20:00:05Z"));
                assert_eq!(pods[0].key.name, "orders-api-7d9f5b-abc12");
            }
            other => panic!("expected a scope tab, got {other:?}"),
        }
    }

    #[test]
    fn a_file_this_build_cannot_read_is_simply_not_there() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        assert!(load(&path).is_none(), "a missing file");

        std::fs::write(&path, "{ not json").unwrap();
        assert!(load(&path).is_none(), "a half-written file");

        let mut written: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&snapshot()).unwrap()).unwrap();
        written["version"] = serde_json::json!(VERSION + 1);
        std::fs::write(&path, written.to_string()).unwrap();
        assert!(load(&path).is_none(), "a version this build does not know");
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_readable_only_by_the_user_who_wrote_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        save(&path, &snapshot()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "secret names are not world-readable");
    }

    #[test]
    fn nothing_in_the_written_file_could_be_a_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        save(&path, &snapshot()).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("db-password"), "the name is the point");
        assert!(written.contains("orders-api-7d9f5b-abc12"), "so is a pod's");
        // The only `"value"`-shaped key a vault answer carries, and the key a
        // configmap or a cluster secret keeps its contents under. Neither can
        // reach here — the types they would come in are not serialisable —
        // and this is the check that says so from the outside.
        assert!(!written.contains("\"value\""), "{written}");
        assert!(!written.contains("\"data\""), "{written}");
    }
}
