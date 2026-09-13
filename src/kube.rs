//! `kubectl`: what a pod is, how a cluster is asked about one, and the thread
//! that keeps asking.
//!
//! Nothing here is stored beyond the cache the next start paints from. A pod
//! is read live and the next read replaces it. The worker has its own thread
//! and its own `kubectl` processes, so the screen never waits on a cluster.
//!
//! Lifted from ticket-tui's AKS tab (`6f73eef^:src/aks.rs`), with the
//! per-cluster sweep turned into a per-scope cadence: the open tab is read
//! every few seconds, the others every half minute for their badges.

use std::cell::Cell;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Scope;
use crate::timestamp::Timestamp;

/// How often the open tab is read when `config.toml` does not say.
pub const DEFAULT_REFRESH: Duration = Duration::from_secs(5);

/// How often a tab nobody is looking at is read, for its badge.
pub const HIDDEN_REFRESH: Duration = Duration::from_secs(30);

/// How far a failing scope's cadence stretches, doubling each time.
const MAX_CADENCE: Duration = Duration::from_secs(120);

/// The bound on every one-shot `kubectl` call's request. A cluster that
/// cannot be reached answers in ten seconds rather than never.
const REQUEST_TIMEOUT: &str = "--request-timeout=10s";

/// The bound on the whole call, request or not: a credential plugin waiting
/// on a device-code login is the one thing `--request-timeout` cannot end.
const CALL_CAP: Duration = Duration::from_secs(20);

/// How much of a log a follow opens on.
const TAIL_LINES: &str = "--tail=500";

/// One pod, by where it lives.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct PodKey {
    /// The cluster's name in `config.toml`, not its context.
    pub cluster: String,
    pub namespace: String,
    pub name: String,
}

/// One container of a pod, as its status reports it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Container {
    pub name: String,
    pub image: String,
    pub ready: bool,
    pub restarts: u32,
    /// `Running`, or the reason it is waiting or has stopped:
    /// `CrashLoopBackOff`, `Completed`, `ExitCode:137`.
    pub state: String,
    /// Why it last stopped, and with what code, when it has stopped before.
    pub last_termination: Option<(String, i64)>,
}

/// One pod, as `kubectl get pods` would print it, with what the details pane
/// wants besides.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Pod {
    pub key: PodKey,
    /// The STATUS word: `Running`, `CrashLoopBackOff`, `Init:1/2`, …
    pub status: String,
    /// Containers ready, and containers in the spec.
    pub ready: (usize, usize),
    pub restarts: u32,
    pub created: Option<Timestamp>,
    pub node: String,
    pub ip: String,
    /// What made it, as `(kind, name)`: `("Deployment", "orders-api")`.
    pub owner: Option<(String, String)>,
    pub containers: Vec<Container>,
    /// Every label, sorted by key.
    pub labels: Vec<(String, String)>,
}

impl Pod {
    /// One `items[]` entry of `kubectl get pods -o json`. `None` for an entry
    /// with no name or namespace, which is not a pod.
    #[must_use]
    pub fn from_json(cluster: &str, item: &Value) -> Option<Self> {
        let metadata = &item["metadata"];
        let name = metadata["name"].as_str()?;
        let namespace = metadata["namespace"].as_str()?;
        let statuses = item["status"]["containerStatuses"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let containers: Vec<Container> = item["spec"]["containers"]
            .as_array()
            .map(|specs| {
                specs
                    .iter()
                    .filter_map(|spec| {
                        let name = spec["name"].as_str()?;
                        let status = statuses
                            .iter()
                            .find(|status| status["name"].as_str() == Some(name));
                        Some(container(name, spec, status))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut labels: Vec<(String, String)> = metadata["labels"]
            .as_object()
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        labels.sort();
        Some(Self {
            key: PodKey {
                cluster: cluster.to_owned(),
                namespace: namespace.to_owned(),
                name: name.to_owned(),
            },
            status: status_word(item),
            ready: (
                containers.iter().filter(|held| held.ready).count(),
                containers.len(),
            ),
            restarts: containers.iter().map(|held| held.restarts).sum(),
            created: metadata["creationTimestamp"]
                .as_str()
                .and_then(Timestamp::parse),
            node: item["spec"]["nodeName"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            ip: item["status"]["podIP"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            owner: owner_of(item),
            containers,
            labels,
        })
    }

    #[must_use]
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels
            .iter()
            .find(|(held, _)| held == key)
            .map(|(_, value)| value.as_str())
    }

    /// `1/2`, the READY column.
    #[must_use]
    pub fn ready_label(&self) -> String {
        format!("{}/{}", self.ready.0, self.ready.1)
    }

    /// `Deployment/orders-api`, or a dash for a pod nothing put there.
    #[must_use]
    pub fn owner_label(&self) -> String {
        self.owner.as_ref().map_or_else(
            || "\u{2014}".to_owned(),
            |(kind, name)| format!("{kind}/{name}"),
        )
    }

    /// What `owner:` filters on: `orders-api`.
    #[must_use]
    pub fn owner_name(&self) -> &str {
        self.owner.as_ref().map_or("", |(_, name)| name.as_str())
    }

    /// Whether deleting it restarts anything: a pod with a controller is put
    /// back by that controller, a bare pod is simply gone.
    #[must_use]
    pub const fn restartable(&self) -> bool {
        self.owner.is_some()
    }

    /// Whether the STATUS word is one somebody has to look at.
    #[must_use]
    pub fn is_unhealthy(&self) -> bool {
        unhealthy_word(self.status.strip_prefix("Init:").unwrap_or(&self.status))
    }

    /// The glyph the conventions give the pod: `●` running and ready, `◐`
    /// on its way somewhere, `✓` finished, `✗` in trouble, `○` anything else.
    #[must_use]
    pub fn glyph(&self) -> &'static str {
        if self.is_unhealthy() {
            "\u{2717}"
        } else if self.status == "Running" && self.ready.1 > 0 && self.ready.0 == self.ready.1 {
            "\u{25cf}"
        } else if matches!(self.status.as_str(), "Completed" | "Succeeded") {
            "\u{2713}"
        } else if matches!(
            self.status.as_str(),
            "Running" | "Pending" | "ContainerCreating" | "PodInitializing" | "Terminating"
        ) || self.status.starts_with("Init:")
        {
            "\u{25d0}"
        } else {
            "\u{25cb}"
        }
    }

    /// The name of the container the log follows when nobody has chosen one.
    #[must_use]
    pub fn first_container(&self) -> Option<&str> {
        self.containers.first().map(|held| held.name.as_str())
    }

    /// The `app` label, or the `app.kubernetes.io/name` one: what `app:`
    /// filters on.
    #[must_use]
    pub fn app(&self) -> Option<&str> {
        self.label("app")
            .or_else(|| self.label("app.kubernetes.io/name"))
    }
}

/// One container, joined from its spec and its status.
fn container(name: &str, spec: &Value, status: Option<&Value>) -> Container {
    let state = status.map(|status| &status["state"]);
    let word = state.map_or_else(
        || "Waiting".to_owned(),
        |state| {
            if !state["running"].is_null() {
                "Running".to_owned()
            } else if let Some(reason) = non_empty(&state["waiting"]["reason"]) {
                reason.to_owned()
            } else if !state["terminated"].is_null() {
                termination_word(&state["terminated"])
            } else {
                "Waiting".to_owned()
            }
        },
    );
    let last = status
        .map(|status| &status["lastState"]["terminated"])
        .filter(|terminated| !terminated.is_null())
        .map(|terminated| {
            (
                non_empty(&terminated["reason"])
                    .unwrap_or("Terminated")
                    .to_owned(),
                terminated["exitCode"].as_i64().unwrap_or_default(),
            )
        });
    Container {
        name: name.to_owned(),
        image: status
            .and_then(|status| non_empty(&status["image"]))
            .or_else(|| non_empty(&spec["image"]))
            .unwrap_or_default()
            .to_owned(),
        ready: status.is_some_and(|status| status["ready"].as_bool() == Some(true)),
        restarts: status
            .and_then(|status| status["restartCount"].as_u64())
            .and_then(|count| u32::try_from(count).ok())
            .unwrap_or_default(),
        state: word,
        last_termination: last,
    }
}

fn non_empty(value: &Value) -> Option<&str> {
    value.as_str().filter(|held| !held.is_empty())
}

/// What a stopped container says: its reason, or its exit code when it gave
/// none.
fn termination_word(terminated: &Value) -> String {
    non_empty(&terminated["reason"]).map_or_else(
        || {
            format!(
                "ExitCode:{}",
                terminated["exitCode"].as_i64().unwrap_or_default()
            )
        },
        str::to_owned,
    )
}

/// The STATUS word `kubectl get pods` prints, cut to the cases that come up:
/// the pod's own reason or phase, overridden by the first init container
/// still going, else by whatever the containers are waiting on or stopped
/// for, and `Terminating` over all of it once a delete is in.
// ponytail: skipped from kubectl's printPod — sidecar init containers,
// Signal:N, NotReady, NodeLost→Unknown, and the "(N ago)" restart suffix.
fn status_word(item: &Value) -> String {
    let status = &item["status"];
    let phase = status["phase"].as_str().unwrap_or("Unknown");
    let mut word = non_empty(&status["reason"]).unwrap_or(phase).to_owned();
    let init_total = item["spec"]["initContainers"]
        .as_array()
        .map_or(0, Vec::len);
    let mut initializing = false;
    for (index, held) in status["initContainerStatuses"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let state = &held["state"];
        let terminated = &state["terminated"];
        if !terminated.is_null() {
            if terminated["exitCode"].as_i64() == Some(0) {
                continue;
            }
            word = format!("Init:{}", termination_word(terminated));
        } else if let Some(reason) =
            non_empty(&state["waiting"]["reason"]).filter(|reason| *reason != "PodInitializing")
        {
            word = format!("Init:{reason}");
        } else {
            word = format!("Init:{index}/{init_total}");
        }
        initializing = true;
        break;
    }
    if !initializing {
        let mut has_running = false;
        // Back to front, the way kubectl reads them, so the first container's
        // reason is the one that stands.
        for held in status["containerStatuses"]
            .as_array()
            .into_iter()
            .flatten()
            .rev()
        {
            let state = &held["state"];
            if let Some(reason) = non_empty(&state["waiting"]["reason"]) {
                word = reason.to_owned();
            } else if !state["terminated"].is_null() {
                word = termination_word(&state["terminated"]);
            } else if held["ready"].as_bool() == Some(true) && !state["running"].is_null() {
                has_running = true;
            }
        }
        if word == "Completed" && has_running {
            word = "Running".to_owned();
        }
    }
    if !item["metadata"]["deletionTimestamp"].is_null() && !matches!(phase, "Succeeded" | "Failed")
    {
        word = "Terminating".to_owned();
    }
    word
}

/// Whether a STATUS word, with any `Init:` in front of it removed, is one
/// somebody has to look at.
fn unhealthy_word(word: &str) -> bool {
    matches!(
        word,
        "CrashLoopBackOff"
            | "Error"
            | "ImagePullBackOff"
            | "ErrImagePull"
            | "InvalidImageName"
            | "CreateContainerConfigError"
            | "CreateContainerError"
            | "OOMKilled"
            | "Evicted"
            | "Failed"
            | "ContainerStatusUnknown"
            | "Unknown"
    ) || word.starts_with("ExitCode:")
}

/// What made the pod. A ReplicaSet named after a pod-template hash is a
/// Deployment's, and is reported as that Deployment, which is the name that
/// means something.
// ponytail: a Job's CronJob is not resolved; a ReplicaSet with no hash label
// stays a ReplicaSet.
fn owner_of(item: &Value) -> Option<(String, String)> {
    let references = item["metadata"]["ownerReferences"].as_array()?;
    let owner = references
        .iter()
        .find(|reference| reference["controller"].as_bool() == Some(true))
        .or_else(|| references.first())?;
    let kind = owner["kind"].as_str()?;
    let name = owner["name"].as_str()?;
    if kind == "ReplicaSet"
        && let Some(hash) = non_empty(&item["metadata"]["labels"]["pod-template-hash"])
        && let Some(base) = name.strip_suffix(&format!("-{hash}"))
    {
        return Some(("Deployment".to_owned(), base.to_owned()));
    }
    Some((kind.to_owned(), name.to_owned()))
}

/// One object in a namespace, as `kubectl describe` and `kubectl get` name
/// it: `pod`, `deployment`, `configmap`, and its name.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ObjectRef {
    /// Lowercase, singular: `pod`, `deployment`, `statefulset`.
    pub kind: String,
    pub namespace: String,
    pub name: String,
}

impl ObjectRef {
    #[must_use]
    pub fn pod(key: &PodKey) -> Self {
        Self {
            kind: "pod".to_owned(),
            namespace: key.namespace.clone(),
            name: key.name.clone(),
        }
    }

    /// `pod/orders-api-7d9f5b-abc12`, as kubectl spells one.
    #[must_use]
    pub fn slash(&self) -> String {
        format!("{}/{}", self.kind, self.name)
    }
}

/// What the log pane is following: the pod, which of its containers, and
/// whether the one before the last restart rather than the one running.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogFollow {
    /// Which tab's scope the pod is in, for the context.
    pub scope: usize,
    pub key: PodKey,
    pub container: Option<String>,
    pub previous: bool,
}

/// A running `kubectl logs -f`: what to read, what it complained about, and
/// the process to kill when the pane moves on.
pub struct LogTail {
    pub child: Option<Child>,
    pub stdout: Box<dyn Read + Send>,
    pub stderr: Option<Box<dyn Read + Send>>,
}

/// What a tab lists. Pods are read for every tab; the rest for the tab on
/// screen while it shows them.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum Kind {
    #[default]
    Pods,
    Events,
    ConfigMaps,
    Secrets,
}

impl Kind {
    pub const ALL: [Self; 4] = [Self::Pods, Self::Events, Self::ConfigMaps, Self::Secrets];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pods => "Pods",
            Self::Events => "Events",
            Self::ConfigMaps => "ConfigMaps",
            Self::Secrets => "Secrets",
        }
    }

    /// The word for one row, for counts: `12 events`.
    #[must_use]
    pub const fn noun(self) -> &'static str {
        match self {
            Self::Pods => "pods",
            Self::Events => "events",
            Self::ConfigMaps => "configmaps",
            Self::Secrets => "secrets",
        }
    }

    /// The key that switches to it.
    #[must_use]
    pub const fn key(self) -> char {
        match self {
            Self::Pods => 'p',
            Self::Events => 'e',
            Self::ConfigMaps => 'm',
            Self::Secrets => 's',
        }
    }

    /// What the session file calls it.
    #[must_use]
    pub const fn session_key(self) -> &'static str {
        match self {
            Self::Pods => "pods",
            Self::Events => "events",
            Self::ConfigMaps => "configmaps",
            Self::Secrets => "secrets",
        }
    }

    #[must_use]
    pub fn from_session_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.session_key() == key)
    }
}

/// One event, as `kubectl get events` lists it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct K8sEvent {
    /// The event's own name, for telling two apart.
    pub name: String,
    pub namespace: String,
    /// When it was last seen.
    pub last: Option<Timestamp>,
    pub first: Option<Timestamp>,
    pub count: i64,
    /// `Normal` or `Warning`.
    pub kind: String,
    pub reason: String,
    /// What it is about.
    pub object: ObjectRef,
    pub message: String,
    pub source: String,
}

impl K8sEvent {
    #[must_use]
    pub fn from_json(item: &Value) -> Option<Self> {
        let metadata = &item["metadata"];
        let name = metadata["name"].as_str()?;
        let namespace = metadata["namespace"].as_str().unwrap_or_default();
        let stamp = |value: &Value| value.as_str().and_then(Timestamp::parse);
        let last = stamp(&item["lastTimestamp"])
            .or_else(|| stamp(&item["series"]["lastObservedTime"]))
            .or_else(|| stamp(&item["eventTime"]))
            .or_else(|| stamp(&metadata["creationTimestamp"]));
        let first = stamp(&item["firstTimestamp"]).or_else(|| stamp(&item["eventTime"]));
        let count = item["count"]
            .as_i64()
            .or_else(|| item["series"]["count"].as_i64())
            .unwrap_or(1);
        let involved = &item["involvedObject"];
        Some(Self {
            name: name.to_owned(),
            namespace: namespace.to_owned(),
            last,
            first,
            count,
            kind: item["type"].as_str().unwrap_or("Normal").to_owned(),
            reason: item["reason"].as_str().unwrap_or_default().to_owned(),
            object: ObjectRef {
                kind: involved["kind"]
                    .as_str()
                    .unwrap_or_default()
                    .to_ascii_lowercase(),
                namespace: involved["namespace"]
                    .as_str()
                    .unwrap_or(namespace)
                    .to_owned(),
                name: involved["name"].as_str().unwrap_or_default().to_owned(),
            },
            message: item["message"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_owned(),
            source: item["source"]["component"]
                .as_str()
                .or_else(|| item["reportingComponent"].as_str())
                .unwrap_or_default()
                .to_owned(),
        })
    }

    /// Whether it is one somebody has to look at.
    #[must_use]
    pub fn is_warning(&self) -> bool {
        self.kind != "Normal"
    }
}

/// One configmap, data and all. The data is held in memory for the run and
/// never cached: a configmap can be large, and a few are not as public as
/// they should be.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigMap {
    pub name: String,
    pub namespace: String,
    pub created: Option<Timestamp>,
    /// `(key, value)`, sorted by key. A binary key's value says its size.
    pub data: Vec<(String, String)>,
}

impl ConfigMap {
    #[must_use]
    pub fn from_json(item: &Value) -> Option<Self> {
        let metadata = &item["metadata"];
        let name = metadata["name"].as_str()?;
        let mut data: Vec<(String, String)> = item["data"]
            .as_object()
            .map(|data| {
                data.iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(binary) = item["binaryData"].as_object() {
            for (key, value) in binary {
                let size = value.as_str().map_or(0, decoded_len);
                data.push((key.clone(), format!("<binary, {size} bytes>")));
            }
        }
        data.sort();
        Some(Self {
            name: name.to_owned(),
            namespace: metadata["namespace"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            created: metadata["creationTimestamp"]
                .as_str()
                .and_then(Timestamp::parse),
            data,
        })
    }

    #[must_use]
    pub fn object(&self) -> ObjectRef {
        ObjectRef {
            kind: "configmap".to_owned(),
            namespace: self.namespace.clone(),
            name: self.name.clone(),
        }
    }
}

/// One secret's shape — its keys and their sizes — with the data stripped on
/// the worker thread before it crosses to the screen. A value is read again,
/// one key at a time, only when `v` or `y` asks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretMeta {
    pub name: String,
    pub namespace: String,
    pub kind: String,
    pub created: Option<Timestamp>,
    /// `(key, decoded size in bytes)`, sorted by key.
    pub keys: Vec<(String, usize)>,
}

impl SecretMeta {
    #[must_use]
    pub fn from_json(item: &Value) -> Option<Self> {
        let metadata = &item["metadata"];
        let name = metadata["name"].as_str()?;
        let mut keys: Vec<(String, usize)> = item["data"]
            .as_object()
            .map(|data| {
                data.iter()
                    .map(|(key, value)| (key.clone(), value.as_str().map_or(0, decoded_len)))
                    .collect()
            })
            .unwrap_or_default();
        keys.sort();
        Some(Self {
            name: name.to_owned(),
            namespace: metadata["namespace"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            kind: item["type"].as_str().unwrap_or("Opaque").to_owned(),
            created: metadata["creationTimestamp"]
                .as_str()
                .and_then(Timestamp::parse),
            keys,
        })
    }

    #[must_use]
    pub fn object(&self) -> ObjectRef {
        ObjectRef {
            kind: "secret".to_owned(),
            namespace: self.namespace.clone(),
            name: self.name.clone(),
        }
    }
}

/// A secret's value, decoded. **The one type in the crate that holds one.**
///
/// `Debug` and `Display` print `[redacted]`, it derives no `Serialize`, and
/// [`Secret::expose`] is the one way to read it — meant to be conspicuous at
/// the call site.
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value. The callers are the line that draws it and the key that
    /// copies it; a grep for this method over `src/` is the audit.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn line_count(&self) -> usize {
        self.0.lines().count().max(1)
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[redacted]")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[redacted]")
    }
}

/// How many bytes a base64 string decodes to, without decoding it.
fn decoded_len(encoded: &str) -> usize {
    let trimmed = encoded.trim_end_matches('=');
    trimmed.len() * 3 / 4
}

/// Standard base64, with or without padding, whitespace ignored.
///
// ponytail: twenty lines rather than the `base64` crate, which is the whole
// of what this program would use it for.
pub fn base64_decode(encoded: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(encoded.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for character in encoded.bytes() {
        let value = match character {
            b'A'..=b'Z' => character - b'A',
            b'a'..=b'z' => character - b'a' + 26,
            b'0'..=b'9' => character - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b' ' | b'\n' | b'\r' | b'\t' => continue,
            other => bail!("not base64: byte {other:#x}"),
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Ok(bytes)
}

/// What a scalable owner says about itself: `spec.replicas` and
/// `status.readyReplicas`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Replicas {
    pub desired: i64,
    pub ready: i64,
}

impl Replicas {
    /// `3/3 ready`.
    #[must_use]
    pub fn label(self) -> String {
        format!("{}/{} ready", self.ready, self.desired)
    }
}

/// Where the worker reads from. `kubectl` in the app; a fake in the tests.
/// Everything but the pod list has a default that answers nothing, so a fake
/// implements what its test needs.
pub trait KubeSource: Send {
    fn pods(&self, scope: &Scope) -> Result<Vec<Pod>>;

    fn events(&self, _scope: &Scope) -> Result<Vec<K8sEvent>> {
        Ok(Vec::new())
    }

    fn configmaps(&self, _scope: &Scope) -> Result<Vec<ConfigMap>> {
        Ok(Vec::new())
    }

    fn secrets(&self, _scope: &Scope) -> Result<Vec<SecretMeta>> {
        Ok(Vec::new())
    }

    /// One key of one secret, decoded. The only read that carries a value.
    fn secret_value(&self, _scope: &Scope, _object: &ObjectRef, _key: &str) -> Result<Secret> {
        Ok(Secret::new(String::new()))
    }

    fn delete_pod(&self, _scope: &Scope, _key: &PodKey) -> Result<()> {
        Ok(())
    }

    fn rollout_restart(&self, _scope: &Scope, _object: &ObjectRef) -> Result<()> {
        Ok(())
    }

    fn scale(&self, _scope: &Scope, _object: &ObjectRef, _replicas: u32) -> Result<()> {
        Ok(())
    }

    fn owner(&self, _scope: &Scope, _object: &ObjectRef) -> Result<Replicas> {
        Ok(Replicas {
            desired: 0,
            ready: 0,
        })
    }

    fn describe(&self, _scope: &Scope, _object: &ObjectRef) -> Result<String> {
        Ok(String::new())
    }

    fn yaml(&self, _scope: &Scope, _object: &ObjectRef) -> Result<String> {
        Ok(String::new())
    }

    fn logs(&self, _scope: &Scope, _target: &LogFollow) -> Result<LogTail> {
        Ok(LogTail {
            child: None,
            stdout: Box::new(std::io::empty()),
            stderr: None,
        })
    }
}

/// The real thing: `kubectl` on the path, with the context the scope names.
pub struct Kubectl;

impl Kubectl {
    /// `kubectl --context C --request-timeout=10s …`: its output, or the one
    /// line of its complaint that says what to fix.
    pub fn run(context: &str, arguments: &[&str]) -> Result<String> {
        let mut command = Command::new("kubectl");
        command
            .arg("--context")
            .arg(context)
            .arg(REQUEST_TIMEOUT)
            .args(arguments);
        run_capped(command, CALL_CAP)
    }
}

/// Runs one command to completion, or kills it at `cap`. Both pipes are
/// drained on threads of their own, so a child that fills one never blocks.
pub(crate) fn run_capped(mut command: Command, cap: Duration) -> Result<String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                anyhow!("{program} is not installed or not on PATH")
            } else {
                anyhow!("{program} could not be run: {error}")
            }
        })?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + cap;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("{program} could not be waited for"))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            // The drains are not joined: a credential plugin the child spawned
            // inherits the pipes and holds them open for as long as it polls,
            // which is the very wait the cap is for. Each thread ends on its
            // own when the pipe finally closes.
            // ponytail: one parked thread per timed-out call; a process group
            // killed as one if that ever shows in a profile.
            bail!(
                "{program} did not answer in {}s — a kubelogin waiting for a device-code login \
                 looks like this; run `kubelogin convert-kubeconfig -l azurecli`",
                cap.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(20));
    };
    let out = stdout.join().unwrap_or_default();
    let err = stderr.join().unwrap_or_default();
    if status.success() {
        Ok(out)
    } else {
        bail!("{}", kubectl_error(&err))
    }
}

/// Reads one pipe to its end on a thread of its own.
fn drain(pipe: Option<impl Read + Send + 'static>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut pipe) = pipe {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            text = String::from_utf8_lossy(&bytes).into_owned();
        }
        text
    })
}

/// `get <kind> -o json`, in the scope's namespace or all of them.
fn list_json(scope: &Scope, kind: &str) -> Result<Value> {
    let mut arguments = vec!["get", kind, "-o", "json"];
    match &scope.namespace {
        Some(namespace) => arguments.extend(["-n", namespace]),
        None => arguments.push("--all-namespaces"),
    }
    let raw = Kubectl::run(&scope.context, &arguments)?;
    serde_json::from_str(&raw).context("kubectl answered with something other than JSON")
}

fn items(listed: &Value) -> impl Iterator<Item = &Value> {
    listed["items"].as_array().into_iter().flatten()
}

impl KubeSource for Kubectl {
    fn events(&self, scope: &Scope) -> Result<Vec<K8sEvent>> {
        Ok(items(&list_json(scope, "events")?)
            .filter_map(K8sEvent::from_json)
            .collect())
    }

    fn configmaps(&self, scope: &Scope) -> Result<Vec<ConfigMap>> {
        Ok(items(&list_json(scope, "configmaps")?)
            .filter_map(ConfigMap::from_json)
            .collect())
    }

    /// The data is in the answer, since `kubectl` has no way to leave it out;
    /// it is dropped here, on this thread, and only the keys cross.
    fn secrets(&self, scope: &Scope) -> Result<Vec<SecretMeta>> {
        Ok(items(&list_json(scope, "secrets")?)
            .filter_map(SecretMeta::from_json)
            .collect())
    }

    fn secret_value(&self, scope: &Scope, object: &ObjectRef, key: &str) -> Result<Secret> {
        let raw = Self::run(
            &scope.context,
            &[
                "get",
                "secret",
                &object.name,
                "-n",
                &object.namespace,
                "-o",
                "json",
            ],
        )?;
        let value: Value = serde_json::from_str(&raw)
            .context("kubectl answered with something other than JSON")?;
        let encoded = value["data"][key]
            .as_str()
            .with_context(|| format!("{} has no key {key}", object.name))?;
        let bytes = base64_decode(encoded)?;
        Ok(Secret::new(String::from_utf8_lossy(&bytes).into_owned()))
    }

    fn delete_pod(&self, scope: &Scope, key: &PodKey) -> Result<()> {
        Self::run(
            &scope.context,
            &[
                "delete",
                "pod",
                &key.name,
                "-n",
                &key.namespace,
                "--wait=false",
            ],
        )
        .map(drop)
    }

    fn rollout_restart(&self, scope: &Scope, object: &ObjectRef) -> Result<()> {
        Self::run(
            &scope.context,
            &[
                "rollout",
                "restart",
                &object.slash(),
                "-n",
                &object.namespace,
            ],
        )
        .map(drop)
    }

    fn scale(&self, scope: &Scope, object: &ObjectRef, replicas: u32) -> Result<()> {
        Self::run(
            &scope.context,
            &[
                "scale",
                &object.slash(),
                "-n",
                &object.namespace,
                &format!("--replicas={replicas}"),
            ],
        )
        .map(drop)
    }

    fn owner(&self, scope: &Scope, object: &ObjectRef) -> Result<Replicas> {
        let raw = Self::run(
            &scope.context,
            &[
                "get",
                &object.slash(),
                "-n",
                &object.namespace,
                "-o",
                "json",
            ],
        )?;
        let value: Value = serde_json::from_str(&raw)
            .context("kubectl answered with something other than JSON")?;
        Ok(Replicas {
            desired: value["spec"]["replicas"].as_i64().unwrap_or(1),
            ready: value["status"]["readyReplicas"].as_i64().unwrap_or(0),
        })
    }

    fn describe(&self, scope: &Scope, object: &ObjectRef) -> Result<String> {
        Self::run(
            &scope.context,
            &[
                "describe",
                &object.kind,
                &object.name,
                "-n",
                &object.namespace,
            ],
        )
    }

    fn yaml(&self, scope: &Scope, object: &ObjectRef) -> Result<String> {
        Self::run(
            &scope.context,
            &[
                "get",
                &object.kind,
                &object.name,
                "-n",
                &object.namespace,
                "-o",
                "yaml",
            ],
        )
    }

    /// `kubectl logs -f`, left running: no request timeout and no cap,
    /// because the stream is meant to last, and killing it is the bound.
    fn logs(&self, scope: &Scope, target: &LogFollow) -> Result<LogTail> {
        let mut command = Command::new("kubectl");
        command
            .arg("--context")
            .arg(&scope.context)
            .args(["logs", "-n", &target.key.namespace, &target.key.name])
            .args(["--timestamps", TAIL_LINES, "-f"]);
        if let Some(container) = &target.container {
            command.args(["-c", container]);
        }
        if target.previous {
            command.arg("-p");
        }
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    anyhow!("kubectl is not installed or not on PATH")
                } else {
                    anyhow!("kubectl could not be run: {error}")
                }
            })?;
        let stdout: Box<dyn Read + Send> = match child.stdout.take() {
            Some(stdout) => Box::new(stdout),
            None => Box::new(std::io::empty()),
        };
        let stderr = child
            .stderr
            .take()
            .map(|stderr| Box::new(stderr) as Box<dyn Read + Send>);
        Ok(LogTail {
            child: Some(child),
            stdout,
            stderr,
        })
    }

    fn pods(&self, scope: &Scope) -> Result<Vec<Pod>> {
        let mut arguments = vec!["get", "pods", "-o", "json"];
        match &scope.namespace {
            Some(namespace) => arguments.extend(["-n", namespace]),
            None => arguments.push("--all-namespaces"),
        }
        let raw = Self::run(&scope.context, &arguments)?;
        let listed: Value = serde_json::from_str(&raw)
            .context("kubectl answered with something other than JSON")?;
        Ok(items(&listed)
            .filter_map(|item| Pod::from_json(&scope.cluster, item))
            .collect())
    }
}

/// The one line of `kubectl`'s complaint that says what to fix. The client
/// logs a retry or two before it gives up, and puts a documentation link
/// after the reason, so neither the first line nor the last is the one.
#[must_use]
pub fn kubectl_error(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !is_klog(line))
        .collect();
    let chosen = lines
        .iter()
        .find(|line| line.contains("az login"))
        .or_else(|| {
            lines.iter().find(|line| {
                line.starts_with("error:")
                    || line.starts_with("Error from server")
                    || line.starts_with("Unable to connect")
            })
        })
        .or_else(|| lines.first());
    chosen.map_or_else(
        || "kubectl failed".to_owned(),
        |line| {
            line.strip_prefix("error:")
                .map_or_else(|| (*line).to_owned(), |rest| rest.trim().to_owned())
        },
    )
}

/// `E0830 12:00:00.000000   12345 round_trippers.go:…] …`: the client's own
/// log line, which says nothing a person can act on.
fn is_klog(line: &str) -> bool {
    let mut characters = line.chars();
    matches!(characters.next(), Some('E' | 'W' | 'I' | 'F'))
        && characters.by_ref().take(4).all(|c| c.is_ascii_digit())
        && characters.next() == Some(' ')
}

/// When one scope is next worth reading. Something never polled is due at
/// once; a read that failed doubles the wait, up to two minutes; a base of
/// zero is "only when asked".
#[derive(Clone, Copy, Debug)]
pub struct Cadence {
    base: Duration,
    current: Duration,
    /// When it was last read, and when it is next due.
    last: Option<Instant>,
    due: Option<Instant>,
    /// Set by `r` or a tab switch: due now whatever the clock says.
    asked: bool,
}

impl Cadence {
    #[must_use]
    pub const fn new(base: Duration) -> Self {
        Self {
            base,
            current: base,
            last: None,
            due: None,
            asked: true,
        }
    }

    /// Changes how often this is read. The next read is the new interval
    /// after the last one: a tab just left is not read again five seconds
    /// later on its way to every thirty.
    pub fn set_base(&mut self, base: Duration) {
        if self.base != base {
            self.base = base;
            self.current = base;
            self.due = self.last.and_then(|last| self.next_after(last));
        }
    }

    /// When a read at `at` makes the next one due — never, on a base of
    /// zero.
    fn next_after(&self, at: Instant) -> Option<Instant> {
        if self.base.is_zero() {
            None
        } else {
            at.checked_add(self.current)
        }
    }

    /// Whether this is due at `now`.
    #[must_use]
    pub fn is_due(&self, now: Instant) -> bool {
        self.asked || self.due.is_some_and(|due| now >= due)
    }

    /// How long until it is due, or `None` while it never will be on its
    /// own.
    #[must_use]
    pub fn until_due(&self, now: Instant) -> Option<Duration> {
        if self.asked {
            return Some(Duration::ZERO);
        }
        self.due.map(|due| due.saturating_duration_since(now))
    }

    /// Records a poll, which sets the next one — never, on a base of zero.
    pub fn polled(&mut self, now: Instant, failed: bool) {
        self.asked = false;
        if failed {
            self.current = (self.current * 2).min(MAX_CADENCE).max(self.base);
        } else {
            self.current = self.base;
        }
        self.last = Some(now);
        self.due = self.next_after(now);
    }

    /// Due now, whatever the clock says.
    pub const fn ask(&mut self) {
        self.asked = true;
    }
}

/// What the run tells the worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Request {
    /// Which tab is on screen, by index, and which of its kinds. The tab's
    /// pods are read at once and then on the fast cadence, the other tabs'
    /// on the slow one; a kind other than pods is read for the open tab only,
    /// on the fast cadence, while it shows.
    Showing(usize, Kind),
    /// Read one scope again now.
    Refresh(usize),
    /// Follow one pod's log, dropping whatever was followed before.
    Follow(LogFollow),
    Unfollow,
    /// What `kubectl describe` says about one object.
    Describe {
        scope: usize,
        object: ObjectRef,
    },
    /// The object as `kubectl get -o yaml` prints it.
    Yaml {
        scope: usize,
        object: ObjectRef,
    },
    /// `kubectl delete pod`, which is how a pod with a controller is
    /// restarted.
    Delete {
        scope: usize,
        key: PodKey,
    },
    /// `kubectl rollout restart` of a pod's owner.
    RolloutRestart {
        scope: usize,
        object: ObjectRef,
    },
    /// `kubectl scale` of a pod's owner.
    Scale {
        scope: usize,
        object: ObjectRef,
        replicas: u32,
    },
    /// An owner's replica counts, for the details pane and the scale modal.
    Owner {
        scope: usize,
        object: ObjectRef,
    },
    /// One key of one secret, decoded. `copy` says `y` asked rather than `v`,
    /// so the answer goes to the clipboard rather than the screen.
    SecretValue {
        scope: usize,
        object: ObjectRef,
        key: String,
        copy: bool,
    },
    Stop,
}

/// What the worker sends back. Nothing here is written anywhere but the
/// cache: the screen shows it, and the next read replaces it.
#[derive(Debug)]
pub enum Event {
    /// A read of this scope has started, for the spinner.
    Reading(usize),
    /// One scope's pods, replacing the last read's and nothing else.
    Pods {
        scope: usize,
        pods: Result<Vec<Pod>, String>,
    },
    Events {
        scope: usize,
        events: Result<Vec<K8sEvent>, String>,
    },
    ConfigMaps {
        scope: usize,
        configmaps: Result<Vec<ConfigMap>, String>,
    },
    Secrets {
        scope: usize,
        secrets: Result<Vec<SecretMeta>, String>,
    },
    /// The one event that carries a value. The worker keeps no copy: it is
    /// built, sent, and gone from this thread.
    SecretValue {
        scope: usize,
        object: ObjectRef,
        key: String,
        copy: bool,
        value: Result<Secret, String>,
    },
    /// Lines of the followed log. `finished` says the stream has ended — the
    /// pod went, the connection dropped, or `kubectl` refused — and when it
    /// refused, the last line says why.
    LogLines {
        target: LogFollow,
        lines: Vec<String>,
        finished: bool,
    },
    /// What describe or `get -o yaml` said about one object, as lines.
    Text {
        scope: usize,
        kind: TextKind,
        object: ObjectRef,
        text: Result<Vec<String>, String>,
    },
    Deleted {
        scope: usize,
        key: PodKey,
        error: Option<String>,
    },
    /// A rollout restart or a scale went out: `verb` names which.
    Acted {
        scope: usize,
        verb: &'static str,
        object: ObjectRef,
        error: Option<String>,
    },
    Owner {
        scope: usize,
        object: ObjectRef,
        replicas: Result<Replicas, String>,
    },
    Stopped,
}

/// Which one-shot text a [`Event::Text`] carries.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TextKind {
    Describe,
    Yaml,
}

/// The worker's own state, apart from the thread it usually runs on, so a
/// test can drive it with a clock of its own.
pub struct Watcher {
    source: Box<dyn KubeSource>,
    events: Sender<Event>,
    /// Each scope and when it is next worth reading. One cadence each, so a
    /// dead cluster backing off never slows a live one.
    scopes: Vec<(Scope, Cadence)>,
    showing: Option<(usize, Kind)>,
    /// When the open tab's kind, when it is not pods, is next read.
    kind_cadence: Cadence,
    fast: Duration,
    /// The stream on and the flag that tells its reader the pane has moved
    /// on.
    follow: Option<(LogFollow, Arc<AtomicBool>)>,
    /// The process behind the stream, in a slot the [`Handle`] shares: a
    /// quit that gives up waiting for this thread still kills it.
    follow_child: Arc<Mutex<Option<Child>>>,
}

impl Watcher {
    #[must_use]
    pub fn new(
        source: Box<dyn KubeSource>,
        events: Sender<Event>,
        scopes: Vec<Scope>,
        fast: Duration,
        follow_child: Arc<Mutex<Option<Child>>>,
    ) -> Self {
        Self {
            source,
            events,
            scopes: scopes
                .into_iter()
                .map(|scope| (scope, Cadence::new(HIDDEN_REFRESH)))
                .collect(),
            showing: None,
            kind_cadence: Cadence::new(fast),
            fast,
            follow: None,
            follow_child,
        }
    }

    /// The scope a request names, or why it cannot be served.
    fn scope(&self, index: usize) -> Result<&Scope> {
        self.scopes
            .get(index)
            .map(|(scope, _)| scope)
            .ok_or_else(|| anyhow!("tab {index} is no longer in config.toml"))
    }

    /// A change went out: the replacement is worth seeing at once, not in
    /// five seconds.
    fn read_again_if(&mut self, scope: usize, changed: bool) {
        if changed && let Some((_, cadence)) = self.scopes.get_mut(scope) {
            cadence.ask();
        }
    }

    /// One owner's replica counts.
    fn owner(&self, scope: usize, object: ObjectRef) {
        let replicas = self
            .scope(scope)
            .and_then(|held| self.source.owner(held, &object))
            .map_err(|error| format!("{error:#}"));
        let _ = self.events.send(Event::Owner {
            scope,
            object,
            replicas,
        });
    }

    /// One describe or yaml, answered as lines.
    fn text(&self, scope: usize, kind: TextKind, object: ObjectRef) {
        let text = self
            .scope(scope)
            .and_then(|held| match kind {
                TextKind::Describe => self.source.describe(held, &object),
                TextKind::Yaml => self.source.yaml(held, &object),
            })
            .map(|text| text.lines().map(str::to_owned).collect())
            .map_err(|error| format!("{error:#}"));
        let _ = self.events.send(Event::Text {
            scope,
            kind,
            object,
            text,
        });
    }

    /// Opens one stream, closing whatever was open. What `kubectl` refuses
    /// goes into the pane where the user is looking, as the stream's one and
    /// only line.
    fn start_follow(&mut self, target: LogFollow) {
        self.unfollow();
        let tail = self
            .scope(target.scope)
            .and_then(|scope| self.source.logs(scope, &target));
        match tail {
            Ok(LogTail {
                child,
                stdout,
                stderr,
            }) => {
                let cancelled = Arc::new(AtomicBool::new(false));
                *self
                    .follow_child
                    .lock()
                    .unwrap_or_else(|held| held.into_inner()) = child;
                self.follow = Some((target.clone(), Arc::clone(&cancelled)));
                let events = self.events.clone();
                let _ = thread::Builder::new()
                    .name("az-tui-log".into())
                    .spawn(move || stream(stdout, stderr, target, &cancelled, &events));
            }
            Err(error) => {
                let _ = self.events.send(Event::LogLines {
                    target,
                    lines: vec![format!("\u{2026} {error:#}")],
                    finished: true,
                });
            }
        }
    }

    /// Closes the stream: the process is killed and reaped, and its reader
    /// told to say nothing more.
    fn unfollow(&mut self) {
        if let Some((_, cancelled)) = self.follow.take() {
            cancelled.store(true, Ordering::SeqCst);
            kill(&self.follow_child);
        }
    }

    /// What the stream is on, for a test.
    #[cfg(test)]
    fn following(&self) -> Option<&LogFollow> {
        self.follow.as_ref().map(|(target, _)| target)
    }

    /// One request. Answers whether to keep going.
    pub fn handle(&mut self, request: Request) -> bool {
        match request {
            Request::Stop => return false,
            Request::Showing(index, kind) => {
                let scope_changed = self.showing.map(|(held, _)| held) != Some(index);
                let kind_changed = self.showing.map(|(_, held)| held) != Some(kind);
                self.showing = Some((index, kind));
                if scope_changed {
                    for (at, (_, cadence)) in self.scopes.iter_mut().enumerate() {
                        cadence.set_base(if at == index {
                            self.fast
                        } else {
                            HIDDEN_REFRESH
                        });
                    }
                    if let Some((_, cadence)) = self.scopes.get_mut(index) {
                        cadence.ask();
                    }
                }
                if (scope_changed || kind_changed) && kind != Kind::Pods {
                    self.kind_cadence = Cadence::new(self.fast);
                }
            }
            Request::Refresh(index) => {
                if let Some((_, cadence)) = self.scopes.get_mut(index) {
                    cadence.ask();
                }
                if self
                    .showing
                    .is_some_and(|(held, kind)| held == index && kind != Kind::Pods)
                {
                    self.kind_cadence.ask();
                }
            }
            Request::SecretValue {
                scope,
                object,
                key,
                copy,
            } => {
                let value = self
                    .scope(scope)
                    .and_then(|held| self.source.secret_value(held, &object, &key))
                    .map_err(|error| format!("{error:#}"));
                let _ = self.events.send(Event::SecretValue {
                    scope,
                    object,
                    key,
                    copy,
                    value,
                });
            }
            Request::Follow(target) => self.start_follow(target),
            Request::Unfollow => self.unfollow(),
            Request::Describe { scope, object } => self.text(scope, TextKind::Describe, object),
            Request::Yaml { scope, object } => self.text(scope, TextKind::Yaml, object),
            Request::Delete { scope, key } => {
                let error = self
                    .scope(scope)
                    .and_then(|held| self.source.delete_pod(held, &key))
                    .err()
                    .map(|error| format!("{error:#}"));
                self.read_again_if(scope, error.is_none());
                let _ = self.events.send(Event::Deleted { scope, key, error });
            }
            Request::RolloutRestart { scope, object } => {
                let error = self
                    .scope(scope)
                    .and_then(|held| self.source.rollout_restart(held, &object))
                    .err()
                    .map(|error| format!("{error:#}"));
                self.read_again_if(scope, error.is_none());
                let _ = self.events.send(Event::Acted {
                    scope,
                    verb: "rollout restart",
                    object,
                    error,
                });
            }
            Request::Scale {
                scope,
                object,
                replicas,
            } => {
                let error = self
                    .scope(scope)
                    .and_then(|held| self.source.scale(held, &object, replicas))
                    .err()
                    .map(|error| format!("{error:#}"));
                self.read_again_if(scope, error.is_none());
                let _ = self.events.send(Event::Acted {
                    scope,
                    verb: "scale",
                    object: object.clone(),
                    error,
                });
                // The new count is worth seeing at once.
                self.owner(scope, object);
            }
            Request::Owner { scope, object } => self.owner(scope, object),
        }
        true
    }

    /// Reads one scope: the one on screen when it is due, else the first
    /// other that is. One read a call, so a request sent during a round is
    /// taken between two reads rather than after the last.
    pub fn poll(&mut self, now: Instant) {
        // The open tab's other kind first: it is what is on screen.
        if let Some((index, kind)) = self.showing
            && kind != Kind::Pods
            && self.kind_cadence.is_due(now)
            && let Some((scope, _)) = self.scopes.get(index)
        {
            let _ = self.events.send(Event::Reading(index));
            let failed = match kind {
                Kind::Events => {
                    let events = self
                        .source
                        .events(scope)
                        .map_err(|error| format!("{error:#}"));
                    let failed = events.is_err();
                    let _ = self.events.send(Event::Events {
                        scope: index,
                        events,
                    });
                    failed
                }
                Kind::ConfigMaps => {
                    let configmaps = self
                        .source
                        .configmaps(scope)
                        .map_err(|error| format!("{error:#}"));
                    let failed = configmaps.is_err();
                    let _ = self.events.send(Event::ConfigMaps {
                        scope: index,
                        configmaps,
                    });
                    failed
                }
                Kind::Secrets => {
                    let secrets = self
                        .source
                        .secrets(scope)
                        .map_err(|error| format!("{error:#}"));
                    let failed = secrets.is_err();
                    let _ = self.events.send(Event::Secrets {
                        scope: index,
                        secrets,
                    });
                    failed
                }
                Kind::Pods => false,
            };
            self.kind_cadence.polled(now, failed);
            return;
        }
        let Some(index) = self.next_due(now) else {
            return;
        };
        let (scope, cadence) = &mut self.scopes[index];
        let _ = self.events.send(Event::Reading(index));
        let pods = self
            .source
            .pods(scope)
            .map_err(|error| format!("{error:#}"));
        cadence.polled(now, pods.is_err());
        let _ = self.events.send(Event::Pods { scope: index, pods });
    }

    /// Whether anything is due at `now`, for a test that wants the whole
    /// round in one call.
    #[cfg(test)]
    fn anything_due(&self, now: Instant) -> bool {
        self.next_due(now).is_some()
            || self
                .showing
                .is_some_and(|(_, kind)| kind != Kind::Pods && self.kind_cadence.is_due(now))
    }

    fn next_due(&self, now: Instant) -> Option<usize> {
        if let Some((index, _)) = self.showing
            && self
                .scopes
                .get(index)
                .is_some_and(|(_, cadence)| cadence.is_due(now))
        {
            return Some(index);
        }
        self.scopes
            .iter()
            .position(|(_, cadence)| cadence.is_due(now))
    }

    /// Every read that is due at `now`, for a test that wants the whole
    /// round in one call.
    #[cfg(test)]
    pub(crate) fn poll_all(&mut self, now: Instant) {
        while self.anything_due(now) {
            self.poll(now);
        }
    }

    /// How long until something is due, or `None` while nothing ever will be
    /// on its own.
    #[must_use]
    pub fn until_due(&self, now: Instant) -> Option<Duration> {
        let kind = self
            .showing
            .filter(|(_, kind)| *kind != Kind::Pods)
            .and_then(|_| self.kind_cadence.until_due(now));
        self.scopes
            .iter()
            .filter_map(|(_, cadence)| cadence.until_due(now))
            .chain(kind)
            .min()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.unfollow();
    }
}

/// Kills and reaps whatever process the slot holds.
fn kill(slot: &Mutex<Option<Child>>) {
    if let Some(mut child) = slot.lock().unwrap_or_else(|held| held.into_inner()).take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// The reader behind a follow: one event per line until the stream ends, then
/// one saying so, with whatever `kubectl` complained about on the way out.
// ponytail: one event per line; read with fill_buf and split if a chatty pod
// ever shows up in a profile.
fn stream(
    stdout: Box<dyn Read + Send>,
    stderr: Option<Box<dyn Read + Send>>,
    target: LogFollow,
    cancelled: &AtomicBool,
    events: &Sender<Event>,
) {
    // Bytes, not `lines()`: one line a pod prints that is not UTF-8 would
    // otherwise end the stream where the pod is still writing.
    let mut reader = BufReader::new(stdout);
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        match reader.read_until(b'\n', &mut bytes) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if cancelled.load(Ordering::SeqCst) {
            return;
        }
        let line = String::from_utf8_lossy(&bytes)
            .trim_end_matches(['\n', '\r'])
            .to_owned();
        let sent = events.send(Event::LogLines {
            target: target.clone(),
            lines: vec![line],
            finished: false,
        });
        if sent.is_err() {
            return;
        }
    }
    if cancelled.load(Ordering::SeqCst) {
        return;
    }
    let mut complaint = String::new();
    if let Some(mut stderr) = stderr {
        let _ = stderr.read_to_string(&mut complaint);
    }
    let lines = if complaint.trim().is_empty() {
        Vec::new()
    } else {
        vec![format!("\u{2026} {}", kubectl_error(&complaint))]
    };
    let _ = events.send(Event::LogLines {
        target,
        lines,
        finished: true,
    });
}

/// The handle the main thread holds: requests in, events out.
pub struct Handle {
    requests: Sender<Request>,
    events: Receiver<Event>,
    stopped: Cell<bool>,
    /// The thread, joined when the handle goes so a child it holds is killed
    /// before the process is: a process on its way out runs no destructor
    /// on another thread.
    thread: Option<thread::JoinHandle<()>>,
    /// The followed log's process, shared with the worker, for a quit that
    /// gives up on the join before the worker could kill it.
    follow_child: Arc<Mutex<Option<Child>>>,
}

/// How long a quit waits for the worker to finish what it has in hand. A
/// read of an unreachable cluster can take ten seconds, and a quit is not
/// worth that; a worker with nothing in hand answers in a millisecond.
const STOP_GRACE: Duration = Duration::from_secs(2);

impl Handle {
    /// Starts the worker on its own thread. It ends when the handle is
    /// dropped.
    pub fn spawn(source: Box<dyn KubeSource>, scopes: Vec<Scope>, fast: Duration) -> Result<Self> {
        let (request_sender, request_receiver) = mpsc::channel();
        let (event_sender, event_receiver) = mpsc::channel();
        let follow_child = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&follow_child);
        let thread = thread::Builder::new()
            .name("az-tui-kube".into())
            .spawn(move || {
                watch(
                    Watcher::new(source, event_sender, scopes, fast, slot),
                    &request_receiver,
                );
            })
            .context("failed to start the cluster worker")?;
        Ok(Self {
            requests: request_sender,
            events: event_receiver,
            stopped: Cell::new(false),
            thread: Some(thread),
            follow_child,
        })
    }

    /// Tells the worker what is worth doing. Fails only when it is gone.
    pub fn send(&self, request: Request) -> Result<()> {
        self.requests
            .send(request)
            .context("the cluster worker stopped")
    }

    /// The next event, if one is waiting.
    pub fn try_event(&self) -> Option<Event> {
        match self.events.try_recv() {
            Ok(event) => Some(event),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                (!self.stopped.replace(true)).then_some(Event::Stopped)
            }
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Stop);
        let Some(thread) = self.thread.take() else {
            return;
        };
        // Joined from a thread of its own, so the wait can be given up: a
        // worker mid-read finishes that read first, and the process does not
        // stand around for it.
        let (done, finished) = mpsc::channel();
        let _ = thread::Builder::new()
            .name("az-tui-kube-stop".into())
            .spawn(move || {
                let _ = thread.join();
                let _ = done.send(());
            });
        let _ = finished.recv_timeout(STOP_GRACE);
        // A worker still inside a read has not run its own drop: the stream's
        // process is killed from here rather than left to outlive the TUI.
        kill(&self.follow_child);
    }
}

/// The loop: read whatever is due, then wait until the next thing is or a
/// request arrives, whichever comes first.
fn watch(mut watcher: Watcher, requests: &Receiver<Request>) {
    loop {
        watcher.poll(Instant::now());
        let wait = watcher
            .until_due(Instant::now())
            .unwrap_or(Duration::from_secs(3600));
        match requests.recv_timeout(wait) {
            Ok(request) => {
                if !watcher.handle(request) {
                    return;
                }
                // Everything else waiting is taken now, so a burst of requests
                // costs one poll rather than one each.
                while let Ok(request) = requests.try_recv() {
                    if !watcher.handle(request) {
                        return;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;

    /// One pod as `kubectl get pods -o json` lists it, with the parts a case
    /// needs and nothing else.
    fn item(name: &str, extra: Value) -> Value {
        let mut base = json!({
            "metadata": {
                "name": name,
                "namespace": "dev",
                "creationTimestamp": "2026-08-30T10:00:00Z",
                "labels": {"app": "orders-api", "pod-template-hash": "7d9f5b"},
                "ownerReferences": [{"kind": "ReplicaSet", "name": "orders-api-7d9f5b", "controller": true}]
            },
            "spec": {
                "nodeName": "aks-nodepool1-0",
                "containers": [{"name": "api", "image": "myacr.azurecr.io/team/orders-api:1.2.3"}]
            },
            "status": {
                "phase": "Running",
                "podIP": "10.0.0.7",
                "containerStatuses": [{
                    "name": "api", "ready": true, "restartCount": 2,
                    "image": "myacr.azurecr.io/team/orders-api:1.2.3",
                    "state": {"running": {"startedAt": "2026-08-30T10:00:05Z"}},
                    "lastState": {"terminated": {"reason": "OOMKilled", "exitCode": 137}}
                }]
            }
        });
        merge(&mut base, extra);
        base
    }

    fn merge(base: &mut Value, extra: Value) {
        match (base, extra) {
            (Value::Object(base), Value::Object(extra)) => {
                for (key, value) in extra {
                    match base.get_mut(&key) {
                        Some(held) if held.is_object() && value.is_object() => merge(held, value),
                        _ => {
                            base.insert(key, value);
                        }
                    }
                }
            }
            (base, extra) => *base = extra,
        }
    }

    pub(crate) fn pod(cluster: &str, namespace: &str, name: &str, status: &str) -> Pod {
        Pod {
            key: PodKey {
                cluster: cluster.to_owned(),
                namespace: namespace.to_owned(),
                name: name.to_owned(),
            },
            status: status.to_owned(),
            ready: (1, 1),
            restarts: 0,
            created: Timestamp::parse("2026-08-30T10:00:00Z"),
            node: "aks-nodepool1-0".to_owned(),
            ip: "10.0.0.7".to_owned(),
            owner: Some(("Deployment".to_owned(), "orders-api".to_owned())),
            containers: vec![Container {
                name: "api".to_owned(),
                image: "myacr.azurecr.io/team/orders-api:1.2.3".to_owned(),
                ready: true,
                restarts: 0,
                state: "Running".to_owned(),
                last_termination: None,
            }],
            labels: vec![("app".to_owned(), "orders-api".to_owned())],
        }
    }

    /// A pod in trouble, which is what the badge counts and the glyph paints.
    pub(crate) fn crashing(cluster: &str, namespace: &str, name: &str) -> Pod {
        let mut pod = pod(cluster, namespace, name, "CrashLoopBackOff");
        pod.ready = (0, 1);
        pod.restarts = 9;
        pod.containers[0].ready = false;
        pod.containers[0].restarts = 9;
        pod.containers[0].state = "CrashLoopBackOff".to_owned();
        pod.containers[0].last_termination = Some(("Error".to_owned(), 1));
        pod
    }

    pub(crate) fn scope(cluster: &str, namespace: Option<&str>) -> Scope {
        Scope {
            cluster: cluster.to_owned(),
            context: format!("aks-{cluster}"),
            namespace: namespace.map(str::to_owned),
        }
    }

    #[test]
    fn a_pod_reads_its_ready_count_restarts_owner_node_and_containers_from_kubectls_json() {
        let pod = Pod::from_json("qa", &item("orders-api-7d9f5b-abc12", json!({}))).unwrap();
        assert_eq!(pod.key.cluster, "qa");
        assert_eq!(pod.key.namespace, "dev");
        assert_eq!(pod.key.name, "orders-api-7d9f5b-abc12");
        assert_eq!(pod.status, "Running");
        assert_eq!(pod.ready_label(), "1/1");
        assert_eq!(pod.restarts, 2);
        assert_eq!(pod.node, "aks-nodepool1-0");
        assert_eq!(pod.ip, "10.0.0.7");
        assert_eq!(
            pod.created.map(Timestamp::to_rfc3339).as_deref(),
            Some("2026-08-30T10:00:00Z")
        );
        assert_eq!(
            pod.owner,
            Some(("Deployment".to_owned(), "orders-api".to_owned()))
        );
        assert_eq!(pod.owner_label(), "Deployment/orders-api");
        assert_eq!(pod.owner_name(), "orders-api");
        assert_eq!(pod.containers.len(), 1);
        assert_eq!(pod.containers[0].state, "Running");
        assert_eq!(
            pod.containers[0].last_termination,
            Some(("OOMKilled".to_owned(), 137))
        );
        assert_eq!(pod.label("app"), Some("orders-api"));
        assert_eq!(pod.app(), Some("orders-api"));
        assert!(pod.restartable());
        assert_eq!(pod.glyph(), "\u{25cf}");
        assert!(Pod::from_json("qa", &json!({"metadata": {}})).is_none());
        // And it survives the cache.
        let written = serde_json::to_string(&pod).unwrap();
        assert_eq!(serde_json::from_str::<Pod>(&written).unwrap(), pod);
    }

    #[test]
    fn the_status_word_follows_kubectl_for_running_pending_creating_crashloop_error_completed_terminating_and_init()
     {
        let cases = [
            (json!({}), "Running", "\u{25cf}"),
            (
                json!({"status": {"phase": "Pending", "containerStatuses": []}}),
                "Pending",
                "\u{25d0}",
            ),
            (
                json!({"status": {"phase": "Pending", "containerStatuses": [
                    {"name": "api", "ready": false, "state": {"waiting": {"reason": "ContainerCreating"}}}
                ]}}),
                "ContainerCreating",
                "\u{25d0}",
            ),
            (
                json!({"status": {"containerStatuses": [
                    {"name": "api", "ready": false, "restartCount": 9, "state": {"waiting": {"reason": "CrashLoopBackOff"}}}
                ]}}),
                "CrashLoopBackOff",
                "\u{2717}",
            ),
            (
                json!({"status": {"phase": "Failed", "containerStatuses": [
                    {"name": "api", "ready": false, "state": {"terminated": {"reason": "Error", "exitCode": 1}}}
                ]}}),
                "Error",
                "\u{2717}",
            ),
            (
                json!({"status": {"phase": "Failed", "containerStatuses": [
                    {"name": "api", "ready": false, "state": {"terminated": {"exitCode": 137}}}
                ]}}),
                "ExitCode:137",
                "\u{2717}",
            ),
            (
                json!({"status": {"phase": "Succeeded", "containerStatuses": [
                    {"name": "api", "ready": false, "state": {"terminated": {"reason": "Completed", "exitCode": 0}}}
                ]}}),
                "Completed",
                "\u{2713}",
            ),
            // A sidecar that finished beside a server still running reads as
            // running, the way kubectl puts it back.
            (
                json!({"spec": {"containers": [{"name": "api"}, {"name": "init-db"}]},
                       "status": {"containerStatuses": [
                    {"name": "api", "ready": true, "state": {"running": {}}},
                    {"name": "init-db", "ready": false, "state": {"terminated": {"reason": "Completed", "exitCode": 0}}}
                ]}}),
                "Running",
                "\u{25d0}",
            ),
            (
                json!({"metadata": {"deletionTimestamp": "2026-08-30T11:00:00Z"}}),
                "Terminating",
                "\u{25d0}",
            ),
            (
                json!({"spec": {"initContainers": [{"name": "migrate"}, {"name": "seed"}]},
                       "status": {"phase": "Pending", "initContainerStatuses": [
                    {"name": "migrate", "state": {"terminated": {"exitCode": 0}}},
                    {"name": "seed", "state": {"running": {}}}
                ]}}),
                "Init:1/2",
                "\u{25d0}",
            ),
            (
                json!({"spec": {"initContainers": [{"name": "migrate"}]},
                       "status": {"phase": "Pending", "initContainerStatuses": [
                    {"name": "migrate", "state": {"waiting": {"reason": "CrashLoopBackOff"}}}
                ]}}),
                "Init:CrashLoopBackOff",
                "\u{2717}",
            ),
            (
                json!({"status": {"phase": "Failed", "reason": "Evicted", "containerStatuses": []}}),
                "Evicted",
                "\u{2717}",
            ),
            (
                json!({"status": {"containerStatuses": [
                    {"name": "api", "ready": false, "state": {"waiting": {"reason": "ImagePullBackOff"}}}
                ]}}),
                "ImagePullBackOff",
                "\u{2717}",
            ),
        ];
        for (extra, word, glyph) in cases {
            let pod = Pod::from_json("qa", &item("p", extra)).unwrap();
            assert_eq!(pod.status, word);
            assert_eq!(pod.glyph(), glyph, "{word}");
        }
    }

    #[test]
    fn a_replica_set_owner_reads_as_its_deployment_when_the_template_hash_says_so() {
        let deployment = Pod::from_json("qa", &item("p", json!({}))).unwrap();
        assert_eq!(
            deployment.owner,
            Some(("Deployment".to_owned(), "orders-api".to_owned()))
        );
        let stateful = Pod::from_json(
            "qa",
            &item(
                "p",
                json!({"metadata": {"ownerReferences": [{"kind": "StatefulSet", "name": "redis", "controller": true}]}}),
            ),
        )
        .unwrap();
        assert_eq!(
            stateful.owner,
            Some(("StatefulSet".to_owned(), "redis".to_owned()))
        );
        let unhashed = Pod::from_json(
            "qa",
            &item(
                "p",
                json!({"metadata": {"labels": {"pod-template-hash": ""}, "ownerReferences": [{"kind": "ReplicaSet", "name": "orders-api-7d9f5b", "controller": true}]}}),
            ),
        )
        .unwrap();
        assert_eq!(
            unhashed.owner,
            Some(("ReplicaSet".to_owned(), "orders-api-7d9f5b".to_owned()))
        );
        let bare = Pod::from_json(
            "qa",
            &item("p", json!({"metadata": {"ownerReferences": []}})),
        )
        .unwrap();
        assert_eq!(bare.owner, None);
        assert!(!bare.restartable());
        assert_eq!(bare.owner_label(), "\u{2014}");
    }

    #[test]
    fn kubectl_errors_read_as_the_one_line_that_says_what_to_fix() {
        assert_eq!(
            kubectl_error("error: context \"aks-qa\" does not exist\n"),
            "context \"aks-qa\" does not exist"
        );
        assert_eq!(
            kubectl_error(
                "E0830 12:00:00.000000   12345 memcache.go:265] couldn't get current server API group list\nUnable to connect to the server: getting credentials: exec: executable kubelogin not found\n\nIt looks like you are trying to use a client-go credential plugin\nSee https://kubernetes.io/docs/reference/access-authn-authz/authentication/#client-go-credential-plugins\n"
            ),
            "Unable to connect to the server: getting credentials: exec: executable kubelogin not found"
        );
        assert_eq!(
            kubectl_error(
                "ERROR: AADSTS700082: The refresh token has expired. Please run 'az login' to setup account.\nUnable to connect to the server: getting credentials: exec: executable kubelogin failed with exit code 1\n"
            ),
            "ERROR: AADSTS700082: The refresh token has expired. Please run 'az login' to setup account."
        );
        assert_eq!(
            kubectl_error(
                "Error from server (Forbidden): pods is forbidden: User \"j\" cannot list resource \"pods\" in API group \"\" in the namespace \"prod\"\n"
            ),
            "Error from server (Forbidden): pods is forbidden: User \"j\" cannot list resource \"pods\" in API group \"\" in the namespace \"prod\""
        );
        assert_eq!(kubectl_error("\n  \n"), "kubectl failed");
    }

    #[test]
    fn a_call_that_will_not_end_is_killed_at_the_cap_and_one_that_answers_is_read_whole() {
        let mut echo = Command::new("sh");
        echo.args(["-c", "printf out; printf err >&2"]);
        assert_eq!(run_capped(echo, Duration::from_secs(5)).unwrap(), "out");

        let mut fails = Command::new("sh");
        fails.args([
            "-c",
            "echo 'error: context \"x\" does not exist' >&2; exit 1",
        ]);
        let error = run_capped(fails, Duration::from_secs(5)).unwrap_err();
        assert_eq!(format!("{error:#}"), "context \"x\" does not exist");

        let mut hangs = Command::new("sleep");
        hangs.arg("30");
        let started = Instant::now();
        let error = run_capped(hangs, Duration::from_millis(200)).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "killed, not waited for"
        );
        assert!(format!("{error:#}").contains("kubelogin"), "{error:#}");

        let error = run_capped(Command::new("az-tui-no-such-program"), CALL_CAP).unwrap_err();
        assert!(format!("{error:#}").contains("not installed"), "{error:#}");
    }

    /// One scope read and what it answers.
    type Answer = (Scope, Result<Vec<Pod>, String>);

    /// A source over canned answers, counting what it was asked.
    #[derive(Clone, Default)]
    pub(crate) struct FakeKube {
        /// What each scope answers with; one with no entry answers nothing.
        pub answers: Arc<Mutex<Vec<Answer>>>,
        pub reads: Arc<Mutex<Vec<Scope>>>,
        pub describe_text: Arc<Mutex<String>>,
        pub yaml_text: Arc<Mutex<String>>,
        pub described: Arc<Mutex<Vec<ObjectRef>>>,
        /// Every change that went out: `("delete", "pod/x")`, `("scale 4",
        /// "deployment/y")`.
        pub acted: Arc<Mutex<Vec<(String, String)>>>,
        pub refuse_changes: Arc<AtomicBool>,
        pub replicas: Arc<Mutex<Replicas>>,
        pub events_list: Arc<Mutex<Vec<K8sEvent>>>,
        pub configmaps_list: Arc<Mutex<Vec<ConfigMap>>>,
        pub secrets_list: Arc<Mutex<Vec<SecretMeta>>>,
        /// Which kinds were read, in order.
        pub kinds_read: Arc<Mutex<Vec<Kind>>>,
        pub secret_values: Arc<Mutex<Vec<(String, String)>>>,
        pub log_text: Arc<Mutex<String>>,
        pub follows: Arc<Mutex<Vec<LogFollow>>>,
        /// Whether a follow hands out a real process — `sleep` — so a test
        /// can see it killed; its pids are kept here.
        pub with_children: Arc<AtomicBool>,
        pub children: Arc<Mutex<Vec<u32>>>,
        /// How long a pods read takes, for a case about a worker mid-read.
        pub read_delay: Arc<Mutex<Duration>>,
    }

    impl FakeKube {
        pub(crate) fn answer(&self, scope: &Scope, pods: Result<Vec<Pod>, &str>) {
            let mut answers = self.answers.lock().unwrap();
            answers.retain(|(held, _)| held != scope);
            answers.push((scope.clone(), pods.map_err(str::to_owned)));
        }
    }

    impl KubeSource for FakeKube {
        fn pods(&self, scope: &Scope) -> Result<Vec<Pod>> {
            self.reads.lock().unwrap().push(scope.clone());
            thread::sleep(*self.read_delay.lock().unwrap());
            let answers = self.answers.lock().unwrap();
            match answers.iter().find(|(held, _)| held == scope) {
                Some((_, Ok(pods))) => Ok(pods.clone()),
                Some((_, Err(message))) => Err(anyhow!(message.clone())),
                None => Ok(Vec::new()),
            }
        }

        fn events(&self, _scope: &Scope) -> Result<Vec<K8sEvent>> {
            self.kinds_read.lock().unwrap().push(Kind::Events);
            Ok(self.events_list.lock().unwrap().clone())
        }

        fn configmaps(&self, _scope: &Scope) -> Result<Vec<ConfigMap>> {
            self.kinds_read.lock().unwrap().push(Kind::ConfigMaps);
            Ok(self.configmaps_list.lock().unwrap().clone())
        }

        fn secrets(&self, scope: &Scope) -> Result<Vec<SecretMeta>> {
            self.kinds_read.lock().unwrap().push(Kind::Secrets);
            if scope.namespace.as_deref() == Some("prod") {
                bail!("Error from server (Forbidden): secrets is forbidden");
            }
            Ok(self.secrets_list.lock().unwrap().clone())
        }

        fn secret_value(&self, _scope: &Scope, object: &ObjectRef, key: &str) -> Result<Secret> {
            let values = self.secret_values.lock().unwrap();
            let held = values
                .iter()
                .find(|(held, _)| held == key)
                .map(|(_, value)| value.clone())
                .with_context(|| format!("{} has no key {key}", object.name))?;
            Ok(Secret::new(held))
        }

        fn delete_pod(&self, _scope: &Scope, key: &PodKey) -> Result<()> {
            self.acted
                .lock()
                .unwrap()
                .push(("delete".to_owned(), format!("pod/{}", key.name)));
            if self.refuse_changes.load(Ordering::SeqCst) {
                bail!(
                    "pods \"{}\" is forbidden: User \"j\" cannot delete",
                    key.name
                );
            }
            Ok(())
        }

        fn rollout_restart(&self, _scope: &Scope, object: &ObjectRef) -> Result<()> {
            self.acted
                .lock()
                .unwrap()
                .push(("rollout restart".to_owned(), object.slash()));
            if self.refuse_changes.load(Ordering::SeqCst) {
                bail!("deployments.apps \"{}\" is forbidden", object.name);
            }
            Ok(())
        }

        fn scale(&self, _scope: &Scope, object: &ObjectRef, replicas: u32) -> Result<()> {
            self.acted
                .lock()
                .unwrap()
                .push((format!("scale {replicas}"), object.slash()));
            if self.refuse_changes.load(Ordering::SeqCst) {
                bail!("deployments.apps \"{}\" is forbidden", object.name);
            }
            self.replicas.lock().unwrap().desired = i64::from(replicas);
            Ok(())
        }

        fn owner(&self, _scope: &Scope, object: &ObjectRef) -> Result<Replicas> {
            self.described.lock().unwrap().push(object.clone());
            Ok(*self.replicas.lock().unwrap())
        }

        fn describe(&self, _scope: &Scope, object: &ObjectRef) -> Result<String> {
            self.described.lock().unwrap().push(object.clone());
            if object.name == "gone" {
                bail!("Error from server (NotFound): pods \"gone\" not found");
            }
            Ok(self.describe_text.lock().unwrap().clone())
        }

        fn yaml(&self, _scope: &Scope, object: &ObjectRef) -> Result<String> {
            self.described.lock().unwrap().push(object.clone());
            Ok(self.yaml_text.lock().unwrap().clone())
        }

        fn logs(&self, _scope: &Scope, target: &LogFollow) -> Result<LogTail> {
            self.follows.lock().unwrap().push(target.clone());
            if target.container.as_deref() == Some("missing") {
                bail!("container missing is not valid for pod {}", target.key.name);
            }
            let child = if self.with_children.load(Ordering::SeqCst) {
                let child = Command::new("sleep")
                    .arg("30")
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .spawn()?;
                self.children.lock().unwrap().push(child.id());
                Some(child)
            } else {
                None
            };
            Ok(LogTail {
                child,
                stdout: Box::new(std::io::Cursor::new(self.log_text.lock().unwrap().clone())),
                stderr: Some(Box::new(std::io::Cursor::new(String::new()))),
            })
        }
    }

    pub(crate) fn key(cluster: &str, namespace: &str, name: &str) -> PodKey {
        PodKey {
            cluster: cluster.to_owned(),
            namespace: namespace.to_owned(),
            name: name.to_owned(),
        }
    }

    pub(crate) fn follow(scope: usize, name: &str) -> LogFollow {
        LogFollow {
            scope,
            key: key("qa", "dev", name),
            container: None,
            previous: false,
        }
    }

    /// Every event the stream sends, waited for until it says it finished.
    fn stream_events(receiver: &Receiver<Event>) -> Vec<(LogFollow, Vec<String>, bool)> {
        let mut events = Vec::new();
        while let Ok(event) = receiver.recv_timeout(Duration::from_secs(5)) {
            if let Event::LogLines {
                target,
                lines,
                finished,
            } = event
            {
                events.push((target, lines, finished));
                if finished {
                    break;
                }
            }
        }
        events
    }

    #[test]
    fn following_a_pod_streams_its_lines_then_says_the_stream_ended() {
        let fake = FakeKube::default();
        *fake.log_text.lock().unwrap() =
            "2026-09-12T10:00:00Z hello\n2026-09-12T10:00:01Z world\n".to_owned();
        let (mut watcher, receiver) = watcher(&fake, Duration::from_secs(5));
        let target = LogFollow {
            container: Some("api".to_owned()),
            ..follow(0, "a")
        };
        watcher.handle(Request::Follow(target.clone()));
        assert_eq!(watcher.following(), Some(&target));
        let events = stream_events(&receiver);
        let lines: Vec<String> = events
            .iter()
            .flat_map(|(_, lines, _)| lines.clone())
            .collect();
        assert_eq!(
            lines,
            vec!["2026-09-12T10:00:00Z hello", "2026-09-12T10:00:01Z world"]
        );
        assert!(events.iter().all(|(held, _, _)| *held == target));
        assert_eq!(events.last().map(|(_, _, finished)| *finished), Some(true));
        assert_eq!(*fake.follows.lock().unwrap(), vec![target]);
    }

    #[test]
    fn following_another_pod_replaces_the_stream_and_a_bad_container_says_so_in_the_pane() {
        let fake = FakeKube::default();
        *fake.log_text.lock().unwrap() = "line\n".to_owned();
        let (mut watcher, receiver) = watcher(&fake, Duration::from_secs(5));
        let first = follow(0, "a");
        let second = LogFollow {
            previous: true,
            ..follow(2, "b")
        };
        watcher.handle(Request::Follow(first.clone()));
        let _ = stream_events(&receiver);
        watcher.handle(Request::Follow(second.clone()));
        assert_eq!(watcher.following(), Some(&second));
        let events = stream_events(&receiver);
        assert!(
            events.iter().all(|(held, _, _)| *held == second),
            "{events:?}"
        );
        assert_eq!(*fake.follows.lock().unwrap(), vec![first, second.clone()]);

        watcher.handle(Request::Unfollow);
        assert_eq!(watcher.following(), None);

        let bad = LogFollow {
            container: Some("missing".to_owned()),
            ..follow(0, "c")
        };
        watcher.handle(Request::Follow(bad.clone()));
        let events = stream_events(&receiver);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, bad);
        assert_eq!(
            events[0].1,
            vec!["\u{2026} container missing is not valid for pod c"]
        );
        assert!(events[0].2);
        assert_eq!(watcher.following(), None);

        let lost = follow(9, "d");
        watcher.handle(Request::Follow(lost));
        let events = stream_events(&receiver);
        assert!(events[0].1[0].contains("tab 9 is no longer"), "{events:?}");
    }

    #[test]
    fn a_describe_and_a_yaml_answer_with_their_text_or_with_the_refusal() {
        let fake = FakeKube::default();
        *fake.describe_text.lock().unwrap() = "Name: a\nNamespace: dev\n".to_owned();
        *fake.yaml_text.lock().unwrap() = "kind: Pod\n".to_owned();
        let (mut watcher, receiver) = watcher(&fake, Duration::from_secs(5));
        let object = ObjectRef::pod(&key("qa", "dev", "a"));
        watcher.handle(Request::Describe {
            scope: 0,
            object: object.clone(),
        });
        watcher.handle(Request::Yaml {
            scope: 0,
            object: object.clone(),
        });
        watcher.handle(Request::Describe {
            scope: 0,
            object: ObjectRef::pod(&key("qa", "dev", "gone")),
        });
        let texts: Vec<(TextKind, String, Result<Vec<String>, String>)> = drain(&receiver)
            .into_iter()
            .filter_map(|event| match event {
                Event::Text {
                    kind, object, text, ..
                } => Some((kind, object.name, text)),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts[0],
            (
                TextKind::Describe,
                "a".to_owned(),
                Ok(vec!["Name: a".to_owned(), "Namespace: dev".to_owned()])
            )
        );
        assert_eq!(
            texts[1],
            (
                TextKind::Yaml,
                "a".to_owned(),
                Ok(vec!["kind: Pod".to_owned()])
            )
        );
        assert!(matches!(&texts[2].2, Err(message) if message.contains("not found")));
        assert_eq!(object.slash(), "pod/a");
    }

    #[test]
    fn a_delete_a_rollout_and_a_scale_go_out_read_the_scope_again_and_say_how_they_went() {
        let fake = FakeKube::default();
        *fake.replicas.lock().unwrap() = Replicas {
            desired: 3,
            ready: 3,
        };
        let (mut watcher, receiver) = watcher(&fake, Duration::from_secs(5));
        watcher.handle(Request::Showing(0, Kind::Pods));
        let start = Instant::now();
        watcher.poll_all(start);
        let _ = drain(&receiver);

        watcher.handle(Request::Delete {
            scope: 0,
            key: key("qa", "dev", "a"),
        });
        assert_eq!(
            watcher.until_due(start + Duration::from_secs(1)),
            Some(Duration::ZERO),
            "the replacement is worth seeing at once"
        );
        watcher.poll_all(start + Duration::from_secs(1));
        let deployment = ObjectRef {
            kind: "deployment".to_owned(),
            namespace: "dev".to_owned(),
            name: "orders-api".to_owned(),
        };
        watcher.handle(Request::RolloutRestart {
            scope: 0,
            object: deployment.clone(),
        });
        watcher.handle(Request::Scale {
            scope: 0,
            object: deployment.clone(),
            replicas: 4,
        });
        watcher.handle(Request::Owner {
            scope: 0,
            object: deployment.clone(),
        });
        assert_eq!(
            *fake.acted.lock().unwrap(),
            vec![
                ("delete".to_owned(), "pod/a".to_owned()),
                (
                    "rollout restart".to_owned(),
                    "deployment/orders-api".to_owned()
                ),
                ("scale 4".to_owned(), "deployment/orders-api".to_owned()),
            ]
        );
        let events = drain(&receiver);
        assert!(
            matches!(
                &events[0],
                Event::Deleted {
                    scope: 0,
                    error: None,
                    ..
                }
            ),
            "{events:?}"
        );
        assert!(
            matches!(&events[1], Event::Reading(0)),
            "the scope is read again at once: {events:?}"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Acted {
                verb: "rollout restart",
                error: None,
                ..
            }
        )));
        let owners: Vec<i64> = events
            .iter()
            .filter_map(|event| match event {
                Event::Owner {
                    replicas: Ok(replicas),
                    ..
                } => Some(replicas.desired),
                _ => None,
            })
            .collect();
        assert_eq!(
            owners,
            [4, 4],
            "the scale re-reads the owner, and so does the ask"
        );

        // Refused: the message comes back and nothing is read again.
        fake.refuse_changes.store(true, Ordering::SeqCst);
        let _ = drain(&receiver);
        watcher.poll_all(start + Duration::from_secs(2));
        let _ = drain(&receiver);
        watcher.handle(Request::Delete {
            scope: 0,
            key: key("qa", "dev", "a"),
        });
        assert_ne!(
            watcher.until_due(start + Duration::from_secs(3)),
            Some(Duration::ZERO)
        );
        let events = drain(&receiver);
        assert!(
            matches!(
                &events[0],
                Event::Deleted { error: Some(message), .. } if message.contains("forbidden")
            ),
            "{events:?}"
        );
        watcher.handle(Request::Delete {
            scope: 9,
            key: key("qa", "dev", "a"),
        });
        let events = drain(&receiver);
        assert!(matches!(
            &events[0],
            Event::Deleted { error: Some(message), .. } if message.contains("tab 9")
        ));
    }

    #[test]
    fn an_event_a_configmap_and_a_secrets_shape_read_from_kubectls_json() {
        let event = K8sEvent::from_json(&json!({
            "metadata": {"name": "orders-worker.1", "namespace": "dev", "creationTimestamp": "2026-09-12T11:00:00Z"},
            "type": "Warning", "reason": "BackOff", "count": 17,
            "firstTimestamp": "2026-09-12T10:00:00Z", "lastTimestamp": "2026-09-12T12:00:00Z",
            "involvedObject": {"kind": "Pod", "name": "orders-worker-5c4d3e-q8zt", "namespace": "dev"},
            "message": "Back-off restarting failed container\n",
            "source": {"component": "kubelet"}
        }))
        .unwrap();
        assert_eq!(event.kind, "Warning");
        assert!(event.is_warning());
        assert_eq!(event.count, 17);
        assert_eq!(event.object.slash(), "pod/orders-worker-5c4d3e-q8zt");
        assert_eq!(event.message, "Back-off restarting failed container");
        assert_eq!(event.last.unwrap().to_rfc3339(), "2026-09-12T12:00:00Z");
        assert_eq!(event.source, "kubelet");
        // A new-style event: eventTime and a series.
        let event = K8sEvent::from_json(&json!({
            "metadata": {"name": "x", "namespace": "dev"},
            "eventTime": "2026-09-12T12:30:00Z",
            "series": {"count": 4, "lastObservedTime": "2026-09-12T12:45:00Z"},
            "reason": "Scheduled", "involvedObject": {"kind": "Pod", "name": "p"},
            "reportingComponent": "default-scheduler"
        }))
        .unwrap();
        assert_eq!(event.kind, "Normal");
        assert_eq!(event.count, 4);
        assert_eq!(event.last.unwrap().to_rfc3339(), "2026-09-12T12:45:00Z");
        assert_eq!(
            event.object.namespace, "dev",
            "the event's when the object names none"
        );
        assert_eq!(event.source, "default-scheduler");

        let configmap = ConfigMap::from_json(&json!({
            "metadata": {"name": "orders-config", "namespace": "dev", "creationTimestamp": "2026-09-01T00:00:00Z"},
            "data": {"LOG_LEVEL": "info", "APP": "orders"},
            "binaryData": {"blob": "AAECAw=="}
        }))
        .unwrap();
        assert_eq!(
            configmap.data,
            [
                ("APP".to_owned(), "orders".to_owned()),
                ("LOG_LEVEL".to_owned(), "info".to_owned()),
                ("blob".to_owned(), "<binary, 4 bytes>".to_owned())
            ]
        );
        assert_eq!(configmap.object().slash(), "configmap/orders-config");

        let secret = SecretMeta::from_json(&json!({
            "metadata": {"name": "db", "namespace": "dev"},
            "type": "Opaque",
            "data": {"password": "aHVudGVyMg==", "user": "YWRtaW4="}
        }))
        .unwrap();
        assert_eq!(
            secret.keys,
            [("password".to_owned(), 7), ("user".to_owned(), 5)]
        );
        let written = format!("{secret:?}");
        assert!(
            !written.contains("aHVudGVyMg"),
            "the data never crosses: {written}"
        );
        assert_eq!(base64_decode("aHVudGVyMg==").unwrap(), b"hunter2");
        assert_eq!(
            base64_decode("aHVudGVyMg").unwrap(),
            b"hunter2",
            "unpadded too"
        );
        assert_eq!(base64_decode("YWRt\naW4=").unwrap(), b"admin");
        assert!(base64_decode("not*base64").is_err());
        let value = Secret::new("hunter2");
        assert_eq!(format!("{value:?} {value}"), "[redacted] [redacted]");
        assert_eq!(value.expose(), "hunter2");
    }

    #[test]
    fn the_open_tabs_other_kind_is_read_on_the_fast_cadence_and_pods_keep_theirs() {
        let fake = FakeKube::default();
        let (mut watcher, receiver) = watcher(&fake, Duration::from_secs(5));
        watcher.handle(Request::Showing(0, Kind::Pods));
        let start = Instant::now();
        watcher.poll_all(start);
        assert!(fake.kinds_read.lock().unwrap().is_empty(), "pods only");

        watcher.handle(Request::Showing(0, Kind::Events));
        assert_eq!(watcher.until_due(start), Some(Duration::ZERO));
        watcher.poll_all(start);
        assert_eq!(*fake.kinds_read.lock().unwrap(), vec![Kind::Events]);
        assert!(drain(&receiver).iter().any(|event| matches!(
            event,
            Event::Events {
                scope: 0,
                events: Ok(_)
            }
        )),);
        // Five seconds on: the events again, and the pods of the open tab.
        watcher.poll_all(start + Duration::from_secs(5));
        assert_eq!(
            *fake.kinds_read.lock().unwrap(),
            vec![Kind::Events, Kind::Events]
        );
        assert_eq!(reads_of(&fake, "qa", "dev"), 2);
        // r reads both again at once.
        watcher.handle(Request::Refresh(0));
        watcher.poll_all(start + Duration::from_secs(6));
        assert_eq!(fake.kinds_read.lock().unwrap().len(), 3);
        assert_eq!(reads_of(&fake, "qa", "dev"), 3);

        // Another kind on another tab; a forbidden read backs off alone.
        watcher.handle(Request::Showing(2, Kind::Secrets));
        watcher.poll_all(start + Duration::from_secs(6));
        let events = drain(&receiver);
        assert!(
            events.iter().any(|event| matches!(
                event,
                Event::Secrets { scope: 2, secrets: Err(message) } if message.contains("Forbidden")
            )),
            "{events:?}"
        );
        watcher.poll_all(start + Duration::from_secs(11));
        assert_eq!(
            fake.kinds_read
                .lock()
                .unwrap()
                .iter()
                .filter(|k| **k == Kind::Secrets)
                .count(),
            1,
            "not again at five seconds: it doubled"
        );
        // Back to pods: nothing else is read.
        watcher.handle(Request::Showing(2, Kind::Pods));
        watcher.poll_all(start + Duration::from_secs(60));
        assert_eq!(
            fake.kinds_read
                .lock()
                .unwrap()
                .iter()
                .filter(|k| **k == Kind::Secrets)
                .count(),
            1
        );

        // A value: one key, decoded, and a key that is not there says so.
        let _ = drain(&receiver);
        fake.secret_values
            .lock()
            .unwrap()
            .push(("password".to_owned(), "hunter2".to_owned()));
        let object = ObjectRef {
            kind: "secret".to_owned(),
            namespace: "dev".to_owned(),
            name: "db".to_owned(),
        };
        watcher.handle(Request::SecretValue {
            scope: 0,
            object: object.clone(),
            key: "password".to_owned(),
            copy: true,
        });
        watcher.handle(Request::SecretValue {
            scope: 0,
            object,
            key: "missing".to_owned(),
            copy: false,
        });
        let events = drain(&receiver);
        assert!(
            matches!(
                &events[0],
                Event::SecretValue { copy: true, value: Ok(value), .. } if value.expose() == "hunter2"
            ),
            "{events:?}"
        );
        assert!(matches!(
            &events[1],
            Event::SecretValue { copy: false, value: Err(message), .. } if message.contains("no key missing")
        ));
    }

    /// Whether a process is still there to be signalled.
    fn alive(pid: u32) -> bool {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn dropping_the_handle_kills_the_stream_before_the_process_can_leave_it_behind() {
        let fake = FakeKube::default();
        fake.with_children.store(true, Ordering::SeqCst);
        let handle =
            Handle::spawn(Box::new(fake.clone()), scopes(), Duration::from_secs(5)).unwrap();
        handle.send(Request::Follow(follow(0, "a"))).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while fake.children.lock().unwrap().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let pid = fake.children.lock().unwrap()[0];
        assert!(alive(pid), "the stream is running");
        drop(handle);
        assert!(!alive(pid), "and gone with the handle");
    }

    fn scopes() -> Vec<Scope> {
        vec![
            scope("qa", Some("dev")),
            scope("qa", Some("qa")),
            scope("prod", Some("prod")),
        ]
    }

    fn watcher(fake: &FakeKube, fast: Duration) -> (Watcher, Receiver<Event>) {
        let (sender, receiver) = mpsc::channel();
        (
            Watcher::new(
                Box::new(fake.clone()),
                sender,
                scopes(),
                fast,
                Arc::default(),
            ),
            receiver,
        )
    }

    fn drain(receiver: &Receiver<Event>) -> Vec<Event> {
        std::iter::from_fn(|| receiver.try_recv().ok()).collect()
    }

    fn pods_events(events: &[Event]) -> Vec<(usize, Result<usize, String>)> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Pods { scope, pods } => {
                    Some((*scope, pods.as_ref().map(Vec::len).map_err(Clone::clone)))
                }
                _ => None,
            })
            .collect()
    }

    fn reads_of(fake: &FakeKube, cluster: &str, namespace: &str) -> usize {
        fake.reads
            .lock()
            .unwrap()
            .iter()
            .filter(|held| held.cluster == cluster && held.namespace.as_deref() == Some(namespace))
            .count()
    }

    #[test]
    fn every_scope_is_read_at_once_the_open_one_first_then_on_its_own_cadence() {
        let fake = FakeKube::default();
        fake.answer(
            &scope("qa", Some("qa")),
            Ok(vec![pod("qa", "qa", "a", "Running")]),
        );
        let (mut watcher, receiver) = watcher(&fake, Duration::from_secs(5));
        watcher.handle(Request::Showing(1, Kind::Pods));
        let start = Instant::now();
        assert_eq!(watcher.until_due(start), Some(Duration::ZERO));
        watcher.poll_all(start);
        let events = drain(&receiver);
        assert!(matches!(events[0], Event::Reading(1)), "the open tab first");
        assert_eq!(
            pods_events(&events),
            vec![(1, Ok(1)), (0, Ok(0)), (2, Ok(0))]
        );
        // The open tab again after five seconds; the others not for thirty.
        watcher.poll_all(start + Duration::from_secs(4));
        assert_eq!(fake.reads.lock().unwrap().len(), 3);
        assert_eq!(
            watcher.until_due(start + Duration::from_secs(4)),
            Some(Duration::from_secs(1))
        );
        watcher.poll_all(start + Duration::from_secs(5));
        assert_eq!(reads_of(&fake, "qa", "qa"), 2);
        assert_eq!(reads_of(&fake, "qa", "dev"), 1);
        watcher.poll_all(start + HIDDEN_REFRESH);
        assert_eq!(reads_of(&fake, "qa", "dev"), 2);
        assert_eq!(reads_of(&fake, "prod", "prod"), 2);
    }

    #[test]
    fn switching_tabs_reads_the_new_one_at_once_and_slows_the_old_one_down() {
        let fake = FakeKube::default();
        let (mut watcher, _receiver) = watcher(&fake, Duration::from_secs(5));
        watcher.handle(Request::Showing(0, Kind::Pods));
        let start = Instant::now();
        watcher.poll_all(start);
        assert_eq!(fake.reads.lock().unwrap().len(), 3);

        watcher.handle(Request::Showing(2, Kind::Pods));
        assert_eq!(
            watcher.until_due(start + Duration::from_secs(1)),
            Some(Duration::ZERO)
        );
        watcher.poll_all(start + Duration::from_secs(1));
        assert_eq!(
            reads_of(&fake, "prod", "prod"),
            2,
            "read the moment it shows"
        );
        assert_eq!(reads_of(&fake, "qa", "dev"), 1);
        // Six seconds on: prod again on the fast cadence, dev still waiting.
        watcher.poll_all(start + Duration::from_secs(7));
        assert_eq!(reads_of(&fake, "prod", "prod"), 3);
        assert_eq!(reads_of(&fake, "qa", "dev"), 1);
        // The same tab again is not a switch.
        watcher.handle(Request::Showing(2, Kind::Pods));
        assert_ne!(
            watcher.until_due(start + Duration::from_secs(7)),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn a_scope_that_fails_backs_off_on_its_own_and_blanks_nothing_else() {
        let fake = FakeKube::default();
        fake.answer(
            &scope("qa", Some("dev")),
            Err("context \"aks-qa\" does not exist"),
        );
        fake.answer(
            &scope("prod", Some("prod")),
            Ok(vec![pod("prod", "prod", "a", "Running")]),
        );
        let (mut watcher, receiver) = watcher(&fake, Duration::from_secs(5));
        watcher.handle(Request::Showing(0, Kind::Pods));
        let start = Instant::now();
        watcher.poll_all(start);
        assert_eq!(
            pods_events(&drain(&receiver)),
            vec![
                (0, Err("context \"aks-qa\" does not exist".to_owned())),
                (1, Ok(0)),
                (2, Ok(1)),
            ]
        );
        // The failing scope waits ten seconds, not five.
        watcher.poll_all(start + Duration::from_secs(5));
        assert_eq!(reads_of(&fake, "qa", "dev"), 1);
        watcher.poll_all(start + Duration::from_secs(10));
        assert_eq!(reads_of(&fake, "qa", "dev"), 2);
        // Twenty more once it has failed twice; a clean read puts it back.
        watcher.poll_all(start + Duration::from_secs(20));
        assert_eq!(reads_of(&fake, "qa", "dev"), 2);
        fake.answer(&scope("qa", Some("dev")), Ok(Vec::new()));
        watcher.poll_all(start + Duration::from_secs(30));
        assert_eq!(reads_of(&fake, "qa", "dev"), 3);
        watcher.poll_all(start + Duration::from_secs(35));
        assert_eq!(reads_of(&fake, "qa", "dev"), 4, "back on the fast cadence");
    }

    #[test]
    fn a_refresh_reads_one_scope_again_at_once_and_a_zero_cadence_reads_only_when_asked() {
        let fake = FakeKube::default();
        let (mut watcher, _receiver) = watcher(&fake, Duration::ZERO);
        watcher.handle(Request::Showing(0, Kind::Pods));
        let start = Instant::now();
        watcher.poll_all(start);
        assert_eq!(fake.reads.lock().unwrap().len(), 3);
        // The open tab is never read again on its own; the hidden ones are.
        assert_eq!(
            watcher.until_due(start),
            Some(HIDDEN_REFRESH),
            "the hidden tabs' cadence is the next thing due"
        );
        watcher.poll_all(start + Duration::from_secs(20));
        assert_eq!(reads_of(&fake, "qa", "dev"), 1);
        watcher.handle(Request::Refresh(0));
        assert_eq!(
            watcher.until_due(start + Duration::from_secs(20)),
            Some(Duration::ZERO)
        );
        watcher.poll_all(start + Duration::from_secs(20));
        assert_eq!(reads_of(&fake, "qa", "dev"), 2);
        assert_eq!(reads_of(&fake, "qa", "qa"), 1, "only the one asked for");
        watcher.handle(Request::Refresh(9));
        assert_ne!(
            watcher.until_due(start + Duration::from_secs(20)),
            Some(Duration::ZERO)
        );
        assert!(!watcher.handle(Request::Stop));
    }

    #[test]
    fn the_handle_runs_the_worker_on_its_own_thread_and_says_once_when_it_stops() {
        let fake = FakeKube::default();
        fake.answer(
            &scope("qa", Some("dev")),
            Ok(vec![pod("qa", "dev", "a", "Running")]),
        );
        let handle = Handle::spawn(Box::new(fake), scopes(), Duration::from_secs(5)).unwrap();
        handle.send(Request::Showing(0, Kind::Pods)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = None;
        while Instant::now() < deadline && seen.is_none() {
            if let Some(Event::Pods { scope: 0, pods }) = handle.try_event() {
                seen = Some(pods.map(|pods| pods.len()));
            } else {
                thread::sleep(Duration::from_millis(10));
            }
        }
        assert_eq!(seen, Some(Ok(1)));
        handle.send(Request::Stop).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stopped = 0;
        while Instant::now() < deadline {
            match handle.try_event() {
                Some(Event::Stopped) => {
                    stopped += 1;
                    break;
                }
                Some(_) => {}
                None => thread::sleep(Duration::from_millis(10)),
            }
        }
        assert_eq!(stopped, 1);
        assert!(handle.try_event().is_none(), "Stopped is said once");
    }

    #[test]
    fn the_cap_holds_when_a_grandchild_keeps_the_pipes_open() {
        // What a credential plugin does: kubectl is killed at the cap, but the
        // plugin it spawned still holds stderr, and the drains must not be
        // waited for.
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 5 & sleep 5"]);
        let started = Instant::now();
        let error = run_capped(command, Duration::from_millis(200)).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert!(error.to_string().contains("did not answer"), "{error:#}");
    }

    #[test]
    fn a_line_that_is_not_utf8_does_not_end_the_stream() {
        let (sender, receiver) = mpsc::channel();
        let text: &[u8] = b"ok\n\xffbad\r\nafter\n";
        stream(
            Box::new(std::io::Cursor::new(text.to_vec())),
            None,
            follow(0, "a"),
            &AtomicBool::new(false),
            &sender,
        );
        let lines: Vec<(Vec<String>, bool)> = receiver
            .try_iter()
            .map(|event| match event {
                Event::LogLines {
                    lines, finished, ..
                } => (lines, finished),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            lines,
            [
                (vec!["ok".to_owned()], false),
                (vec!["\u{fffd}bad".to_owned()], false),
                (vec!["after".to_owned()], false),
                (Vec::new(), true),
            ]
        );
    }

    #[test]
    fn a_quit_that_gives_up_on_a_worker_mid_read_still_kills_the_stream() {
        let fake = FakeKube::default();
        fake.with_children.store(true, Ordering::SeqCst);
        let handle =
            Handle::spawn(Box::new(fake.clone()), scopes(), Duration::from_secs(5)).unwrap();
        handle.send(Request::Follow(follow(0, "a"))).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while fake.children.lock().unwrap().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let pid = fake.children.lock().unwrap()[0];
        assert!(alive(pid), "the stream is running");
        // The next read takes longer than the quit is willing to wait.
        *fake.read_delay.lock().unwrap() = STOP_GRACE * 3;
        let reads = fake.reads.lock().unwrap().len();
        handle.send(Request::Refresh(0)).unwrap();
        while fake.reads.lock().unwrap().len() == reads && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let started = Instant::now();
        drop(handle);
        assert!(started.elapsed() < STOP_GRACE * 2, "gave up on the join");
        assert!(!alive(pid), "and the stream went with the handle");
    }
}
