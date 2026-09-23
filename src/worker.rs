//! The background threads. Requests in, events out, and the UI never waits
//! on either.
//!
//! Every HTTPS call and every `az` shell-out in the program happens here: on
//! the one loop thread, or on the threads a refresh fans out over
//! ([`Azure::threads`] of them, this one included). The run loop drains
//! [`Worker::try_recv`] each turn and redraws; a vault that will not answer
//! is one event carrying one string, not a frozen table.
//!
//! The loop's one rule beyond that: **somebody is waiting on a detail.** A
//! refresh walking three hundred repositories pumps the request channel
//! between the items the loop thread takes itself, so a `v` pressed halfway
//! through is answered after one round trip rather than after the walk.

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;

use crate::azure::auth::Audience;
use crate::azure::transport::{Client, api_error, is_no_login, said};
use crate::azure::{
    Inventory, Manifest, Registry, Repository, Secret, SecretRow, SecretVersion, Tag, Vault, acr,
    graph, vault,
};
use crate::config::Azure;
use crate::parallel::each;

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
        /// `y` asked, so the answer is for the clipboard. Echoed back on the
        /// event so the screen routes each answer by what asked for it.
        copy: bool,
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
        copy: bool,
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
    pub fn start(azure: Azure, mut client: Client, known: Inventory) -> Self {
        let (requests, inbox) = channel();
        let (outbox, events) = channel();
        // A throttle wait is said on screen before it is taken: a worker
        // sleeping in silence for the thirty seconds Key Vault asks for reads
        // as a hang, with the spinner stuck on the last vault's name.
        let progress = outbox.clone();
        client.set_sleep(Box::new(move |wait| {
            let _ = progress.send(Event::Progress(format!(
                "throttled — waiting {} s…",
                wait.as_secs()
            )));
            std::thread::sleep(wait);
        }));
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
    /// Says stop and does not wait. The thread may be inside a thirty-second
    /// timeout, a throttle wait, or an `az` that will not come back, and a
    /// quit that hangs on any of those reads as a hang. Nothing on it needs
    /// to finish — the cache and the session are written on the main thread
    /// — and the process's exit ends it.
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Stop);
        self.handle.take();
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

/// Whether a refresh is wanted, once a batch of requests has been collapsed.
/// Two queued refreshes are one refresh; one asked for during a refresh runs
/// straight after it.
#[derive(Default)]
struct Batch {
    refresh: bool,
}

impl Loop {
    fn run(mut self) {
        loop {
            // One blocking wait, then everything else that had piled up
            // behind it.
            let Ok(first) = self.inbox.recv() else { return };
            let mut batch = Batch::default();
            if !self.take(first, &mut batch) {
                return;
            }
            while let Ok(queued) = self.inbox.try_recv() {
                if !self.take(queued, &mut batch) {
                    return;
                }
            }
            // A loop rather than a call from the end of `refresh`: a run
            // whose refreshes keep being asked for mid-refresh must not
            // nest one stack frame per refresh.
            while batch.refresh {
                batch.refresh = false;
                if !self.refresh(&mut batch) {
                    return;
                }
            }
        }
    }

    /// Serves a detail request now, or notes that a refresh is wanted.
    /// Returns false on a `Stop`, which wins over everything else queued.
    fn take(&self, request: Request, batch: &mut Batch) -> bool {
        match request {
            Request::Stop => return false,
            Request::Refresh => batch.refresh = true,
            detail => self.serve(detail),
        }
        true
    }

    /// Drains whatever has arrived without blocking, serving details at once.
    /// Returns false when the loop should stop — a `Stop` during a refresh,
    /// or a UI that has gone away.
    fn pump(&self, batch: &mut Batch) -> bool {
        loop {
            match self.inbox.try_recv() {
                Ok(request) => {
                    if !self.take(request, batch) {
                        return false;
                    }
                }
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
    ///
    /// Three phases, each fanned out over [`Azure::threads`] threads with this
    /// one among them: every vault, then every registry's catalog, then the
    /// per-repository fill. Between the items this thread takes it pumps the
    /// inbox, so a detail asked for mid-refresh waits for one call.
    ///
    /// Only a login that cannot mint a token stops the pass: that is nobody's
    /// vault's problem, and it is said once — by minting the one token a
    /// phase shares before the phase starts. A plane refusing a token that
    /// was minted a moment ago — a vault left behind in another tenant, a
    /// registry this login has no role on — is that plane's problem, and the
    /// next one is still asked.
    ///
    // ponytail: a detail asked for while this thread is inside its own call
    // waits for that call, up to the transport's 30 s timeout — the same
    // ceiling as the serial walk had. Serving details on a thread of their
    // own would need the inventory shared and requests routed; add it if a
    // `v` ever visibly waits.
    fn refresh(&mut self, batch: &mut Batch) -> bool {
        self.progress("reading the subscription…");
        let inventory = match graph::inventory(&self.client, &self.azure) {
            Ok(inventory) => inventory,
            Err(error) => {
                self.send(Event::Inventory(Err(said(&error))));
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
        let limit = self.azure.threads();
        let (client, outbox) = (&self.client, &self.outbox);

        // The vaults. The one token they all share is minted first, once:
        // otherwise every thread would shell out to `az` at the same moment,
        // and a login that is gone would be said once per vault.
        //
        // ponytail: a token that expires *during* a phase is re-minted by up
        // to `limit` threads at once, once an hour. A per-audience mint lock
        // in the client is the fix if that ever shows on a laptop.
        if !vaults.is_empty()
            && let Err(error) = client.token(&Audience::Vault)
            && is_no_login(&error)
        {
            return self.stop_signed_out(&error);
        }
        self.progress(format!("reading {} vaults…", vaults.len()));
        let done = AtomicUsize::new(0);
        let walked = each(
            &vaults,
            limit,
            || self.pump(batch),
            |_, vault| {
                let result = vault::secrets(client, vault).map_err(|error| format!("{error:#}"));
                let _ = outbox.send(Event::Secrets {
                    vault: vault.name.clone(),
                    result,
                });
                let _ = outbox.send(Event::Progress(format!(
                    "reading vaults ({}/{})…",
                    done.fetch_add(1, Relaxed) + 1,
                    vaults.len()
                )));
            },
        );
        if !walked {
            return false;
        }

        // The catalogs, one per registry.
        if !registries.is_empty()
            && let Err(error) = client.token(&Audience::ContainerRegistry)
            && is_no_login(&error)
        {
            return self.stop_signed_out(&error);
        }
        self.progress(format!("reading {} registries…", registries.len()));
        let catalogs: Mutex<Vec<(usize, Vec<String>)>> = Mutex::new(Vec::new());
        let walked = each(
            &registries,
            limit,
            || self.pump(batch),
            |index, registry| {
                let result = match acr::repositories(client, registry) {
                    Ok(names) => {
                        let rows = names
                            .iter()
                            .map(|name| Repository::unfilled(&registry.name, name))
                            .collect();
                        catalogs.lock().unwrap().push((index, names));
                        Ok(rows)
                    }
                    Err(error) => Err(format!("{error:#}")),
                };
                let _ = outbox.send(Event::Repositories {
                    registry: registry.name.clone(),
                    result,
                });
            },
        );
        if !walked {
            return false;
        }

        // The fill: one call per repository, behind the names that are
        // already on screen.
        let catalogs = catalogs.into_inner().unwrap();
        let fill: Vec<(&Registry, &str)> = catalogs
            .iter()
            .flat_map(|(index, names)| {
                let registry = &registries[*index];
                names.iter().map(move |name| (registry, name.as_str()))
            })
            .collect();
        let done = AtomicUsize::new(0);
        // A registry that stopped answering. Anything but a 404 ends its
        // fill: a host that has gone quiet would otherwise cost a timeout per
        // repository, for hours. The names stay on screen, marked stale, with
        // the reason in the footer — once, however many threads saw it.
        let dead: Mutex<HashSet<&str>> = Mutex::new(HashSet::new());
        let walked = each(
            &fill,
            limit,
            || self.pump(batch),
            |_, &(registry, name)| {
                if dead.lock().unwrap().contains(registry.name.as_str()) {
                    return;
                }
                let done = done.fetch_add(1, Relaxed) + 1;
                if done.is_multiple_of(20) {
                    let _ = outbox.send(Event::Progress(format!(
                        "filling repository details ({done}/{})…",
                        fill.len()
                    )));
                }
                match acr::attributes(client, registry, name) {
                    Ok(repository) => {
                        let _ = outbox.send(Event::Repository {
                            registry: registry.name.clone(),
                            repository,
                        });
                    }
                    // A repository deleted since the catalog was read is
                    // simply skipped.
                    Err(error) if api_error(&error).is_some_and(|e| e.status == 404) => {}
                    Err(error) => {
                        if dead.lock().unwrap().insert(registry.name.as_str()) {
                            let _ = outbox.send(Event::Repositories {
                                registry: registry.name.clone(),
                                result: Err(format!("{error:#}")),
                            });
                        }
                    }
                }
            },
        );
        if !walked {
            return false;
        }

        self.send(Event::Idle)
    }

    /// A missing login is not one vault's problem, so the refresh stops and
    /// says so once, with the reason `az` gave.
    fn stop_signed_out(&self, error: &anyhow::Error) -> bool {
        self.send(Event::Inventory(Err(said(error)))) && self.send(Event::Idle)
    }

    /// One detail, read now. Nothing here is retried and nothing is cached on
    /// this thread — least of all a value.
    fn serve(&self, request: Request) {
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
                copy,
            } => {
                let result = self.with_vault(&name, |client, vault| {
                    vault::value(client, vault, &secret, version.as_deref())
                });
                self.send(Event::Value {
                    vault: name,
                    name: secret,
                    copy,
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
        read(&self.client, vault).map_err(|error| said(&error))
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
            return Err(self.cannot_place(name));
        };
        read(&self.client, registry).map_err(|error| said(&error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::transport::fake::{Answer, client as fake_client};
    use serde_json::json;
    use std::time::{Duration, Instant};

    /// One thread, so the fake's canned answers land in the order they were
    /// given. Every assertion below is about an order; the one test about
    /// threads builds its own configuration.
    fn serial() -> Azure {
        Azure {
            parallel: Some(1),
            ..Azure::default()
        }
    }

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
        let worker = Worker::start(serial(), client, Inventory::default());
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
    fn a_refresh_reads_vaults_side_by_side() {
        use crate::azure::auth::FixedTokens;
        use crate::azure::transport::fake::FakeTransport;

        // Two vaults and two interchangeable listings: which thread takes
        // which answer does not matter, only that both were in flight at once.
        let transport = FakeTransport::answering([
            inventory_answer(),
            secrets_answer("one"),
            secrets_answer("two"),
            Answer::json(json!({ "refresh_token": "r" })),
            Answer::json(json!({ "access_token": "a" })),
            Answer::json(json!({ "repositories": [] })),
        ])
        .slow(Duration::from_millis(30));
        let client = Client::new(Box::new(FixedTokens::new()), Box::new(transport.clone()));
        let azure = Azure {
            parallel: Some(2),
            ..Azure::default()
        };
        let worker = Worker::start(azure, client, Inventory::default());
        worker.send(Request::Refresh);
        let said = until_idle(&worker);
        assert!(said.contains(&"secrets(kv-a,ok)".to_owned()), "{said:?}");
        assert!(said.contains(&"secrets(kv-b,ok)".to_owned()), "{said:?}");
        assert_eq!(said.last().map(String::as_str), Some("idle"));
        assert!(
            transport.peak() >= 2,
            "the vaults were read one after another: {said:?}"
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
        let worker = Worker::start(serial(), client, Inventory::default());
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
        let worker = Worker::start(serial(), client, Inventory::default());
        worker.send(Request::Refresh);
        let said = until_idle(&worker);
        assert_eq!(said.len(), 2, "{said:?}");
        assert!(
            said[0].starts_with("inventory-err(not signed in — run `az login` (Azure refused"),
            "the fixed words, then why: {said:?}"
        );
        assert_eq!(said[1], "idle");
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
        let worker = Worker::start(serial(), client, Inventory::default());
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
                    copy: false,
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
        let worker = Worker::start(serial(), client, Inventory::default());
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
        let worker = Worker::start(serial(), client, Inventory::default());
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
                resource_group: "rg".into(),
                location: "eastus".into(),
                uri: "https://kv-a.vault.azure.net/".into(),
            }],
            registries: Vec::new(),
        };
        let worker = Worker::start(serial(), client, known);
        worker.send(Request::Value {
            vault: "kv-a".into(),
            name: "one".into(),
            version: None,
            copy: false,
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
        assert!(
            message.starts_with("not signed in — run `az login` ("),
            "{message}"
        );
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
                resource_group: "rg".into(),
                location: "eastus".into(),
                uri: "https://kv-a.vault.azure.net/".into(),
            }],
            registries: Vec::new(),
        };
        let worker = Worker::start(serial(), client, known);
        worker.send(Request::Value {
            vault: "kv-a".into(),
            name: "one".into(),
            version: None,
            copy: false,
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
        let worker = Worker::start(serial(), client, Inventory::default());
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
        let worker = Worker::start(serial(), client, Inventory::default());
        worker.send(Request::Stop);
        // `Drop` no longer waits — a quit must not hang on a request in
        // flight — so the test waits itself, briefly, for the thread to
        // honour the Stop it was sent.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !worker.handle.as_ref().unwrap().is_finished() {
            assert!(Instant::now() < deadline, "the thread ignored Stop");
            std::thread::sleep(Duration::from_millis(2));
        }
        drop(worker);
    }

    #[test]
    fn a_login_that_lapses_mid_refresh_stops_the_refresh_and_says_so_once() {
        use crate::azure::auth::FixedTokens;
        use crate::azure::transport::NoLogin;
        use crate::azure::transport::fake::FakeTransport;

        // The ARM token mints; the vault token does not — `az` has nothing to
        // give — so the first vault is where the login is found to be gone.
        let tokens = FixedTokens::new();
        tokens
            .answers
            .lock()
            .unwrap()
            .push_back(Ok("arm-token".to_owned()));
        tokens
            .answers
            .lock()
            .unwrap()
            .push_back(Err(anyhow::Error::new(NoLogin("gone".to_owned()))));
        let transport = FakeTransport::answering([inventory_answer()]);
        let client = Client::new(Box::new(tokens), Box::new(transport.clone()));
        let worker = Worker::start(serial(), client, Inventory::default());
        worker.send(Request::Refresh);
        assert_eq!(
            until_idle(&worker),
            [
                "inventory",
                "inventory-err(not signed in — run `az login` (gone))",
                "idle"
            ],
            "said once, not once per vault"
        );
        assert_eq!(
            transport.sent().len(),
            1,
            "kv-b and the registry were never asked"
        );
    }

    #[test]
    fn a_vault_that_refuses_a_fresh_token_is_that_vaults_problem_not_the_logins() {
        // A vault in another tenant answers 401 to a token that was minted a
        // moment ago. The retry re-mints, the vault refuses again, and that
        // is one vault's line in the footer — not "not signed in" for a login
        // that is fine, and not the end of the refresh.
        let (client, _, _) = fake_client([
            inventory_answer(),
            Answer::status(401, r#"{"error":{"message":"AKV10032: Invalid issuer"}}"#),
            Answer::status(401, r#"{"error":{"message":"AKV10032: Invalid issuer"}}"#),
            secrets_answer("two"),
            Answer::json(json!({ "refresh_token": "r" })),
            Answer::json(json!({ "access_token": "a" })),
            Answer::json(json!({ "repositories": [] })),
        ]);
        let worker = Worker::start(serial(), client, Inventory::default());
        worker.send(Request::Refresh);
        let said = until_idle(&worker);
        assert_eq!(
            said,
            [
                "inventory",
                "secrets(kv-a,err)",
                "secrets(kv-b,ok)",
                "repositories(acra,ok)",
                "idle",
            ]
        );
    }

    #[test]
    fn a_registry_that_stops_answering_ends_its_fill_rather_than_timing_out_per_repository() {
        // The catalog lands; the first attributes call fails for want of an
        // answer; the second repository is never asked about.
        let (client, transport, _) = fake_client([
            inventory_answer(),
            secrets_answer("one"),
            secrets_answer("two"),
            Answer::json(json!({ "refresh_token": "r" })),
            Answer::json(json!({ "access_token": "a" })),
            Answer::json(json!({ "repositories": ["api", "web"] })),
            Answer::json(json!({ "access_token": "api-token" })),
        ]);
        let worker = Worker::start(serial(), client, Inventory::default());
        worker.send(Request::Refresh);
        assert_eq!(
            until_idle(&worker),
            [
                "inventory",
                "secrets(kv-a,ok)",
                "secrets(kv-b,ok)",
                "repositories(acra,ok)",
                "repositories(acra,err)",
                "idle",
            ]
        );
        let urls = transport.urls();
        let catalog = urls
            .iter()
            .position(|url| url.contains("/_catalog"))
            .unwrap();
        assert_eq!(
            urls[catalog + 1..]
                .iter()
                .filter(|url| url.ends_with("/oauth2/token"))
                .count(),
            1,
            "no token was minted for web: {urls:?}"
        );
    }

    #[test]
    fn a_throttle_wait_is_said_on_screen_before_it_is_taken() {
        let (client, _, _) = fake_client([
            Answer::status(429, "{}").with_header("Retry-After", "1"),
            inventory_answer(),
            secrets_answer("one"),
            secrets_answer("two"),
            Answer::json(json!({ "refresh_token": "r" })),
            Answer::json(json!({ "access_token": "a" })),
            Answer::json(json!({ "repositories": [] })),
        ]);
        let worker = Worker::start(serial(), client, Inventory::default());
        worker.send(Request::Refresh);
        // Raw events this time: `pump_until` drops the progress lines, and
        // the progress line is the point.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        loop {
            match worker.try_recv() {
                Some(Event::Progress(said)) => seen.push(said),
                Some(Event::Inventory(_)) => break,
                Some(_) => {}
                None => {
                    assert!(Instant::now() < deadline, "no inventory: {seen:?}");
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
        assert!(
            seen.iter()
                .any(|said| said.contains("throttled — waiting 1 s")),
            "{seen:?}"
        );
    }
}
