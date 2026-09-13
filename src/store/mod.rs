//! What the run holds in memory: the Azure half today, the AKS scopes once
//! they land beside it.

pub mod azure;

pub use azure::{Applied, AzureStore, problem_line};

use crate::cache::Snapshot;

#[derive(Default)]
pub struct Store {
    pub azure: AzureStore,
}

impl Store {
    /// The store as the last run left it.
    #[must_use]
    pub fn from_cache(snapshot: Snapshot) -> Self {
        Self {
            azure: AzureStore::from_cache(snapshot),
        }
    }

    /// What the next save writes.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        self.azure.snapshot()
    }
}
