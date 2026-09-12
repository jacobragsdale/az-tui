//! The model both screens read, and the one place a worker event changes
//! anything.
//!
//! The store holds rows and problems. It does not hold a value: an
//! `Event::Value` passes straight through [`Store::apply`] and comes back out
//! as [`Applied::Value`] for the screen that asked, which is the only field
//! in the crate that keeps one.

use std::collections::HashMap;
use std::collections::HashSet;

use crate::azure::{Inventory, Manifest, Repository, Secret, SecretRow, SecretVersion, Tag};
use crate::cache::Snapshot;
use crate::timestamp::Timestamp;
use crate::worker::Event;

/// What an event changed, so a screen knows whether to re-filter, re-sort, or
/// leave its cursor alone.
#[derive(Debug)]
pub enum Applied {
    /// Nothing a screen needs to redraw for.
    Nothing,
    /// The rows of the Secrets table moved.
    Secrets,
    /// The rows of the Registries table moved.
    Repositories,
    /// Something the details pane shows arrived.
    Detail,
    /// The status bar changed and nothing else.
    Status,
    /// A value, on its way to the one screen that asked for it.
    Value {
        vault: String,
        name: String,
        result: Result<(Secret, String), String>,
    },
}

#[derive(Default)]
pub struct Store {
    pub inventory: Inventory,
    /// Every vault's secrets, concatenated in the inventory's order.
    pub secrets: Vec<SecretRow>,
    pub repositories: Vec<Repository>,
    /// Read once per row per run. The error is kept as well as the answer:
    /// a pane that only knows "not here yet" says `reading…` for ever after
    /// a vault refuses, which is the wrong half of the truth.
    pub versions: HashMap<(String, String), Result<Vec<SecretVersion>, String>>,
    pub tags: HashMap<(String, String), Result<Vec<Tag>, String>>,
    pub manifests: HashMap<(String, String, String), Result<Manifest, String>>,
    /// `(vault or registry, message)`, rebuilt over a refresh rather than
    /// appended to for ever.
    pub problems: Vec<(String, String)>,
    /// The vaults and registries whose last read failed. Their rows are the
    /// previous read's and are painted `muted` to say so.
    pub stale: HashSet<String>,
    /// When the newest complete refresh finished, or when the cache it was
    /// painted from was written.
    pub read_at: Option<Timestamp>,
    pub refreshing: bool,
    pub progress: Option<String>,
    /// Cleared at the start of each refresh and moved into `problems` and
    /// `stale` as the refresh goes, so a vault that has started answering
    /// again stops being stale at the same moment its rows land.
    refreshed: HashSet<String>,
    /// Whether the refresh now ending actually read anything. A refresh that
    /// could not even list the subscription has not made the rows on screen
    /// any newer, and must not say it has.
    read_something: bool,
}

impl Store {
    /// The store as the last run left it.
    #[must_use]
    pub fn from_cache(snapshot: Snapshot) -> Self {
        Self {
            inventory: snapshot.inventory,
            secrets: snapshot.secrets,
            repositories: snapshot.repositories,
            read_at: Some(snapshot.read_at),
            ..Self::default()
        }
    }

    /// What the next save writes. Rows a failed read left standing are in it:
    /// yesterday's names beat no names, and the status bar says how old they
    /// are.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        Snapshot::new(
            self.read_at.unwrap_or_else(Timestamp::now),
            self.inventory.clone(),
            self.secrets.clone(),
            self.repositories.clone(),
        )
    }

    /// True while nothing has ever been read — the first frame of a first
    /// run, which says so rather than showing an empty table.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty() && self.repositories.is_empty() && self.read_at.is_none()
    }

    /// The first problem, for the status bar. `?` lists them all.
    #[must_use]
    pub fn first_problem(&self) -> Option<&(String, String)> {
        self.problems.first()
    }

    pub fn apply(&mut self, event: Event) -> Applied {
        match event {
            Event::Inventory(Ok(inventory)) => {
                self.refreshing = true;
                self.read_something = true;
                self.refreshed.clear();
                self.problems.clear();
                // Rows of a vault that has gone from the subscription go with
                // it; rows of one still there are replaced as it answers.
                let vaults: HashSet<&str> = inventory
                    .vaults
                    .iter()
                    .map(|held| held.name.as_str())
                    .collect();
                let registries: HashSet<&str> = inventory
                    .registries
                    .iter()
                    .map(|held| held.name.as_str())
                    .collect();
                self.secrets
                    .retain(|row| vaults.contains(row.vault.as_str()));
                self.repositories
                    .retain(|row| registries.contains(row.registry.as_str()));
                self.stale.retain(|name| {
                    vaults.contains(name.as_str()) || registries.contains(name.as_str())
                });
                self.inventory = inventory;
                Applied::Secrets
            }
            Event::Inventory(Err(message)) => {
                self.refreshing = false;
                self.read_something = false;
                self.progress = None;
                self.problems.push((String::new(), message));
                Applied::Status
            }
            Event::Secrets { vault, result } => match result {
                Ok(rows) => {
                    self.refreshed.insert(vault.clone());
                    self.stale.remove(&vault);
                    self.replace_vault(&vault, rows);
                    Applied::Secrets
                }
                Err(message) => {
                    self.problem(vault, message);
                    Applied::Status
                }
            },
            Event::Repositories { registry, result } => match result {
                Ok(rows) => {
                    self.refreshed.insert(registry.clone());
                    self.stale.remove(&registry);
                    self.replace_registry(&registry, rows);
                    Applied::Repositories
                }
                Err(message) => {
                    self.problem(registry, message);
                    Applied::Status
                }
            },
            Event::Repository {
                registry,
                repository,
            } => {
                if let Some(held) = self
                    .repositories
                    .iter_mut()
                    .find(|held| held.registry == registry && held.name == repository.name)
                {
                    *held = repository;
                    return Applied::Repositories;
                }
                Applied::Nothing
            }
            Event::Versions {
                vault,
                name,
                result,
            } => {
                self.versions.insert((vault, name), result);
                Applied::Detail
            }
            // Straight through. Nothing here keeps it.
            Event::Value {
                vault,
                name,
                result,
            } => Applied::Value {
                vault,
                name,
                result,
            },
            Event::Tags {
                registry,
                repo,
                result,
            } => {
                self.tags.insert((registry, repo), result);
                Applied::Detail
            }
            Event::Manifest {
                registry,
                repo,
                digest,
                result,
            } => {
                self.manifests.insert((registry, repo, digest), result);
                Applied::Detail
            }
            Event::Progress(said) => {
                self.progress = Some(said);
                Applied::Status
            }
            Event::Idle => {
                self.refreshing = false;
                self.progress = None;
                if self.read_something {
                    self.read_at = Some(Timestamp::now());
                }
                Applied::Status
            }
        }
    }

    /// One vault's rows, replaced in place so the other vaults keep their
    /// order. A refresh that reorders nothing leaves the cursor where it was.
    fn replace_vault(&mut self, vault: &str, rows: Vec<SecretRow>) {
        let at = self
            .secrets
            .iter()
            .position(|row| row.vault == vault)
            .unwrap_or(self.secrets.len());
        self.secrets.retain(|row| row.vault != vault);
        let at = at.min(self.secrets.len());
        self.secrets.splice(at..at, rows);
    }

    fn replace_registry(&mut self, registry: &str, rows: Vec<Repository>) {
        let at = self
            .repositories
            .iter()
            .position(|row| row.registry == registry)
            .unwrap_or(self.repositories.len());
        self.repositories.retain(|row| row.registry != registry);
        let at = at.min(self.repositories.len());
        self.repositories.splice(at..at, rows);
    }

    /// A vault or registry that would not answer: its rows stand, marked
    /// stale, and the footer says why.
    fn problem(&mut self, who: String, message: String) {
        self.stale.insert(who.clone());
        self.problems.push((who, message));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::{Registry, Vault};
    use crate::timestamp::ts;

    fn vault(name: &str) -> Vault {
        Vault {
            id: format!("/vaults/{name}"),
            name: name.to_owned(),
            subscription_id: "s".into(),
            resource_group: "rg".into(),
            location: "eastus".into(),
            sku: "standard".into(),
            uri: format!("https://{name}.vault.azure.net/"),
        }
    }

    fn registry(name: &str) -> Registry {
        Registry {
            id: format!("/registries/{name}"),
            name: name.to_owned(),
            subscription_id: "s".into(),
            resource_group: "rg".into(),
            location: "eastus".into(),
            sku: "Premium".into(),
            login_server: format!("{name}.azurecr.io"),
        }
    }

    fn secret(vault: &str, name: &str) -> SecretRow {
        SecretRow {
            vault: vault.to_owned(),
            name: name.to_owned(),
            enabled: true,
            created: None,
            updated: None,
            expires: None,
            not_before: None,
            content_type: None,
            tags: Vec::new(),
            managed: false,
        }
    }

    fn stocked() -> Store {
        let mut store = Store::default();
        store.apply(Event::Inventory(Ok(Inventory {
            vaults: vec![vault("kv-a"), vault("kv-b")],
            registries: vec![registry("acra")],
        })));
        store.apply(Event::Secrets {
            vault: "kv-a".into(),
            result: Ok(vec![secret("kv-a", "one"), secret("kv-a", "two")]),
        });
        store.apply(Event::Secrets {
            vault: "kv-b".into(),
            result: Ok(vec![secret("kv-b", "three")]),
        });
        store
    }

    #[test]
    fn a_vaults_rows_are_replaced_in_place_and_the_others_keep_their_order() {
        let mut store = stocked();
        assert_eq!(
            store
                .secrets
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            ["one", "two", "three"]
        );

        store.apply(Event::Secrets {
            vault: "kv-a".into(),
            result: Ok(vec![secret("kv-a", "renamed")]),
        });
        assert_eq!(
            store
                .secrets
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            ["renamed", "three"],
            "a deleted secret disappears and kv-b stays put"
        );
    }

    #[test]
    fn a_vault_that_would_not_answer_keeps_its_rows_and_gains_a_problem() {
        let mut store = stocked();
        store.apply(Event::Secrets {
            vault: "kv-a".into(),
            result: Err("kv-a: no permission to read secrets".into()),
        });
        assert_eq!(store.secrets.len(), 3, "yesterday's names beat no names");
        assert!(store.stale.contains("kv-a"));
        assert!(!store.stale.contains("kv-b"));
        assert_eq!(store.first_problem().unwrap().0, "kv-a");

        // It answers on the next refresh and stops being stale.
        store.apply(Event::Secrets {
            vault: "kv-a".into(),
            result: Ok(vec![secret("kv-a", "one")]),
        });
        assert!(!store.stale.contains("kv-a"));
    }

    #[test]
    fn a_detail_that_failed_is_kept_as_a_failure_rather_than_as_nothing() {
        let mut store = stocked();
        store.apply(Event::Versions {
            vault: "kv-a".into(),
            name: "one".into(),
            result: Err("kv-a: no permission to read secrets".into()),
        });
        let held = store.versions.get(&("kv-a".to_owned(), "one".to_owned()));
        assert!(
            matches!(held, Some(Err(message)) if message.contains("no permission")),
            "otherwise the pane says `reading…` for ever: {held:?}"
        );
        assert!(
            store.problems.is_empty(),
            "one row's refusal is that row's business, not the whole tab's"
        );
    }

    #[test]
    fn a_value_passes_through_and_the_store_holds_none_afterwards() {
        let mut store = stocked();
        let applied = store.apply(Event::Value {
            vault: "kv-a".into(),
            name: "one".into(),
            result: Ok((Secret::new("hunter2"), "v1".into())),
        });
        match applied {
            Applied::Value {
                result: Ok((secret, version)),
                ..
            } => {
                assert_eq!(secret.expose(), "hunter2");
                assert_eq!(version, "v1");
            }
            other => panic!("expected a value, got {other:?}"),
        }
        // The store has no field a value could be in; this is the check that
        // says so from the outside.
        let written = serde_json::to_string(&store.snapshot()).unwrap();
        assert!(!written.contains("hunter2"), "{written}");
    }

    #[test]
    fn a_vault_that_left_the_subscription_takes_its_rows_with_it() {
        let mut store = stocked();
        store.apply(Event::Inventory(Ok(Inventory {
            vaults: vec![vault("kv-a")],
            registries: Vec::new(),
        })));
        assert_eq!(
            store
                .secrets
                .iter()
                .map(|row| row.vault.as_str())
                .collect::<Vec<_>>(),
            ["kv-a", "kv-a"]
        );
    }

    #[test]
    fn a_refresh_clears_the_problems_it_is_about_to_re_find() {
        let mut store = stocked();
        store.apply(Event::Secrets {
            vault: "kv-a".into(),
            result: Err("boom".into()),
        });
        assert_eq!(store.problems.len(), 1);
        store.apply(Event::Inventory(Ok(Inventory {
            vaults: vec![vault("kv-a"), vault("kv-b")],
            registries: Vec::new(),
        })));
        assert!(store.problems.is_empty(), "problems belong to one refresh");
    }

    #[test]
    fn an_attributes_fill_lands_on_the_row_the_catalog_left() {
        let mut store = stocked();
        store.apply(Event::Repositories {
            registry: "acra".into(),
            result: Ok(vec![Repository {
                registry: "acra".into(),
                name: "api".into(),
                tag_count: None,
                manifest_count: None,
                created: None,
                updated: None,
            }]),
        });
        store.apply(Event::Repository {
            registry: "acra".into(),
            repository: Repository {
                registry: "acra".into(),
                name: "api".into(),
                tag_count: Some(48),
                manifest_count: Some(51),
                created: None,
                updated: Some(ts("2026-09-11T18:00:00Z")),
            },
        });
        assert_eq!(store.repositories[0].tag_count, Some(48));

        // A fill for a repository that is no longer listed is dropped.
        let applied = store.apply(Event::Repository {
            registry: "acra".into(),
            repository: Repository {
                registry: "acra".into(),
                name: "gone".into(),
                tag_count: Some(1),
                manifest_count: None,
                created: None,
                updated: None,
            },
        });
        assert!(matches!(applied, Applied::Nothing));
    }

    #[test]
    fn idle_stamps_the_read_and_progress_says_what_is_happening() {
        let mut store = Store::default();
        store.apply(Event::Inventory(Ok(Inventory::default())));
        assert!(store.refreshing);
        store.apply(Event::Progress("reading kv-a (1/2)…".into()));
        assert_eq!(store.progress.as_deref(), Some("reading kv-a (1/2)…"));
        store.apply(Event::Idle);
        assert!(!store.refreshing);
        assert!(store.progress.is_none());
        assert!(store.read_at.is_some());
    }

    #[test]
    fn a_refresh_that_read_nothing_does_not_claim_the_rows_are_new() {
        let mut store = Store::default();
        store.apply(Event::Inventory(Err("not signed in".into())));
        store.apply(Event::Idle);
        assert!(
            store.read_at.is_none(),
            "a failed refresh leaves the cache's age alone"
        );

        // And an older read is not overwritten by a later failure either.
        let mut store = stocked();
        store.apply(Event::Idle);
        let first = store.read_at.expect("a read");
        store.apply(Event::Inventory(Err("not signed in".into())));
        store.apply(Event::Idle);
        assert_eq!(store.read_at, Some(first));
    }
}
