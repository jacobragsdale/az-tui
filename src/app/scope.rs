//! One tab: four lists over one namespace of one cluster — pods, events,
//! configmaps, secrets — and the state that is this tab's alone: which kind
//! shows, each kind's cursor and search, the text pane under the details,
//! the question on top of the table, and the one secret value on screen.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::cursor::ScrollState;
use super::list::{ListState, Row};
use super::screen::{AppAction, Target};
use super::shell::{Focus, Shell};
use crate::columns::ColumnId;
use crate::config::Tab;
use crate::kube::{
    ConfigMap, K8sEvent, Kind, LogFollow, ObjectRef, Pod, Replicas, Request, Secret, SecretMeta,
    TextKind,
};
use crate::store::ScopeData;
use crate::text_input::TextInput;

/// How many log lines the pane keeps. Past this the oldest go, and the first
/// line says how many.
pub const LOG_LINE_CAP: usize = 20_000;

/// How long a revealed secret stays on screen. Long enough to read one off
/// and type it somewhere, short enough that a walked-away-from terminal is
/// not showing a production password.
pub const REVEAL_FOR: Duration = Duration::from_secs(60);

/// What a confirmation is about to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Verb {
    /// Delete the pod and let its owner put a new one up.
    Restart(crate::kube::PodKey),
    /// `kubectl rollout restart` of the owner.
    Rollout(ObjectRef),
}

impl Verb {
    /// The modal's yes button.
    #[must_use]
    pub const fn button(&self) -> &'static str {
        match self {
            Self::Restart(_) => "Restart",
            Self::Rollout(_) => "Rollout restart",
        }
    }

    /// What the key line under the buttons says.
    #[must_use]
    pub const fn hint(&self) -> &'static str {
        match self {
            Self::Restart(_) => "x again to restart it",
            Self::Rollout(_) => "X again to restart the rollout",
        }
    }
}

/// A question on top of the table, taking every key until it is answered.
#[derive(Debug)]
pub enum Modal {
    Confirm {
        title: String,
        body: Vec<String>,
        verb: Verb,
    },
    Scale {
        object: ObjectRef,
        input: TextInput,
        current: Option<Replicas>,
    },
}

/// The owner kinds `kubectl scale` takes.
const SCALABLE: &[&str] = &["deployment", "statefulset", "replicaset"];
/// The owner kinds `kubectl rollout restart` takes.
const ROLLABLE: &[&str] = &["deployment", "statefulset", "daemonset"];

/// What the text pane under the details is showing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PaneText {
    /// The pod's log, tailed.
    #[default]
    Log,
    Describe,
    Yaml,
    /// One key of a configmap or a secret.
    Value,
}

/// A secret's value, on screen, and when it got there.
///
/// **This is the one field in the crate that holds a [`Secret`].** It is
/// dropped when the cursor moves, on `r`, on a kind or tab switch, on `v`
/// again, and sixty seconds after it arrived.
pub struct Revealed {
    pub object: ObjectRef,
    pub key: String,
    value: Secret,
    at: Instant,
}

impl Revealed {
    /// Whole seconds until it goes.
    #[must_use]
    pub fn clears_in(&self, now: Instant) -> u64 {
        REVEAL_FOR
            .saturating_sub(now.saturating_duration_since(self.at))
            .as_secs()
    }

    #[must_use]
    pub fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.at) >= REVEAL_FOR
    }
}

/// The pod's owner as an object `kubectl` can be asked about:
/// `deployment/orders-api`.
#[must_use]
pub fn owner_object(pod: &Pod) -> Option<ObjectRef> {
    let (kind, name) = pod.owner.as_ref()?;
    Some(ObjectRef {
        kind: kind.to_ascii_lowercase(),
        namespace: pod.key.namespace.clone(),
        name: name.clone(),
    })
}

const fn index(kind: Kind) -> usize {
    match kind {
        Kind::Pods => 0,
        Kind::Events => 1,
        Kind::ConfigMaps => 2,
        Kind::Secrets => 3,
    }
}

pub struct ScopeScreen {
    pub kind: Kind,
    lists: [ListState; 4],
    /// How far down the details pane is scrolled.
    pub details_scroll: ScrollState,
    /// Which key of a configmap or a secret the details cursor is on.
    pub key_cursor: usize,

    // ── The text pane ──────────────────────────────────────────────────
    /// Whether the text pane is open under the details at all.
    pub pane_open: bool,
    pub pane: PaneText,
    pub pane_scroll: ScrollState,
    /// Whether the pane has the whole details area to itself.
    pub pane_zoom: bool,
    /// `/` inside the pane: only lines containing every word are shown.
    pub pane_filter: TextInput,
    /// What the Value pane paints, rebuilt each frame from the key under the
    /// details cursor.
    pane_value: Vec<String>,
    /// Whether the log pane is pinned to the tail.
    log_follow: bool,
    /// Whether the stream has ended: the pod went, or `kubectl` refused.
    log_finished: bool,
    /// What the pane is tailing now, which is also what the worker is told
    /// to follow. The lines held are this target's and nobody else's.
    log_target: Option<LogFollow>,
    log_lines: Vec<String>,
    /// How many lines have gone off the top of the buffer, for the line that
    /// says so.
    log_skipped: usize,
    /// The container chosen with `C`, for the pod the pane is on, and whether
    /// `P` has asked for the run before the last restart.
    container: Option<String>,
    previous: bool,
    /// What describe and `get -o yaml` said this run, by object, and the one
    /// that is out and not yet back.
    texts: HashMap<(TextKind, ObjectRef), Result<Vec<String>, String>>,
    pending: Option<(TextKind, ObjectRef)>,

    // ── Actions ────────────────────────────────────────────────────────
    /// The question on top of the table, while one is asked.
    pub modal: Option<Modal>,
    /// What each scalable owner said about its replicas this run, and the
    /// one asked and not yet answered.
    owners: HashMap<ObjectRef, Result<Replicas, String>>,
    owner_pending: Option<ObjectRef>,

    // ── Secrets ────────────────────────────────────────────────────────
    /// The one place a value lives. See [`Revealed`].
    revealed: Option<Revealed>,
    /// A value request out and not yet back: the object, the key, and
    /// whether `y` sent it, so the answer goes to the clipboard rather than
    /// the screen.
    reading_secret: Option<(ObjectRef, String, bool)>,
    /// What the cluster said when it would not hand one over. Cleared when
    /// the cursor moves.
    refusal: Option<String>,
}

impl ScopeScreen {
    /// A tab's screen. A tab over every namespace says which each row is in.
    #[must_use]
    pub fn new(all_namespaces: bool) -> Self {
        let mut lists = [
            ListState::new(Kind::Pods, Pod::DEFAULT_SORT),
            ListState::new(Kind::Events, K8sEvent::DEFAULT_SORT),
            ListState::new(Kind::ConfigMaps, ConfigMap::DEFAULT_SORT),
            ListState::new(Kind::Secrets, SecretMeta::DEFAULT_SORT),
        ];
        if all_namespaces {
            for list in &mut lists {
                list.layout.set_visible(ColumnId::Namespace, true);
            }
        }
        Self {
            kind: Kind::Pods,
            lists,
            details_scroll: ScrollState::default(),
            key_cursor: 0,
            pane_open: false,
            pane: PaneText::Log,
            pane_scroll: ScrollState::default(),
            pane_zoom: false,
            pane_filter: TextInput::default(),
            pane_value: Vec::new(),
            log_follow: true,
            log_finished: false,
            log_target: None,
            log_lines: Vec::new(),
            log_skipped: 0,
            container: None,
            previous: false,
            texts: HashMap::new(),
            pending: None,
            modal: None,
            owners: HashMap::new(),
            owner_pending: None,
            revealed: None,
            reading_secret: None,
            refusal: None,
        }
    }

    // ── The lists ──────────────────────────────────────────────────────

    /// The list of the kind showing.
    #[must_use]
    pub fn list(&self) -> &ListState {
        &self.lists[index(self.kind)]
    }

    pub fn list_mut(&mut self) -> &mut ListState {
        &mut self.lists[index(self.kind)]
    }

    #[must_use]
    pub fn list_of(&self, kind: Kind) -> &ListState {
        &self.lists[index(kind)]
    }

    pub fn list_of_mut(&mut self, kind: Kind) -> &mut ListState {
        &mut self.lists[index(kind)]
    }

    /// The default sort of the kind showing, for the header click's third
    /// state.
    #[must_use]
    pub const fn default_sort(&self) -> ColumnId {
        match self.kind {
            Kind::Pods => Pod::DEFAULT_SORT,
            Kind::Events => K8sEvent::DEFAULT_SORT,
            Kind::ConfigMaps => ConfigMap::DEFAULT_SORT,
            Kind::Secrets => SecretMeta::DEFAULT_SORT,
        }
    }

    /// Another kind: its list is where it was left; the pane, the details
    /// cursor and any value on screen are not carried across.
    pub fn set_kind(&mut self, kind: Kind) {
        if self.kind == kind {
            return;
        }
        self.kind = kind;
        self.close_pane();
        self.details_scroll.scroll_to(0);
        self.key_cursor = 0;
        self.revealed = None;
        self.refusal = None;
    }

    /// Rebuilds the shown rows of the kind showing.
    pub fn refilter(&mut self, data: &ScopeData) {
        self.refilter_kind(self.kind, data);
    }

    pub fn refilter_kind(&mut self, kind: Kind, data: &ScopeData) {
        let list = &mut self.lists[index(kind)];
        match kind {
            Kind::Pods => list.refilter(&data.pods.rows),
            Kind::Events => list.refilter(&data.events.rows),
            Kind::ConfigMaps => list.refilter(&data.configmaps.rows),
            Kind::Secrets => list.refilter(&data.secrets.rows),
        }
    }

    /// After a read of one kind: the list's rows moved, so it is rebuilt
    /// under a cursor that stays on its own row.
    pub fn rows_changed(&mut self, kind: Kind, data: &ScopeData, was: Option<String>) {
        let list = &mut self.lists[index(kind)];
        list.invalidate();
        match kind {
            Kind::Pods => list.keep_cursor(&data.pods.rows, was),
            Kind::Events => list.keep_cursor(&data.events.rows, was),
            Kind::ConfigMaps => list.keep_cursor(&data.configmaps.rows, was),
            Kind::Secrets => list.keep_cursor(&data.secrets.rows, was),
        }
        if kind == self.kind {
            let keys = self.keys(data).len();
            self.key_cursor = self.key_cursor.min(keys.saturating_sub(1));
        }
    }

    /// What one kind's cursor is on, by identity, before its rows move.
    #[must_use]
    pub fn cursor_identity(&self, kind: Kind, data: &ScopeData) -> Option<String> {
        let list = &self.lists[index(kind)];
        match kind {
            Kind::Pods => list.cursor_identity(&data.pods.rows),
            Kind::Events => list.cursor_identity(&data.events.rows),
            Kind::ConfigMaps => list.cursor_identity(&data.configmaps.rows),
            Kind::Secrets => list.cursor_identity(&data.secrets.rows),
        }
    }

    // ── What is under the cursor ───────────────────────────────────────

    /// The pod under the Pods list's cursor, whatever kind shows.
    #[must_use]
    pub fn selected_pod<'a>(&self, data: &'a ScopeData) -> Option<&'a Pod> {
        data.pods
            .rows
            .get(self.list_of(Kind::Pods).selected_index()?)
    }

    #[must_use]
    pub fn selected_event<'a>(&self, data: &'a ScopeData) -> Option<&'a K8sEvent> {
        data.events
            .rows
            .get(self.list_of(Kind::Events).selected_index()?)
    }

    #[must_use]
    pub fn selected_configmap<'a>(&self, data: &'a ScopeData) -> Option<&'a ConfigMap> {
        data.configmaps
            .rows
            .get(self.list_of(Kind::ConfigMaps).selected_index()?)
    }

    #[must_use]
    pub fn selected_secret<'a>(&self, data: &'a ScopeData) -> Option<&'a SecretMeta> {
        data.secrets
            .rows
            .get(self.list_of(Kind::Secrets).selected_index()?)
    }

    /// The object the kind showing has under its cursor: the pod, what the
    /// event is about, the configmap, the secret.
    #[must_use]
    pub fn selected_object(&self, data: &ScopeData) -> Option<ObjectRef> {
        match self.kind {
            Kind::Pods => self.selected_pod(data).map(|pod| ObjectRef::pod(&pod.key)),
            Kind::Events => self.selected_event(data).map(|event| event.object.clone()),
            Kind::ConfigMaps => self.selected_configmap(data).map(ConfigMap::object),
            Kind::Secrets => self.selected_secret(data).map(SecretMeta::object),
        }
    }

    /// The name of what is under the cursor, for `y`.
    #[must_use]
    pub fn selected_name(&self, data: &ScopeData) -> Option<String> {
        self.selected_object(data).map(|object| object.name)
    }

    /// The keys of the configmap or secret under the cursor, each with what
    /// the details pane says beside it.
    #[must_use]
    pub fn keys(&self, data: &ScopeData) -> Vec<(String, String)> {
        match self.kind {
            Kind::ConfigMaps => self
                .selected_configmap(data)
                .map(|held| {
                    held.data
                        .iter()
                        .map(|(key, value)| {
                            let lines = value.lines().count();
                            let said = if lines > 1 {
                                format!("{lines} lines")
                            } else {
                                format!("{} bytes", value.len())
                            };
                            (key.clone(), said)
                        })
                        .collect()
                })
                .unwrap_or_default(),
            Kind::Secrets => self
                .selected_secret(data)
                .map(|held| {
                    held.keys
                        .iter()
                        .map(|(key, size)| (key.clone(), format!("{size} bytes")))
                        .collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// The key under the details cursor.
    #[must_use]
    pub fn selected_key(&self, data: &ScopeData) -> Option<String> {
        self.keys(data)
            .get(self.key_cursor)
            .map(|(key, _)| key.clone())
    }

    /// What the bottom border says: how many rows of how many, and the sort.
    #[must_use]
    pub fn status(&self, data: &ScopeData) -> String {
        self.list().status(data.listing(self.kind).count)
    }

    /// `✗ N` while N pods are in trouble.
    #[must_use]
    pub fn badge(data: &ScopeData) -> Option<String> {
        let count = data.unhealthy();
        (count > 0).then(|| format!("\u{2717} {count}"))
    }

    // ── Jumps between kinds ────────────────────────────────────────────

    /// `e` on a pod: its events, the search box narrowed to its name.
    pub fn events_for_selected_pod(&mut self, data: &ScopeData) {
        let name = self.selected_pod(data).map(|pod| pod.key.name.clone());
        self.set_kind(Kind::Events);
        if let Some(name) = name {
            self.list_mut().input.set_text(name);
            self.list_mut().cursor.reset();
        }
    }

    /// `Enter` on an event: the pod it is about, when it is about one that is
    /// on the table.
    pub fn jump_to_object(&mut self, shell: &mut Shell, data: &ScopeData) {
        let Some(object) = self.selected_event(data).map(|event| event.object.clone()) else {
            return;
        };
        if object.kind != "pod" {
            shell.set_status(format!("{} is not a pod; d describes it", object.slash()));
            return;
        }
        let identity = format!("{}/{}", object.namespace, object.name);
        if self
            .list_of_mut(Kind::Pods)
            .select(&data.pods.rows, &identity)
        {
            self.set_kind(Kind::Pods);
        } else {
            shell.set_status(format!("{} is no longer on the table", object.name));
        }
    }

    // ── The text pane ──────────────────────────────────────────────────

    /// What the log pane should be following: the pod under the cursor, the
    /// container chosen if the pane is still on the pod it was chosen for,
    /// and whether the run before the last restart was asked for. `None`
    /// while the pane is closed, showing something else, or the tab is on
    /// another kind. The app diffs this against what the worker was last
    /// told.
    #[must_use]
    pub fn log_target(&self, scope: usize, data: &ScopeData) -> Option<LogFollow> {
        if !self.pane_open || self.pane != PaneText::Log || self.kind != Kind::Pods {
            return None;
        }
        let pod = self.selected_pod(data)?;
        let same_pod = self
            .log_target
            .as_ref()
            .is_some_and(|held| held.key == pod.key);
        let container = self
            .container
            .as_ref()
            .filter(|_| same_pod)
            .filter(|name| pod.containers.iter().any(|held| held.name == **name))
            .cloned();
        Some(LogFollow {
            scope,
            key: pod.key.clone(),
            container,
            previous: self.previous,
        })
    }

    /// Settles the pane on a new stream. A different target is a different
    /// stream: the lines held were the last one's, and the pane goes back to
    /// the tail of the new one.
    pub fn begin_follow(&mut self, target: Option<LogFollow>) {
        self.container = target.as_ref().and_then(|held| held.container.clone());
        self.log_target = target;
        self.log_lines.clear();
        self.log_skipped = 0;
        self.log_finished = false;
        self.log_follow = true;
        self.pane_scroll = ScrollState::default();
    }

    /// What the pane is on now, which is what the lines held belong to.
    #[must_use]
    pub fn following(&self) -> Option<&LogFollow> {
        self.log_target.as_ref()
    }

    /// Folds lines onto the end of the log. Lines for a stream the pane has
    /// already left are dropped rather than mixed into the one it is on.
    pub fn append_log(&mut self, target: &LogFollow, lines: Vec<String>, finished: bool) {
        if Some(target) != self.log_target.as_ref() {
            return;
        }
        self.log_lines.extend(lines);
        if self.log_lines.len() > LOG_LINE_CAP {
            // One more than the overflow, because the line saying what went
            // takes a place of its own — and when there already is one, it
            // is the first line to go and its count carries on.
            let overflow = self.log_lines.len() - LOG_LINE_CAP + 1;
            let dropped = overflow - usize::from(self.log_skipped > 0);
            self.log_lines.drain(..overflow);
            self.log_skipped += dropped;
            self.log_lines.insert(
                0,
                format!("\u{2026} {} earlier lines skipped", self.log_skipped),
            );
        }
        self.log_finished = finished;
    }

    #[must_use]
    pub fn log_lines(&self) -> &[String] {
        &self.log_lines
    }

    /// Whether the log pane is pinned to the tail, which is what `End` puts
    /// it back to and scrolling up takes it out of.
    #[must_use]
    pub const fn log_following(&self) -> bool {
        self.log_follow
    }

    /// Whether the stream the pane is on has ended.
    #[must_use]
    pub const fn log_ended(&self) -> bool {
        self.log_finished
    }

    #[must_use]
    pub const fn previous(&self) -> bool {
        self.previous
    }

    /// What describe or yaml said about one object, if it has come back.
    #[must_use]
    pub fn text(&self, kind: TextKind, object: &ObjectRef) -> Option<&Result<Vec<String>, String>> {
        self.texts.get(&(kind, object.clone()))
    }

    /// Whether a describe or yaml for this object is out and not yet back.
    #[must_use]
    pub fn text_pending(&self, kind: TextKind, object: &ObjectRef) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|(held, held_object)| *held == kind && held_object == object)
    }

    /// One text has come back. Kept whichever row the cursor is on now: the
    /// cursor coming back to that object shows it without asking again.
    pub fn set_text(
        &mut self,
        kind: TextKind,
        object: ObjectRef,
        text: Result<Vec<String>, String>,
    ) {
        if self.pending.as_ref() == Some(&(kind, object.clone())) {
            self.pending = None;
        }
        self.texts.insert((kind, object), text);
    }

    /// `Enter` or `l` on a pod: the log pane, open with the pod's log; again,
    /// closed. Says whether the pane is open afterwards.
    pub fn toggle_log(&mut self) -> bool {
        if self.pane_open && self.pane == PaneText::Log {
            self.close_pane();
            false
        } else {
            self.open_pane(PaneText::Log);
            true
        }
    }

    /// `d` or `v`: the pane on that text, and the request that fetches it
    /// when nothing has yet, for the object under the cursor.
    pub fn show_text(&mut self, scope: usize, kind: TextKind, data: &ScopeData) -> Option<Request> {
        let object = self.selected_object(data)?;
        self.open_pane(match kind {
            TextKind::Describe => PaneText::Describe,
            TextKind::Yaml => PaneText::Yaml,
        });
        if self.texts.contains_key(&(kind, object.clone())) {
            return None;
        }
        self.pending = Some((kind, object.clone()));
        Some(match kind {
            TextKind::Describe => Request::Describe { scope, object },
            TextKind::Yaml => Request::Yaml { scope, object },
        })
    }

    fn open_pane(&mut self, pane: PaneText) {
        self.pane_open = true;
        if self.pane != pane {
            self.pane_scroll.scroll_to(0);
        }
        self.pane = pane;
        if pane == PaneText::Log {
            self.log_follow = true;
        }
    }

    /// `Esc` with the pane showing: closed, and the follow goes with it on
    /// the next tick.
    pub fn close_pane(&mut self) {
        self.pane_open = false;
        self.pane_zoom = false;
        self.pane_filter.clear();
    }

    /// `z`: the text pane alone in the details area, and back.
    pub fn toggle_zoom(&mut self) {
        if self.pane_open {
            self.pane_zoom = !self.pane_zoom;
        }
    }

    /// `C`: the log moves to the pod's next container, round to the first
    /// again. A pod with one container says so rather than doing nothing.
    pub fn next_container(&mut self, shell: &mut Shell, data: &ScopeData) {
        let Some(pod) = self.selected_pod(data) else {
            return;
        };
        let names: Vec<&str> = pod
            .containers
            .iter()
            .map(|container| container.name.as_str())
            .collect();
        if names.len() < 2 {
            shell.set_status(format!("{} has one container", pod.key.name));
            return;
        }
        let current = self
            .container
            .as_ref()
            .and_then(|held| names.iter().position(|name| *name == held.as_str()))
            .unwrap_or(0);
        let next = names[(current + 1) % names.len()].to_owned();
        shell.set_status(format!("Following {next}"));
        self.container = Some(next);
        // The choice is for the pod the pane is on; the follow sync reads it
        // from here.
        if self
            .log_target
            .as_ref()
            .is_none_or(|held| held.key != pod.key)
        {
            self.log_target = Some(LogFollow {
                scope: 0,
                key: pod.key.clone(),
                container: None,
                previous: self.previous,
            });
        }
        self.open_pane(PaneText::Log);
    }

    /// `P`: the `-p` on the log the pane follows, on or off. The run before
    /// the last restart is where a crash loop says why.
    pub fn toggle_previous(&mut self, shell: &mut Shell) {
        self.previous = !self.previous;
        shell.set_status(if self.previous {
            "Following the log from before the last restart"
        } else {
            "Following the running log"
        });
        self.open_pane(PaneText::Log);
    }

    /// `r`: what describe, yaml and the owners said is stale, and a value on
    /// screen goes; the lists re-read on their own.
    pub fn on_refresh(&mut self) {
        self.texts.clear();
        self.pending = None;
        self.owners.clear();
        self.owner_pending = None;
        self.look_away();
    }

    /// Leaving the tab: a value on screen goes, and one on its way is not
    /// wanted when it lands.
    pub fn look_away(&mut self) {
        self.revealed = None;
        self.reading_secret = None;
        self.refusal = None;
    }

    // ── Values ─────────────────────────────────────────────────────────

    /// `Enter` or `v` on a configmap or a secret: the key's value in the text
    /// pane. A configmap's is on file; a secret's is asked for, one key at a
    /// time, and shown for sixty seconds — or hidden again when it already
    /// shows.
    pub fn show_value(&mut self, scope: usize, data: &ScopeData) -> Option<Request> {
        let object = self.selected_object(data)?;
        let key = self.selected_key(data)?;
        self.open_pane(PaneText::Value);
        if self.kind != Kind::Secrets {
            return None;
        }
        if self
            .revealed
            .as_ref()
            .is_some_and(|held| held.object == object && held.key == key)
        {
            self.revealed = None;
            return None;
        }
        self.refusal = None;
        self.reading_secret = Some((object.clone(), key.clone(), false));
        Some(Request::SecretValue {
            scope,
            object,
            key,
            copy: false,
        })
    }

    /// `y` on a configmap or a secret: the key's value on the clipboard. A
    /// secret's is read for that alone and never shown.
    pub fn copy_value(&mut self, shell: &mut Shell, scope: usize, data: &ScopeData) -> AppAction {
        let Some(object) = self.selected_object(data) else {
            return AppAction::None;
        };
        let Some(key) = self.selected_key(data) else {
            shell.set_error(format!("{} has no keys", object.name));
            return AppAction::None;
        };
        match self.kind {
            Kind::ConfigMaps => {
                let value = self
                    .selected_configmap(data)
                    .and_then(|held| held.data.iter().find(|(k, _)| *k == key))
                    .map(|(_, value)| value.clone())
                    .unwrap_or_default();
                AppAction::Copy {
                    text: value,
                    label: format!("Copied {key} of {}", object.name),
                }
            }
            Kind::Secrets => {
                if let Some(held) = self
                    .revealed
                    .as_ref()
                    .filter(|held| held.object == object && held.key == key)
                {
                    // One of the two places a value is read out.
                    return AppAction::Copy {
                        text: held.value.expose().to_owned(),
                        label: format!("Copied {key} of {}", object.name),
                    };
                }
                self.refusal = None;
                self.reading_secret = Some((object.clone(), key.clone(), true));
                shell.set_status(format!("Reading {key}\u{2026}"));
                AppAction::Kube(Request::SecretValue {
                    scope,
                    object,
                    key,
                    copy: true,
                })
            }
            _ => AppAction::None,
        }
    }

    /// A value has come back. Kept only if the cursor is still on the key
    /// that asked; a value for a key somebody has left is dropped on the
    /// floor rather than shown next to the wrong name.
    pub fn on_secret_value(
        &mut self,
        shell: &mut Shell,
        data: &ScopeData,
        object: ObjectRef,
        key: String,
        copy: bool,
        value: Result<Secret, String>,
    ) -> AppAction {
        let now = Instant::now();
        let asked = self
            .reading_secret
            .as_ref()
            .is_some_and(|(held, held_key, _)| *held == object && *held_key == key);
        if !asked {
            return AppAction::None;
        }
        self.reading_secret = None;
        let still_here = self.kind == Kind::Secrets
            && self.selected_object(data).as_ref() == Some(&object)
            && self.selected_key(data).as_deref() == Some(key.as_str());
        match value {
            Err(message) => {
                if copy {
                    shell.set_error(message.clone());
                }
                if still_here {
                    self.refusal = Some(message);
                }
                AppAction::None
            }
            // The other place a value is read out: `y` with nothing on
            // screen, copying blind, which is the common case.
            Ok(secret) if copy => AppAction::Copy {
                text: secret.expose().to_owned(),
                label: format!("Copied {key} of {}", object.name),
            },
            Ok(secret) => {
                if still_here {
                    self.revealed = Some(Revealed {
                        object,
                        key,
                        value: secret,
                        at: now,
                    });
                }
                AppAction::None
            }
        }
    }

    /// The value on screen, if it is this key's.
    #[must_use]
    pub fn revealed_here(&self, data: &ScopeData) -> Option<&Revealed> {
        let object = self.selected_object(data)?;
        let key = self.selected_key(data)?;
        self.revealed
            .as_ref()
            .filter(|held| held.object == object && held.key == key)
    }

    /// Whether a value for the key under the cursor has been asked for and
    /// not come back.
    #[must_use]
    pub fn reading_here(&self, data: &ScopeData) -> bool {
        let Some(object) = self.selected_object(data) else {
            return false;
        };
        self.reading_secret.as_ref().is_some_and(|(held, key, _)| {
            *held == object && Some(key) == self.selected_key(data).as_ref()
        })
    }

    #[must_use]
    pub const fn refusal(&self) -> Option<&String> {
        self.refusal.as_ref()
    }

    /// Rebuilds what the Value pane paints from the key under the details
    /// cursor: a configmap's value, a revealed secret's, or nothing.
    pub fn sync_value(&mut self, data: &ScopeData) {
        if self.pane != PaneText::Value {
            return;
        }
        let lines = match self.kind {
            Kind::ConfigMaps => self
                .selected_key(data)
                .and_then(|key| {
                    self.selected_configmap(data)
                        .and_then(|held| held.data.iter().find(|(k, _)| *k == key))
                        .map(|(_, value)| value.lines().map(str::to_owned).collect())
                })
                .unwrap_or_default(),
            Kind::Secrets => self
                .revealed_here(data)
                .map(|held| held.value.expose().lines().map(str::to_owned).collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        if lines != self.pane_value {
            self.pane_value = lines;
            self.pane_scroll.scroll_to(0);
        }
    }

    /// What the Value pane paints.
    #[must_use]
    pub fn pane_value(&self) -> &[String] {
        &self.pane_value
    }

    /// One turn of the clock: a value that has run out goes.
    pub fn tick_reveal(&mut self, now: Instant) {
        if self.revealed.as_ref().is_some_and(|held| held.expired(now)) {
            self.revealed = None;
        }
    }

    /// Whether the run loop should wake every second: a value is counting
    /// down or being waited for.
    #[must_use]
    pub const fn is_ticking(&self) -> bool {
        self.revealed.is_some() || self.reading_secret.is_some()
    }

    /// The `kubectl` line that does by hand what the pane shows: what `Y`
    /// copies.
    #[must_use]
    pub fn kubectl_line(&self, tab: &Tab, data: &ScopeData) -> Option<String> {
        let object = self.selected_object(data)?;
        let prefix = format!(
            "kubectl --context {} -n {}",
            tab.scope.context, object.namespace
        );
        Some(match (self.kind, self.pane_open, self.pane) {
            (_, true, PaneText::Describe) => format!("{prefix} describe {}", object.slash()),
            (_, true, PaneText::Yaml) => format!("{prefix} get {} -o yaml", object.slash()),
            (Kind::Pods, _, _) => {
                let mut line = format!("{prefix} logs -f {}", object.name);
                if let Some(container) = self
                    .log_target
                    .as_ref()
                    .filter(|held| held.key.name == object.name)
                    .and_then(|held| held.container.as_deref())
                {
                    line.push_str(&format!(" -c {container}"));
                }
                if self.previous {
                    line.push_str(" -p");
                }
                line
            }
            (Kind::Events, _, _) => format!("{prefix} describe {}", object.slash()),
            (Kind::ConfigMaps, _, _) => format!("{prefix} get {} -o yaml", object.slash()),
            (Kind::Secrets, _, _) => {
                let key = self.selected_key(data).unwrap_or_default();
                format!(
                    "{prefix} get secret {} -o jsonpath='{{.data.{key}}}' | base64 -d",
                    object.name
                )
            }
        })
    }

    // ── Actions ────────────────────────────────────────────────────────

    /// The replica counts of the pod's owner, once they have come back.
    #[must_use]
    pub fn owner_of(&self, pod: &Pod) -> Option<&Replicas> {
        owner_object(pod)
            .and_then(|object| self.owners.get(&object))
            .and_then(|held| held.as_ref().ok())
    }

    /// The owner read the cursor has settled on, when its counts are not on
    /// file and it is a kind that has any.
    pub fn owner_request(&mut self, scope: usize, data: &ScopeData) -> Option<Request> {
        if self.kind != Kind::Pods {
            return None;
        }
        let object = self.selected_pod(data).and_then(owner_object)?;
        if !SCALABLE.contains(&object.kind.as_str())
            || self.owners.contains_key(&object)
            || self.owner_pending.as_ref() == Some(&object)
        {
            return None;
        }
        self.owner_pending = Some(object.clone());
        Some(Request::Owner { scope, object })
    }

    pub fn set_owner(&mut self, object: ObjectRef, replicas: Result<Replicas, String>) {
        if self.owner_pending.as_ref() == Some(&object) {
            self.owner_pending = None;
        }
        // The scale modal, if it is open on this owner, learns the count too.
        if let Some(Modal::Scale {
            object: held,
            input,
            current,
        }) = &mut self.modal
            && *held == object
            && let Ok(replicas) = &replicas
        {
            if input.is_empty() {
                input.set_text(replicas.desired.to_string());
                input.move_end();
            }
            *current = Some(*replicas);
        }
        self.owners.insert(object, replicas);
    }

    /// `x`: asks, rather than deleting. A pod nothing put there is refused
    /// outright — deleting it would take it away for good rather than
    /// restart it.
    pub fn restart_prompt(&mut self, shell: &mut Shell, data: &ScopeData) {
        let Some(pod) = self.selected_pod(data) else {
            shell.set_error("No pod is selected");
            return;
        };
        let Some((kind, owner)) = &pod.owner else {
            shell.set_error(format!(
                "{} has no controller to put it back; deleting it would not restart it",
                pod.key.name
            ));
            return;
        };
        self.modal = Some(Modal::Confirm {
            title: "Restart pod".to_owned(),
            body: vec![
                format!("Restart {}?", pod.key.name),
                String::new(),
                format!("Deletes the pod; {kind} {owner} replaces it."),
            ],
            verb: Verb::Restart(pod.key.clone()),
        });
    }

    /// `X`: a rollout restart of the owner, which replaces every pod of it
    /// one at a time. Refused for an owner that has no rollout.
    pub fn rollout_prompt(&mut self, shell: &mut Shell, data: &ScopeData) {
        let Some(pod) = self.selected_pod(data) else {
            shell.set_error("No pod is selected");
            return;
        };
        let Some(object) = owner_object(pod).filter(|o| ROLLABLE.contains(&o.kind.as_str())) else {
            shell.set_error(format!(
                "{} is not under a deployment, statefulset or daemonset; nothing to roll",
                pod.key.name
            ));
            return;
        };
        self.modal = Some(Modal::Confirm {
            title: "Rollout restart".to_owned(),
            body: vec![
                format!("Restart the rollout of {}?", object.slash()),
                String::new(),
                "Every pod of it is replaced, one at a time.".to_owned(),
            ],
            verb: Verb::Rollout(object),
        });
    }

    /// `=`: the scale modal on the owner, with the current count filled in
    /// once it is known. The request reads it when it is not on file.
    pub fn scale_prompt(
        &mut self,
        shell: &mut Shell,
        scope: usize,
        data: &ScopeData,
    ) -> Option<Request> {
        let Some(pod) = self.selected_pod(data) else {
            shell.set_error("No pod is selected");
            return None;
        };
        let Some(object) = owner_object(pod).filter(|o| SCALABLE.contains(&o.kind.as_str())) else {
            shell.set_error(format!(
                "{} is not under a deployment, statefulset or replicaset; nothing to scale",
                pod.key.name
            ));
            return None;
        };
        let current = self
            .owners
            .get(&object)
            .and_then(|held| held.as_ref().ok().copied());
        let mut input =
            TextInput::new(current.map_or_else(String::new, |held| held.desired.to_string()));
        input.move_end();
        self.modal = Some(Modal::Scale {
            object: object.clone(),
            input,
            current,
        });
        // Not on file: ask, and the answer fills the box when it lands.
        if current.is_some() || self.owner_pending.as_ref() == Some(&object) {
            return None;
        }
        self.owner_pending = Some(object.clone());
        Some(Request::Owner { scope, object })
    }

    /// A key while a modal is open. Answers the request when the modal was
    /// answered yes; closes it on `Esc` and, for a confirmation, on any key
    /// that is not its own.
    pub fn modal_key(
        &mut self,
        shell: &mut Shell,
        scope: usize,
        key: crossterm::event::KeyEvent,
    ) -> Option<Request> {
        use crossterm::event::KeyCode;
        match &mut self.modal {
            None => None,
            Some(Modal::Confirm { verb, .. }) => {
                // With a modifier it is not the key: Ctrl-X asks nothing.
                let plain = key
                    .modifiers
                    .difference(crossterm::event::KeyModifiers::SHIFT)
                    .is_empty();
                let yes = plain
                    && matches!(
                        (&*verb, key.code),
                        (_, KeyCode::Enter)
                            | (Verb::Restart(_), KeyCode::Char('x'))
                            | (Verb::Rollout(_), KeyCode::Char('X'))
                    );
                if yes {
                    self.confirm(shell, scope)
                } else {
                    self.dismiss();
                    None
                }
            }
            Some(Modal::Scale { input, .. }) => match key.code {
                KeyCode::Enter => self.confirm(shell, scope),
                KeyCode::Esc => {
                    self.dismiss();
                    None
                }
                _ => {
                    input.handle_key(key);
                    None
                }
            },
        }
    }

    /// The modal's yes, however it was given: the one place a change is
    /// sent from.
    pub fn confirm(&mut self, shell: &mut Shell, scope: usize) -> Option<Request> {
        match self.modal.take()? {
            Modal::Confirm { verb, .. } => Some(match verb {
                Verb::Restart(key) => {
                    shell.set_status(format!("Restarting {}\u{2026}", key.name));
                    Request::Delete { scope, key }
                }
                Verb::Rollout(object) => {
                    shell.set_status(format!(
                        "Restarting the rollout of {}\u{2026}",
                        object.slash()
                    ));
                    Request::RolloutRestart { scope, object }
                }
            }),
            Modal::Scale {
                object,
                input,
                current,
            } => match input.text().trim().parse::<u32>() {
                Ok(replicas) => {
                    shell.set_status(format!("Scaling {} to {replicas}\u{2026}", object.slash()));
                    Some(Request::Scale {
                        scope,
                        object,
                        replicas,
                    })
                }
                Err(_) => {
                    shell.set_error("Replicas must be a whole number");
                    self.modal = Some(Modal::Scale {
                        object,
                        input,
                        current,
                    });
                    None
                }
            },
        }
    }

    pub fn dismiss(&mut self) {
        self.modal = None;
    }

    /// What the delete said. A refusal is the user's to see; a delete that
    /// went through is news, because the pod it names is on its way out and
    /// another on its way in.
    pub fn deleted(
        &mut self,
        shell: &mut Shell,
        data: &ScopeData,
        key: &crate::kube::PodKey,
        error: Option<String>,
    ) {
        match error {
            Some(message) => {
                shell.set_error(format!("Could not restart {}: {message}", key.name));
            }
            None => {
                let owner = data
                    .pods
                    .rows
                    .iter()
                    .find(|pod| pod.key == *key)
                    .and_then(|pod| pod.owner.as_ref())
                    .map(|(kind, name)| format!("; {kind} {name} is putting a new one up"))
                    .unwrap_or_default();
                shell.set_status(format!("Deleted {}{owner}", key.name));
            }
        }
    }

    /// What a rollout restart or a scale said.
    pub fn acted(
        &mut self,
        shell: &mut Shell,
        verb: &str,
        object: &ObjectRef,
        error: Option<String>,
    ) {
        match error {
            Some(message) => {
                shell.set_error(format!("Could not {verb} {}: {message}", object.slash()));
            }
            None => shell.set_status(format!("{} {verb} sent", object.slash())),
        }
    }

    /// What `b` runs a shell in: the pod under the cursor, on the container
    /// the log follows when it follows one.
    #[must_use]
    pub fn bash_target(&self, tab: &Tab, data: &ScopeData) -> Option<AppAction> {
        let pod = self.selected_pod(data)?;
        Some(AppAction::Exec {
            context: tab.scope.context.clone(),
            namespace: pod.key.namespace.clone(),
            pod: pod.key.name.clone(),
            container: self
                .log_target
                .as_ref()
                .filter(|held| held.key == pod.key)
                .and_then(|held| held.container.clone()),
        })
    }

    // ── Keys ───────────────────────────────────────────────────────────

    /// One key the shell did not take: movement and sorting in the table;
    /// scrolling in whichever pane has the details focus, or the key cursor
    /// on a configmap or a secret.
    pub fn handle_key(
        &mut self,
        shell: &mut Shell,
        data: &ScopeData,
        key: crossterm::event::KeyEvent,
    ) -> AppAction {
        use crossterm::event::KeyCode;
        if shell.focus == Focus::Details {
            let walking_keys = matches!(self.kind, Kind::ConfigMaps | Kind::Secrets)
                && (!self.pane_open || self.pane == PaneText::Value);
            if walking_keys && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
                // The value pane scrolls by the page; the keys move by the row.
                self.pane_key(key.code);
            } else if self.pane_open && !walking_keys {
                self.pane_key(key.code);
            } else if walking_keys {
                let count = self.keys(data).len();
                let before = self.key_cursor;
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        self.key_cursor = (self.key_cursor + 1).min(count.saturating_sub(1));
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        self.key_cursor = self.key_cursor.saturating_sub(1);
                    }
                    KeyCode::Home => self.key_cursor = 0,
                    KeyCode::End => self.key_cursor = count.saturating_sub(1),
                    _ => {}
                }
                if self.key_cursor != before {
                    self.revealed = None;
                    self.refusal = None;
                }
            } else {
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        self.details_scroll.scroll_by(1);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        self.details_scroll.scroll_by(-1);
                    }
                    KeyCode::PageDown => {
                        self.details_scroll
                            .scroll_by(i32::try_from(self.details_scroll.page_step()).unwrap_or(1));
                    }
                    KeyCode::PageUp => {
                        self.details_scroll.scroll_by(
                            -i32::try_from(self.details_scroll.page_step()).unwrap_or(1),
                        );
                    }
                    _ => {}
                }
            }
            return AppAction::None;
        }
        let moved = match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.list_mut().move_cursor(1, false),
            KeyCode::Char('k') | KeyCode::Up => self.list_mut().move_cursor(-1, false),
            KeyCode::PageDown => self.list_mut().move_cursor(1, true),
            KeyCode::PageUp => self.list_mut().move_cursor(-1, true),
            KeyCode::Home => self.list_mut().move_cursor(isize::MIN / 2, false),
            KeyCode::End => self.list_mut().move_cursor(isize::MAX / 2, false),
            KeyCode::Char('S') => {
                self.list_mut().next_sort();
                false
            }
            KeyCode::Char('R') => {
                let list = self.list_mut();
                list.descending = !list.descending;
                false
            }
            _ => false,
        };
        if moved {
            self.cursor_moved();
        }
        AppAction::None
    }

    /// Moving the cursor takes a value off the screen with it, clears what
    /// the last key refused with, and puts the details back at their top.
    fn cursor_moved(&mut self) {
        self.details_scroll.scroll_to(0);
        self.key_cursor = 0;
        self.revealed = None;
        self.refusal = None;
    }

    /// The text pane's keys: scrolling, and following again with `End`.
    /// Scrolling up leaves follow mode; scrolling down to the tail resumes
    /// it.
    fn pane_key(&mut self, code: crossterm::event::KeyCode) {
        use crossterm::event::KeyCode;
        let page = i32::try_from(self.pane_scroll.page_step()).unwrap_or(1);
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.scroll_pane(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_pane(-1),
            KeyCode::PageDown => self.scroll_pane(page),
            KeyCode::PageUp => self.scroll_pane(-page),
            KeyCode::Home => self.scroll_pane(i32::MIN / 2),
            KeyCode::End => {
                self.pane_scroll.scroll_to(usize::MAX / 2);
                self.log_follow = true;
            }
            _ => {}
        }
    }

    /// Scrolls the text pane by hand, wherever the scroll came from, and
    /// keeps the follow flag honest: off when the tail goes out of view, on
    /// when it comes back.
    pub fn scroll_pane(&mut self, delta: i32) {
        self.pane_scroll.scroll_by(delta);
        self.log_follow = self.pane_scroll.offset >= self.pane_scroll.max_offset();
    }

    /// A click on a row moves the cursor there; on a header, sorts by it; on
    /// a key of a configmap or secret, puts the details cursor on it.
    pub fn handle_click(&mut self, shell: &mut Shell, target: Target) -> AppAction {
        match target {
            Target::Row(index) => {
                if self.list_mut().click_row(index) {
                    self.cursor_moved();
                }
            }
            Target::Header(column) => {
                let default = self.default_sort();
                self.list_mut().sort_by(column, default);
            }
            Target::KeyRow(index) => {
                shell.focus = Focus::Details;
                if self.key_cursor != index {
                    self.key_cursor = index;
                    self.revealed = None;
                    self.refusal = None;
                }
            }
            _ => {}
        }
        AppAction::None
    }

    pub fn handle_wheel(&mut self, _shell: &mut Shell, target: Option<Target>, delta: i32) {
        match target {
            Some(Target::Details | Target::KeyRow(_) | Target::Button(_)) => {
                self.details_scroll.scroll_by(delta);
            }
            Some(Target::TextPane) => self.scroll_pane(delta),
            _ => {
                if self.list_mut().wheel(delta) {
                    self.cursor_moved();
                }
            }
        }
    }

    #[must_use]
    pub fn footer_hint(&self, shell: &Shell) -> String {
        match shell.focus {
            Focus::Search => {
                "Esc/Enter keep the filter  Esc again clears it  Ctrl-U empties the box".to_owned()
            }
            Focus::PaneSearch => "Esc/Enter keep the filter  Ctrl-U empties it".to_owned(),
            Focus::Details if self.pane_open => {
                "j/k scroll  End follow  / filter  z zoom  P previous  C container  Tab table  Esc close"
                    .to_owned()
            }
            Focus::Details if matches!(self.kind, Kind::ConfigMaps | Kind::Secrets) => {
                "j/k key  Enter value  y copy  Tab table".to_owned()
            }
            _ => match self.kind {
                Kind::Pods => {
                    "↑↓/jk move  Enter logs  b bash  x restart  = scale  d describe  e events  / search  ? help"
                        .to_owned()
                }
                Kind::Events => {
                    "↑↓/jk move  Enter pod  d describe  v yaml  p pods  / search  ? help".to_owned()
                }
                Kind::ConfigMaps => {
                    "↑↓/jk move  Enter value  y copy  d describe  p pods  / search  ? help".to_owned()
                }
                Kind::Secrets => {
                    "↑↓/jk move  Enter reveal 60 s  y copy unseen  d describe  p pods  ? help"
                        .to_owned()
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube::tests::{crashing, key, pod};

    fn data() -> ScopeData {
        let mut old = pod("qa", "dev", "billing-worker-1a2b3c-old01", "Completed");
        old.created = crate::timestamp::Timestamp::parse("2026-01-01T00:00:00Z");
        old.owner = Some(("Job".to_owned(), "billing-worker".to_owned()));
        old.ready = (0, 1);
        let mut data = ScopeData::default();
        data.pods.rows = vec![
            pod("qa", "dev", "orders-api-7d9f5b-k9x2p", "Running"),
            crashing("qa", "dev", "orders-api-7d9f5b-abc12"),
            old,
        ];
        data.events.rows = vec![
            K8sEvent::from_json(&serde_json::json!({
                "metadata": {"name": "w1", "namespace": "dev"},
                "lastTimestamp": "2026-09-12T12:00:00Z", "type": "Warning", "reason": "BackOff", "count": 9,
                "involvedObject": {"kind": "Pod", "name": "orders-api-7d9f5b-abc12", "namespace": "dev"},
                "message": "Back-off restarting failed container"
            }))
            .unwrap(),
            K8sEvent::from_json(&serde_json::json!({
                "metadata": {"name": "n1", "namespace": "dev"},
                "lastTimestamp": "2026-09-12T11:00:00Z", "type": "Normal", "reason": "ScalingReplicaSet",
                "involvedObject": {"kind": "Deployment", "name": "orders-api", "namespace": "dev"},
                "message": "Scaled up replica set"
            }))
            .unwrap(),
        ];
        data.configmaps.rows = vec![
            ConfigMap::from_json(&serde_json::json!({
                "metadata": {"name": "orders-config", "namespace": "dev"},
                "data": {"LOG_LEVEL": "info", "APP_YAML": "a: 1\nb: 2\n"}
            }))
            .unwrap(),
        ];
        data.secrets.rows = vec![
            SecretMeta::from_json(&serde_json::json!({
                "metadata": {"name": "db", "namespace": "dev"},
                "type": "Opaque",
                "data": {"password": "aHVudGVyMg==", "user": "YWRtaW4="}
            }))
            .unwrap(),
        ];
        data
    }

    fn tab() -> Tab {
        crate::config::parse(crate::config::tests::TWO_CLUSTERS)
            .unwrap()
            .tabs()
            .remove(0)
    }

    fn secret_object() -> ObjectRef {
        ObjectRef {
            kind: "secret".to_owned(),
            namespace: "dev".to_owned(),
            name: "db".to_owned(),
        }
    }

    #[test]
    fn each_kind_keeps_its_own_list_and_a_switch_drops_the_pane() {
        let data = data();
        let mut screen = ScopeScreen::new(false);
        screen.refilter(&data);
        screen.list_mut().input.set_text("abc12");
        screen.refilter(&data);
        assert_eq!(screen.list().visible().len(), 1);
        screen.toggle_log();
        screen.set_kind(Kind::Events);
        assert!(!screen.pane_open, "the pane was the pods'");
        screen.refilter(&data);
        assert_eq!(
            screen.list().visible().len(),
            2,
            "its own list, its own search"
        );
        assert_eq!(screen.status(&data), "2 · Age ↑");
        assert_eq!(
            screen.selected_object(&data).map(|object| object.slash()),
            Some("pod/orders-api-7d9f5b-abc12".to_owned()),
            "newest first: the warning"
        );
        screen.set_kind(Kind::Pods);
        screen.refilter(&data);
        assert_eq!(screen.list().input.text(), "abc12", "as it was left");
        assert_eq!(screen.log_target(0, &data), None, "closed on the way out");
    }

    #[test]
    fn e_narrows_the_events_to_the_pod_and_enter_on_an_event_goes_back_to_it() {
        let data = data();
        let mut screen = ScopeScreen::new(false);
        let mut shell = Shell::default();
        screen.refilter(&data);
        screen.list_mut().cursor.focus(1); // orders-api-7d9f5b-abc12
        screen.events_for_selected_pod(&data);
        assert_eq!(screen.kind, Kind::Events);
        assert_eq!(screen.list().input.text(), "orders-api-7d9f5b-abc12");
        screen.refilter(&data);
        assert_eq!(screen.list().visible().len(), 1);

        screen.list_mut().input.clear();
        screen.refilter(&data);
        screen.list_mut().cursor.focus(1); // the deployment's event
        screen.jump_to_object(&mut shell, &data);
        assert_eq!(screen.kind, Kind::Events, "not a pod");
        assert!(
            shell
                .notification()
                .is_some_and(|(said, _)| said.contains("not a pod"))
        );
        screen.list_mut().cursor.focus(0);
        screen.list_of_mut(Kind::Pods).input.set_text("k9x2p");
        screen.jump_to_object(&mut shell, &data);
        assert_eq!(screen.kind, Kind::Pods);
        assert!(
            screen.list().input.is_empty(),
            "the query hid it, so it went"
        );
        assert_eq!(
            screen.selected_pod(&data).map(|pod| pod.key.name.as_str()),
            Some("orders-api-7d9f5b-abc12")
        );
        assert_eq!(
            screen.kubectl_line(&tab(), &data).as_deref(),
            Some("kubectl --context aks-qa -n dev logs -f orders-api-7d9f5b-abc12")
        );
    }

    #[test]
    fn a_configmaps_keys_are_walked_in_the_details_and_a_value_shows_in_the_pane() {
        let data = data();
        let mut screen = ScopeScreen::new(false);
        let mut shell = Shell::default();
        screen.set_kind(Kind::ConfigMaps);
        screen.refilter(&data);
        assert_eq!(
            screen.keys(&data),
            [
                ("APP_YAML".to_owned(), "2 lines".to_owned()),
                ("LOG_LEVEL".to_owned(), "4 bytes".to_owned())
            ]
        );
        assert_eq!(screen.show_value(0, &data), None, "on file: nothing to ask");
        assert!(screen.pane_open && screen.pane == PaneText::Value);
        screen.sync_value(&data);
        assert_eq!(screen.pane_value(), ["a: 1", "b: 2"]);

        shell.focus = Focus::Details;
        screen.handle_key(
            &mut shell,
            &data,
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('j'),
                crossterm::event::KeyModifiers::NONE,
            ),
        );
        assert_eq!(screen.selected_key(&data).as_deref(), Some("LOG_LEVEL"));
        screen.close_pane();
        screen.sync_value(&data);
        screen.show_value(0, &data);
        screen.sync_value(&data);
        assert_eq!(screen.pane_value(), ["info"]);
        assert_eq!(
            screen.copy_value(&mut shell, 0, &data),
            AppAction::Copy {
                text: "info".to_owned(),
                label: "Copied LOG_LEVEL of orders-config".to_owned()
            }
        );
        assert_eq!(
            screen.kubectl_line(&tab(), &data).as_deref(),
            Some("kubectl --context aks-qa -n dev get configmap/orders-config -o yaml")
        );
        screen.handle_click(&mut shell, Target::KeyRow(0));
        assert_eq!(screen.key_cursor, 0);
    }

    #[test]
    fn a_secret_is_read_one_key_at_a_time_shown_for_sixty_seconds_and_copied_unseen() {
        let data = data();
        let mut screen = ScopeScreen::new(false);
        let mut shell = Shell::default();
        screen.set_kind(Kind::Secrets);
        screen.refilter(&data);
        assert_eq!(screen.keys(&data)[0].0, "password");
        let now = Instant::now();

        // v: asked for, then shown, then gone at sixty seconds.
        let request = screen.show_value(0, &data);
        assert_eq!(
            request,
            Some(Request::SecretValue {
                scope: 0,
                object: secret_object(),
                key: "password".to_owned(),
                copy: false,
            })
        );
        assert!(screen.reading_here(&data));
        assert!(screen.is_ticking());
        let action = screen.on_secret_value(
            &mut shell,
            &data,
            secret_object(),
            "password".to_owned(),
            false,
            Ok(Secret::new("hunter2")),
        );
        assert_eq!(action, AppAction::None);
        screen.sync_value(&data);
        assert_eq!(screen.pane_value(), ["hunter2"]);
        assert!(screen.revealed_here(&data).unwrap().clears_in(now) >= 59);
        assert_eq!(screen.show_value(0, &data), None, "v again hides it");
        assert!(screen.revealed_here(&data).is_none());
        screen.show_value(0, &data);
        screen.on_secret_value(
            &mut shell,
            &data,
            secret_object(),
            "password".to_owned(),
            false,
            Ok(Secret::new("hunter2")),
        );
        screen.tick_reveal(Instant::now() + REVEAL_FOR);
        assert!(screen.revealed_here(&data).is_none(), "gone at sixty");
        screen.sync_value(&data);
        assert!(screen.pane_value().is_empty());

        // y with nothing on screen: read for the clipboard alone.
        let action = screen.copy_value(&mut shell, 0, &data);
        assert!(matches!(
            action,
            AppAction::Kube(Request::SecretValue { copy: true, .. })
        ));
        let action = screen.on_secret_value(
            &mut shell,
            &data,
            secret_object(),
            "password".to_owned(),
            true,
            Ok(Secret::new("hunter2")),
        );
        assert_eq!(
            action,
            AppAction::Copy {
                text: "hunter2".to_owned(),
                label: "Copied password of db".to_owned()
            }
        );
        assert!(screen.revealed_here(&data).is_none(), "and nothing kept");

        // A refusal takes the value's place; a key left behind drops its
        // answer on the floor.
        screen.show_value(0, &data);
        screen.on_secret_value(
            &mut shell,
            &data,
            secret_object(),
            "password".to_owned(),
            false,
            Err("secrets \"db\" is forbidden".to_owned()),
        );
        assert_eq!(
            screen.refusal().map(String::as_str),
            Some("secrets \"db\" is forbidden")
        );
        screen.show_value(0, &data);
        screen.key_cursor = 1;
        screen.on_secret_value(
            &mut shell,
            &data,
            secret_object(),
            "password".to_owned(),
            false,
            Ok(Secret::new("hunter2")),
        );
        assert!(screen.revealed_here(&data).is_none());
        assert!(
            screen
                .kubectl_line(&tab(), &data)
                .unwrap()
                .ends_with("get secret db -o jsonpath='{.data.user}' | base64 -d")
        );
        // r drops everything the screen held.
        screen.on_refresh();
        assert!(!screen.is_ticking());
        let mut fresh = ScopeScreen::new(false);
        fresh.set_kind(Kind::Secrets);
        let no_keys = ScopeData::default();
        assert_eq!(fresh.copy_value(&mut shell, 0, &no_keys), AppAction::None);
    }

    #[test]
    fn the_log_pane_follows_the_pod_under_the_cursor_once_it_is_open_and_nothing_before() {
        let data = data();
        let mut screen = ScopeScreen::new(false);
        screen.refilter(&data);
        assert_eq!(
            screen.log_target(0, &data),
            None,
            "closed: nothing followed"
        );

        assert!(screen.toggle_log());
        let target = screen
            .log_target(0, &data)
            .expect("the pod under the cursor");
        assert_eq!(target.key.name, "billing-worker-1a2b3c-old01");
        assert_eq!(target.scope, 0);
        assert_eq!(target.container, None);
        screen.begin_follow(Some(target.clone()));

        screen.append_log(&target, vec!["starting".to_owned()], false);
        screen.append_log(&target, vec!["listening".to_owned()], false);
        assert_eq!(screen.log_lines(), ["starting", "listening"]);
        assert!(screen.log_following());
        assert!(!screen.log_ended());
        let stale = LogFollow {
            key: key("prod", "prod", "other"),
            ..target.clone()
        };
        screen.append_log(&stale, vec!["not mine".to_owned()], false);
        assert_eq!(screen.log_lines(), ["starting", "listening"]);
        screen.append_log(&target, Vec::new(), true);
        assert!(screen.log_ended(), "the stream said it was over");

        screen.list_mut().cursor.focus(1);
        let next = screen.log_target(0, &data).unwrap();
        assert_ne!(next.key, target.key);
        screen.begin_follow(Some(next));
        assert!(screen.log_lines().is_empty());
        assert!(!screen.log_ended());

        assert!(!screen.toggle_log(), "again closes it");
        assert_eq!(screen.log_target(0, &data), None);
    }

    #[test]
    fn a_log_past_the_cap_keeps_the_tail_and_says_how_much_it_dropped() {
        let data = data();
        let mut screen = ScopeScreen::new(false);
        screen.refilter(&data);
        screen.toggle_log();
        let target = screen.log_target(0, &data).unwrap();
        screen.begin_follow(Some(target.clone()));
        let lines: Vec<String> = (1..=LOG_LINE_CAP + 10)
            .map(|line| format!("line {line}"))
            .collect();
        screen.append_log(&target, lines, false);
        let held = screen.log_lines();
        assert_eq!(held.len(), LOG_LINE_CAP, "the cap holds");
        assert!(held[0].contains("earlier lines skipped"), "{}", held[0]);
        assert_eq!(held.last().map(String::as_str), Some("line 20010"));
    }

    #[test]
    fn c_moves_to_the_next_container_of_this_pod_only_and_p_asks_for_the_last_run() {
        let mut data = data();
        data.pods.rows[0].containers.push(crate::kube::Container {
            name: "istio-proxy".to_owned(),
            image: "docker.io/istio/proxyv2:1.20".to_owned(),
            ready: true,
            restarts: 0,
            state: "Running".to_owned(),
            last_termination: None,
        });
        let mut screen = ScopeScreen::new(false);
        let mut shell = Shell::default();
        screen.refilter(&data);
        screen.list_mut().cursor.focus(2);
        screen.next_container(&mut shell, &data);
        assert!(screen.pane_open, "C opens the log");
        let target = screen.log_target(0, &data).unwrap();
        assert_eq!(target.container.as_deref(), Some("istio-proxy"));
        screen.begin_follow(Some(target));
        screen.next_container(&mut shell, &data);
        assert_eq!(
            screen.log_target(0, &data).unwrap().container.as_deref(),
            Some("api"),
            "round to the first again"
        );
        assert_eq!(
            screen.kubectl_line(&tab(), &data).as_deref(),
            Some("kubectl --context aks-qa -n dev logs -f orders-api-7d9f5b-k9x2p -c istio-proxy"),
            "the line copies what the worker was last told, not the choice in flight"
        );

        screen.list_mut().cursor.focus(1);
        let next = screen.log_target(0, &data).unwrap();
        assert_eq!(next.container, None);
        screen.begin_follow(Some(next));
        screen.next_container(&mut shell, &data);
        assert_eq!(
            shell.notification().map(|(said, _)| said),
            Some("orders-api-7d9f5b-abc12 has one container")
        );

        screen.toggle_previous(&mut shell);
        assert!(screen.log_target(0, &data).unwrap().previous);
        assert!(screen.previous());
        assert!(
            screen
                .kubectl_line(&tab(), &data)
                .unwrap()
                .ends_with("logs -f orders-api-7d9f5b-abc12 -p")
        );
    }

    #[test]
    fn d_and_v_fetch_a_text_once_per_object_and_the_pane_shows_it_for_that_object() {
        let data = data();
        let mut screen = ScopeScreen::new(false);
        screen.refilter(&data);
        let object = screen.selected_object(&data).unwrap();
        assert_eq!(object.slash(), "pod/billing-worker-1a2b3c-old01");

        let request = screen.show_text(0, TextKind::Describe, &data);
        assert_eq!(
            request,
            Some(Request::Describe {
                scope: 0,
                object: object.clone()
            })
        );
        assert!(screen.pane_open);
        assert_eq!(screen.pane, PaneText::Describe);
        assert!(screen.text_pending(TextKind::Describe, &object));
        assert_eq!(screen.log_target(0, &data), None, "nothing is followed");

        screen.set_text(
            TextKind::Describe,
            object.clone(),
            Ok(vec!["Name: x".to_owned()]),
        );
        assert!(!screen.text_pending(TextKind::Describe, &object));
        assert_eq!(
            screen.text(TextKind::Describe, &object),
            Some(&Ok(vec!["Name: x".to_owned()]))
        );
        assert_eq!(
            screen.show_text(0, TextKind::Describe, &data),
            None,
            "already on file"
        );
        assert_eq!(
            screen.show_text(0, TextKind::Yaml, &data),
            Some(Request::Yaml {
                scope: 0,
                object: object.clone()
            }),
        );
        assert_eq!(screen.pane, PaneText::Yaml);
        assert_eq!(
            screen.kubectl_line(&tab(), &data).as_deref(),
            Some("kubectl --context aks-qa -n dev get pod/billing-worker-1a2b3c-old01 -o yaml")
        );
        // On an event, the text is about what the event is about.
        screen.set_kind(Kind::Events);
        screen.refilter(&data);
        assert_eq!(
            screen.show_text(0, TextKind::Describe, &data),
            Some(Request::Describe {
                scope: 0,
                object: ObjectRef::pod(&key("qa", "dev", "orders-api-7d9f5b-abc12"))
            })
        );
        screen.on_refresh();
        assert!(screen.text(TextKind::Describe, &object).is_none());
    }

    #[test]
    fn scrolling_up_leaves_follow_mode_and_coming_back_to_the_tail_resumes_it() {
        let data = data();
        let mut screen = ScopeScreen::new(false);
        let mut shell = Shell::default();
        screen.refilter(&data);
        screen.toggle_log();
        let target = screen.log_target(0, &data).unwrap();
        screen.begin_follow(Some(target.clone()));
        screen.append_log(
            &target,
            (0..50).map(|line| format!("line {line}")).collect(),
            false,
        );
        screen.pane_scroll.set_viewport(10, 50);
        screen.pane_scroll.scroll_to(40);
        shell.focus = Focus::Details;
        let press = |screen: &mut ScopeScreen, shell: &mut Shell, code| {
            screen.handle_key(
                shell,
                &data,
                crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
            );
        };
        press(
            &mut screen,
            &mut shell,
            crossterm::event::KeyCode::Char('k'),
        );
        assert!(!screen.log_following());
        assert_eq!(screen.pane_scroll.offset, 39);
        press(
            &mut screen,
            &mut shell,
            crossterm::event::KeyCode::Char('j'),
        );
        assert!(screen.log_following(), "back at the tail");
        screen.scroll_pane(-20);
        assert!(!screen.log_following());
        press(&mut screen, &mut shell, crossterm::event::KeyCode::End);
        assert!(screen.log_following());
        screen.toggle_zoom();
        assert!(screen.pane_zoom);
        screen.close_pane();
        assert!(!screen.pane_zoom && !screen.pane_open);
    }

    #[test]
    fn capital_s_walks_the_columns_and_capital_r_turns_the_sort_over() {
        let data = data();
        let mut shell = Shell::default();
        let mut screen = ScopeScreen::new(false);
        screen.refilter(&data);
        let press = |screen: &mut ScopeScreen, shell: &mut Shell, code| {
            screen.handle_key(
                shell,
                &data,
                crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
            )
        };
        let was = screen.list().sort;
        press(
            &mut screen,
            &mut shell,
            crossterm::event::KeyCode::Char('R'),
        );
        assert!(screen.list().descending);
        assert_eq!(screen.list().sort, was, "R keeps the column");
        press(
            &mut screen,
            &mut shell,
            crossterm::event::KeyCode::Char('S'),
        );
        assert!(!screen.list().descending, "a new column starts ascending");
    }
}
