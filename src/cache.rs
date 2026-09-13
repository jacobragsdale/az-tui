//! The snapshot the next start paints from, before any network call.
//!
//! Names and metadata only. **No value ever reaches this file**, and it
//! cannot: the newtype a value lives in derives no `Serialize`, and this
//! module cannot so much as name that type — a word-boundary grep for it over
//! this file comes back empty. `SecretRow` is a different word: it carries a
//! secret's name, not its value.
//!
//! Names alone are mildly sensitive — `stripe-prod-key` says a good deal — so
//! the file is written `0600` on unix.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::azure::{Inventory, Repository, SecretRow};
use crate::timestamp::Timestamp;

/// The schema this build writes. A file of any other version is ignored
/// rather than migrated: it is a cache, and the next refresh rewrites it.
const VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Snapshot {
    version: u32,
    pub read_at: Timestamp,
    #[serde(flatten)]
    pub inventory: Inventory,
    pub secrets: Vec<SecretRow>,
    pub repositories: Vec<Repository>,
}

impl Snapshot {
    #[must_use]
    pub fn new(
        read_at: Timestamp,
        inventory: Inventory,
        secrets: Vec<SecretRow>,
        repositories: Vec<Repository>,
    ) -> Self {
        Self {
            version: VERSION,
            read_at,
            inventory,
            secrets,
            repositories,
        }
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
        Snapshot::new(
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
    }

    #[test]
    fn a_snapshot_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("cache.json");
        save(&path, &snapshot()).unwrap();

        let read = load(&path).unwrap();
        assert_eq!(read.read_at, ts("2026-09-11T20:00:00Z"));
        assert_eq!(read.inventory.vaults[0].name, "kv-prod");
        assert_eq!(read.secrets[0].name, "db-password");
        assert_eq!(
            read.secrets[0].tags,
            [("env".to_owned(), "prod".to_owned())]
        );
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
        written["version"] = serde_json::json!(2);
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
        // The only `"value"`-shaped key a vault answer carries. It cannot
        // reach here — the type it would come in is not serialisable — and
        // this is the check that says so from the outside.
        assert!(!written.contains("\"value\""), "{written}");
    }
}
