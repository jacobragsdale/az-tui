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

    /// The first problem, for the status bar. `?` lists them all.
    #[must_use]
    pub fn first_problem(&self) -> Option<&(String, String)> {
        self.problems.first()
    }

    /// The vault a row names, as the inventory describes it.
    #[must_use]
    pub fn vault(&self, name: &str) -> Option<&crate::azure::Vault> {
        self.inventory.vaults.iter().find(|held| held.name == name)
    }

    /// The registry a row names, as the inventory describes it.
    #[must_use]
    pub fn registry(&self, name: &str) -> Option<&crate::azure::Registry> {
        self.inventory
            .registries
            .iter()
            .find(|held| held.name == name)
    }

    pub fn apply(&mut self, event: Event) -> Applied {
        match event {
            Event::Inventory(Ok(inventory)) => {
                self.refreshing = true;
                self.read_something = true;
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
                // One inventory problem at a time: a TUI left open overnight
                // while signed out would otherwise list the same line once
                // per refresh under `?`.
                self.problems.retain(|(who, _)| !who.is_empty());
                self.problems.push((String::new(), message));
                Applied::Status
            }
            Event::Progress(said) => {
                // The first word of a refresh arrives before the inventory
                // does, and the spinner should turn from that word on.
                self.refreshing = true;
                self.progress = Some(said);
                Applied::Status
            }
            Event::Secrets { vault, result } => match result {
                Ok(rows) => {
                    self.stale.remove(&vault);
                    let order = &self.inventory.vaults;
                    replace(
                        &mut self.secrets,
                        &vault,
                        rows,
                        |row| &row.vault,
                        |name| order.iter().position(|held| held.name == name),
                    );
                    Applied::Secrets
                }
                Err(message) => {
                    self.problem(vault, message);
                    Applied::Status
                }
            },
            Event::Repositories { registry, result } => match result {
                Ok(rows) => {
                    self.stale.remove(&registry);
                    let order = &self.inventory.registries;
                    replace(
                        &mut self.repositories,
                        &registry,
                        rows,
                        |row| &row.registry,
                        |name| order.iter().position(|held| held.name == name),
                    );
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

    /// A vault or registry that would not answer: its rows stand, marked
    /// stale, and the footer says why.
    fn problem(&mut self, who: String, message: String) {
        self.stale.insert(who.clone());
        self.problems.push((who, message));
    }
}

/// One owner's rows, replaced in place so the other owners keep their order.
/// A refresh that reorders nothing leaves the cursor where it was.
///
/// An owner not there yet goes where `rank` — its place in the inventory —
/// says, not at the end: the vaults answer side by side and in any order,
/// and the cache and the shell listing should read the same whichever
/// finished first. An owner the inventory does not name goes last.
fn replace<T>(
    rows: &mut Vec<T>,
    owner: &str,
    new: Vec<T>,
    owner_of: impl Fn(&T) -> &str,
    rank: impl Fn(&str) -> Option<usize>,
) {
    let at = rows
        .iter()
        .position(|row| owner_of(row) == owner)
        .unwrap_or_else(|| {
            let mine = rank(owner).unwrap_or(usize::MAX);
            rows.iter()
                .position(|row| rank(owner_of(row)).unwrap_or(usize::MAX) > mine)
                .unwrap_or(rows.len())
        });
    rows.retain(|row| owner_of(row) != owner);
    let at = at.min(rows.len());
    rows.splice(at..at, new);
}

/// What the footer and the help say for one problem: who, then what, or just
/// what when it is nobody's in particular.
#[must_use]
pub fn problem_line((who, message): &(String, String)) -> String {
    if who.is_empty() {
        message.clone()
    } else {
        format!("{who}: {message}")
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
            resource_group: "rg".into(),
            location: "eastus".into(),
            uri: format!("https://{name}.vault.azure.net/"),
        }
    }

    fn registry(name: &str) -> Registry {
        Registry {
            id: format!("/registries/{name}"),
            name: name.to_owned(),
            resource_group: "rg".into(),
            location: "eastus".into(),
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
    fn rows_landing_in_any_order_read_in_the_inventorys_order() {
        let mut store = Store::default();
        store.apply(Event::Inventory(Ok(Inventory {
            vaults: vec![vault("kv-a"), vault("kv-b"), vault("kv-c")],
            registries: Vec::new(),
        })));
        // kv-c answers first, then kv-a, then kv-b — the order three threads
        // might finish in — and a vault the inventory does not name at all.
        for (vault, name) in [
            ("kv-c", "three"),
            ("kv-a", "one"),
            ("kv-gone", "zero"),
            ("kv-b", "two"),
        ] {
            store.apply(Event::Secrets {
                vault: vault.into(),
                result: Ok(vec![secret(vault, name)]),
            });
        }
        assert_eq!(
            store
                .secrets
                .iter()
                .map(|row| row.vault.as_str())
                .collect::<Vec<_>>(),
            ["kv-a", "kv-b", "kv-c", "kv-gone"]
        );
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
    fn a_refresh_that_cannot_start_says_so_once_however_often_it_is_tried() {
        let mut store = stocked();
        store.apply(Event::Secrets {
            vault: "kv-a".into(),
            result: Err("boom".into()),
        });
        store.apply(Event::Inventory(Err("not signed in".into())));
        store.apply(Event::Inventory(Err("not signed in".into())));
        assert_eq!(
            store.problems.len(),
            2,
            "kv-a's problem stands; the inventory's is said once"
        );
        assert_eq!(problem_line(&store.problems[0]), "kv-a: boom");
        assert_eq!(problem_line(&store.problems[1]), "not signed in");
    }

    #[test]
    fn the_spinner_turns_from_the_first_word_of_a_refresh() {
        let mut store = Store::default();
        store.apply(Event::Progress("reading the subscription…".into()));
        assert!(store.refreshing, "before the inventory has landed");
        store.apply(Event::Inventory(Err("not signed in".into())));
        assert!(!store.refreshing);
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
