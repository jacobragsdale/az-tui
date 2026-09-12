//! The one background thread. Requests in, events out, and the UI never
//! waits on either.
//!
//! Every HTTPS call and every `az` shell-out in the program happens here. The
//! run loop drains [`Worker::try_recv`] each turn and redraws; a vault that
//! will not answer is one event carrying one string, not a frozen table.
//!
//! The loop's one rule beyond that: **somebody is waiting on a detail.** A
//! refresh walking three hundred repositories checks the request channel
//! between every call, so a `v` pressed halfway through is answered in one
//! round trip rather than after the walk.

use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;

use crate::azure::transport::{Client, is_signed_out};
use crate::azure::{
    Inventory, Manifest, Registry, Repository, Secret, SecretRow, SecretVersion, Tag, Vault, acr,
    graph, vault,
};
use crate::config::Azure;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Request {
    /// The inventory, then every vault, then every registry, then the
    /// per-repository fill.
    Refresh,
    Versions {
        vault: String,
        name: String,
    },
    Value {
        vault: String,
        name: String,
        version: Option<String>,
    },
    Tags {
        registry: String,
        repo: String,
    },
    Manifest {
        registry: String,
        repo: String,
        digest: String,
    },
    Stop,
}

/// What the worker found. Errors are `String`s because they cross a thread
/// and the UI only prints them; they are formatted with `{error:#}` at the
/// source so the chain of causes survives the crossing.
#[derive(Debug)]
pub enum Event {
    Inventory(Result<Inventory, String>),
    Secrets {
        vault: String,
        result: Result<Vec<SecretRow>, String>,
    },
    /// Names only; every count is `None` until a [`Event::Repository`] fills
    /// it in.
    Repositories {
        registry: String,
        result: Result<Vec<Repository>, String>,
    },
    /// One repository's attributes, arriving one at a time behind the names.
    Repository {
        registry: String,
        repository: Repository,
    },
    Versions {
        vault: String,
        name: String,
        result: Result<Vec<SecretVersion>, String>,
    },
    /// The one event that carries a value. The worker keeps no copy: it is
    /// built, sent, and gone from this thread.
    Value {
        vault: String,
        name: String,
        result: Result<(Secret, String), String>,
    },
    Tags {
        registry: String,
        repo: String,
        result: Result<Vec<Tag>, String>,
    },
    Manifest {
        registry: String,
        repo: String,
        digest: String,
        result: Result<Manifest, String>,
    },
    /// What the status bar says while a refresh runs.
    Progress(String),
    /// The refresh has finished, whether or not everything answered.
    Idle,
}

pub struct Worker {
    requests: Sender<Request>,
    events: Receiver<Event>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    /// Starts the thread.
    ///
    /// The client is built here but mints nothing until the thread asks it
    /// to, so a missing `az` never delays the first frame. `known` is the
    /// inventory the cache was painted from: without it a `v` pressed on the
    /// first frame would be told the vault is not in the subscription, when
    /// really the refresh behind the frame has not finished yet.
    #[must_use]
    pub fn start(azure: Azure, client: Client, known: Inventory) -> Self {
        let (requests, inbox) = channel();
        let (outbox, events) = channel();
        let handle = std::thread::Builder::new()
            .name("az-tui-worker".to_owned())
            .spawn(move || {
                Loop {
                    azure,
                    client,
                    inbox,
                    outbox,
                    vaults: known.vaults,
                    registries: known.registries,
                }
                .run();
            })
            .expect("the worker thread could not be started");
        Self {
            requests,
            events,
            handle: Some(handle),
        }
    }

    /// Asks for something. A closed channel means the thread is already gone,
    /// which is not worth reporting: the run is ending.
    pub fn send(&self, request: Request) {
        let _ = self.requests.send(request);
    }

    /// The next event, or nothing. Never blocks.
    pub fn try_recv(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    /// A handle that can ask for something from somewhere else. Only the
    /// tests use it, to put a request in the queue at an exact moment.
    #[cfg(test)]
    pub fn sender(&self) -> Sender<Request> {
        self.requests.clone()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Stop);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// What runs on the thread.
struct Loop {
    azure: Azure,
    client: Client,
    inbox: Receiver<Request>,
    outbox: Sender<Event>,
    /// What the last refresh found, so a detail request knows which host to
    /// ask. The screens name a vault or a registry; only this thread knows
    /// where either lives.
    vaults: Vec<Vault>,
    registries: Vec<Registry>,
}

/// What the loop decided to do next, once a batch of requests has been
/// collapsed.
#[derive(Default)]
struct Batch {
    refresh: bool,
    stop: bool,
}

impl Loop {
    fn run(mut self) {
        loop {
            // One blocking wait, then everything else that had piled up
            // behind it.
            let Ok(first) = self.inbox.recv() else { return };
            let mut batch = Batch::default();
            self.take(first, &mut batch);
            while let Ok(queued) = self.inbox.try_recv() {
                self.take(queued, &mut batch);
            }
            if batch.stop {
                return;
            }
            if batch.refresh && !self.refresh() {
                return;
            }
        }
    }

    /// Serves a detail request now, or notes that a refresh or a stop is
    /// wanted. Two queued refreshes are one refresh; a stop anywhere in the
    /// batch wins.
    fn take(&mut self, request: Request, batch: &mut Batch) {
        match request {
            Request::Stop => batch.stop = true,
            Request::Refresh => batch.refresh = true,
            detail => self.serve(detail),
        }
    }

    /// Drains whatever has arrived without blocking, serving details at once.
    /// Returns false when the loop should stop — a `Stop` during a refresh,
    /// or a UI that has gone away.
    fn pump(&mut self, batch: &mut Batch) -> bool {
        loop {
            match self.inbox.try_recv() {
                Ok(Request::Stop) => return false,
                Ok(Request::Refresh) => batch.refresh = true,
                Ok(detail) => self.serve(detail),
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => return false,
            }
        }
    }

    fn send(&self, event: Event) -> bool {
        self.outbox.send(event).is_ok()
    }

    fn progress(&self, said: impl Into<String>) -> bool {
        self.send(Event::Progress(said.into()))
    }

    /// One pass over everything. Returns false when the loop should stop.
    fn refresh(&mut self) -> bool {
        let mut batch = Batch::default();
        self.progress("reading the subscription…");
        let inventory = match graph::inventory(&self.client, &self.azure) {
            Ok(inventory) => inventory,
            Err(error) => {
                self.send(Event::Inventory(Err(said(error))));
                return self.send(Event::Idle);
            }
        };
        let vaults = inventory.vaults.clone();
        let registries = inventory.registries.clone();
        self.vaults = vaults.clone();
        self.registries = registries.clone();
        if !self.send(Event::Inventory(Ok(inventory))) {
            return false;
        }

        for (index, vault) in vaults.iter().enumerate() {
            if !self.pump(&mut batch) {
                return false;
            }
            self.progress(format!(
                "reading {} ({}/{})…",
                vault.name,
                index + 1,
                vaults.len()
            ));
            if !self.read_vault(vault) {
                return false;
            }
        }

        let mut catalogs: Vec<(Registry, Vec<String>)> = Vec::new();
        for (index, registry) in registries.iter().enumerate() {
            if !self.pump(&mut batch) {
                return false;
            }
            self.progress(format!(
                "reading {} ({}/{})…",
                registry.name,
                index + 1,
                registries.len()
            ));
            match acr::repositories(&self.client, registry) {
                Ok(names) => {
                    let rows = names
                        .iter()
                        .map(|name| Repository {
                            registry: registry.name.clone(),
                            name: name.clone(),
                            tag_count: None,
                            manifest_count: None,
                            created: None,
                            updated: None,
                        })
                        .collect();
                    if !self.send(Event::Repositories {
                        registry: registry.name.clone(),
                        result: Ok(rows),
                    }) {
                        return false;
                    }
                    catalogs.push((registry.clone(), names));
                }
                Err(error) => {
                    if is_signed_out(&error) {
                        return self.stop_signed_out();
                    }
                    if !self.send(Event::Repositories {
                        registry: registry.name.clone(),
                        result: Err(format!("{error:#}")),
                    }) {
                        return false;
                    }
                }
            }
        }

        // The fill: one call per repository, behind the names that are
        // already on screen, yielding to anything anyone is waiting on.
        let total: usize = catalogs.iter().map(|(_, names)| names.len()).sum();
        let mut done = 0_usize;
        for (registry, names) in &catalogs {
            for name in names {
                if !self.pump(&mut batch) {
                    return false;
                }
                done += 1;
                if done.is_multiple_of(20) {
                    self.progress(format!("filling repository details ({done}/{total})…"));
                }
                if let Ok(repository) = acr::attributes(&self.client, registry, name)
                    && !self.send(Event::Repository {
                        registry: registry.name.clone(),
                        repository,
                    })
                {
                    return false;
                }
            }
        }

        if !self.send(Event::Idle) {
            return false;
        }
        // A refresh asked for during this one runs now rather than waiting
        // for the next keystroke.
        if batch.refresh { self.refresh() } else { true }
    }

    fn read_vault(&mut self, vault: &Vault) -> bool {
        match vault::secrets(&self.client, vault) {
            Ok(rows) => self.send(Event::Secrets {
                vault: vault.name.clone(),
                result: Ok(rows),
            }),
            Err(error) if is_signed_out(&error) => self.stop_signed_out(),
            Err(error) => self.send(Event::Secrets {
                vault: vault.name.clone(),
                result: Err(format!("{error:#}")),
            }),
        }
    }

    /// A signed-out login is not one vault's problem, so the refresh stops
    /// and says so once.
    fn stop_signed_out(&self) -> bool {
        self.send(Event::Inventory(Err(
            "not signed in — run `az login`".to_owned()
        ))) && self.send(Event::Idle)
    }

    /// One detail, read now. Nothing here is retried and nothing is cached on
    /// this thread — least of all a value.
    fn serve(&mut self, request: Request) {
        match request {
            Request::Versions {
                vault: name,
                name: secret,
            } => {
                let result = self.with_vault(&name, |client, vault| {
                    vault::versions(client, vault, &secret)
                });
                self.send(Event::Versions {
                    vault: name,
                    name: secret,
                    result,
                });
            }
            Request::Value {
                vault: name,
                name: secret,
                version,
            } => {
                let result = self.with_vault(&name, |client, vault| {
                    vault::value(client, vault, &secret, version.as_deref())
                });
                self.send(Event::Value {
                    vault: name,
                    name: secret,
                    result,
                });
            }
            Request::Tags {
                registry: name,
                repo,
            } => {
                let result = self
                    .with_registry(&name, |client, registry| acr::tags(client, registry, &repo));
                self.send(Event::Tags {
                    registry: name,
                    repo,
                    result,
                });
            }
            Request::Manifest {
                registry: name,
                repo,
                digest,
            } => {
                let result = self.with_registry(&name, |client, registry| {
                    acr::manifest(client, registry, &repo, &digest)
                });
                self.send(Event::Manifest {
                    registry: name,
                    repo,
                    digest,
                    result,
                });
            }
            Request::Refresh | Request::Stop => {}
        }
    }

    /// Why a name could not be turned into a host to ask. There are two
    /// reasons and they want different words: the subscription has not been
    /// read yet, or it has and the thing is gone.
    fn cannot_place(&self, name: &str) -> String {
        if self.vaults.is_empty() && self.registries.is_empty() {
            format!("{name}: the subscription has not been read yet")
        } else {
            format!("{name} is no longer in the subscription")
        }
    }

    fn with_vault<T>(
        &self,
        name: &str,
        read: impl FnOnce(&Client, &Vault) -> anyhow::Result<T>,
    ) -> Result<T, String> {
        let Some(vault) = self.vaults.iter().find(|vault| vault.name == name) else {
            return Err(self.cannot_place(name));
        };
        read(&self.client, vault).map_err(said)
    }

    fn with_registry<T>(
        &self,
        name: &str,
        read: impl FnOnce(&Client, &Registry) -> anyhow::Result<T>,
    ) -> Result<T, String> {
        let Some(registry) = self
            .registries
            .iter()
            .find(|registry| registry.name == name)
        else {
            return Err(format!("{name} is no longer in the subscription"));
        };
        read(&self.client, registry).map_err(said)
    }
}

/// What the screen shows for one failure.
///
/// A signed-out login is the one refusal worth rewording: the CLI answers it
/// with five lines of its own stack, and what a person needs is the two words
/// that fix it. Every detail request goes through here, so the Value line and
/// the versions list say the same thing the status bar does.
fn said(error: anyhow::Error) -> String {
    if is_signed_out(&error) {
        return "not signed in — run `az login`".to_owned();
    }
    format!("{error:#}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::transport::fake::{Answer, client as fake_client};
    use serde_json::json;
    use std::time::{Duration, Instant};

    /// Reads events until one of them is named in `until`, or five seconds
    /// pass. Every assertion below is about an order, so the wait is for the
    /// thread rather than for the network — there is none.
    fn pump_until(worker: &Worker, seen: &mut Vec<String>, until: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match worker.try_recv() {
                Some(event) => {
                    let held = name(&event);
                    let done = held == until;
                    if held != "progress" {
                        seen.push(held);
                    }
                    if done {
                        return;
                    }
                }
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
    }

    /// Everything a refresh reported, in order, without the progress lines.
    fn until_idle(worker: &Worker) -> Vec<String> {
        let mut seen = Vec::new();
        pump_until(worker, &mut seen, "idle");
        seen
    }

    fn name(event: &Event) -> String {
        match event {
            Event::Inventory(Ok(_)) => "inventory".into(),
            Event::Inventory(Err(message)) => format!("inventory-err({message})"),
            Event::Secrets { vault, result } => {
                format!(
                    "secrets({vault},{})",
                    if result.is_ok() { "ok" } else { "err" }
                )
            }
            Event::Repositories { registry, result } => format!(
                "repositories({registry},{})",
                if result.is_ok() { "ok" } else { "err" }
            ),
            Event::Repository { repository, .. } => format!("repository({})", repository.name),
            Event::Versions { name, .. } => format!("versions({name})"),
            Event::Value { name, .. } => format!("value({name})"),
            Event::Tags { repo, .. } => format!("tags({repo})"),
            Event::Manifest { repo, .. } => format!("manifest({repo})"),
            Event::Progress(_) => "progress".into(),
            Event::Idle => "idle".into(),
        }
    }

    fn inventory_answer() -> Answer {
        Answer::json(json!({
            "data": [
                {
                    "id": "/vaults/kv-a", "name": "kv-a", "type": "microsoft.keyvault/vaults",
                    "subscriptionId": "s", "resourceGroup": "rg", "location": "eastus",
                    "sku": "standard", "vaultUri": "https://kv-a.vault.azure.net/", "loginServer": "",
                },
                {
                    "id": "/vaults/kv-b", "name": "kv-b", "type": "microsoft.keyvault/vaults",
                    "subscriptionId": "s", "resourceGroup": "rg", "location": "eastus",
                    "sku": "standard", "vaultUri": "https://kv-b.vault.azure.net/", "loginServer": "",
                },
                {
                    "id": "/registries/acra", "name": "acra",
                    "type": "microsoft.containerregistry/registries",
                    "subscriptionId": "s", "resourceGroup": "rg", "location": "eastus",
                    "sku": "Premium", "loginServer": "acra.azurecr.io", "vaultUri": "",
                },
            ],
        }))
    }

    fn secrets_answer(name: &str) -> Answer {
        Answer::json(json!({
            "value": [{ "id": format!("https://x.vault.azure.net/secrets/{name}"), "attributes": { "enabled": true } }],
        }))
    }

    #[test]
    fn a_refresh_reports_each_piece_as_it_lands_and_then_goes_idle() {
        let (client, _, _) = fake_client([
            inventory_answer(),
            secrets_answer("one"),
            secrets_answer("two"),
            // The registry's two token posts, then the catalog.
            Answer::json(json!({ "refresh_token": "r" })),
            Answer::json(json!({ "access_token": "a" })),
            Answer::json(json!({ "repositories": ["api", "web"] })),
            // The fill: one token per repository scope, one attributes call.
            Answer::json(json!({ "access_token": "api-token" })),
            Answer::json(json!({ "imageName": "api", "tagCount": 3 })),
            Answer::json(json!({ "access_token": "web-token" })),
            Answer::json(json!({ "imageName": "web", "tagCount": 1 })),
        ]);
        let worker = Worker::start(Azure::default(), client, Inventory::default());
        worker.send(Request::Refresh);
        assert_eq!(
            until_idle(&worker),
            [
                "inventory",
                "secrets(kv-a,ok)",
                "secrets(kv-b,ok)",
                "repositories(acra,ok)",
                "repository(api)",
                "repository(web)",
                "idle",
            ]
        );
    }

    #[test]
    fn a_vault_that_refuses_does_not_stop_the_next_one() {
        let (client, _, _) = fake_client([
            inventory_answer(),
            Answer::status(403, r#"{"error":{"code":"Forbidden","message":"no"}}"#),
            secrets_answer("two"),
            Answer::json(json!({ "refresh_token": "r" })),
            Answer::json(json!({ "access_token": "a" })),
            Answer::json(json!({ "repositories": [] })),
        ]);
        let worker = Worker::start(Azure::default(), client, Inventory::default());
        worker.send(Request::Refresh);
        let said = until_idle(&worker);
        assert!(said.contains(&"secrets(kv-a,err)".to_owned()), "{said:?}");
        assert!(said.contains(&"secrets(kv-b,ok)".to_owned()), "{said:?}");
        assert!(said.contains(&"idle".to_owned()), "{said:?}");
    }

    #[test]
    fn a_signed_out_login_stops_the_refresh_and_says_so_once() {
        let (client, transport, _) =
            fake_client([Answer::status(401, "{}"), Answer::status(401, "{}")]);
        let worker = Worker::start(Azure::default(), client, Inventory::default());
        worker.send(Request::Refresh);
        assert_eq!(
            until_idle(&worker),
            ["inventory-err(not signed in — run `az login`)", "idle"]
        );
        assert_eq!(transport.sent().len(), 2, "no vault was even asked");
    }

    #[test]
    fn a_value_asked_for_mid_refresh_is_answered_before_the_rest_of_the_fill() {
        let (client, transport, _) = fake_client([
            inventory_answer(),
            secrets_answer("one"),
            secrets_answer("two"),
            Answer::json(json!({ "refresh_token": "r" })),
            Answer::json(json!({ "access_token": "a" })),
            Answer::json(json!({ "repositories": ["api", "web"] })),
            // The Value request is in the queue by now, and is served first.
            Answer::json(
                json!({ "value": "s3cr3t", "id": "https://kv-a.vault.azure.net/secrets/one/v1" }),
            ),
            Answer::json(json!({ "access_token": "api-token" })),
            Answer::json(json!({ "imageName": "api" })),
            Answer::json(json!({ "access_token": "web-token" })),
            Answer::json(json!({ "imageName": "web" })),
        ]);
        let worker = Worker::start(Azure::default(), client, Inventory::default());
        // The ask lands while the catalog call is still in flight, which is
        // the moment this test is about: the fill has not started, and the
        // refresh has to notice the request before it does. Sending it from
        // the transport rather than after a sleep makes that exact.
        let asking = worker.sender();
        transport.watch(move |request| {
            if request.url.contains("/_catalog") {
                let _ = asking.send(Request::Value {
                    vault: "kv-a".to_owned(),
                    name: "one".to_owned(),
                    version: None,
                });
            }
        });
        worker.send(Request::Refresh);
        let mut seen = Vec::new();
        pump_until(&worker, &mut seen, "idle");
        let value_at = seen
            .iter()
            .position(|held| held == "value(one)")
            .expect("a value");
        let last_fill = seen
            .iter()
            .rposition(|held| held.starts_with("repository("))
            .expect("the fill");
        assert!(
            value_at < last_fill,
            "the copy did not wait for the walk: {seen:?}"
        );
    }

    #[test]
    fn two_queued_refreshes_run_once() {
        let (client, transport, _) = fake_client([
            inventory_answer(),
            secrets_answer("one"),
            secrets_answer("two"),
            Answer::json(json!({ "refresh_token": "r" })),
            Answer::json(json!({ "access_token": "a" })),
            Answer::json(json!({ "repositories": [] })),
        ]);
        let worker = Worker::start(Azure::default(), client, Inventory::default());
        worker.send(Request::Refresh);
        worker.send(Request::Refresh);
        worker.send(Request::Refresh);
        assert_eq!(
            until_idle(&worker)
                .iter()
                .filter(|held| *held == "inventory")
                .count(),
            1,
            "three asks, one read"
        );
        assert_eq!(transport.remaining(), 0);
    }

    #[test]
    fn a_detail_for_something_the_inventory_does_not_hold_is_an_answer_not_a_hang() {
        let (client, _, _) = fake_client([]);
        let worker = Worker::start(Azure::default(), client, Inventory::default());
        worker.send(Request::Versions {
            vault: "kv-gone".into(),
            name: "x".into(),
        });
        let mut seen = Vec::new();
        pump_until(&worker, &mut seen, "versions(x)");
        assert_eq!(seen, ["versions(x)"], "an answer, not a hang");
    }

    #[test]
    fn a_detail_that_fails_for_want_of_a_login_says_the_two_words_that_fix_it() {
        let (client, _, _) = fake_client([Answer::status(401, "{}"), Answer::status(401, "{}")]);
        let known = Inventory {
            vaults: vec![crate::azure::Vault {
                id: "/vaults/kv-a".into(),
                name: "kv-a".into(),
                subscription_id: "s".into(),
                resource_group: "rg".into(),
                location: "eastus".into(),
                sku: "standard".into(),
                uri: "https://kv-a.vault.azure.net/".into(),
            }],
            registries: Vec::new(),
        };
        let worker = Worker::start(Azure::default(), client, known);
        worker.send(Request::Value {
            vault: "kv-a".into(),
            name: "one".into(),
            version: None,
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let message = loop {
            if let Some(Event::Value {
                result: Err(message),
                ..
            }) = worker.try_recv()
            {
                break message;
            }
            assert!(Instant::now() < deadline, "no answer");
            std::thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(message, "not signed in — run `az login`");
    }

    #[test]
    fn a_detail_asked_for_before_the_first_refresh_reaches_the_host_the_cache_named() {
        // The shape of a cache-first start with no login: the app knows the
        // vault, the worker has read nothing yet, and `v` must still go out.
        let (client, transport, _) = fake_client([Answer::json(json!({
            "value": "s3cr3t",
            "id": "https://kv-a.vault.azure.net/secrets/one/v1",
        }))]);
        let known = Inventory {
            vaults: vec![crate::azure::Vault {
                id: "/vaults/kv-a".into(),
                name: "kv-a".into(),
                subscription_id: "s".into(),
                resource_group: "rg".into(),
                location: "eastus".into(),
                sku: "standard".into(),
                uri: "https://kv-a.vault.azure.net/".into(),
            }],
            registries: Vec::new(),
        };
        let worker = Worker::start(Azure::default(), client, known);
        worker.send(Request::Value {
            vault: "kv-a".into(),
            name: "one".into(),
            version: None,
        });
        let mut seen = Vec::new();
        pump_until(&worker, &mut seen, "value(one)");
        assert_eq!(seen, ["value(one)"]);
        assert_eq!(
            transport.urls(),
            ["https://kv-a.vault.azure.net/secrets/one?api-version=7.4"]
        );
    }

    #[test]
    fn a_name_the_worker_cannot_place_says_which_of_the_two_reasons_it_is() {
        let (client, _, _) = fake_client([]);
        let worker = Worker::start(Azure::default(), client, Inventory::default());
        worker.send(Request::Versions {
            vault: "kv-gone".into(),
            name: "x".into(),
        });
        let mut seen = Vec::new();
        pump_until(&worker, &mut seen, "versions(x)");
        assert_eq!(seen, ["versions(x)"]);
    }

    #[test]
    fn dropping_the_worker_stops_the_thread() {
        let (client, _, _) = fake_client([]);
        let worker = Worker::start(Azure::default(), client, Inventory::default());
        worker.send(Request::Stop);
        // `Drop` sends another Stop and joins; a thread that ignored the
        // first would hang this test rather than fail it.
        drop(worker);
    }
}
